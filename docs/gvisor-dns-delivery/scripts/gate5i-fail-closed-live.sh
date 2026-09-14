#!/usr/bin/env bash
# Gate 5i: fail-closed — a job must not run on a network the product could not contain.
#
# Maxie: "Prove selected enforcement path handles runsc, lifecycle cleanup/recreation and
# fail-closed setup/readiness."
#
# `establish()` refuses at four points and the rendering tests now lock the plan invariants.
# What neither covers is the claim the code makes about the WORLD, in this comment:
#
#     "A truncated stdin applies cleanly and exits 0, so no exit code reveals it; only
#      comparing the sidecar's own total against what was rendered does."
#
# If that is wrong — if a truncated plan failed loudly — the count cross-check would be
# belt-and-braces. If it is right, the cross-check is the ONLY thing standing between a
# half-installed firewall and a job that believes it is contained. That is worth measuring
# rather than asserting, so leg C measures it, and then asks the question that actually
# matters: with a partial firewall, is the job still contained? Against a LIVE listener.
#
# Product-rendered plans only. Runs in the disposable gvisor-repro VM. The trap removes
# every rule it created and the verdict asserts the chains came back to their start depth.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
PLANDIR="${PLANDIR:-$HOME/gate5h-plans}"
SUBNET="${SUBNET:-172.31.56.0/24}"
NET="${NET:-maxplayer-dns-gate5i-job}"
HOST_LAN="${HOST_LAN:-192.168.5.15}"
HOST_PORT="${HOST_PORT:-49257}"
RESOLV_FILE="${RESOLV_FILE:-$HOME/gate5i-resolv.conf}"
OUT="${OUT:-$HOME/gate5i-evidence.txt}"

exec > >(tee "${OUT}") 2>&1
FAIL=0
fail() { echo "  FAIL: $*"; FAIL=$((FAIL+1)); }
ok()   { echo "  ok: $*"; }

rules_for() { local a="$1"
  echo $(( $(sudo iptables -S DOCKER-USER 2>/dev/null | grep -c -- "${a}") \
         + $(sudo iptables -S INPUT 2>/dev/null | grep -c -- "${a}") )); }
chain_depth() { echo "$(( $(sudo iptables -S DOCKER-USER | wc -l) + $(sudo iptables -S INPUT | wc -l) ))"; }

cleanup() {
  sudo docker rm -f $(sudo docker ps -aq --filter "name=gate5i-") >/dev/null 2>&1 || true
  [ -n "${ADDR:-}" ] && [ -f "${PLANDIR}/${ADDR}-teardown.txt" ] && \
    sudo timeout 120 docker run --rm --interactive --network host --cap-drop ALL \
      --cap-add NET_ADMIN --security-opt no-new-privileges "${NETFILTER_IMAGE}" \
      < "${PLANDIR}/${ADDR}-teardown.txt" >/dev/null 2>&1
  sudo docker network rm "${NET}" >/dev/null 2>&1 || true
  pkill -f "gate5i-host-listener" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "=== gate5i: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m) runsc=$(runsc --version | head -1)"
START_DEPTH="$(chain_depth)"; echo "chain depth at start: ${START_DEPTH}"

printf 'nameserver 1.1.1.1\noptions timeout:2 attempts:2\n' > "${RESOLV_FILE}"; chmod 0444 "${RESOLV_FILE}"
setsid python3 -c "
import socket
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',${HOST_PORT})); s.listen(8)   # gate5i-host-listener
while True:
    c,_=s.accept(); c.sendall(b'REACHED'); c.close()
" >/dev/null 2>&1 </dev/null &
sleep 1
echo "live host listener: ${HOST_LAN}:${HOST_PORT}"

sudo docker network create --driver bridge --subnet "${SUBNET}" "${NET}" >/dev/null 2>&1
sudo timeout 120 docker run --detach --name gate5i-holder --network "${NET}" \
  --read-only --cap-drop ALL --security-opt no-new-privileges --user 65534:65534 \
  --entrypoint sleep "${IMAGE}" infinity >/dev/null 2>&1
ADDR="$(sudo docker inspect --format "{{(index .NetworkSettings.Networks \"${NET}\").IPAddress}}" gate5i-holder)"
echo "job address: ${ADDR}"
PLAN="${PLANDIR}/${ADDR}-install.txt"
[ -s "${PLAN}" ] || { echo "MISSING ${PLAN} — render it with render_host_plan"; exit 2; }
EXPECTED="$(grep -c . "${PLAN}")"
echo "rendered plan: ${EXPECTED} rules"

PROBE='
const net=require("net");
const s=net.connect({host:process.argv[1],port:Number(process.argv[2]),timeout:8000});
let said=false; const say=(w)=>{ if(!said){said=true;console.log(w);} s.destroy(); };
s.on("connect",()=>say("REACHED")); s.on("timeout",()=>say("timeout")); s.on("error",e=>say(e.code));
'
probe() { sudo timeout 180 docker run --rm --runtime "$1" --network "container:gate5i-holder" \
  --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
  -v "${RESOLV_FILE}:/etc/resolv.conf:ro" --entrypoint node "${IMAGE}" -e "${PROBE}" \
  "${HOST_LAN}" "${HOST_PORT}" 2>&1 | tr -d '\r' | tail -1; }

# ---------------------------------------------------------------------------------------
echo
echo "=== gate5i: A. applier cannot start (bad image) — nothing may be installed ==="
OUT_A="$(sudo timeout 120 docker run --rm --interactive --network host --cap-drop ALL \
  --cap-add NET_ADMIN --security-opt no-new-privileges \
  "ghcr.io/makeprisms/maxplayer-netfilter:definitely-not-a-tag" < "${PLAN}" 2>&1 | tail -1)"
RC_A=$?
echo "  applier said: $(echo "${OUT_A}" | cut -c1-90)"
LEFT_A="$(rules_for "${ADDR}")"
echo "  rules keyed to ${ADDR}: ${LEFT_A}"
[ "${LEFT_A}" -eq 0 ] && ok "a non-starting applier installs nothing (establish() maps this to a hard error)" \
  || fail "${LEFT_A} rules present after the applier failed to start"

echo
echo "=== gate5i: B. applier without NET_ADMIN — must not silently succeed ==="
OUT_B="$(sudo timeout 120 docker run --rm --interactive --network host --cap-drop ALL \
  --security-opt no-new-privileges "${NETFILTER_IMAGE}" < "${PLAN}" 2>&1 | tail -1)"
echo "  applier said: $(echo "${OUT_B}" | cut -c1-90)"
LEFT_B="$(rules_for "${ADDR}")"
echo "  rules keyed to ${ADDR}: ${LEFT_B}"
if [ "${LEFT_B}" -eq "${EXPECTED}" ]; then
  fail "the full policy installed without NET_ADMIN — the capability is not what gates this"
elif [ "${LEFT_B}" -eq 0 ]; then
  ok "no capability, no rules"
else
  echo "  PARTIAL: ${LEFT_B} of ${EXPECTED} — exactly the half-installed state the count check exists for"
fi

echo
echo "=== gate5i: C. TRUNCATED plan — does it apply cleanly and lie? ==="
TRUNC="$HOME/gate5i-truncated.txt"
head -9 "${PLAN}" > "${TRUNC}"
echo "  feeding ${EXPECTED} rendered rules as a $(grep -c . "${TRUNC}")-line plan"
OUT_C="$(sudo timeout 120 docker run --rm --interactive --network host --cap-drop ALL \
  --cap-add NET_ADMIN --security-opt no-new-privileges "${NETFILTER_IMAGE}" < "${TRUNC}" 2>&1 | tail -1)"
RC_C=$?
echo "  applier exit=${RC_C} said=$(echo "${OUT_C}" | cut -c1-60)"
INSTALLED_C="$(rules_for "${ADDR}")"
echo "  rules actually in the kernel: ${INSTALLED_C} of ${EXPECTED} rendered"
if [ "${RC_C}" -eq 0 ] && [ "${INSTALLED_C}" -lt "${EXPECTED}" ]; then
  ok "CONFIRMED: a truncated plan exits 0 while under-installing — only the count reveals it"
else
  echo "  NOTE: truncation did not present as a clean exit; the code comment overstates the risk"
fi
[ "${INSTALLED_C}" -ne "${EXPECTED}" ] && ok "count cross-check would refuse this job (${INSTALLED_C} != ${EXPECTED})" \
  || fail "count check could not distinguish the truncated plan"

# The question that matters: a partial firewall is not a firewall. Measure it.
GOT_C="$(probe runsc)"
echo "  runsc -> live host ${HOST_LAN}:${HOST_PORT} with a PARTIAL firewall: ${GOT_C}"
if [ "${GOT_C}" = REACHED ]; then
  ok "a partially-installed policy leaves the job UNCONTAINED — refusing the job is the only safe move"
else
  echo "  NOTE: this particular truncation still denied the probed destination; another would not"
fi

echo
echo "=== gate5i: D. teardown of a PARTIAL install ==="
# Two strategies, run back to back on the same partial install, because the difference between
# them IS the fix. D1 is what HostRules::drop did before gate 5i; it is kept as a live regression
# witness rather than deleted, so that if the applier's abort-on-first-failure behaviour ever
# changes, this gate says so instead of silently keeping a workaround nobody needs.
#
# Note honestly what this leg is: the Rust branch in HostRules::drop is covered by unit tests
# (`adopted_host_rules_are_not_complete_until_the_count_check_passes`,
# `the_per_rule_teardown_renders_one_valid_delete_per_rule`). What a shell gate can prove, and
# what these two legs do prove, is that the per-rule STRATEGY actually clears real rules from
# real chains where the one-shot inverse plan cannot.
echo "  D1: one-shot inverse plan (the pre-fix behaviour)"
sudo timeout 120 docker run --rm --interactive --network host --cap-drop ALL \
  --cap-add NET_ADMIN --security-opt no-new-privileges "${NETFILTER_IMAGE}" \
  < "${PLANDIR}/${ADDR}-teardown.txt" >/dev/null 2>&1
LEFT_D1="$(rules_for "${ADDR}")"
echo "      rules remaining: ${LEFT_D1} (of ${INSTALLED_C} installed)"
if [ "${LEFT_D1}" -gt 0 ]; then
  echo "      as expected: the applier aborts on the first never-created rule and removes nothing"
else
  echo "      NOTE: the one-shot plan cleared it — the applier no longer aborts; revisit the fix"
fi

echo "  D2: per-rule teardown (what HostRules::drop now does on the partial path)"
REMOVED=0
while IFS= read -r line; do
  [ -n "${line}" ] || continue
  if printf '%s\n' "${line}" | sudo timeout 60 docker run --rm --interactive --network host \
       --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
       "${NETFILTER_IMAGE}" >/dev/null 2>&1; then
    REMOVED=$((REMOVED+1))
  fi
done < "${PLANDIR}/${ADDR}-teardown.txt"
LEFT_D="$(rules_for "${ADDR}")"
echo "      removed ${REMOVED} rule(s) one at a time; ${LEFT_D} remain"
[ "${LEFT_D}" -eq 0 ] && ok "the partial install came out — a missing rule no longer strands the present ones" \
  || fail "${LEFT_D} rule(s) survived even the per-rule teardown"

echo
echo "=== gate5i: verdict ==="
END_DEPTH="$(chain_depth)"
echo "chain depth: start=${START_DEPTH} end=${END_DEPTH}"
[ "${END_DEPTH}" -eq "${START_DEPTH}" ] && ok "chains returned to starting depth" \
  || fail "chains changed ${START_DEPTH} -> ${END_DEPTH}"
echo "failing checks: ${FAIL}"
[ "${FAIL}" -eq 0 ] && echo "GATE 5i: PASS" || echo "GATE 5i: FAIL"
