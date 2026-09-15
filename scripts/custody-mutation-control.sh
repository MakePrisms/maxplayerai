#!/usr/bin/env bash
# Mutation controls for the delivery custody handoff (PR #1006).
#
# A custody test that cannot go RED is a comment. This applies the two mutations that matter to
# `delivery_turn::CustodyBailiff::attempt_handoff`, one at a time, runs the custody suite against
# each, and REQUIRES a failure — then restores the file and requires a pass.
#
#   M-PREMATURE  the confirmed-exit condition is deleted: the seat moves on "the work ended",
#                which in the executor's vocabulary is "a signal was issued and nobody looked".
#   M-DEADLINE   the work-stopped condition is replaced by the clock: the seat moves once the
#                deadline has passed, which is cleanup by calendar rather than by observation.
#
# Exit 0 means both mutants were CAUGHT and the unmutated tree is green. Any other exit means a
# mutation survived, which is a hole in the suite and not a passing run.
#
# Usage: scripts/custody-mutation-control.sh [log-dir]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="$root/crates/maxplayer-core/src/delivery_turn.rs"
logs="${1:-$root/target/custody-mutation}"
mkdir -p "$logs"
backup="$(mktemp)"
cp "$target" "$backup"
restore() { cp "$backup" "$target"; rm -f "$backup"; }
trap restore EXIT

suite=(cargo test -p maxplayer-core --all-features --locked
       --test delivery_push_stalled_supervisor)

mutate() {
  python3 - "$target" "$1" "$2" <<'PY'
import sys
path, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
body = open(path).read()
if body.count(old) != 1:
    sys.exit(f"mutation anchor appears {body.count(old)} times, expected exactly 1")
open(path, "w").write(body.replace(old, new))
PY
}

expect_red() {
  local name="$1"
  if "${suite[@]}" > "$logs/$name.log" 2>&1; then
    echo "SURVIVED: $name — the suite passed against a mutant. See $logs/$name.log"
    exit 1
  fi
  echo "CAUGHT:   $name — $(grep -c '^test .* FAILED\|^---- .* stdout' "$logs/$name.log" || true) failing assertion(s); $logs/$name.log"
}

echo "== M-PREMATURE: release without a confirmed exit =="
cp "$backup" "$target"
mutate '        if !self.turn.exit_confirmed.load(Ordering::SeqCst) {
            return CustodyHandoff::ExitUnconfirmed;
        }
' ''
expect_red m-premature

echo "== M-DEADLINE: fence on the clock instead of on the work having stopped =="
cp "$backup" "$target"
mutate '        if self.turn.state.load(Ordering::SeqCst) != ENDED {
            return CustodyHandoff::WorkStillRunning;
        }' '        if Instant::now() < self.turn.deadline {
            return CustodyHandoff::WorkStillRunning;
        }'
expect_red m-deadline

echo "== CONTROL: unmutated tree =="
cp "$backup" "$target"
if ! "${suite[@]}" > "$logs/control.log" 2>&1; then
  echo "the unmutated suite is RED; the mutants above prove nothing. See $logs/control.log"
  exit 1
fi
echo "GREEN:    unmutated — $logs/control.log"
echo "both mutants caught, control green"
