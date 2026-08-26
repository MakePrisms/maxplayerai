//! #905: `maxplayer sandbox-reap --seat <hex> --dry-run` SELECTS and prints; it must never remove.
//!
//! The claim under test is a negative — "no container was removed" — and a negative about a
//! subprocess cannot be proved by reading the code that spawns it. So this drives the real binary
//! against a **recording stand-in `docker`** on PATH: a script that logs every argv it is handed,
//! answers the three read commands with a fabricated holder, and would log a `rm` if one were ever
//! issued. `sandbox_netns` reaches docker through `Command::new("docker")`, which resolves on PATH,
//! so the stand-in is what the binary actually talks to.
//!
//! Two legs, because the negative alone can pass vacuously:
//!
//!   POSITIVE CONTROL — the same invocation WITHOUT `--dry-run` must log `docker rm`. Without this
//!   leg, a stand-in that was never reached, a PATH that did not take, or a selection that matched
//!   nothing would all make the dry-run assertion pass while proving nothing.
//!   THE PROPERTY — with `--dry-run`, the log holds the reads and NO `rm`, and stdout says so.
//!
//! Also vacuity-guarded on the way in: the fabricated holder id is a string only the stand-in can
//! produce, and both legs assert it appears in the binary's stdout. If the binary had talked to a
//! real docker, or to nothing, that id could not be there.
//!
//! RED-ON-REVERT: point the `--dry-run` branch in `sandbox_reap::run` at `reap_orphans` and the
//! `rm`-absent assertion fails.
//!
//! What this does NOT prove: that `docker rm` removes a real container, or that a real holder's
//! namespace is idle. Those need a docker daemon and live containers, which
//! `crates/maxplayer-core/tests/sandbox_netns_live.rs` is for — and that file is `#[ignore]`d, so
//! its assertions have never executed. This test measures the argv boundary and stops there.

#![cfg(all(unix, feature = "acp"))]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A fabricated 64-hex seat: the retired seat an operator would name.
const RETIRED_SEAT: &str = "dead00000000000000000000000000000000000000000000000000000000beef";

/// The holder the stand-in reports. Only the stand-in can produce this string, which is what makes
/// its appearance in stdout proof that the stand-in was reached.
const FAKE_HOLDER: &str = "c0ffee1111111111111111111111111111111111111111111111111111111111";

/// A private scratch dir for one leg of the test.
fn scratch(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "maxplayer-reap-dryrun-{tag}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("scratch dir");
    path
}

/// Install a recording stand-in `docker` in `bin`, and return the argv log path.
///
/// It answers the three reads `reapable_holders_live` makes — the labelled listing (tab-separated
/// `<id>\t<seat>`), the host-wide id listing, and the network-mode inspect — with a single holder
/// that is owned by `RETIRED_SEAT` and has nothing joined to it, so the selection has exactly one
/// candidate. A `rm` is logged and reported as succeeding: the point is to record that it was asked
/// for, and a failure would be indistinguishable from not asking.
fn install_recording_docker(bin: &Path) -> PathBuf {
    fs::create_dir_all(bin).expect("bin dir");
    let log = bin.join("argv.log");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$1" in
  ps)
    for arg in "$@"; do
      if [ "$arg" = "--format" ]; then
        printf '{holder}\t{seat}\n'
        exit 0
      fi
    done
    printf '{holder}\n'
    ;;
  inspect)
    # Nothing is joined to the holder's namespace: no `container:<id>` mode in the answer.
    printf 'bridge\n'
    ;;
  rm)
    ;;
esac
exit 0
"#,
        log = log.display(),
        holder = FAKE_HOLDER,
        seat = RETIRED_SEAT,
    );
    let docker = bin.join("docker");
    fs::write(&docker, script).expect("write stand-in docker");
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).expect("chmod stand-in");
    log
}

/// Run the real binary with `bin` as the ONLY PATH entry, so `docker` resolves to the stand-in and
/// a stray real docker cannot answer instead.
fn run_reap(bin: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_maxplayer"))
        .arg("sandbox-reap")
        .args(args)
        .env("PATH", bin)
        .output()
        .expect("run maxplayer sandbox-reap");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn logged_argv(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// POSITIVE CONTROL: a real reap of the same selection DOES issue `docker rm`. This is what makes
/// the dry-run leg below non-vacuous — it proves the stand-in is reached, the PATH took, and the
/// selection matched the holder.
#[test]
fn a_real_reap_issues_docker_rm_for_the_selected_holder() {
    let root = scratch("control");
    let log = install_recording_docker(&root);

    let (ok, out, err) = run_reap(&root, &["--seat", RETIRED_SEAT]);
    assert!(ok, "reap must succeed:\nstdout={out}\nstderr={err}");
    assert!(
        out.contains(FAKE_HOLDER),
        "the stand-in docker must be the one answering:\nstdout={out}"
    );
    assert!(out.contains("reaped"), "stdout={out}");

    let argv = logged_argv(&log);
    assert!(
        argv.iter().any(|line| line.starts_with("rm ") && line.contains(FAKE_HOLDER)),
        "a real reap must ask docker to remove the holder:\n{argv:#?}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// THE PROPERTY (#905): `--dry-run` prints what would go and removes nothing. Same seat, same
/// stand-in, same selection as the control above — only the removal is absent.
#[test]
fn dry_run_selects_and_prints_but_never_removes() {
    let root = scratch("dryrun");
    let log = install_recording_docker(&root);

    let (ok, out, err) = run_reap(&root, &["--seat", RETIRED_SEAT, "--dry-run"]);
    assert!(ok, "dry run must succeed:\nstdout={out}\nstderr={err}");

    // Vacuity guard: this id exists nowhere but the stand-in, so its presence proves the selection
    // really ran and really found the holder. A dry run that quietly selected nothing would pass the
    // no-`rm` assertion below for the wrong reason.
    assert!(
        out.contains(FAKE_HOLDER),
        "the dry run must name the holder it would remove:\nstdout={out}"
    );
    assert!(out.contains("would reap"), "stdout={out}");
    assert!(
        out.contains("nothing was removed"),
        "the dry run must say plainly that it removed nothing:\nstdout={out}"
    );

    let argv = logged_argv(&log);
    // The reads happened — so the selection is the same code path, not a skipped one.
    assert!(argv.iter().any(|line| line.starts_with("ps ")), "{argv:#?}");
    assert!(argv.iter().any(|line| line.starts_with("inspect ")), "{argv:#?}");
    // And the removal did not.
    assert!(
        !argv.iter().any(|line| line.starts_with("rm ")),
        "a dry run must not ask docker to remove anything:\n{argv:#?}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// The refusal, through the shipped binary rather than through `cli::run`: `--all` exits non-zero,
/// NAMES itself, and reaches docker not at all. A refusal that still ran the reads would be a reap
/// waiting for a bug to finish it.
#[test]
fn all_is_refused_by_the_binary_before_docker_is_touched() {
    let root = scratch("refuse");
    let log = install_recording_docker(&root);

    let (ok, out, err) = run_reap(&root, &["--all"]);
    assert!(!ok, "`--all` must fail:\nstdout={out}\nstderr={err}");
    assert!(out.is_empty(), "a refusal prints nothing to stdout:\nstdout={out}");
    assert!(err.contains("--all"), "the refusal must name the flag:\nstderr={err}");
    assert!(err.contains("retired"), "the refusal must say why:\nstderr={err}");
    assert!(
        logged_argv(&log).is_empty(),
        "a refused invocation must not reach docker at all:\n{:#?}",
        logged_argv(&log)
    );
    let _ = fs::remove_dir_all(&root);
}
