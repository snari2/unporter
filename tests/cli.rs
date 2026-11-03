//! End-to-end tests that run the real `unporter` binary against a listener
//! owned by a child process.

use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const HELPER_ENV: &str = "UNPORTER_TEST_LISTENER";

fn unporter(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unporter"))
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run unporter")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A child process (this test binary, re-executed) listening on a random port.
struct Listener {
    child: Child,
    port: u16,
}

impl Listener {
    fn spawn() -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "listener_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn helper");
        let reader = BufReader::new(child.stdout.take().unwrap());
        let port = reader
            .lines()
            .map_while(Result::ok)
            .find_map(|l| Some(l.split_once("PORT=")?.1.trim().parse().unwrap()))
            .expect("helper printed its port");
        Listener { child, port }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait for the child to exit, up to `timeout`.
    fn exited_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Not a real test: the body of the child process used by [`Listener`].
#[test]
#[ignore]
fn listener_helper() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    println!("PORT={}", l.local_addr().unwrap().port());
    std::thread::sleep(Duration::from_secs(120));
}

fn unused_port() -> u16 {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    l.local_addr().unwrap().port()
}

#[test]
fn info_json_identifies_the_listening_process() {
    let l = Listener::spawn();
    let out = unporter(&["info", &l.port.to_string(), "--json"]);
    assert!(out.status.success(), "{out:?}");
    let entries: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entry = &entries[0];
    assert_eq!(entry["port"], l.port);
    assert_eq!(entry["pid"], l.pid());
    assert_eq!(entry["addresses"][0], "127.0.0.1");
    assert_eq!(entry["mine"], true);
}

#[test]
fn port_shorthand_prints_details() {
    let l = Listener::spawn();
    let out = unporter(&[&format!(":{}", l.port)]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.contains(&format!(":{}", l.port)), "{text}");
    assert!(text.contains(&l.pid().to_string()), "{text}");
    assert!(
        text.contains(&format!("http://localhost:{}", l.port)),
        "{text}"
    );
}

#[test]
fn info_on_unused_port_exits_1() {
    let port = unused_port();
    let out = unporter(&[&port.to_string()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("Nothing is listening"));
}

#[test]
fn list_all_json_includes_the_listener() {
    let l = Listener::spawn();
    let out = unporter(&["list", "--all", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["port"] == l.port && e["pid"] == l.pid())
    );
}

#[test]
fn kill_frees_the_port() {
    let mut l = Listener::spawn();
    let out = unporter(&["kill", &l.port.to_string(), "--yes"]);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("freed"), "{}", stdout(&out));
    assert!(l.exited_within(Duration::from_secs(5)));
    let after = unporter(&["info", &l.port.to_string()]);
    assert_eq!(after.status.code(), Some(1));
}

#[test]
fn kill_dry_run_leaves_the_process_alone() {
    let mut l = Listener::spawn();
    let out = unporter(&["kill", &l.port.to_string(), "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("would stop"));
    assert!(!l.exited_within(Duration::from_millis(300)));
}

#[test]
fn killing_a_free_port_succeeds() {
    let port = unused_port();
    let out = unporter(&["kill", &port.to_string()]);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("already free"));
}

#[test]
fn free_prints_bindable_ports() {
    let out = unporter(&["free", "--from", "20000", "-n", "2"]);
    assert!(out.status.success(), "{out:?}");
    let ports: Vec<u16> = stdout(&out).lines().map(|l| l.parse().unwrap()).collect();
    assert_eq!(ports.len(), 2);
    assert!(ports[0] >= 20000 && ports[1] > ports[0]);
}

#[test]
fn wait_succeeds_for_listener_and_times_out_otherwise() {
    let l = Listener::spawn();
    let out = unporter(&["wait", &l.port.to_string(), "-t", "5"]);
    assert!(out.status.success(), "{out:?}");

    let port = unused_port();
    let start = Instant::now();
    let out = unporter(&["wait", &port.to_string(), "-t", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(start.elapsed() < Duration::from_secs(4));
}

#[test]
fn bad_arguments_exit_2() {
    assert_eq!(unporter(&["info", "notaport"]).status.code(), Some(2));
    assert_eq!(unporter(&["kill"]).status.code(), Some(2));
    assert_eq!(unporter(&["kill", "0"]).status.code(), Some(2));
    assert_eq!(unporter(&["kill", "3005-3000"]).status.code(), Some(2));
}

#[test]
fn completions_generate() {
    for shell in ["bash", "zsh", "fish"] {
        let out = unporter(&["completions", shell]);
        assert!(out.status.success());
        assert!(stdout(&out).contains("unporter"));
    }
}
