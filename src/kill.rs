//! Stopping whatever holds a port: signal the process (SIGTERM, escalating to
//! SIGKILL), or `docker stop` the container when Docker owns the socket.

use std::collections::BTreeMap;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

use crate::scan::PortEntry;
use crate::util;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Signal {
        pid: u32,
        /// Guards against signalling a different process that reused the pid.
        start_time: Option<u64>,
        name: String,
        ports: Vec<u16>,
    },
    /// `docker stop` / `podman stop` the container publishing the ports.
    ContainerStop {
        runtime: &'static str,
        container: String,
        ports: Vec<u16>,
    },
    Skip {
        reason: String,
        ports: Vec<u16>,
    },
}

impl Action {
    pub fn ports(&self) -> &[u16] {
        match self {
            Action::Signal { ports, .. }
            | Action::ContainerStop { ports, .. }
            | Action::Skip { ports, .. } => ports,
        }
    }

    /// Short description for confirmation prompts, e.g. `node (pid 4242)`.
    pub fn describe(&self) -> String {
        match self {
            Action::Signal { pid, name, .. } => format!("{name} (pid {pid})"),
            Action::ContainerStop {
                runtime, container, ..
            } => format!("{runtime} container {container}"),
            Action::Skip { reason, .. } => reason.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct KillOptions {
    /// Send SIGKILL straight away instead of SIGTERM first.
    pub force: bool,
    /// How long to wait after SIGTERM before escalating to SIGKILL.
    pub timeout: Duration,
}

impl Default for KillOptions {
    fn default() -> Self {
        Self {
            force: false,
            timeout: Duration::from_secs(3),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Exited after the first signal.
    Exited,
    /// Ignored SIGTERM and had to be SIGKILLed.
    Killed,
    /// Already gone by the time we got to it.
    Gone,
    /// `docker stop` / `podman stop` succeeded.
    Stopped,
    Skipped(String),
    Failed(String),
}

impl Outcome {
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            Outcome::Exited | Outcome::Killed | Outcome::Gone | Outcome::Stopped
        )
    }
}

/// Decide what to do for each entry. One action per process (a process on
/// several ports is signalled once) and one per container. Container
/// runtimes themselves (Docker Desktop, Podman's gvproxy) are never killed.
pub fn plan<'a>(entries: impl IntoIterator<Item = &'a PortEntry>) -> Vec<Action> {
    let me = std::process::id();
    let mut procs: BTreeMap<u32, Action> = BTreeMap::new();
    let mut containers: BTreeMap<(&'static str, String), Vec<u16>> = BTreeMap::new();
    let mut skips = Vec::new();

    for e in entries {
        if let Some(runtime) = e.container_runtime() {
            match &e.container {
                Some(c) => push_port(containers.entry((runtime, c.clone())).or_default(), e.port),
                None if e.container_lookup_failed => skips.push(Action::Skip {
                    reason: format!(
                        ":{} belongs to {runtime} ({}), but `{runtime} ps` didn't answer, so \
                         unporter can't tell which container publishes it; stop it with \
                         `{runtime} stop <container>`",
                        e.port, e.process
                    ),
                    ports: vec![e.port],
                }),
                None => skips.push(Action::Skip {
                    reason: format!(
                        ":{} is held by {runtime} ({}) but no running container publishes it; \
                         unporter won't kill the container engine itself",
                        e.port, e.process
                    ),
                    ports: vec![e.port],
                }),
            }
            continue;
        }
        if e.pid <= 1 || e.pid == me {
            skips.push(Action::Skip {
                reason: format!(
                    ":{} is held by {} (pid {}), which unporter won't kill",
                    e.port, e.process, e.pid
                ),
                ports: vec![e.port],
            });
            continue;
        }
        match procs.entry(e.pid).or_insert_with(|| Action::Signal {
            pid: e.pid,
            start_time: e.start_time,
            name: e.process.clone(),
            ports: Vec::new(),
        }) {
            Action::Signal { ports, .. } => push_port(ports, e.port),
            _ => unreachable!(),
        }
    }

    let mut actions: Vec<Action> = procs.into_values().collect();
    actions.extend(containers.into_iter().map(|((runtime, container), ports)| {
        Action::ContainerStop {
            runtime,
            container,
            ports,
        }
    }));
    actions.extend(skips);
    actions
}

fn push_port(ports: &mut Vec<u16>, port: u16) {
    if !ports.contains(&port) {
        ports.push(port);
        ports.sort_unstable();
    }
}

pub fn execute(action: &Action, opts: KillOptions) -> Outcome {
    match action {
        Action::Signal {
            pid, start_time, ..
        } => signal_process(*pid, *start_time, opts),
        Action::ContainerStop {
            runtime, container, ..
        } => container_stop(runtime, container),
        Action::Skip { reason, .. } => Outcome::Skipped(reason.clone()),
    }
}

fn container_stop(runtime: &str, container: &str) -> Outcome {
    match util::output_with_timeout(
        Command::new(runtime).args(["stop", container]),
        Duration::from_secs(30),
    ) {
        Some(_) => Outcome::Stopped,
        None => Outcome::Failed(format!("`{runtime} stop {container}` failed or timed out")),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum State {
    Running,
    Exited,
    /// The pid now belongs to a different process.
    Reused,
}

fn state(sys: &mut System, pid: u32, start_time: Option<u64>) -> State {
    let spid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[spid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    match sys.process(spid) {
        None => State::Exited,
        Some(p) if matches!(p.status(), ProcessStatus::Zombie | ProcessStatus::Dead) => {
            State::Exited
        }
        Some(p) => match start_time {
            Some(t) if p.start_time() != t => State::Reused,
            _ => State::Running,
        },
    }
}

fn wait_for_exit(sys: &mut System, pid: u32, start_time: Option<u64>, timeout: Duration) -> bool {
    // A huge --timeout would overflow Instant; treat it as "wait forever".
    let deadline = Instant::now().checked_add(timeout);
    loop {
        if state(sys, pid, start_time) != State::Running {
            return true;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn signal_process(pid: u32, start_time: Option<u64>, opts: KillOptions) -> Outcome {
    let mut sys = System::new();
    match state(&mut sys, pid, start_time) {
        State::Exited => return Outcome::Gone,
        State::Reused => {
            return Outcome::Failed(format!(
                "pid {pid} now belongs to a different process; run unporter again"
            ));
        }
        State::Running => {}
    }

    let first = if opts.force { Sig::Kill } else { Sig::Term };
    if let Err(outcome) = send(&sys, pid, first) {
        return outcome;
    }
    let grace = if opts.force {
        Duration::from_secs(2)
    } else {
        opts.timeout
    };
    if wait_for_exit(&mut sys, pid, start_time, grace) {
        return Outcome::Exited;
    }
    if opts.force {
        return Outcome::Failed(format!("pid {pid} is still running after SIGKILL"));
    }
    if let Err(outcome) = send(&sys, pid, Sig::Kill) {
        return outcome;
    }
    if wait_for_exit(&mut sys, pid, start_time, Duration::from_secs(2)) {
        Outcome::Killed
    } else {
        Outcome::Failed(format!("pid {pid} is still running after SIGKILL"))
    }
}

#[derive(Clone, Copy)]
enum Sig {
    Term,
    Kill,
}

#[cfg(unix)]
fn send(_sys: &System, pid: u32, sig: Sig) -> Result<(), Outcome> {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill};
    let signal = match sig {
        Sig::Term => Signal::SIGTERM,
        Sig::Kill => Signal::SIGKILL,
    };
    let Ok(raw) = i32::try_from(pid) else {
        return Err(Outcome::Failed(format!("invalid pid {pid}")));
    };
    let target = nix::unistd::Pid::from_raw(raw);
    match kill(target, signal) {
        Ok(()) => {
            // A suspended (Ctrl-Z) process can't act on SIGTERM until it is
            // continued, which is what shells do for `kill %job` too.
            if matches!(sig, Sig::Term) {
                let _ = kill(target, Signal::SIGCONT);
            }
            Ok(())
        }
        Err(Errno::ESRCH) => Err(Outcome::Gone),
        Err(Errno::EPERM) => Err(Outcome::Failed(format!(
            "permission denied: pid {pid} belongs to another user (try again with sudo)"
        ))),
        Err(e) => Err(Outcome::Failed(format!("could not signal pid {pid}: {e}"))),
    }
}

#[cfg(not(unix))]
fn send(sys: &System, pid: u32, _sig: Sig) -> Result<(), Outcome> {
    // Windows has no SIGTERM; TerminateProcess is the only option.
    match sys.process(Pid::from_u32(pid)) {
        None => Err(Outcome::Gone),
        Some(p) if p.kill() => Ok(()),
        Some(_) => Err(Outcome::Failed(format!(
            "could not terminate pid {pid} (try an elevated terminal)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(port: u16, pid: u32) -> PortEntry {
        PortEntry {
            port,
            addresses: vec!["127.0.0.1".parse().unwrap()],
            pid,
            process: "node".into(),
            command: vec![],
            exe: None,
            cwd: None,
            project: None,
            kind: None,
            container: None,
            container_lookup_failed: false,
            parent_pid: None,
            parent_command: None,
            start_time: Some(100),
            uptime_secs: None,
            memory_bytes: None,
            user: None,
            mine: true,
            dev: true,
        }
    }

    #[test]
    fn plan_dedupes_processes_and_containers() {
        let mut pg1 = entry(5432, 900);
        pg1.process = "com.docker.backend".into();
        pg1.kind = Some("Docker");
        pg1.container = Some("pg".into());
        let mut pg2 = pg1.clone();
        pg2.port = 5433;
        let mut orphan = pg1.clone();
        orphan.port = 9999;
        orphan.container = None;
        let entries = [
            entry(3000, 42),
            entry(3001, 42),
            entry(5173, 43),
            pg1,
            pg2,
            orphan,
            entry(80, 1),
        ];
        let actions = plan(&entries);
        assert_eq!(actions.len(), 5);
        assert!(
            matches!(&actions[0], Action::Signal { pid: 42, ports, .. } if ports == &[3000, 3001])
        );
        assert!(matches!(&actions[1], Action::Signal { pid: 43, .. }));
        assert!(
            matches!(&actions[2], Action::ContainerStop { runtime: "docker", container, ports } if container == "pg" && ports == &[5432, 5433])
        );
        assert!(matches!(&actions[3], Action::Skip { ports, .. } if ports == &[9999]));
        assert!(matches!(&actions[4], Action::Skip { ports, .. } if ports == &[80]));
    }

    #[test]
    fn container_runtimes_are_never_signalled() {
        let mut gvproxy = entry(8080, 700);
        gvproxy.process = "gvproxy".into();
        gvproxy.kind = Some("Podman");
        let actions = plan(&[gvproxy.clone()]);
        assert!(
            matches!(&actions[0], Action::Skip { reason, .. } if reason.contains("won't kill"))
        );

        gvproxy.container_lookup_failed = true;
        let actions = plan(&[gvproxy.clone()]);
        assert!(
            matches!(&actions[0], Action::Skip { reason, .. } if reason.contains("`podman ps` didn't answer"))
        );

        gvproxy.container = Some("web".into());
        let actions = plan(&[gvproxy]);
        assert!(
            matches!(&actions[0], Action::ContainerStop { runtime: "podman", container, .. } if container == "web")
        );
    }

    #[test]
    fn never_plans_to_kill_itself() {
        let actions = plan(&[entry(3000, std::process::id())]);
        assert!(matches!(&actions[0], Action::Skip { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn terminates_a_real_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        // Reap the child in the background so it doesn't linger as a zombie.
        let reaper = thread::spawn(move || child.wait());
        let outcome = signal_process(pid, None, KillOptions::default());
        assert_eq!(outcome, Outcome::Exited);
        reaper.join().unwrap().unwrap();
        assert_eq!(
            signal_process(pid, None, KillOptions::default()),
            Outcome::Gone
        );
    }

    #[cfg(unix)]
    #[test]
    fn escalates_to_sigkill_when_sigterm_is_ignored() {
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30 & wait"])
            .spawn()
            .unwrap();
        let pid = child.id();
        thread::sleep(Duration::from_millis(200)); // let the trap install
        let reaper = thread::spawn(move || child.wait());
        let opts = KillOptions {
            force: false,
            timeout: Duration::from_millis(300),
        };
        assert_eq!(signal_process(pid, None, opts), Outcome::Killed);
        reaper.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stops_a_suspended_process_with_sigterm() {
        use nix::sys::signal::{Signal, kill};
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGSTOP).unwrap();
        thread::sleep(Duration::from_millis(100));
        let reaper = thread::spawn(move || child.wait());
        let opts = KillOptions {
            force: false,
            timeout: Duration::from_secs(2),
        };
        assert_eq!(signal_process(pid, None, opts), Outcome::Exited);
        reaper.join().unwrap().unwrap();
    }

    #[test]
    fn huge_timeouts_do_not_overflow() {
        let mut sys = System::new();
        // The pid is gone, so this returns immediately instead of panicking.
        assert!(wait_for_exit(&mut sys, u32::MAX - 1, None, Duration::MAX));
    }

    #[test]
    fn refuses_reused_pid() {
        let me = std::process::id();
        let outcome = signal_process(me, Some(1), KillOptions::default());
        assert!(matches!(outcome, Outcome::Failed(m) if m.contains("different process")));
    }
}
