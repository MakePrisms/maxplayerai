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
#   T3  delivery_push_wire_abort_polled_through_reap
#       CONTROL: the abort is not issued, and nothing else about the run changes.
#
#       Two product mutants were tried here first and BOTH SURVIVED, which is reported rather than
#       buried:
#         - deleting the drive loop's periodic authority ask: survived, 7.91s;
#         - deleting the parent's authority ANSWER in all four places it is produced (the reply to
#           the child's own across-the-pipe question, and the three waits that re-ask the owner):
#           survived, 7.9s.
#       So on this branch an aborted wire delivery is not stopped by authority propagation at all;
#       something else ends it well inside the bound, and neither mutant discriminates. Those two
#       logs are kept beside this script's receipts. Finding WHICH mechanism stops it is a real
#       question about the branch and is reported to the reviewer rather than answered by widening
#       this fold.
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

echo "all three gates went red for their named reason and are green unmutated; receipts: $receipts"
