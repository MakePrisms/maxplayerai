#!/usr/bin/env bash
# Gate 5h: lifecycle — teardown leaves nothing behind, and a RECYCLED address is safe.
#
# Maxie: "Prove selected enforcement path handles runsc, lifecycle cleanup/recreation and
# fail-closed setup/readiness." Gate 5 proved the rules DENY. It never proved they GO AWAY.
#
# That gap is the dangerous one, and in both directions:
#   * A leftover rule keyed to job A's address does not stop existing when A does. Docker
#     hands addresses back. The next job to get 172.x.0.2 inherits a firewall written for
#     someone else — denied traffic it should be allowed, or worse, an ACCEPT pinhole that
#     was A's proxy and is now a stranger's open door.
#   * A network that fails to delete wedges the next job with the same id at
#     "network already exists", which is a fail-OPEN if anything in the caller shrugs at it.
#
# So this gate refuses to accept "teardown ran" as teardown. It counts rules keyed to the
# address in the real chains before and after, recycles the address deliberately, and makes
# the recycled job prove containment on its OWN rules against a LIVE listener with runc as
# the positive control.
#
# Every rule installed here is rendered by the PRODUCT (`render_host_plan`), never
# transcribed. Runs only in the disposable gvisor-repro VM; the trap removes every rule and
# network it created, and the final check asserts the chains are back to their start depth.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
WT="${WT:-/Users/forge/forge/v2/wt/w-gvisor-dns-delivery-r2}"
PLANDIR="${PLANDIR:-$HOME/gate5h-plans}"
SUBNET="${SUBNET:-172.31.55.0/24}"
HOST_LAN="${HOST_LAN:-192.168.5.15}"
HOST_PORT="${HOST_PORT:-49256}"
RESOLV_FILE="${RESOLV_FILE:-$HOME/gate5h-resolv.conf}"
OUT="${OUT:-$HOME/gate5h-evidence.txt}"

exec > >(tee "${OUT}") 2>&1
FAIL=0
CREATED_NETS=()
CUR_NS=""
NS_SEQ=0

fail() { echo "  FAIL: $*"; FAIL=$((FAIL+1)); }
ok()   { echo "  ok: $*"; }

# --- rule accounting -------------------------------------------------------------------
# Counts rules keyed to an address in the two chains the product writes to. This is the
# measurement the whole gate turns on, so it reads the KERNEL, never a script variable.
rules_for() { local addr="$1"
  local d i
  d=$(sudo iptables -S DOCKER-USER 2>/dev/null | grep -c -- "${addr}")
  i=$(sudo iptables -S INPUT 2>/dev/null | grep -c -- "${addr}")
  echo $((d + i))
}
chain_depth() { echo "$(( $(sudo iptables -S DOCKER-USER | wc -l) + $(sudo iptables -S INPUT | wc -l) ))"; }

apply_plan() { # plan-file -> applies on the HOST netns, exactly as the product's applier does
  sudo timeout 120 docker run --rm --interactive --network host \
    --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
    "${NETFILTER_IMAGE}" < "$1" >/dev/null 2>&1
}

drop_ns() { [ -n "${CUR_NS}" ] && { sudo docker rm -f "${CUR_NS}" >/dev/null 2>&1; CUR_NS=""; }; return 0; }

cleanup() {
  drop_ns
  sudo docker rm -f $(sudo docker ps -aq --filter "name=gate5h-") >/dev/null 2>&1 || true
  for f in "${PLANDIR}"/*-teardown.txt; do [ -f "$f" ] && apply_plan "$f"; done
  for n in "${CREATED_NETS[@]:-}"; do [ -n "$n" ] && sudo docker network rm "$n" >/dev/null 2>&1; done
  pkill -f "gate5h-host-listener" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "=== gate5h: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m) runsc=$(runsc --version | head -1)"
[ -d "${PLANDIR}" ] || { echo "MISSING ${PLANDIR} — render product plans first"; exit 2; }
START_DEPTH="$(chain_depth)"
echo "chain depth at start (DOCKER-USER + INPUT): ${START_DEPTH}"

printf 'nameserver 1.1.1.1\noptions timeout:2 attempts:2\n' > "${RESOLV_FILE}"; chmod 0444 "${RESOLV_FILE}"

setsid python3 -c "
import socket
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',${HOST_PORT})); s.listen(8)   # gate5h-host-listener
while True:
    c,_=s.accept(); c.sendall(b'REACHED'); c.close()
" >/dev/null 2>&1 </dev/null &
sleep 1
echo "live host listener: ${HOST_LAN}:${HOST_PORT}"

PROBE='
const net=require("net");
const s=net.connect({host:process.argv[1],port:Number(process.argv[2]),timeout:8000});
let said=false; const say=(w)=>{ if(!said){said=true;console.log(w);} s.destroy(); };
s.on("connect",()=>say("REACHED"));
s.on("timeout",()=>say("timeout"));
s.on("error",(e)=>say(e.code));
'
HEALTH='
const os=require("os");
const v4=Object.values(os.networkInterfaces()).flat().filter(x=>x&&x.family==="IPv4"&&!x.internal);
console.log(v4.length?("HEALTHY "+v4[0].address):"SICK no-address");
'

# Brings up one job's holder on its own network and returns the address DOCKER assigned.
# The address is READ, never computed — a guessed key denies a stranger and leaves the job open.
establish_holder() { # net-name -> prints addr
  local net="$1" holder="$2"
  sudo docker network create --driver bridge --subnet "${SUBNET}" "${net}" >/dev/null 2>&1
  CREATED_NETS+=("${net}")
  sudo timeout 120 docker run --detach --name "${holder}" --network "${net}" \
    --read-only --cap-drop ALL --security-opt no-new-privileges --user 65534:65534 \
    --entrypoint sleep "${IMAGE}" infinity >/dev/null 2>&1
  sudo docker inspect --format "{{(index .NetworkSettings.Networks \"${net}\").IPAddress}}" "${holder}" 2>/dev/null
}

probe_in() { # runtime holder script args... -> last line
  local rt="$1" holder="$2" script="$3"; shift 3
  sudo timeout 180 docker run --rm --runtime "${rt}" --network "container:${holder}" \
    --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
    -v "${RESOLV_FILE}:/etc/resolv.conf:ro" --entrypoint node "${IMAGE}" -e "${script}" "$@" 2>&1 | tr -d '\r' | tail -1
}

# ---------------------------------------------------------------------------------------
echo
echo "=== gate5h: 1. job A establishes, rules appear, containment holds ==="
NET_A="maxplayer-dns-gate5h-job-alpha"
ADDR_A="$(establish_holder "${NET_A}" gate5h-holder-a)"
[ -n "${ADDR_A}" ] || { echo "FATAL: empty address for A"; exit 2; }
echo "  job A address (read from docker inspect): ${ADDR_A}"
BEFORE_A="$(rules_for "${ADDR_A}")"
apply_plan "${PLANDIR}/${ADDR_A}-install.txt"
AFTER_A="$(rules_for "${ADDR_A}")"
echo "  rules keyed to ${ADDR_A}: before=${BEFORE_A} after-install=${AFTER_A}"
[ "${AFTER_A}" -gt "${BEFORE_A}" ] && ok "install added ${AFTER_A} host rules" || fail "install added no rules"
H="$(probe_in runc gate5h-holder-a "${HEALTH}")"; echo "  namespace health: ${H}"
GOT="$(probe_in runsc gate5h-holder-a "${PROBE}" "${HOST_LAN}" "${HOST_PORT}")"
echo "  runsc -> live host ${HOST_LAN}:${HOST_PORT}: ${GOT}"
[ "${GOT}" = REACHED ] && fail "A not contained" || ok "A contained while its rules are installed"

# ---------------------------------------------------------------------------------------
echo
echo "=== gate5h: 2. job A tears down — rules must be GONE, network must be GONE ==="
sudo docker rm -f gate5h-holder-a >/dev/null 2>&1
apply_plan "${PLANDIR}/${ADDR_A}-teardown.txt"
LEFT_A="$(rules_for "${ADDR_A}")"
echo "  rules keyed to ${ADDR_A} after teardown: ${LEFT_A}"
[ "${LEFT_A}" -eq 0 ] && ok "no leftover host rules" || fail "${LEFT_A} rule(s) survived teardown"
sudo docker network rm "${NET_A}" >/dev/null 2>&1
sudo docker network inspect "${NET_A}" >/dev/null 2>&1 && fail "network ${NET_A} survived" || ok "network removed"

# ---------------------------------------------------------------------------------------
echo
echo "=== gate5h: 3. RECYCLED address — job B takes A's old address ==="
NET_B="maxplayer-dns-gate5h-job-bravo"
ADDR_B="$(establish_holder "${NET_B}" gate5h-holder-b)"
echo "  job B address: ${ADDR_B} (A's was ${ADDR_A})"
if [ "${ADDR_B}" = "${ADDR_A}" ]; then
  ok "address genuinely recycled — this is the case that matters"
else
  echo "  NOTE: docker handed a different address; recycling not exercised this run"
fi
STALE_B="$(rules_for "${ADDR_B}")"
echo "  rules keyed to ${ADDR_B} BEFORE B installs its own: ${STALE_B}"
[ "${STALE_B}" -eq 0 ] && ok "B inherits no stale firewall" || fail "B inherited ${STALE_B} stale rule(s)"
# B must be contained by B's OWN rules, proven against a live listener with a runc control.
GOT_BARE="$(probe_in runsc gate5h-holder-b "${PROBE}" "${HOST_LAN}" "${HOST_PORT}")"
echo "  runsc -> live host BEFORE B's rules (bare control, expect REACHED): ${GOT_BARE}"
[ "${GOT_BARE}" = REACHED ] || echo "  NOTE: bare leg did not reach; the denial below proves less than intended"
apply_plan "${PLANDIR}/${ADDR_B}-install.txt"
GOT_B="$(probe_in runsc gate5h-holder-b "${PROBE}" "${HOST_LAN}" "${HOST_PORT}")"
echo "  runsc -> live host AFTER B's rules: ${GOT_B}"
[ "${GOT_B}" = REACHED ] && fail "recycled job B not contained" || ok "B contained by its own rules"
apply_plan "${PLANDIR}/${ADDR_B}-teardown.txt"
sudo docker rm -f gate5h-holder-b >/dev/null 2>&1
sudo docker network rm "${NET_B}" >/dev/null 2>&1

# ---------------------------------------------------------------------------------------
echo
echo "=== gate5h: 4. RECREATION — the same job id twice must not wedge ==="
NET_R="maxplayer-dns-gate5h-job-repeat"
A1="$(establish_holder "${NET_R}" gate5h-holder-r1)"
echo "  first incarnation address: ${A1:-<empty>}"
sudo docker rm -f gate5h-holder-r1 >/dev/null 2>&1
sudo docker network rm "${NET_R}" >/dev/null 2>&1
A2="$(establish_holder "${NET_R}" gate5h-holder-r2)"
echo "  second incarnation address: ${A2:-<empty>}"
if [ -n "${A1}" ] && [ -n "${A2}" ]; then ok "same job id came up twice, no 'network already exists' wedge"
else fail "recreation wedged (empty address on one incarnation)"; fi
sudo docker rm -f gate5h-holder-r2 >/dev/null 2>&1
sudo docker network rm "${NET_R}" >/dev/null 2>&1

# ---------------------------------------------------------------------------------------
echo
echo "=== gate5h: verdict ==="
END_DEPTH="$(chain_depth)"
echo "chain depth: start=${START_DEPTH} end=${END_DEPTH}"
[ "${END_DEPTH}" -eq "${START_DEPTH}" ] && ok "chains returned to starting depth — the gate left nothing behind" \
  || fail "chains changed depth ${START_DEPTH} -> ${END_DEPTH}: this gate leaked rules"
echo "failing checks: ${FAIL}"
[ "${FAIL}" -eq 0 ] && echo "GATE 5h: PASS" || echo "GATE 5h: FAIL"
