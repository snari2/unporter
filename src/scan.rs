//! Find listening TCP sockets and describe the process behind each one.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};

use crate::{kind, project, project::Project, util};

/// One process listening on one port (possibly on several addresses).
#[derive(Debug, Clone, Serialize)]
pub struct PortEntry {
    pub port: u16,
    /// Every address this process listens on for `port`, e.g. `127.0.0.1` and `::1`.
    pub addresses: Vec<IpAddr>,
    pub pid: u32,
    pub process: String,
    pub command: Vec<String>,
    pub exe: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub project: Option<Project>,
    /// Framework or server guess, e.g. `Next.js`, `Flask`, `PostgreSQL`.
    pub kind: Option<&'static str>,
    /// Docker container publishing this port, when Docker owns the socket.
    pub container: Option<String>,
    pub parent_pid: Option<u32>,
    pub parent_command: Option<String>,
    /// Process start time in seconds since the Unix epoch.
    pub start_time: Option<u64>,
    pub uptime_secs: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub user: Option<String>,
    /// Owned by the user running porthog.
    pub mine: bool,
    /// Looks like a development server rather than a system service or
    /// desktop app. `porthog list` hides the rest unless `--all` is given.
    pub dev: bool,
}

impl PortEntry {
    pub fn url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    pub fn is_docker(&self) -> bool {
        self.container.is_some() || self.kind == Some("Docker")
    }

    /// `localhost`, `*` (all interfaces) or the specific addresses.
    pub fn bind_label(&self) -> String {
        if self.addresses.iter().any(IpAddr::is_unspecified) {
            "*".to_owned()
        } else if self.addresses.iter().all(IpAddr::is_loopback) {
            "localhost".to_owned()
        } else {
            self.addresses
                .iter()
                .map(IpAddr::to_string)
                .collect::<Vec<_>>()
                .join(",")
        }
    }

    pub fn command_line(&self) -> String {
        if self.command.is_empty() {
            self.process.clone()
        } else {
            self.command.join(" ")
        }
    }
}

/// A listening socket as reported by the OS, before process enrichment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawListener {
    pub port: u16,
    pub pid: u32,
    pub addr: IpAddr,
    pub name: String,
}

/// Every listening TCP socket on the machine that we're allowed to see,
/// enriched with process details and sorted by port. Non-dev entries are
/// included; filter on [`PortEntry::dev`] to hide them.
pub fn scan() -> Result<Vec<PortEntry>> {
    let raw = raw_listeners()?;
    Ok(enrich(group(raw)))
}

/// All entries listening on `port`.
pub fn scan_port(port: u16) -> Result<Vec<PortEntry>> {
    let raw = raw_listeners()?.into_iter().filter(|l| l.port == port);
    Ok(enrich(group(raw)))
}

pub fn raw_listeners() -> Result<Vec<RawListener>> {
    let all = listeners::get_all().map_err(|e| anyhow!("could not list open sockets: {e}"))?;
    Ok(all
        .into_iter()
        .filter(|l| {
            l.protocol == listeners::Protocol::TCP && l.state == listeners::SocketState::Listen
        })
        .map(|l| RawListener {
            port: l.socket.port(),
            pid: l.process.pid,
            addr: normalize_ip(l.socket.ip()),
            name: l.process.name,
        })
        .collect())
}

/// Map IPv4-mapped IPv6 addresses (`::ffff:127.0.0.1`) back to IPv4.
fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

pub struct Group {
    pub port: u16,
    pub pid: u32,
    pub addrs: BTreeSet<IpAddr>,
    pub name: String,
}

/// Merge sockets of the same process on the same port (IPv4 + IPv6, or
/// several interfaces) into one group, ordered by port then pid.
pub fn group(raw: impl IntoIterator<Item = RawListener>) -> Vec<Group> {
    let mut map: BTreeMap<(u16, u32), Group> = BTreeMap::new();
    for l in raw {
        if l.port == 0 {
            continue;
        }
        map.entry((l.port, l.pid))
            .or_insert_with(|| Group {
                port: l.port,
                pid: l.pid,
                addrs: BTreeSet::new(),
                name: l.name.clone(),
            })
            .addrs
            .insert(l.addr);
    }
    map.into_values().collect()
}

fn enrich(groups: Vec<Group>) -> Vec<PortEntry> {
    if groups.is_empty() {
        return Vec::new();
    }
    let mut sys = System::new();
    let pids: Vec<Pid> = groups
        .iter()
        .map(|g| Pid::from_u32(g.pid))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .with_user(UpdateKind::Always)
            .with_memory(),
    );

    // Parents (e.g. `npm run dev` above the `node` that owns the socket).
    let parents: Vec<Pid> = pids
        .iter()
        .filter_map(|p| sys.process(*p)?.parent())
        .filter(|p| !pids.contains(p))
        .collect();
    let my_pid = sysinfo::get_current_pid().ok();
    let mut extra = parents;
    extra.extend(my_pid.filter(|p| !pids.contains(p)));
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&extra),
        false,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );

    let my_uid = my_pid.and_then(|p| sys.process(p)?.user_id().cloned());
    let users = Users::new_with_refreshed_list();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut entries: Vec<PortEntry> = groups
        .into_iter()
        .map(|g| {
            let proc = sys.process(Pid::from_u32(g.pid));
            let command: Vec<String> = proc
                .map(|p| p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            let process = proc
                .map(|p| p.name().to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or(g.name);
            let cwd = proc.and_then(|p| p.cwd()).map(PathBuf::from);
            let exe = proc.and_then(|p| p.exe()).map(PathBuf::from);
            let project = cwd
                .as_deref()
                .and_then(project::detect)
                // Binaries run from a Cargo target dir belong to that crate even
                // when started from elsewhere.
                .or_else(|| {
                    let exe = exe.as_deref()?;
                    let s = exe.to_string_lossy();
                    (s.contains("/target/debug/") || s.contains("/target/release/"))
                        .then(|| project::detect(exe.parent()?))
                        .flatten()
                });
            let kind = kind::detect(&process, &command);
            let uid = proc.and_then(|p| p.user_id());
            let parent_pid = proc.and_then(|p| p.parent());
            let parent_command = parent_pid.and_then(|pp| sys.process(pp)).map(|pp| {
                let cmd: Vec<String> = pp.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect();
                if cmd.is_empty() {
                    pp.name().to_string_lossy().into_owned()
                } else {
                    cmd.join(" ")
                }
            });
            let start_time = proc.map(|p| p.start_time()).filter(|t| *t > 0);
            PortEntry {
                port: g.port,
                addresses: g.addrs.into_iter().collect(),
                pid: g.pid,
                dev: kind.is_some() || project.is_some(),
                process,
                command,
                exe,
                cwd,
                project,
                kind,
                container: None,
                parent_pid: parent_pid.map(|p| p.as_u32()),
                parent_command,
                start_time,
                uptime_secs: start_time.map(|t| now.saturating_sub(t)),
                memory_bytes: proc.map(|p| p.memory()).filter(|m| *m > 0),
                user: uid
                    .and_then(|u| users.get_user_by_id(u))
                    .map(|u| u.name().to_owned()),
                mine: uid.is_some() && uid == my_uid.as_ref(),
            }
        })
        .collect();

    if entries.iter().any(PortEntry::is_docker) {
        let containers = docker_containers();
        for e in entries.iter_mut().filter(|e| e.is_docker()) {
            e.container = containers.get(&e.port).cloned();
            e.dev = true;
        }
    }
    entries
}

/// Host port -> container name for running Docker containers.
fn docker_containers() -> HashMap<u16, String> {
    let out = util::output_with_timeout(
        Command::new("docker").args(["ps", "--format", "{{.Names}}\t{{.Ports}}"]),
        Duration::from_secs(3),
    );
    out.map(|o| parse_docker_ps(&o)).unwrap_or_default()
}

/// Parse `docker ps --format '{{.Names}}\t{{.Ports}}'` output, e.g.
/// `pg\t0.0.0.0:5432->5432/tcp, [::]:5432->5432/tcp`.
pub fn parse_docker_ps(output: &str) -> HashMap<u16, String> {
    let mut map = HashMap::new();
    for line in output.lines() {
        let Some((name, ports)) = line.split_once('\t') else {
            continue;
        };
        for mapping in ports.split(',') {
            let Some((host, container)) = mapping.trim().split_once("->") else {
                continue;
            };
            if !container.ends_with("/tcp") {
                continue;
            }
            let Some((_, host_ports)) = host.rsplit_once(':') else {
                continue;
            };
            let range = match host_ports.split_once('-') {
                Some((a, b)) => a.parse::<u16>().ok().zip(b.parse::<u16>().ok()),
                None => host_ports.parse::<u16>().ok().map(|p| (p, p)),
            };
            if let Some((start, end)) = range {
                for p in start..=end {
                    map.insert(p, name.to_owned());
                }
            }
        }
    }
    map
}

/// True if something accepts TCP connections on localhost:`port` (IPv4 or IPv6).
pub fn is_listening(port: u16) -> bool {
    let timeout = Duration::from_millis(150);
    [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)]
        .into_iter()
        .any(|ip| TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout).is_ok())
}

/// True if a dev server could bind `port` on localhost right now.
pub fn is_free(port: u16, known_listening: &HashSet<u16>) -> bool {
    if port == 0 || known_listening.contains(&port) || is_listening(port) {
        return false;
    }
    let v4_ok = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
        && TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok();
    let v6_ok = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(_) => true,
        // No IPv6 on this machine: nothing can be squatting on it either.
        Err(e) => e.kind() != std::io::ErrorKind::AddrInUse,
    };
    v4_ok && v6_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(port: u16, pid: u32, addr: &str) -> RawListener {
        RawListener {
            port,
            pid,
            addr: addr.parse().unwrap(),
            name: "node".into(),
        }
    }

    #[test]
    fn groups_ipv4_and_ipv6_of_same_process() {
        let groups = group([
            raw(5173, 10, "::1"),
            raw(3000, 20, "127.0.0.1"),
            raw(5173, 10, "127.0.0.1"),
            raw(3000, 21, "::"),
            raw(0, 30, "0.0.0.0"),
        ]);
        let keys: Vec<_> = groups.iter().map(|g| (g.port, g.pid, g.addrs.len())).collect();
        assert_eq!(keys, vec![(3000, 20, 1), (3000, 21, 1), (5173, 10, 2)]);
    }

    #[test]
    fn ipv4_mapped_addresses_are_normalized() {
        let ip: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert_eq!(normalize_ip(ip), IpAddr::V4(Ipv4Addr::LOCALHOST));
        let ip: IpAddr = "::1".parse().unwrap();
        assert_eq!(normalize_ip(ip), ip);
    }

    #[test]
    fn docker_ps_parsing() {
        let out = "pg\t0.0.0.0:5432->5432/tcp, [::]:5432->5432/tcp\n\
                   web\t127.0.0.1:8080-8082->80-82/tcp\n\
                   dns\t0.0.0.0:53->53/udp\n\
                   internal\t6379/tcp\n";
        let map = parse_docker_ps(out);
        assert_eq!(map.get(&5432).map(String::as_str), Some("pg"));
        assert_eq!(map.get(&8081).map(String::as_str), Some("web"));
        assert_eq!(map.get(&53), None);
        assert_eq!(map.get(&6379), None);
        assert_eq!(map.len(), 4);
    }

    fn entry(addrs: &[&str]) -> PortEntry {
        PortEntry {
            port: 3000,
            addresses: addrs.iter().map(|a| a.parse().unwrap()).collect(),
            pid: 1,
            process: "node".into(),
            command: vec![],
            exe: None,
            cwd: None,
            project: None,
            kind: None,
            container: None,
            parent_pid: None,
            parent_command: None,
            start_time: None,
            uptime_secs: None,
            memory_bytes: None,
            user: None,
            mine: true,
            dev: true,
        }
    }

    #[test]
    fn bind_labels() {
        assert_eq!(entry(&["127.0.0.1", "::1"]).bind_label(), "localhost");
        assert_eq!(entry(&["0.0.0.0"]).bind_label(), "*");
        assert_eq!(entry(&["::", "127.0.0.1"]).bind_label(), "*");
        assert_eq!(entry(&["192.168.1.5"]).bind_label(), "192.168.1.5");
    }

    #[test]
    fn listening_and_free_detection() {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(is_listening(port));
        assert!(!is_free(port, &HashSet::new()));
        drop(l);
        assert!(!is_free(port, &HashSet::from([port])));
    }

    #[test]
    fn scan_finds_our_own_listener() {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let entries = scan_port(port).expect("scan works");
        let me = std::process::id();
        let e = entries.iter().find(|e| e.pid == me).expect("own listener visible");
        assert!(e.mine);
        assert_eq!(e.bind_label(), "localhost");
        assert!(e.start_time.is_some());
    }
}
