//! `maxplayer team watch` through the shipped binary: a worker stuck in a relay step is restarted
//! by its supervisor, a second watch on the same home is refused, and killing the supervisor stops
//! its worker. The relay is a TCP listener that never answers the websocket handshake.
#![cfg(all(feature = "wallet", target_os = "linux"))]

use std::io::Read;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn watch(home: &std::path::Path, relay: &str, stall_secs: u32, stderr: Stdio) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("MAXPLAYER_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("MAXPLAYER_TEAM_WATCH_STALL_SECS", stall_secs.to_string())
        .args(["team", "--home"])
        .arg(home)
        .args(["--relay", relay, "watch"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap()
}
/// Live children of `pid` (from /proc, by parent pid).
fn children(pid: u32) -> Vec<u32> {
    let mut out = vec![];
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{child}/stat")) else { continue };
        // Fields after the parenthesised command: state, ppid, …
        let rest = &stat[stat.rfind(')').unwrap() + 2..];
        let mut fields = rest.split(' ');
        let state = fields.next().unwrap();
        if fields.next().unwrap().parse::<u32>().unwrap() == pid && state != "Z" {
            out.push(child);
        }
    }
    out
}
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s[s.rfind(')').unwrap() + 2..].starts_with('Z'))
}

#[test]
fn a_stuck_watch_worker_is_restarted_and_dies_with_its_supervisor() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = format!("ws://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = vec![];
        for s in listener.incoming().flatten() {
            held.push(s);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let log = dir.path().join("stderr.log");
    let mut supervisor = watch(&home, &relay, 2, Stdio::from(std::fs::File::create(&log).unwrap()));

    // Two stalls in a row, each a fresh worker under the same supervisor.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut workers = std::collections::BTreeSet::new();
    loop {
        workers.extend(children(supervisor.id()));
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.matches("(stuck in connect); restarting").count() >= 2 && workers.len() >= 2 {
            break;
        }
        assert!(supervisor.try_wait().unwrap().is_none(), "supervisor exited: {text}");
        assert!(Instant::now() < deadline, "no two restarts: {text}");
        std::thread::sleep(Duration::from_millis(100));
    }

    // A second watch on the same home is refused while the first runs.
    let mut second = watch(&home, &relay, 2, Stdio::piped());
    let refused_by = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = second.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > refused_by {
            second.kill().unwrap();
            panic!("a second watch on the same home kept running");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut err = String::new();
    second.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(err.contains("already running"), "{err}");

    // SAFETY: plain kill of a child process this test started.
    assert_eq!(unsafe { libc::kill(supervisor.id() as i32, libc::SIGKILL) }, 0);
    supervisor.wait().unwrap();

    // Killing the supervisor stops its worker at once. A long stall limit here, so the worker's
    // own watchdog cannot be what ends it; the home's watch claim is free again too.
    let mut supervisor = watch(&home, &relay, 60, Stdio::null());
    let deadline = Instant::now() + Duration::from_secs(30);
    let worker = loop {
        if let Some(&w) = children(supervisor.id()).first() {
            break w;
        }
        assert!(supervisor.try_wait().unwrap().is_none(), "second supervisor exited (claim still held?)");
        assert!(Instant::now() < deadline, "no worker running");
        std::thread::sleep(Duration::from_millis(50));
    };
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: plain kill of a child process this test started.
    assert_eq!(unsafe { libc::kill(supervisor.id() as i32, libc::SIGKILL) }, 0);
    supervisor.wait().unwrap();
    let gone_by = Instant::now() + Duration::from_secs(2);
    while alive(worker) {
        assert!(Instant::now() < gone_by, "worker {worker} outlived its supervisor");
        std::thread::sleep(Duration::from_millis(50));
    }
}
