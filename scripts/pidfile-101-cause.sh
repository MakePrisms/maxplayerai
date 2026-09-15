#!/usr/bin/env bash
# THE CAUSE OF THE 101 IN gate-final.log. Not a flake waiver — an experiment.
#
# What the gate recorded, once, in an otherwise green workspace run:
#
#   test a_local_phase_that_refuses_to_stop_is_ended_at_the_deadline_and_its_exit_is_confirmed ... FAILED
#   panicked at crates/maxplayer-core/tests/delivery_push_production_child.rs:138:10:
#   the fixture child must have recorded its pid: Os { code: 2, kind: NotFound, ... }
#   test result: FAILED. 9 passed; 1 failed;  →  error: test failed  →  exit 101
#
# The failing read is the LAST assertion of that test. Everything before it passed, which is the
# whole diagnosis in one line: the delivery really did return `Cancelled`, the refusal really did
# say "was killed" AND "confirmed the exit", and the return really did land inside
# [budget, budget + REAP_BOUND). The parent behaved. What was missing was the FIXTURE's pidfile.
#
# The fixture is a /bin/sh script whose first line after `trap '' TERM` is `echo $$ > child.pid`,
# and the test's budget is 1500ms measured from BEFORE the delivery is started. So the child has to
# be forked, exec'd, and through its first line within a window that also has to cover the
# neutralize step and the spawn. Miss that window and the parent kills a child that has not yet
# written anything — the pidfile never exists, `read_to_string` returns ENOENT, and the test panics
# on a premise rather than on a bound.
#
# Two experiments, both recorded here:
#
#   E1  DETERMINISTIC REPRODUCTION. `sleep 2` is prepended to the fixture body, so the child is
#       provably still ahead of its first line when the 1500ms deadline lands. If the diagnosis is
#       right this reproduces the gate's failure EXACTLY — same test, same panic, same 9-passed
#       1-failed shape. Nothing about the parent, the deadline, or the kill is touched.
#
#   E2  THE GATE'S OWN CONDITION. The unmutated test, run repeatedly, first on an idle machine and
#       then under the CPU load a --workspace gate actually produces. A failure count that is zero
#       idle and non-zero loaded is the in-situ confirmation: the window is reachable on this
#       machine, and `gate-final.log` is where it was reached.
#
#   E3  THE MARGIN, AS A NUMBER. E2 says whether the window was lost on one occasion; it cannot say
#       how close the test runs to losing it. E3 measures that directly by shrinking the BUDGET —
#       the only thing standing between the child and the kill — until the test starts failing on
#       the missing pidfile, idle. The largest budget that still fails is what this machine needs,
#       idle, to get a /bin/sh through its first line behind a neutralize and a spawn; 1500ms minus
#       that is the entire headroom the gate has. A small headroom is the quantitative half of the
#       diagnosis, and it is reported as whatever it measures.
#
#   E4  THE REAL GATE CONDITION. E2's load was spin loops, and E3 explains why that was never going
#       to be enough: idle, this test survives a budget of 250ms, so the headroom at 1500ms is about
#       1350ms and a busy CPU alone does not spend it. What `gate-final.log` was actually doing is
#       different in kind — dozens of test binaries resident at once, each forking children, all
#       against one disk. E4 reproduces THAT: every other already-built test binary in the workspace
#       is run in parallel, continuously, while the named test is run against them. No cargo is
#       involved in the measurement, so nothing waits on the build lock and the load is the gate's
#       own, not a simulation of it.
#
# Usage: scripts/pidfile-101-cause.sh [logdir]
#        PIDFILE101_PHASES="e4" scripts/pidfile-101-cause.sh [logdir]   # re-run one phase
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
logs="${1:-$root/target/pidfile-101-cause}"
mkdir -p "$logs"
receipts="$logs/receipts.txt"
targets=()
: > "$receipts"

# shellcheck source=mutation-lib.sh
source "$root/scripts/mutation-lib.sh"
receipt "pidfile-101 cause: experiments E1 (deterministic) and E2 (in situ)"
receipt ""

gate="$root/crates/maxplayer-core/tests/delivery_push_production_child.rs"
register_target "$gate"

test_name=a_local_phase_that_refuses_to_stop_is_ended_at_the_deadline_and_its_exit_is_confirmed

phases="${PIDFILE101_PHASES:-e1 e2 e3 e4}"
run_phase() { case " $phases " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

if run_phase e1; then
echo "== E1: the child is still ahead of its first line when the deadline lands =="
# The WHOLE file runs, not just the named test, so the reproduction can be compared shape-for-shape
# with the gate's "9 passed; 1 failed".
suite=(cargo test -p maxplayer-core --all-features --locked
       --test delivery_push_production_child)
run_mutant "E1/M-CHILD-STARTS-LATE" "$gate" \
'\necho $$ > {}\n{HELLO}\nwhile :;' '\nsleep 2\necho $$ > {}\n{HELLO}\nwhile :;' \
  1 \
  "the fixture child must have recorded its pid" \
  "$logs/e1-child-starts-late.log" \
  "$test_name"

restore_targets
fi

if run_phase e2; then
echo "== E2: how often the unmutated test loses that window, idle vs under spin load =="
# Build first, so compilation is never part of what is being timed or loaded.
cargo test -p maxplayer-core --all-features --locked \
  --test delivery_push_production_child --no-run > "$logs/e2-build.log" 2>&1

# Runs are deliberately few: each is a real 1.5s deadline plus reap, and the point is a count, not
# a distribution.
runs="${PIDFILE101_RUNS:-12}"
cpus="$(sysctl -n hw.ncpu 2> /dev/null || nproc)"

measure() {
  local label="$1" log="$2" failures=0 enoent=0 i
  : > "$log"
  for i in $(seq 1 "$runs"); do
    echo "--- run $i ---" >> "$log"
    if ! cargo test -p maxplayer-core --all-features --locked \
      --test delivery_push_production_child \
      -- --exact "$test_name" >> "$log" 2>&1; then
      failures=$((failures + 1))
      if grep -q "the fixture child must have recorded its pid" "$log"; then
        enoent=$((enoent + 1))
      fi
    fi
  done
  echo "$label: $failures/$runs failed, $enoent of them on the missing pidfile. $log"
  receipt "  $label"
  receipt "    runs                 = $runs"
  receipt "    failed               = $failures"
  receipt "    failed_on_pidfile    = $enoent"
  receipt "    log_sha256           = $(sha_of "$log")"
  # Echoed so the two halves of E2 can be compared as numbers by someone who did not run it.
  eval "${label}_failed=$failures"
}

receipt "== E2 (unmutated, $runs runs each)"
receipt "  head      = $(git -C "$root" rev-parse HEAD)"
receipt "  source_sha256 = $(sha_of "$gate")"
receipt "  cpus      = $cpus"

measure idle "$logs/e2-idle.log"

# The load a `cargo test --workspace` gate actually puts on this machine: every core busy, so a
# forked /bin/sh waits behind runnable work before it reaches its first line.
hogs=()
for _ in $(seq 1 "$((cpus * 2))"); do
  (while :; do :; done) &
  hogs+=("$!")
done
trap 'kill "${hogs[@]}" 2> /dev/null || true; cleanup_mutations' EXIT
measure loaded "$logs/e2-loaded.log"
kill "${hogs[@]}" 2> /dev/null || true
hogs=()
trap 'cleanup_mutations' EXIT

receipt ""
receipt "== VERDICT"
if [ "${loaded_failed:-0}" -gt 0 ] && [ "${idle_failed:-0}" -eq 0 ]; then
  echo "CAUSE ESTABLISHED: idle ${idle_failed}/$runs, loaded ${loaded_failed}/$runs — the window is lost under gate load."
  receipt "  idle_failed   = ${idle_failed}"
  receipt "  loaded_failed = ${loaded_failed}"
  receipt "  cause = the fixture child had not reached its first line when the 1500ms deadline"
  receipt "          landed; the parent killed a child that had written no pidfile, and the test's"
  receipt "          closing premise check — not any bound it asserts — is what failed."
else
  echo "NOT REPRODUCED IN SITU: idle ${idle_failed:-0}/$runs, loaded ${loaded_failed:-0}/$runs. E1 stands; E2 did not hit the window on this run."
  receipt "  idle_failed   = ${idle_failed:-0}"
  receipt "  loaded_failed = ${loaded_failed:-0}"
  receipt "  note = E2 did not reach the window in this many runs. E1's deterministic reproduction"
  receipt "         is the standing evidence; this is reported as measured, not rounded up."
fi

fi

if run_phase e3; then
echo "== E3: how much headroom the 1500ms budget actually has, idle =="
receipt ""
receipt "== E3 (budget sweep, idle, unmutated except the budget)"
restore_targets
threshold=""
for ms in 1200 900 700 500 350 250 150; do
  restore_targets
  # Anchored through the comment above it: the same literal is the budget of a second test in this
  # file, and only this test's is being moved.
  mutate "$gate" \
    "about luck.
    let budget = Duration::from_millis(1_500);" \
    "about luck.
    let budget = Duration::from_millis($ms);"
  log="$logs/e3-budget-${ms}ms.log"
  if cargo test -p maxplayer-core --all-features --locked \
    --test delivery_push_production_child \
    -- --exact "$test_name" > "$log" 2>&1; then
    outcome="passed"
  elif grep -q "the fixture child must have recorded its pid" "$log"; then
    outcome="FAILED on the missing pidfile"
    [ -z "$threshold" ] && threshold="$ms"
  else
    # A budget small enough to change WHICH assertion goes first is no longer measuring startup,
    # so it is recorded by name rather than counted as the same finding.
    outcome="failed on something else: $(grep -m1 'panicked at' "$log" | sed 's/.*panicked at //')"
  fi
  echo "  budget ${ms}ms: $outcome"
  receipt "  budget_${ms}ms = $outcome"
done
restore_targets
receipt "  source_restored_sha256 = $(sha_of "$gate")"

if [ -n "$threshold" ]; then
  echo "MARGIN: the pidfile is already missing at a ${threshold}ms budget, idle — headroom $((1500 - threshold))ms of 1500ms."
  receipt "  first_failing_budget = ${threshold}ms"
  receipt "  headroom_at_1500ms   = $((1500 - threshold))ms"
else
  echo "MARGIN: no swept budget lost the pidfile idle; the headroom is larger than the sweep's floor."
  receipt "  first_failing_budget = none in sweep"
fi
fi

if run_phase e4; then
echo "== E4: the named test against the rest of the workspace's test binaries, all running =="
restore_targets

# Everything is already built by the phases above (or by the gate); this only resolves paths.
cargo test --workspace --all-features --locked --no-run --message-format=json \
  > "$logs/e4-binaries.json" 2> "$logs/e4-build.log"
bins="$logs/e4-binaries.txt"
python3 - "$logs/e4-binaries.json" > "$bins" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") == "compiler-artifact" and m.get("profile", {}).get("test"):
        exe = m.get("executable")
        if exe:
            print(exe)
PY

subject="$(grep "/delivery_push_production_child-" "$bins" | head -1)"
[ -n "$subject" ] || { echo "could not resolve the test binary"; exit 1; }

# The load: every OTHER test binary, looping. This is the same set the gate runs, so the contention
# is the gate's — forks, pipes, temp files and disk, not just cycles.
loadpids=()
while read -r bin; do
  [ "$bin" = "$subject" ] && continue
  [ -x "$bin" ] || continue
  (while :; do "$bin" > /dev/null 2>&1 || true; done) &
  loadpids+=("$!")
done < "$bins"
trap 'kill "${loadpids[@]}" 2> /dev/null || true; cleanup_mutations' EXIT
load_count="${#loadpids[@]}"
echo "  load: $load_count workspace test binaries looping"

e4_runs="${PIDFILE101_E4_RUNS:-15}"
e4_log="$logs/e4-under-gate-load.log"
: > "$e4_log"
e4_failed=0
e4_pidfile=0
for i in $(seq 1 "$e4_runs"); do
  echo "--- run $i ---" >> "$e4_log"
  before="$(grep -c 'the fixture child must have recorded its pid' "$e4_log" || true)"
  if ! "$subject" --exact "$test_name" --nocapture >> "$e4_log" 2>&1; then
    e4_failed=$((e4_failed + 1))
    after="$(grep -c 'the fixture child must have recorded its pid' "$e4_log" || true)"
    [ "$after" -gt "$before" ] && e4_pidfile=$((e4_pidfile + 1))
  fi
done

kill "${loadpids[@]}" 2> /dev/null || true
wait 2> /dev/null || true
loadpids=()
trap 'cleanup_mutations' EXIT

# Any child left behind by a killed load binary is this phase's mess, not the next run's evidence.
pkill -f '__delivery-push' 2> /dev/null || true

echo "  under gate load: $e4_failed/$e4_runs failed, $e4_pidfile of them on the missing pidfile. $e4_log"
receipt ""
receipt "== E4 (unmutated, under the workspace's own test binaries)"
receipt "  parallel_load_binaries = $load_count"
receipt "  runs                   = $e4_runs"
receipt "  failed                 = $e4_failed"
receipt "  failed_on_pidfile      = $e4_pidfile"
receipt "  log_sha256             = $(sha_of "$e4_log")"
if [ "$e4_pidfile" -gt 0 ]; then
  echo "CAUSE ESTABLISHED IN SITU: the missing pidfile reproduces under the gate's own load."
  receipt "  verdict = reproduced in situ; gate-final.log's 101 is this window, lost under the load"
  receipt "            of a --workspace run, not an unexplained flake."
else
  receipt "  verdict = not reproduced in this many runs; E1 remains the standing mechanism proof."
fi
fi

echo "receipts: $receipts"
