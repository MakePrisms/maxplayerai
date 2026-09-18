#!/usr/bin/env bash
# RED-BEFORE-GREEN provenance for the three integration gates of PR #1006.
#
# Review round 1 established that the commit order is tests-before-product, and said plainly that
# commit order is NOT red-before-green execution: a test committed first still proves nothing until
# someone has watched it fail for the reason it claims to be about. This script is that watching,
# recorded, for each of the three gates — one product mutation per gate, chosen to remove the exact
# mechanism the gate says it measures.
#
#   T1  delivery_push_local_packing_stall
#       MUTANT: the kill signal is not sent. `kill_and_reap` still reports, still times its reap,
#       and simply never signals anything. T1's own comment names this mutant as the one its
#       process-table assertions exist to catch, so it is the honest one to run against it.
#
#   T2  delivery_push_production_signer_integrated
#       MUTANT: deadline enforcement is removed from BOTH places that hold it — the watchdog is
#       never armed, and the drive loop's own expiry check never fires.
#
#       Removing only the watchdog was tried first and SURVIVED, in 7.57s: the thread parked inside
#       the real signer is not the drive loop, so the loop was still free to kill at the deadline
#       on its own. Removing the loop's expiry check as well ALSO survived, in 7.56s — because the
#       child carries the same absolute deadline and stops itself.
#
#       Raising the child's clock as well ALSO survived, in 7.56s — because the turn's own deadline
#       reaches the executor a fourth way, through the periodic authority ask. Those three survivals
#       are recorded here rather than hidden: this bound is defended in four independent places, so
#       deleting deadline enforcement is the wrong instrument for asking what T2 can detect.
#
#       The mutant kept is the one T2's own assertion message describes: the parent's wait for the
#       mint answer is made unbounded, so the stop WAITS ON THE SIGNER. One site, no deletion of
#       any safety mechanism, and it survives the watchdog — the child is still killed on time,
#       while the parent stays parked in the mint until the signer's own 60-second clock releases
#       it. That is exactly the failure T2a and T2b exist to refuse, and it is the difference
#       between "the delivery stopped" and "the seat came back on time".
#
#       Measured: the delivery takes 60.04s against a 2.5s budget. Both gates refuse it, and the
#       required assertion is the BOUND — T2b's "outside [budget, budget + REAP_BOUND + 1.5s)".
#       T2a additionally fires its own validity guard ("the signer's own deadline expired during
#       this test: the stop could have been the signer giving up rather than the executor stopping
#       the delivery"), which is the gate noticing that the mutation had destroyed its premise.
#
#   R3-1a  an_exit_confirmation_is_withheld_until_cleanup_is_established   (lib unit test)
#       MUTANT: the confirmation is taken at the reap again, without asking whether the delivery's
#       pipe has been cleaned up. This is the review's PRIMARY finding restored in one line, and it
#       is the exact condition the test's first assertion names.
#
#   R3-1b  a_watchdog_reap_that_times_out_is_charged_against_the_bound          (lib unit test)
#       MUTANT: the timeout path returns without charging, as it did before. The reap still runs
#       and still gives up on time; what it stops doing is paying for the window it used.
#
#   T3  delivery_push_wire_abort_polled_through_reap
#       CONTROL: the abort is not issued, and nothing else about the run changes.
#
#       Two product mutants were tried here first and BOTH SURVIVED:
#         - deleting the drive loop's periodic authority ask: survived, 7.91s;
#         - deleting the parent's authority ANSWER in all four places it is produced: survived, 7.9s.
#
#       ROUND 3 SETTLED WHY, and the answer is not "the gate is weak": NEITHER MUTANT IS ON THIS
#       PATH. This gate calls `neutralize_then_push_in_child_off_runtime` with `authority: None`,
#       so the executor's authority ask and answer are not wired into an aborted wire delivery at
#       all. Deleting them could not change a run that never used them.
#
#       What actually ends an aborted wire delivery, end to end:
#         1. `first.abort()` drops the delivery future at its await. The blocking push is NOT
#            cancelled by this — it runs under `spawn_blocking`, and dropping that JoinHandle
#            leaves the closure running.
#         2. Dropping the future drops the supervisor's `TurnControl`, whose `Drop` calls `end()`
#            ("a supervisor that is dropped — cancelled at an await, aborted, or unwound — revokes
#            exactly as one that returned"). That is the ONLY thing the abort itself does.
#         3. The still-running blocking closure holds a `WorkLifetime` over the same turn, wired
#            into the transport as its per-leg/per-chunk gate. The next gate check sees `WorkEnded`
#            and fails the leg, and the child is then killed and reaped.
#       So the abort stops the delivery through TURN REVOCATION OBSERVED BY THE TRANSPORT GATE,
#       not through authority propagation and not through task cancellation.
#
#       R3-2/M-NO-REVOKE-ON-DROP below PROVES that at product level: `TurnControl::drop` is made a
#       no-op and both abort gates go red at the abort-relative bound (B took the seat 58.7s and
#       60.1s after the abort, against the 13.05s this stop is allowed) — which is precisely the
#       "cancellation ignored until the delivery's own 60s deadline" shape. Both TIMEOUT gates in
#       the same file stay GREEN, so the mutant discriminates the abort path rather than breaking
#       the file.
#
#       What the round-2 verdict asked for is narrower and is settled here: the abort-relative bound
#       must FAIL when a cancellation is ignored and the delivery dies at its natural 60-second
#       deadline instead. Removing the abort call reproduces exactly that situation — the gate's own
#       stop becomes the deadline's — and the new assertion is what refuses it. This is a FIXTURE
#       control, not a product mutant, and it is labelled as one: it proves the new oracle is
#       load-bearing, which is the claim round 2 required evidence for.
#
# Exit 0 means each gate went red for its named reason against its mutant, every mutated tree is
# bound to a hash, every restoration matched, and each gate is green unmutated.
#
# Usage: scripts/oracle-red-before-green.sh [log-dir]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
logs="${1:-$root/target/oracle-red-before-green}"
mkdir -p "$logs"
receipts="$logs/receipts.txt"
: > "$receipts"

# shellcheck source=scripts/mutation-lib.sh
. "$root/scripts/mutation-lib.sh"

executor="$root/crates/maxplayer-core/src/delivery_executor.rs"
register_target "$executor"
wire_abort_gate="$root/crates/maxplayer/tests/delivery_push_wire_abort_polled_through_reap.rs"
register_target "$wire_abort_gate"
turn="$root/crates/maxplayer-core/src/delivery_turn.rs"
register_target "$turn"

receipt "red-before-green receipts for T1/T2/T3"

# A mutant that removes the kill leaves the parked child alive: it is blocked forever on a FIFO
# with no writer, and nothing in the mutated build will ever signal it. Sweeping by name would be
# reckless — another gate may be running its own children on this host — so only ORPHANS are
# collected: a child whose parent is gone (ppid 1) cannot belong to a live run.
sweep_orphaned_children() {
  local pid ppid rest killed=0
  while read -r pid ppid rest; do
    case "$rest" in
      *__delivery-push*)
        if [[ "$ppid" == "1" ]]; then
          kill -9 "$pid" 2> /dev/null && killed=$((killed + 1))
        fi
        ;;
    esac
  done < <(ps -eo pid=,ppid=,command= 2> /dev/null || ps -Ao pid=,ppid=,command=)
  receipt "  swept_orphaned_children = $killed"
  echo "swept $killed orphaned delivery child(ren) left by the mutant"
}

echo "== T1: the kill signal is never sent =="
# SCOPED TO THE NAMED TEST, and only for this mutant. A child that is never signalled is never
# confirmed dead, and this crate's fail-closed rule then refuses to START another delivery in the
# same process: the file's positive control fails too, with "1 earlier delivery push child(ren)
# could not be confirmed to have exited". That second red is a true consequence of the mutation and
# a nice demonstration of the rule, but it depends on which test ran first, so it must not sit
# inside a count this script enforces. The gate's own red is the one being recorded here; the
# unmutated control below runs the whole file.
suite=(cargo test -p maxplayer --all-features --locked
       --test delivery_push_local_packing_stall
       -- --exact a_delivery_parked_in_real_local_packing_is_stopped_at_its_deadline_before_any_pack_upload)
run_mutant "T1/M-NO-SIGNAL" "$executor" \
'        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
            libc::kill(self.pid, libc::SIGKILL);
        }
        true' '        true' \
  1 \
  "a child parked in libgit2's object walk must be killed, not awaited" \
  "$logs/t1-no-signal.log" \
  a_delivery_parked_in_real_local_packing_is_stopped_at_its_deadline_before_any_pack_upload
sweep_orphaned_children

echo "== T2: the parent's wait for the mint is made unbounded =="
suite=(cargo test -p maxplayer --all-features --locked
       --test delivery_push_production_signer_integrated)
# The slice stays in the expression, so nothing goes unused and the mutant builds clean; what it
# loses is the ceiling that keeps this wait shorter than the delivery.
run_mutant "T2/M-STOP-WAITS-ON-SIGNER" "$executor" \
'                    match answer_rx.recv_timeout(slice) {' '                    match answer_rx.recv_timeout(slice.max(Duration::from_secs(3600))) {' \
  2 \
  "outside [budget, budget + REAP_BOUND" \
  "$logs/t2-stop-waits-on-signer.log" \
  a_delivery_parked_in_the_real_signer_is_stopped_at_its_own_deadline_not_the_signers \
  a_delivery_behind_a_saturated_real_signer_is_stopped_at_its_own_deadline

echo "== T3: the abort is never issued, so the deadline does the stopping =="
suite=(cargo test -p maxplayer --all-features --locked
       --test delivery_push_wire_abort_polled_through_reap)
# The stamp stays, so the bound is still measured from the instant the abort WOULD have been
# issued; what goes is the abort itself.
run_mutant "T3/C-NO-ABORT" "$wire_abort_gate" \
'        first.abort();
        Some(Instant::now())' '        Some(Instant::now())' \
  2 \
  "a cancellation that is merely ignored until the delivery" \
  "$logs/t3-no-abort.log" \
  a_pack_upload_whose_task_is_aborted_never_overlaps_a_second_delivery_polled_through_the_reap \
  an_advertisement_whose_task_is_aborted_never_overlaps_a_second_delivery_polled_through_the_reap

echo "== R3-2: an aborted delivery's turn is never revoked =="
# THE PRODUCT MUTANT for T3, and the answer to the two survivals recorded above. The whole file is
# run, not just the two abort gates: the timeout gates staying green is the evidence that this
# mutant removes the abort's stop specifically and not the file's premise.
suite=(cargo test -p maxplayer --all-features --locked
       --test delivery_push_wire_abort_polled_through_reap)
run_mutant "R3-2/M-NO-REVOKE-ON-DROP" "$turn" \
'    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// The work'"'"'s end of the turn, before the work has started.' '    fn drop(&mut self) {}
}

/// The work'"'"'s end of the turn, before the work has started.' \
  2 \
  "a cancellation that is merely ignored until the delivery" \
  "$logs/r3-2-no-revoke-on-drop.log" \
  a_pack_upload_whose_task_is_aborted_never_overlaps_a_second_delivery_polled_through_the_reap \
  an_advertisement_whose_task_is_aborted_never_overlaps_a_second_delivery_polled_through_the_reap

echo "== R3-1a: the confirmation is published at the reap, before cleanup =="
suite=(cargo test -p maxplayer-core --all-features --locked --lib
       -- --exact delivery_executor::tests::an_exit_confirmation_is_withheld_until_cleanup_is_established)
run_mutant "R3-1a/M-PUBLISH-AT-REAP" "$executor" \
'            let confirm = if self.cleanup_established {
                self.confirm.take()
            } else {
                None
            };
            return (outcome, confirm);' '            return (outcome, self.confirm.take());' \
  1 \
  "the seat was released at the reap, while the delivery" \
  "$logs/r3-1a-publish-at-reap.log" \
  delivery_executor::tests::an_exit_confirmation_is_withheld_until_cleanup_is_established

echo "== R3-1b: the timed-out reap goes uncharged =="
suite=(cargo test -p maxplayer-core --all-features --locked --lib
       -- --exact delivery_executor::tests::a_watchdog_reap_that_times_out_is_charged_against_the_bound)
run_mutant "R3-1b/M-UNCHARGED-TIMEOUT" "$executor" \
'                if started.elapsed() >= budget {
                    // UNCONFIRMED, AND CHARGED. The seat keeps the turn; see `kill_and_reap`.
                    charge(started);
                    return;
                }' '                if started.elapsed() >= budget {
                    return;
                }' \
  1 \
  "charged nothing" \
  "$logs/r3-1b-uncharged-timeout.log" \
  delivery_executor::tests::a_watchdog_reap_that_times_out_is_charged_against_the_bound

echo "== CONTROL: all three gates, unmutated =="
restore_targets
receipt ""
receipt "== CONTROL (unmutated)"
receipt "  source_sha256       = $(sha_of "$executor")"
receipt "  root_tree           = $(root_tree)"

for gate in delivery_push_local_packing_stall \
            delivery_push_production_signer_integrated \
            delivery_push_wire_abort_polled_through_reap; do
  suite=(cargo test -p maxplayer --all-features --locked --test "$gate")
  receipt "  -- $gate"
  expect_green "CONTROL/$gate" "$logs/control-$gate.log"
done

suite=(cargo test -p maxplayer-core --all-features --locked --lib
       -- --exact delivery_executor::tests::an_exit_confirmation_is_withheld_until_cleanup_is_established
          delivery_executor::tests::a_watchdog_reap_that_times_out_is_charged_against_the_bound)
receipt "  -- delivery_executor lib: exit-confirmation cleanup gate and reap charging"
expect_green "CONTROL/r3-item1-lib" "$logs/control-r3-item1-lib.log"

echo "all six mutants went red for their named reason and are green unmutated; receipts: $receipts"
