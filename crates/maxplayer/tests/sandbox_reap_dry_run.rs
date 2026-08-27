//! #905: `maxplayer sandbox-reap --seat <hex> --dry-run` SELECTS and prints; it must never remove.
//!
//! The claim under test is a negative — "no container was removed" — and a negative about a
//! subprocess cannot be proved by reading the code that spawns it. So this drives the real binary
//! against a **recording stand-in `docker`** on PATH: a script that logs every argv it is handed,
//! answers the three read commands with a fabricated holder, and would log a `rm` if one were ever
//! issued. `sandbox_netns` reaches docker through `Command::new("docker")`, which resolves on PATH,
//! so the stand-in is what the binary actually talks to.
//!
//! Three legs, because the negative alone can pass vacuously:
//!
//!   POSITIVE CONTROL — the same invocation WITHOUT `--dry-run` must log `docker rm`. Without this
//!   leg, a stand-in that was never reached, a PATH that did not take, or a selection that matched
//!   nothing would all make the dry-run assertion pass while proving nothing.
//!   THE PROPERTY — with `--dry-run`, the log holds the reads and NO `rm`, and stdout says so.
//!   TOTAL FAILURE — when every selected holder's `rm` exits nonzero, the command must exit 2 and
//!   stdout must carry no success claim. A removal failure that was dropped rather than returned
//!   made this print "no reapable containment holders for seat <hex>" and exit 0 — a false
//!   statement about the host, on the line automation reads.
//!   PARTIAL FAILURE — three holders; the first and the last refuse `rm`, the middle one succeeds.
//!   All three removals must be ATTEMPTED (`reap_orphans` continues past a stuck holder, and always
//!   has), the one that went must be reported as reaped because it truly was, BOTH failures must be
//!   named, and the run must still exit 2. This is the state that had no cell in the old
//!   `Result<Vec<String>, String>` return and is why the failure went to `eprintln!`: "some reaped,
//!   some failed" could only be spelled as "all fine" or as "could not list at all".
//!
//! ── Every leg asserts the EXACT exit code ────────────────────────────────────────────────────────
//! The usage string publishes three codes — 0 success, 1 usage error, 2 runtime error. A helper that
//! answered `status.success()` returns a BOOLEAN, so no test in this file could tell 1 from 2, and
//! the exit contract was unverifiable however many legs were added. `run_reap` therefore returns
//! `status.code()` and every leg names the code it expects.
//!
//! Also vacuity-guarded on the way in: the fabricated holder ids are strings only the stand-in can
//! produce, and the legs assert they appear in the binary's stdout or in the argv log. If the binary
//! had talked to a real docker, or to nothing, those ids could not be there.
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

/// The command's documented exit codes, asserted exactly. Mirrored from `sandbox_reap`'s private
/// constants: they are a published interface, so a test that only checked "non-zero" would let a
/// usage error stand in for a runtime error and vice versa.
const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

/// A fabricated 64-hex seat: the retired seat an operator would name.
const RETIRED_SEAT: &str = "dead00000000000000000000000000000000000000000000000000000000beef";

/// The holder the stand-in reports. Only the stand-in can produce this string, which is what makes
/// its appearance in stdout proof that the stand-in was reached.
const FAKE_HOLDER: &str = "c0ffee1111111111111111111111111111111111111111111111111111111111";

/// Two more holders of the same retired seat, used only by the partial-failure leg. The loop's
/// "one stuck holder must not stop the others being cleaned up" invariant cannot be observed with a
/// single candidate: with one holder, continuing and stopping look identical. `THIRD_HOLDER` refuses
/// as well, so "every failure is reported" is a real claim about a set rather than about one item.
const OTHER_HOLDER: &str = "b0bcafe222222222222222222222222222222222222222222222222222222222";
const THIRD_HOLDER: &str = "decade3333333333333333333333333333333333333333333333333333333333";

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
/// `<id>\t<seat>`), the host-wide id listing, and the network-mode inspect — with `holders`, all
/// owned by `RETIRED_SEAT` and with nothing joined to them, so every one of them is selected.
///
/// `rm_refuses` names the holders whose `docker rm` exits nonzero; every other `rm` succeeds. The
/// refusal has to be PER HOLDER, not a global on/off switch: an all-or-nothing stub cannot express
/// "some reaped, some failed", which is exactly the state the old two-valued return had no cell for.
/// Every `rm` is logged either way, so the log answers "was it attempted" independently of "did it
/// work" — that is how the leg below proves the loop continued past a refusal rather than stopping.
///
/// The reads always succeed, so a failing `rm` leg still gets a full SELECTION. That separation is
/// what makes "selected two, removed one, reported none" reachable at all.
fn install_recording_docker(bin: &Path, holders: &[&str], rm_refuses: &[&str]) -> PathBuf {
    fs::create_dir_all(bin).expect("bin dir");
    let log = bin.join("argv.log");
    // One `<id>\t<seat>` line per holder for the labelled read, and one bare id per holder for the
    // host-wide read. Built here rather than in the shell so the stub stays a lookup, not a program.
    let labelled: String =
        holders.iter().map(|id| format!("{id}\t{RETIRED_SEAT}\n")).collect();
    let ids: String = holders.iter().map(|id| format!("{id}\n")).collect();
    // Space-delimited so the shell can test membership with one glob and no loop over a second list.
    let refuses = format!(" {} ", rm_refuses.join(" "));
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$1" in
  ps)
    for arg in "$@"; do
      if [ "$arg" = "--format" ]; then
        printf '%s' '{labelled}'
        exit 0
      fi
    done
    printf '%s' '{ids}'
    ;;
  inspect)
    # Nothing is joined to any holder's namespace: no `container:<id>` mode in the answer.
    printf 'bridge\n'
    ;;
  rm)
    for arg in "$@"; do
      case '{refuses}' in
        *" $arg "*)
          printf 'Error response from daemon: cannot remove container %s: device or resource busy\n' "$arg" >&2
          exit 1
          ;;
      esac
    done
    ;;
esac
exit 0
"#,
        log = log.display(),
        labelled = labelled,
        ids = ids,
        refuses = refuses,
    );
    let docker = bin.join("docker");
    fs::write(&docker, script).expect("write stand-in docker");
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).expect("chmod stand-in");
    log
}

/// Run the real binary with `bin` as the ONLY PATH entry, so `docker` resolves to the stand-in and
/// a stray real docker cannot answer instead.
///
/// Returns the EXIT CODE, not `status.success()`. `None` means killed by a signal, which is itself a
/// distinct answer worth failing on rather than folding into "not success".
fn run_reap(bin: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_maxplayer"))
        .arg("sandbox-reap")
        .args(args)
        .env("PATH", bin)
        .output()
        .expect("run maxplayer sandbox-reap");
    (
        output.status.code(),
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
    let log = install_recording_docker(&root, &[FAKE_HOLDER], &[]);

    let (code, out, err) = run_reap(&root, &["--seat", RETIRED_SEAT]);
    assert_eq!(code, Some(SUCCESS), "reap must exit 0:\nstdout={out}\nstderr={err}");
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
    let log = install_recording_docker(&root, &[FAKE_HOLDER], &[]);

    let (code, out, err) = run_reap(&root, &["--seat", RETIRED_SEAT, "--dry-run"]);
    assert_eq!(code, Some(SUCCESS), "dry run must exit 0:\nstdout={out}\nstderr={err}");

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

/// The refusal, through the shipped binary rather than through `cli::run`: `--all` exits with the
/// USAGE code, NAMES itself, and reaches docker not at all. A refusal that still ran the reads would
/// be a reap waiting for a bug to finish it.
///
/// The code is asserted exactly, and 1 rather than 2 is the point: a refused flag is the operator's
/// mistake, not the host's, and automation that retries runtime errors must not retry this.
#[test]
fn all_is_refused_by_the_binary_before_docker_is_touched() {
    let root = scratch("refuse");
    let log = install_recording_docker(&root, &[FAKE_HOLDER], &[]);

    let (code, out, err) = run_reap(&root, &["--all"]);
    assert_eq!(code, Some(USAGE_ERROR), "`--all` must exit 1:\nstdout={out}\nstderr={err}");
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

/// THE FAILURE PATH: every selected holder fails to be removed, so the command must exit 2 and must
/// NOT claim there was nothing to reap.
///
/// The selection here is identical to the positive control's — same seat, same stand-in, same single
/// candidate — and only `docker rm` differs. That is what pins the failure to the removal rather
/// than to the selection: the control proves this exact setup reaches `rm` and reports "reaped".
///
/// The assertion that matters is the absence one. "no reapable containment holders for seat <hex>"
/// on exit 0 is not a quiet failure, it is a FALSE statement about the host, printed on the line an
/// operator or a script reads to decide the leak is gone.
#[test]
fn a_removal_that_fails_exits_two_and_never_claims_success() {
    let root = scratch("rmfails");
    let log = install_recording_docker(&root, &[FAKE_HOLDER], &[FAKE_HOLDER]);

    let (code, out, err) = run_reap(&root, &["--seat", RETIRED_SEAT]);
    assert_eq!(
        code,
        Some(RUNTIME_ERROR),
        "a failed removal is a runtime error:\nstdout={out}\nstderr={err}"
    );

    // Vacuity guard: the removal really was attempted, so this is the failure path and not a
    // selection that quietly matched nothing.
    let argv = logged_argv(&log);
    assert!(
        argv.iter().any(|line| line.starts_with("rm ") && line.contains(FAKE_HOLDER)),
        "the holder must have been selected and a removal attempted:\n{argv:#?}"
    );

    // No success claim of any kind on stdout. Both spellings are named because both were reachable:
    // the empty-list line is what a TOTAL failure printed, and the summary is what a PARTIAL one
    // would print while dropping the rest.
    assert!(
        !out.contains("no reapable containment holders"),
        "a failed removal must never report that there was nothing to reap:\nstdout={out}"
    );
    assert!(
        !out.contains("containment holder(s) left by seat"),
        "a failed removal must not print the success summary:\nstdout={out}"
    );

    // And the failure is reported through the command's OWN error writer, naming the holder and
    // carrying docker's reason.
    assert!(err.contains(FAKE_HOLDER), "the failure must name the holder:\nstderr={err}");
    assert!(
        err.contains("resource busy") || err.contains("exit 1"),
        "the failure must carry docker's reason:\nstderr={err}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// PARTIAL FAILURE: three holders, the first and last refused. The middle one must still be removed,
/// every refusal must be named, and the run must still exit 2.
///
/// Four separate properties, and each has been wrong in some shape of this code. None of them can be
/// dropped, because each of the other three passes vacuously without it:
///
///   * **The loop continues.** `reap_orphans` has always carried "one stuck holder must not stop the
///     others being cleaned up", and the obvious strict fix — returning `Err` on the first refused
///     `rm` — deletes it; two stuck holders would then reap neither. The refusals are placed FIRST
///     and LAST on purpose: with one candidate, or with every failure last, stopping and continuing
///     produce an identical argv log.
///   * **The success is still reported.** `OTHER_HOLDER` really was removed, so suppressing it
///     because its siblings failed is its own false report, in the opposite direction. Without this
///     assertion a stub that reaped nothing at all would pass the other three.
///   * **Every failure is named, not counted.** Exit 2 is for scripts; a person needs to know WHICH
///     holder would not go and why, or the command is silent in a quieter way than before. Two
///     refusals rather than one is what makes this a claim about a set.
///   * **The process exited, and exited 2.** `run_reap` returns `status.code()`, which is `None` for
///     a process killed by a signal — so `Some(RUNTIME_ERROR)` asserts both that nothing aborted and
///     that the code is exactly the documented runtime error, never 1 and never "some non-zero".
#[test]
fn a_partial_failure_still_reaps_the_rest_and_still_exits_two() {
    let root = scratch("partial");
    // The refusals bracket the success: the middle removal can only happen if the loop carried on
    // past the first refusal, and the last refusal can only be reported if it carried on past both.
    let log = install_recording_docker(
        &root,
        &[FAKE_HOLDER, OTHER_HOLDER, THIRD_HOLDER],
        &[FAKE_HOLDER, THIRD_HOLDER],
    );

    let (code, out, err) = run_reap(&root, &["--seat", RETIRED_SEAT]);
    // Exited (not signalled), and exited with the runtime code exactly.
    assert_eq!(
        code,
        Some(RUNTIME_ERROR),
        "a holder left behind is a runtime error even when others went:\nstdout={out}\nstderr={err}"
    );

    // The loop continued: ALL THREE removals were attempted.
    let argv = logged_argv(&log);
    let rm_of = |id: &str| argv.iter().any(|line| line.starts_with("rm ") && line.contains(id));
    assert!(rm_of(FAKE_HOLDER), "the first refused holder must be attempted:\n{argv:#?}");
    assert!(
        rm_of(OTHER_HOLDER),
        "a stuck holder must not stop the others being cleaned up:\n{argv:#?}"
    );
    assert!(
        rm_of(THIRD_HOLDER),
        "the loop must carry on past a SECOND refusal too:\n{argv:#?}"
    );

    // What went, went — and is reported. Without this the leg would pass on a run that reaped
    // nothing whatsoever.
    assert!(
        out.contains(&format!("reaped {OTHER_HOLDER}")),
        "the holder that was removed must be reported as removed:\nstdout={out}"
    );
    // What stayed, stayed — and stdout claims nothing about it.
    for still_there in [FAKE_HOLDER, THIRD_HOLDER] {
        assert!(
            !out.contains(still_there),
            "stdout must not name a holder that is still on the host: {still_there}\nstdout={out}"
        );
    }
    assert!(
        !out.contains("containment holder(s) left by seat"),
        "a partial reap must not print the success summary:\nstdout={out}"
    );

    // EVERY failure is named on the command's own error writer, each with docker's reason. A count
    // alone would leave the operator knowing something survived and not which thing.
    for failed in [FAKE_HOLDER, THIRD_HOLDER] {
        assert!(
            err.contains(&format!("could not reap containment holder {failed}")),
            "every failure must be named, not just counted: {failed}\nstderr={err}"
        );
        assert!(
            err.contains(&format!("cannot remove container {failed}")),
            "each failure must carry docker's own reason for THAT holder:\nstderr={err}"
        );
    }
    // And the count is measured against what was SELECTED, not against what succeeded.
    assert!(
        err.contains("reaped 1 of the 3"),
        "the summary must count the selection, not the successes:\nstderr={err}"
    );
    let _ = fs::remove_dir_all(&root);
}
