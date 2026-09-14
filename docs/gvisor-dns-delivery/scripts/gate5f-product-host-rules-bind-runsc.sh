#!/usr/bin/env bash
# Gate 5f: does the PRODUCT's own host-side policy bind a gVisor job?
#
# gate5b found the hole: the namespace OUTPUT plan does not bind a runsc job, because
# gVisor's netstack writes frames straight to the veth and the host OUTPUT chain in that
# namespace only ever sees host sockets. gate5c and gate5e then found where a runsc job
# CAN be bound: routed destinations, in DOCKER-USER, and host-directed ones in INPUT —
# while a same-bridge peer is bound by neither, this host having no br_netfilter.
#
# This is the regression proof for the fix built on those findings, and the rules under
# test are rendered by the product (`--example render_host_plan`, from HostPolicy), never
# transcribed here. A transcription would drift from the policy and this gate would keep
# passing against a firewall the product no longer builds.
#
# They are also installed the way the product installs them: piped into the SAME applier
# image the sidecar uses, in a --network host container, which is itself under test — if
# that image cannot write the root namespace's chains, this gate must fail, not paper over it.
#
# Discipline from gate5c/5d, which is what made those results trustworthy:
#   * a FRESH holder+plan namespace per probe, ONE gVisor container in each (the namespace
#     is single-use for gVisor: runsc takes the addresses into its netstack and never
#     gives them back, which voided an earlier run of gate5c);
#   * a health check printed beside every leg, UNSOUND instead of a verdict if it is sick;
#   * LIVE listeners. A timeout against an address where nothing listens is NO EVIDENCE:
#     absence and enforcement are indistinguishable. That mistake is why part of gate 2's
#     evidence was retracted.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
NET_J1="${NET_J1:-maxplayer-dns-gate5f-job1}"
NET_J2="${NET_J2:-maxplayer-dns-gate5f-job2}"
SUBNET_J1="${SUBNET_J1:-172.31.21.0/24}"
GATEWAY_J1="${GATEWAY_J1:-172.31.21.1}"
SUBNET_J2="${SUBNET_J2:-172.31.22.0/24}"
GATEWAY_J2="${GATEWAY_J2:-172.31.22.1}"
# The probe namespace's address is PINNED, because the host plan is rendered ahead of time
# for exactly this source key and a plan keyed to the wrong address would deny some other
# container while leaving this one open. The script refuses to run if they disagree.
JOB_ADDR="${JOB_ADDR:-172.31.21.10}"
SAME_BRIDGE_ADDR="${SAME_BRIDGE_ADDR:-172.31.21.20}"
J2_ADDR="${J2_ADDR:-172.31.22.10}"
RESOLVER="${RESOLVER:-1.1.1.1}"
HOST_LAN="${HOST_LAN:-192.168.5.15}"
HOST_PORT="${HOST_PORT:-49253}"
PUBLIC_REPO="${PUBLIC_REPO:-https://github.com/octocat/Hello-World.git}"
PLAN_FILE="${PLAN_FILE:-$HOME/gate5f-plan.txt}"
HOST_INSTALL="${HOST_INSTALL:-$HOME/gate5f-host-install.txt}"
HOST_TEARDOWN="${HOST_TEARDOWN:-$HOME/gate5f-host-teardown.txt}"
RESOLV_FILE="${RESOLV_FILE:-$HOME/gate5f-resolv.conf}"
OUT="${OUT:-$HOME/gate5f-evidence.txt}"
HOLDER_SAME="gate5f-holder-same-bridge"
NEIGH_SAME="gate5f-neighbour-same-bridge"
HOLDER_J2="gate5f-holder-job2"
NEIGH_J2="gate5f-neighbour-job2"

exec > >(tee "${OUT}") 2>&1
NS_SEQ=0
CUR_NS=""
HOST_RULES_UP=0

for f in "${PLAN_FILE}" "${HOST_INSTALL}" "${HOST_TEARDOWN}"; do
  [ -s "${f}" ] || { echo "MISSING rendered plan ${f} — render it on the host first"; exit 2; }
done
# The stale-plan guard. Cheap, and it is the difference between measuring this job's policy
# and measuring a rule that belongs to nothing.
grep -q -- "-s ${JOB_ADDR}/32" "${HOST_INSTALL}" || {
  echo "REFUSING TO RUN: ${HOST_INSTALL} carries no -s ${JOB_ADDR}/32 source key"; exit 2; }

echo "=== gate5f: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m) docker=$(sudo docker version --format '{{.Server.Version}}') runsc=$(runsc --version | head -1)"
echo "docker default-runtime=$(sudo docker info --format '{{.DefaultRuntime}}') (helpers inherit this; the JOB always carries --runtime runsc)"
echo "br_netfilter=$(cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>/dev/null || echo absent)"
echo "host plan rendered by the product for job_addr=${JOB_ADDR}: $(wc -l < "${HOST_INSTALL}") rules"

BASE_DOCKER_USER="$(sudo iptables -S DOCKER-USER | wc -l)"
BASE_INPUT="$(sudo iptables -S INPUT | wc -l)"
echo "chain depth before anything: DOCKER-USER=${BASE_DOCKER_USER} INPUT=${BASE_INPUT}"

host_rules() { # install|teardown
  local file="${HOST_INSTALL}" verb="install"
  [ "$1" = teardown ] && { file="${HOST_TEARDOWN}"; verb="teardown"; }
  # The product's applier, on the product's plan, in the root namespace. Same image as the
  # sidecar: one applier in this design, not two.
  local applied
  applied="$(sudo timeout 120 docker run --rm --interactive --network host \
    --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
    "${NETFILTER_IMAGE}" < "${file}" 2>&1 | tr -d '\r' | tail -1)"
  local expected; expected="$(wc -l < "${file}" | tr -d ' ')"
  echo "  host-rule ${verb}: applier reported ${applied}, plan had ${expected} rules"
  [ "${verb}" = install ] && HOST_RULES_UP=1 || HOST_RULES_UP=0
  # The applier's own account is not proof it reached the ROOT namespace's chains, so the
  # host kernel is asked directly. This readback is the whole reason the leg is trustworthy.
  local landed; landed="$(sudo iptables -S DOCKER-USER | grep -c -- "-s ${JOB_ADDR}/32")"
  local landed_in; landed_in="$(sudo iptables -S INPUT | grep -c -- "-s ${JOB_ADDR}/32")"
  echo "  readback from the host kernel: DOCKER-USER carries ${landed}, INPUT carries ${landed_in} rules for ${JOB_ADDR}"
}

drop_ns() {
  [ -n "${CUR_NS}" ] && { sudo docker rm -f "${CUR_NS}" >/dev/null 2>&1; CUR_NS=""; }
  return 0
}
cleanup() {
  drop_ns
  [ "${HOST_RULES_UP}" = 1 ] && host_rules teardown >/dev/null 2>&1
  sudo docker rm -f "${HOLDER_SAME}" "${NEIGH_SAME}" "${HOLDER_J2}" "${NEIGH_J2}" >/dev/null 2>&1 || true
  sudo docker network rm "${NET_J1}" "${NET_J2}" >/dev/null 2>&1 || true
  pkill -f "gate5f-host-listener" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

rm -f "${RESOLV_FILE}"
printf 'nameserver %s\noptions timeout:2 attempts:2\n' "${RESOLVER}" > "${RESOLV_FILE}"
chmod 0444 "${RESOLV_FILE}"

sudo docker network create --subnet "${SUBNET_J1}" --gateway "${GATEWAY_J1}" "${NET_J1}" >/dev/null
sudo docker network create --subnet "${SUBNET_J2}" --gateway "${GATEWAY_J2}" "${NET_J2}" >/dev/null

# A live listener on the VM's own LAN address: private, reached by ROUTE, and the one
# destination in this gate where the INPUT half of the plan can be caught working.
setsid python3 -c "
import socket
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',${HOST_PORT})); s.listen(8)   # gate5f-host-listener
while True:
    c,_=s.accept(); c.sendall(b'REACHED'); c.close()
" >/dev/null 2>&1 </dev/null &
sleep 1

# Neighbour A: another job on the SAME bridge — the shared-network arrangement this fix
# replaces. Neighbour B: a job on its OWN network — the arrangement the fix creates.
sudo timeout 120 docker run --detach --name "${HOLDER_SAME}" --network "${NET_J1}" \
  --ip "${SAME_BRIDGE_ADDR}" --read-only --cap-drop ALL --security-opt no-new-privileges \
  --user 65534:65534 --entrypoint sleep "${IMAGE}" infinity >/dev/null
sudo timeout 60 docker run --detach --name "${NEIGH_SAME}" --network "container:${HOLDER_SAME}" \
  --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges --entrypoint node "${IMAGE}" \
  -e "require('http').createServer((q,r)=>r.end('REACHED')).listen(8080,'0.0.0.0')" >/dev/null
sudo timeout 120 docker run --detach --name "${HOLDER_J2}" --network "${NET_J2}" \
  --ip "${J2_ADDR}" --read-only --cap-drop ALL --security-opt no-new-privileges \
  --user 65534:65534 --entrypoint sleep "${IMAGE}" infinity >/dev/null
sudo timeout 60 docker run --detach --name "${NEIGH_J2}" --network "container:${HOLDER_J2}" \
  --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges --entrypoint node "${IMAGE}" \
  -e "require('http').createServer((q,r)=>r.end('REACHED')).listen(8080,'0.0.0.0')" >/dev/null
sleep 3
echo "live listeners: same-bridge ${SAME_BRIDGE_ADDR}:8080 · own-network ${J2_ADDR}:8080 · host ${HOST_LAN}:${HOST_PORT}"

in_ns() { # runtime script args...
  local rt="$1"; shift
  local script="$1"; shift
  sudo timeout 180 docker run --rm --runtime "${rt}" --network "container:${CUR_NS}" \
    --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
    -v "${RESOLV_FILE}:/etc/resolv.conf:ro" --entrypoint node "${IMAGE}" \
    -e "${script}" "$@" 2>&1 | tr -d '\r' | tail -1
}
HEALTH='
const os=require("os"),dns=require("dns");
const v4=Object.values(os.networkInterfaces()).flat().filter(x=>x&&x.family==="IPv4"&&!x.internal);
if(!v4.length){console.log("SICK no-address");process.exit(0);}
dns.lookup("relay.maxplayer.ai",(e,a)=>console.log(e?("SICK dns-"+e.code):("HEALTHY "+v4[0].address)));
'
PROBE='
const net=require("net");
const s=net.connect({host:process.argv[1],port:Number(process.argv[2]),timeout:8000});
let said=false; const say=(w)=>{ if(!said){said=true;console.log(w);} s.destroy(); };
s.on("connect",()=>say("REACHED"));
s.on("timeout",()=>say("timeout"));
s.on("error",(e)=>say(e.code));
'

fresh_ns() {
  drop_ns
  NS_SEQ=$((NS_SEQ + 1))
  CUR_NS="gate5f-ns-${NS_SEQ}"
  sudo timeout 120 docker run --detach --name "${CUR_NS}" --network "${NET_J1}" \
    --ip "${JOB_ADDR}" --read-only --cap-drop ALL --security-opt no-new-privileges \
    --user 65534:65534 --entrypoint sleep "${IMAGE}" infinity >/dev/null
  sudo timeout 120 docker run --rm --interactive --network "container:${CUR_NS}" \
    --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
    "${NETFILTER_IMAGE}" < "${PLAN_FILE}" >/dev/null
  local got; got="$(sudo docker inspect "${CUR_NS}" --format "{{(index .NetworkSettings.Networks \"${NET_J1}\").IPAddress}}")"
  [ "${got}" = "${JOB_ADDR}" ] || echo "  WARNING: namespace came up as ${got}, not the ${JOB_ADDR} the host plan is keyed to"
}
leg() { # label runtime host port
  local label="$1" rt="$2" host="$3" port="$4"
  fresh_ns
  local h; h="$(in_ns runc "${HEALTH}")"
  case "${h}" in
    HEALTHY*) echo "  ${label}: $(in_ns "${rt}" "${PROBE}" "${host}" "${port}")" ;;
    *)        echo "  ${label}: UNSOUND — namespace sick before the probe (${h})" ;;
  esac
}

echo
echo "=== gate5f: 1. BEFORE — the hole, with no host rules ==="
leg "runsc -> same-bridge neighbour ${SAME_BRIDGE_ADDR}:8080" runsc "${SAME_BRIDGE_ADDR}" 8080
leg "runsc -> own-network neighbour ${J2_ADDR}:8080"          runsc "${J2_ADDR}" 8080
leg "runsc -> host ${HOST_LAN}:${HOST_PORT}"                  runsc "${HOST_LAN}" "${HOST_PORT}"
leg "runc  -> host ${HOST_LAN}:${HOST_PORT}"                  runc  "${HOST_LAN}" "${HOST_PORT}"

echo
echo "=== gate5f: 2. installing the product's rendered host policy ==="
host_rules install

echo
echo "=== gate5f: 3. AFTER — the same probes, unchanged ==="
leg "runsc -> own-network neighbour ${J2_ADDR}:8080  (MUST be denied)" runsc "${J2_ADDR}" 8080
leg "runsc -> host ${HOST_LAN}:${HOST_PORT}          (MUST be denied)" runsc "${HOST_LAN}" "${HOST_PORT}"
leg "runc  -> host ${HOST_LAN}:${HOST_PORT}          (MUST be denied)" runc  "${HOST_LAN}" "${HOST_PORT}"
leg "runsc -> same-bridge neighbour ${SAME_BRIDGE_ADDR}:8080  (known unbindable)" runsc "${SAME_BRIDGE_ADDR}" 8080

echo
echo "=== gate5f: 4. the public route must survive the policy ==="
fresh_ns
echo "  health: $(in_ns runc "${HEALTH}")"
in_ns runsc '
const dns=require("dns"),https=require("https");
dns.lookup("relay.maxplayer.ai",(e,a)=>{
  if(e) return console.log("PUBLIC-FAIL dns "+e.code);
  const r=https.request({host:"relay.maxplayer.ai",port:443,path:"/",method:"HEAD",timeout:15000},(res)=>{
    console.log((res.socket.authorized?"PUBLIC-PASS":"PUBLIC-FAIL")+" dns="+a+" tls="+res.statusCode+" verified="+res.socket.authorized);
    res.resume(); r.destroy();
  });
  r.on("timeout",()=>{console.log("PUBLIC-FAIL tls timeout");r.destroy();});
  r.on("error",(x)=>console.log("PUBLIC-FAIL tls "+x.code));
  r.end();
});
' | sed 's/^/  /'
fresh_ns
echo "  health: $(in_ns runc "${HEALTH}")"
sudo timeout 180 docker run --rm --runtime runsc --network "container:${CUR_NS}" \
  --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
  -v "${RESOLV_FILE}:/etc/resolv.conf:ro" --entrypoint sh "${IMAGE}" -c '
    export HOME=/tmp GIT_TERMINAL_PROMPT=0
    cd /tmp && git clone --quiet "$1" repo 2>&1 && echo "PUBLIC-PASS git $(git -C repo rev-parse HEAD)"
  ' gate5f "${PUBLIC_REPO}" 2>&1 | tr -d '\r' | sed 's/^/  /'

echo
echo "=== gate5f: 5. teardown must leave the shared chains exactly as it found them ==="
drop_ns
host_rules teardown
AFTER_DOCKER_USER="$(sudo iptables -S DOCKER-USER | wc -l)"
AFTER_INPUT="$(sudo iptables -S INPUT | wc -l)"
echo "  chain depth after teardown: DOCKER-USER=${AFTER_DOCKER_USER} (was ${BASE_DOCKER_USER}) INPUT=${AFTER_INPUT} (was ${BASE_INPUT})"
if [ "${AFTER_DOCKER_USER}" = "${BASE_DOCKER_USER}" ] && [ "${AFTER_INPUT}" = "${BASE_INPUT}" ]; then
  echo "  CLEAN — no rule leaked (this is the failure mode the HostRules drop guard exists to prevent)"
else
  echo "  LEAKED — the shared chains kept rules after teardown"
fi

echo
echo "=== gate5f: how to read this ==="
echo "The fix is proved by section 3 differing from section 1 on the own-network and host legs."
echo "The same-bridge leg is expected to stay REACHED: switched frames enter no chain on a host"
echo "without br_netfilter, which is WHY the product gives every job its own network rather than"
echo "trying to rule its way out of a shared one. It is measured here so that reason stays evidenced."
echo "Section 4 must pass, or the policy costs the job the delivery route it exists to have."
