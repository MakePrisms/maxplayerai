#!/usr/bin/env bash
# Mutation controls for the delivery custody handoff (PR #1006).
#
# A custody test that cannot go RED is a comment. This applies the two mutations that matter to
# `delivery_turn::CustodyBailiff::attempt_handoff`, one at a time, runs the custody suite against
# each, and requires the NAMED test to fail for the NAMED reason — then restores the file and
# requires a pass.
#
#   M-PREMATURE  the confirmed-exit condition is deleted: the seat moves on "the work ended",
#                which in the executor's vocabulary is "a signal was issued and nobody looked".
#   M-DEADLINE   the work-stopped condition is replaced by the clock: the seat moves once the
#                deadline has passed, which is cleanup by calendar rather than by observation.
#
# What changed after review round 1, and why: the previous version accepted ANY cargo failure as a
# catch. A mutant that did not compile would have been reported as caught, while nothing ran. It
# also printed a failure count it never enforced, and its receipts could not be tied to the tree
# they were taken from. All three are now conditions of passing — see `scripts/mutation-lib.sh`.
#
# Exit 0 means both mutants were CAUGHT for their stated reason and the unmutated tree is green.
#
# Usage: scripts/custody-mutation-control.sh [log-dir]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
logs="${1:-$root/target/custody-mutation}"
mkdir -p "$logs"
receipts="$logs/receipts.txt"
: > "$receipts"

# shellcheck source=scripts/mutation-lib.sh
. "$root/scripts/mutation-lib.sh"

target="$root/crates/maxplayer-core/src/delivery_turn.rs"
register_target "$target"

suite=(cargo test -p maxplayer-core --all-features --locked
       --test delivery_push_stalled_supervisor)

receipt "custody mutation receipts"
receipt "suite = ${suite[*]}"

echo "== M-PREMATURE: release without a confirmed exit =="
run_mutant "M-PREMATURE" "$target" \
'        if !self.turn.exit_confirmed.load(Ordering::SeqCst) {
            return CustodyHandoff::ExitUnconfirmed;
        }
' '' \
  1 \
  "an exit this process never observed must not release the seat" \
  "$logs/m-premature.log" \
  a_signal_without_a_confirmed_exit_does_not_hand_custody_on

echo "== M-DEADLINE: fence on the clock instead of on the work having stopped =="
run_mutant "M-DEADLINE" "$target" \
'        if self.turn.state.load(Ordering::SeqCst) != ENDED {
            return CustodyHandoff::WorkStillRunning;
        }' '        if Instant::now() < self.turn.deadline {
            return CustodyHandoff::WorkStillRunning;
        }' \
  2 \
  "the clock is not a report that the work stopped" \
  "$logs/m-deadline.log" \
  a_passed_deadline_alone_does_not_hand_custody_on \
  a_supervisor_inside_a_shared_state_section_is_not_fenced_out_from_under_itself

echo "== CONTROL: unmutated tree =="
restore_targets
receipt ""
receipt "== CONTROL (unmutated)"
receipt "  source_sha256       = $(sha_of "$target")"
receipt "  root_tree           = $(root_tree)"
expect_green "CONTROL" "$logs/control.log"

echo "both mutants caught for their stated reason, control green; receipts: $receipts"
