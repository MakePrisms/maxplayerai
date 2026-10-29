//! `AcpDriver::shutdown` must TERM the agent's whole process group, not just the direct child.
//!
//! Field report (seller sandbox image, multi-turn job parked after "container alive, 1202s
//! elapsed"): shutdown printed a `kill` usage dump and the agent's grandchildren (the claude CLI an
//! ACP adapter launches) survived inside the parked container. Cause: shutdown shelled out to
//! `kill -s TERM -- -<pgid>`, and procps-ng 4.0.6's `kill` — the nixpkgs one in the sandbox image —
//! rejects `-s TERM` / `--signal TERM` with a NEGATIVE (process-group) operand: usage, exit 1,
//! nothing signalled. Ubuntu's procps 4.0.4 accepts the same argv, so CI never saw it.
//!
//! The test drives the real driver against a stub agent that backgrounds a grandchild (a `sleep`
//! in the agent's own process group, exactly like an adapter's CLI child), then shuts the driver
//! down and requires the grandchild to be gone.
//!
//! `kill` on PATH is made to behave like procps-ng 4.0.6 for the whole test binary, so a head that
//! still relies on an external `kill` accepting the old form fails here on any host:
//! - default: a shim (written by the test) that rejects `-s`/`--signal` with a negative operand the
//!   way 4.0.6 does and passes every other form to the system `kill`;
//! - `MAXPLAYER_TEST_KILL_DIR=<dir>`: put that directory first on PATH instead, e.g. the real
//!   `/nix/store/…-procps-4.0.6/bin`.
//!
//! Linux-only: it reads `/proc/<pid>/stat` to observe the grandchild.

#![cfg(all(feature = "acp", target_os = "linux"))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use maxplayer_core::driver::{AcpDriver, AgentCommand, Driver, PermissionOutcome};

/// procps-ng 4.0.6's observable behaviour for the argv shapes that matter here: `-s SIG` or
/// `--signal SIG` followed (after an optional `--`) by a negative operand prints usage and exits 1
/// without signalling anything. `-SIG -- -PG` and `-15 -- -PG` work, as does any positive pid.
const PROCPS_406_KILL_SHIM: &str = r#"#!/bin/sh
named=0
for a in "$@"; do
  case "$a" in
    -s|--signal) named=1 ;;
    --) ;;
    -[0-9]*) if [ "$named" = 1 ]; then
        echo "Usage: kill [options] <pid> [...]  (procps-ng 4.0.6 shim: -s/--signal with a group operand)" >&2
        exit 1
      fi ;;
  esac
done
for k in /bin/kill /usr/bin/kill; do
  if [ -x "$k" ]; then exec "$k" "$@"; fi
done
echo "no system kill" >&2
exit 1
"#;

fn stat_fields(pid: u32) -> Option<(char, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut after_comm = stat.rsplit_once(')')?.1.split_whitespace();
    let state = after_comm.next()?.chars().next()?;
    let _ppid = after_comm.next()?;
    let pgrp = after_comm.next()?.parse().ok()?;
    Some((state, pgrp))
}

/// Gone from the process table, or a zombie awaiting its (new) parent's reap: either way it no
/// longer runs.
fn is_dead(pid: u32) -> bool {
    !matches!(stat_fields(pid), Some((state, _)) if state != 'Z' && state != 'X')
}

fn read_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "{} was never written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Put a procps-4.0.6-like `kill` first on PATH for this whole process. This binary holds only
/// this one test, and it runs before any runtime thread exists, so mutating the environment races
/// nothing.
fn install_procps_406_kill(dir: &Path) {
    let kill_dir = match std::env::var_os("MAXPLAYER_TEST_KILL_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            use std::os::unix::fs::PermissionsExt;
            let shim = dir.join("kill");
            std::fs::write(&shim, PROCPS_406_KILL_SHIM).expect("write kill shim");
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
                .expect("chmod kill shim");
            dir.to_path_buf()
        }
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    let joined =
        std::env::join_paths(std::iter::once(kill_dir).chain(std::env::split_paths(&path)))
            .expect("join PATH");
    // SAFETY: single-test binary, called before any other thread is started (see above).
    unsafe { std::env::set_var("PATH", joined) };
}

#[test]
fn shutdown_terms_the_agents_grandchildren_with_procps_4_0_6_kill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim_dir = dir.path().join("bin");
    std::fs::create_dir_all(&shim_dir).expect("create shim dir");
    install_procps_406_kill(&shim_dir);

    // Control: the PATH `kill` really rejects the old group form, so a red head is red for the
    // reason this test names and not by accident.
    {
        use std::os::unix::process::CommandExt;
        let mut victim = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn control sleep");
        let status = std::process::Command::new("kill")
            .args(["-s", "TERM", "--", &format!("-{}", victim.id())])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run PATH kill");
        assert!(
            !status.success(),
            "the PATH `kill` must reject `-s TERM -- -<pgid>` like procps-ng 4.0.6"
        );
        let _ = victim.kill();
        let _ = victim.wait();
    }

    let agent_pid_file = dir.path().join("agent.pid");
    let grandchild_pid_file = dir.path().join("grandchild.pid");
    let script = dir.path().join("stub-acp-agent.sh");
    // The grandchild is backgrounded with its stdio detached, so only a signal can end it; it
    // inherits the agent's process group (a non-interactive sh does no job control).
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             echo $$ > '{agent}'\n\
             sleep 300 </dev/null >/dev/null 2>&1 &\n\
             echo $! > '{grandchild}'\n\
             read _req\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":1}}}}'\n\
             while read _req; do :; done\n",
            agent = agent_pid_file.display(),
            grandchild = grandchild_pid_file.display(),
        ),
    )
    .expect("write stub agent");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let mut driver = AcpDriver::new(
            AgentCommand::new(
                "/bin/sh".into(),
                vec![script.to_string_lossy().into_owned()],
            ),
            PermissionOutcome::Allow,
            Duration::from_secs(30),
        );
        driver.ready().await.expect("stub agent initializes");

        let agent = read_pid(&agent_pid_file);
        let grandchild = read_pid(&grandchild_pid_file);
        // Control: the grandchild is alive and in the agent's group, which the agent leads — the
        // exact shape shutdown is supposed to clean up.
        let (state, pgrp) = stat_fields(grandchild).expect("grandchild running before shutdown");
        assert_ne!(state, 'Z', "grandchild already dead before shutdown");
        assert_eq!(
            pgrp, agent,
            "grandchild must share the agent's process group"
        );
        assert_eq!(
            stat_fields(agent).map(|(_, pgrp)| pgrp),
            Some(agent),
            "agent leads its group"
        );

        driver.shutdown().await.expect("shutdown");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !is_dead(grandchild) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survived = !is_dead(grandchild);
        if survived {
            // Do not leak the sleep past a red run.
            let _ = std::process::Command::new("kill")
                .args(["-9", &grandchild.to_string()])
                .status();
        }
        assert!(
            !survived,
            "grandchild {grandchild} of the ACP agent survived shutdown: the group TERM never \
             reached it (procps-ng 4.0.6 `kill` rejects `-s TERM -- -<pgid>`)"
        );
    });
}
