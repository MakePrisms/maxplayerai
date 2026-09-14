#!/usr/bin/env bash
# Runs the gVisor DNS/delivery gates in order, bounded, and reports a verdict per gate.
#
# The gates are the deliverable, but a gate nobody can re-run is a claim, not evidence.
# This is how someone else reproduces the whole set on a fresh VM without knowing which
# script needs which rendered plan.
#
# Two rules it will not bend:
#
#   * **BOUNDED.** Every gate gets a `timeout`, and the run gets a total budget. A gate
#     that hangs is a FAIL with a reason, never a run that sits there until a watchdog
#     kills the session and leaves no evidence at all.
#   * **A GATE THAT DID NOT RUN IS NOT A PASS.** Missing prerequisites print SKIPPED with
#     the reason and count against the run. The failure mode this exists to prevent is a
#     green summary produced by a suite that quietly executed nothing — which is exactly
#     the class of bug gates 2 and 5 were caught making about their own probes.
#
# Prerequisites: the repro VM from provision-repro-vm.sh, and the rendered plans copied to
# $HOME (see PLANS below). Plans are rendered on the REPO host, because rendering them here
# would mean this script decides what the rules are — and then the gates would be testing
# the script instead of the product.
set -uo pipefail

EVIDENCE_DIR="${EVIDENCE_DIR:-$HOME/gate-evidence}"
PER_GATE_TIMEOUT="${PER_GATE_TIMEOUT:-900}"
TOTAL_BUDGET="${TOTAL_BUDGET:-3600}"
SUMMARY="${SUMMARY:-$EVIDENCE_DIR/summary.txt}"
STARTED=$(date +%s)

mkdir -p "${EVIDENCE_DIR}"
declare -a NAMES=() VERDICTS=() REASONS=()

record() { NAMES+=("$1"); VERDICTS+=("$2"); REASONS+=("$3"); }

# gate -> the files it needs in $HOME before it can say anything true
plans_for() {
  case "$1" in
    gate1) echo "" ;;
    gate2) echo "gate2-plan.txt" ;;
    gate4) echo "gate4-plan.txt" ;;
    gate5) echo "gate5-plans/plan-d.txt gate5-plans/host-d.txt gate5-plans/host-d-teardown.txt" ;;
    gate5f) echo "gate5f-plan.txt gate5f-host-install.txt gate5f-host-teardown.txt" ;;
  esac
}
script_for() {
  case "$1" in
    gate1)  echo "gate1-repro.sh" ;;
    gate2)  echo "gate2-namespace-dns-tls.sh" ;;
    gate4)  echo "gate4-container-git-delivery.sh" ;;
    gate5)  echo "gate5-denial-and-concurrent-success.sh" ;;
    gate5f) echo "gate5f-product-host-rules-bind-runsc.sh" ;;
  esac
}
# The line in a gate's own output that decides it. Grepping the gate's verdict rather than
# trusting its exit code is deliberate: several of these scripts run under `set -uo pipefail`
# without `-e` and exit 0 while reporting a failure inside.
verdict_of() { # gate logfile
  local gate="$1" log="$2"
  case "${gate}" in
    gate1)
      grep -q "EAI_AGAIN" "${log}" && grep -qi "runc" "${log}" && echo PASS || echo FAIL ;;
    gate2)
      grep -q "verified=true\|cert-verified" "${log}" && echo PASS || echo FAIL ;;
    gate4)
      grep -q "GATE4: PASS" "${log}" && echo PASS || echo FAIL ;;
    gate5)
      grep -q "GATE5-DENIAL: PASS" "${log}" && echo PASS || echo FAIL ;;
    gate5f)
      # No self-verdict line: it passes when the host-directed leg changed under the policy
      # and the public route survived, and when nothing leaked.
      grep -q "CLEAN" "${log}" && grep -q "PUBLIC-PASS git" "${log}" && echo PASS || echo FAIL ;;
  esac
}

run_gate() { # gate
  local gate="$1"
  local script; script="$(script_for "${gate}")"
  local log="${EVIDENCE_DIR}/${gate}-$(date -u +%Y%m%dT%H%M%SZ).log"

  local elapsed=$(( $(date +%s) - STARTED ))
  if [ "${elapsed}" -ge "${TOTAL_BUDGET}" ]; then
    record "${gate}" SKIPPED "total budget ${TOTAL_BUDGET}s exhausted"
    return
  fi
  if [ ! -x "${HOME}/${script}" ]; then
    record "${gate}" SKIPPED "missing ${HOME}/${script}"
    return
  fi
  local missing="" p
  for p in $(plans_for "${gate}"); do
    [ -s "${HOME}/${p}" ] || missing="${missing} ${p}"
  done
  if [ -n "${missing}" ]; then
    record "${gate}" SKIPPED "missing rendered plan(s):${missing}"
    return
  fi

  echo "--- ${gate}: running (timeout ${PER_GATE_TIMEOUT}s) -> ${log}"
  local budget_left=$(( TOTAL_BUDGET - elapsed ))
  local limit=$(( PER_GATE_TIMEOUT < budget_left ? PER_GATE_TIMEOUT : budget_left ))
  timeout "${limit}" "${HOME}/${script}" > "${log}" 2>&1
  local rc=$?
  if [ "${rc}" -eq 124 ]; then
    record "${gate}" FAIL "timed out after ${limit}s (partial log kept)"
    return
  fi
  record "${gate}" "$(verdict_of "${gate}" "${log}")" "exit=${rc} log=$(basename "${log}")"
}

echo "=== gVisor DNS/delivery gates ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m) docker=$(sudo docker version --format '{{.Server.Version}}' 2>/dev/null) runsc=$(runsc --version 2>/dev/null | head -1)"
echo "per-gate timeout=${PER_GATE_TIMEOUT}s total budget=${TOTAL_BUDGET}s evidence=${EVIDENCE_DIR}"
echo "NOTE: results are labelled by architecture on purpose — x86_64 is NOT covered by this run."
echo

GATES="${GATES:-gate1 gate2 gate4 gate5 gate5f}"
for g in ${GATES}; do run_gate "${g}"; done

{
  echo "=== summary ==="
  date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
  echo "arch=$(uname -m) runsc=$(runsc --version 2>/dev/null | head -1)"
  fails=0
  for i in "${!NAMES[@]}"; do
    printf '%-8s %-8s %s\n' "${NAMES[i]}" "${VERDICTS[i]}" "${REASONS[i]}"
    [ "${VERDICTS[i]}" = PASS ] || fails=$((fails + 1))
  done
  echo "total=$(( $(date +%s) - STARTED ))s gates=${#NAMES[@]} not-passing=${fails}"
  # SKIPPED counts against the run. A suite that ran nothing must never read green.
  [ "${fails}" -eq 0 ] && echo "ALL GATES: PASS" || echo "ALL GATES: FAIL (${fails} not passing)"
} | tee "${SUMMARY}"
