#!/usr/bin/env bash
# Gate 5e: with a per-job network, which host-side chain actually binds a gVisor job?
#
# gate5c settled two things: a same-bridge peer is unreachable to iptables (this host
# has no br_netfilter, so switched frames never enter FORWARD), and a per-job network
# turns cross-job traffic into ROUTED traffic that DOCKER-ISOLATION already drops.
# What is still unmeasured is everything a job reaches by ROUTE rather than by switch:
# the host itself, and the metadata address.
#
# Discipline carried over, because it is what made the last two results trustworthy:
#   * a FRESH holder+plan namespace for every probe, ONE gVisor container in each;
#   * a health check printed beside every leg, and UNSOUND rather than a verdict if
#     the namespace was already sick;
#   * live listeners. Where a live listener is impossible (the metadata address), the
#     leg is reported as NO EVIDENCE unless bare and ruled runs DIFFER.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
NET_J1="${NET_J1:-maxplayer-dns-gate5e-job1}"
NET_J2="${NET_J2:-maxplayer-dns-gate5e-job2}"
SUBNET_J1="${SUBNET_J1:-172.31.19.0/24}"
GATEWAY_J1="${GATEWAY_J1:-172.31.19.1}"
SUBNET_J2="${SUBNET_J2:-172.31.20.0/24}"
GATEWAY_J2="${GATEWAY_J2:-172.31.20.1}"
RESOLVER="${RESOLVER:-1.1.1.1}"
HOST_LAN="${HOST_LAN:-192.168.5.15}"
HOST_PORT="${HOST_PORT:-49252}"
PUBLIC_REPO="${PUBLIC_REPO:-https://github.com/octocat/Hello-World.git}"
PLAN_FILE="${PLAN_FILE:-$HOME/gate5e-plan.txt}"
RESOLV_FILE="${RESOLV_FILE:-$HOME/gate5e-resolv.conf}"
OUT="${OUT:-$HOME/gate5e-evidence.txt}"
HOLDER_J2="gate5e-holder-job2"
NEIGH_J2="gate5e-neighbour-job2"

exec > >(tee "${OUT}") 2>&1
NS_SEQ=0
CUR_NS=""
NS_ADDR=""
declare -a ADDED_RULES=()

echo "=== gate5e: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m) docker=$(sudo docker version --format '{{.Server.Version}}') runsc=$(runsc --version | head -1)"
echo "br_netfilter=$(cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>/dev/null || echo absent)"
echo "host_lan=${HOST_LAN} (the VM's own eth0 — host-directed traffic lands in INPUT, not FORWARD)"

undo_rules() {
  local i
  for ((i = ${#ADDED_RULES[@]} - 1; i >= 0; i--)); do
    local chain="${ADDED_RULES[i]%%|*}" spec="${ADDED_RULES[i]#*|}"
    sudo iptables -D "${chain}" ${spec} >/dev/null 2>&1
  done
  ADDED_RULES=()
}
drop_ns() {
  undo_rules
  [ -n "${CUR_NS}" ] && { sudo docker rm -f "${CUR_NS}" >/dev/null 2>&1; CUR_NS=""; }
  return 0
}
cleanup() {
  drop_ns
  sudo docker rm -f "${HOLDER_J2}" "${NEIGH_J2}" >/dev/null 2>&1 || true
  sudo docker network rm "${NET_J1}" "${NET_J2}" >/dev/null 2>&1 || true
  pkill -f "gate5e-host-listener" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

# chmod 0444 below makes the file unwritable, so a second run of this script fails
# to rewrite it unless it is removed first. That cost a run's worth of noise once.
rm -f "${RESOLV_FILE}"
printf 'nameserver %s\noptions timeout:2 attempts:2\n' "${RESOLVER}" > "${RESOLV_FILE}"
chmod 0444 "${RESOLV_FILE}"

sudo docker network create --subnet "${SUBNET_J1}" --gateway "${GATEWAY_J1}" "${NET_J1}" >/dev/null
sudo docker network create --subnet "${SUBNET_J2}" --gateway "${GATEWAY_J2}" "${NET_J2}" >/dev/null
echo "per-job networks: job1=${SUBNET_J1} job2=${SUBNET_J2}"

# A live listener on the VM's own LAN address: a private destination reached by ROUTE,
# which docker isolation does not block, and which no rule blocks yet.
setsid python3 -c "
import socket,sys
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',${HOST_PORT})); s.listen(8)   # gate5e-host-listener
while True:
    c,_=s.accept(); c.sendall(b'REACHED'); c.close()
" >/dev/null 2>&1 </dev/null &
sleep 1
echo "live host listener: ${HOST_LAN}:${HOST_PORT} (bare tcp, answers every connection)"

# Job 2: a whole second job, on its own network, with a live listener in its namespace.
sudo timeout 120 docker run --detach --name "${HOLDER_J2}" --runtime runc --network "${NET_J2}" \
  --read-only --cap-drop ALL --security-opt no-new-privileges --user 65534:65534 \
  --entrypoint sleep "${IMAGE}" infinity >/dev/null
sudo timeout 60 docker run --detach --name "${NEIGH_J2}" --runtime runc \
  --network "container:${HOLDER_J2}" --user 65534:65534 --cap-drop ALL \
  --security-opt no-new-privileges --entrypoint node "${IMAGE}" \
  -e "require('http').createServer((q,r)=>r.end('REACHED')).listen(8080,'0.0.0.0')" >/dev/null
sleep 3
J2_ADDR="$(sudo docker inspect "${HOLDER_J2}" --format "{{(index .NetworkSettings.Networks \"${NET_J2}\").IPAddress}}")"
echo "live neighbour in job 2's own namespace: ${J2_ADDR}:8080"

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

fresh_ns() { # rules... each "CHAIN|spec with %NS% for this namespace's address"
  drop_ns
  NS_SEQ=$((NS_SEQ + 1))
  CUR_NS="gate5e-ns-${NS_SEQ}"
  sudo timeout 120 docker run --detach --name "${CUR_NS}" --runtime runc --network "${NET_J1}" \
    --read-only --cap-drop ALL --security-opt no-new-privileges --user 65534:65534 \
    --entrypoint sleep "${IMAGE}" infinity >/dev/null
  sudo timeout 120 docker run --rm --interactive --runtime runc --network "container:${CUR_NS}" \
    --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
    "${NETFILTER_IMAGE}" < "${PLAN_FILE}" >/dev/null
  NS_ADDR="$(sudo docker inspect "${CUR_NS}" --format "{{(index .NetworkSettings.Networks \"${NET_J1}\").IPAddress}}")"
  local r
  for r in "$@"; do
    [ -z "${r}" ] && continue
    local chain="${r%%|*}" spec="${r#*|}"
    spec="${spec//%NS%/${NS_ADDR}}"
    sudo iptables -I "${chain}" ${spec}
    ADDED_RULES+=("${chain}|${spec}")
  done
}
leg() { # label runtime host port rules...
  local label="$1" rt="$2" host="$3" port="$4"; shift 4
  fresh_ns "$@"
  local h; h="$(in_ns runc "${HEALTH}")"
  local shown="none"; [ ${#ADDED_RULES[@]} -gt 0 ] && shown="${ADDED_RULES[*]}"
  case "${h}" in
    HEALTHY*) echo "  ${label}: $(in_ns "${rt}" "${PROBE}" "${host}" "${port}")   [ns=${NS_ADDR} rules=${shown}]" ;;
    *)        echo "  ${label}: UNSOUND — namespace sick before the probe (${h})" ;;
  esac
}

echo
echo "=== gate5e: 1. the host itself, a routed private destination with a LIVE listener ==="
leg "runc  -> ${HOST_LAN}:${HOST_PORT}, bare"        runc  "${HOST_LAN}" "${HOST_PORT}"
leg "runsc -> ${HOST_LAN}:${HOST_PORT}, bare"        runsc "${HOST_LAN}" "${HOST_PORT}"
leg "runsc -> ${HOST_LAN}:${HOST_PORT}, DOCKER-USER" runsc "${HOST_LAN}" "${HOST_PORT}" \
  "DOCKER-USER|-s %NS%/32 -d 192.168.0.0/16 -j DROP"
leg "runsc -> ${HOST_LAN}:${HOST_PORT}, INPUT"       runsc "${HOST_LAN}" "${HOST_PORT}" \
  "INPUT|-s %NS%/32 -d 192.168.0.0/16 -j DROP"

echo
echo "=== gate5e: 2. the metadata address (no listener anywhere — read only the DIFFERENCE) ==="
leg "runsc -> 169.254.169.254:80, bare"        runsc "169.254.169.254" 80
leg "runsc -> 169.254.169.254:80, DOCKER-USER" runsc "169.254.169.254" 80 \
  "DOCKER-USER|-s %NS%/32 -d 169.254.169.254/32 -j DROP"

echo
echo "=== gate5e: 3. cross-job, each job on its OWN network (the regression proof) ==="
leg "runsc -> job 2's live listener" runsc "${J2_ADDR}" 8080

echo
echo "=== gate5e: 4. the public route, with the candidate rules installed ==="
fresh_ns "DOCKER-USER|-s %NS%/32 -d 169.254.169.254/32 -j DROP" \
         "INPUT|-s %NS%/32 -d 192.168.0.0/16 -j DROP"
echo "  health: $(in_ns runc "${HEALTH}")  rules=${ADDED_RULES[*]}"
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

fresh_ns "DOCKER-USER|-s %NS%/32 -d 169.254.169.254/32 -j DROP" \
         "INPUT|-s %NS%/32 -d 192.168.0.0/16 -j DROP"
echo "  health: $(in_ns runc "${HEALTH}")"
sudo timeout 180 docker run --rm --runtime runsc --network "container:${CUR_NS}" \
  --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
  -v "${RESOLV_FILE}:/etc/resolv.conf:ro" --entrypoint sh "${IMAGE}" -c '
    export HOME=/tmp GIT_TERMINAL_PROMPT=0
    cd /tmp && git clone --quiet "$1" repo 2>&1 && echo "PUBLIC-PASS git $(git -C repo rev-parse HEAD)"
  ' gate5e "${PUBLIC_REPO}" 2>&1 | tr -d '\r' | sed 's/^/  /'

echo
echo "=== gate5e: how to read this ==="
echo "leg 1 names the chain that binds a routed, host-directed destination for runsc."
echo "leg 2 is evidence ONLY if bare and ruled differ; identical timeouts prove nothing."
echo "leg 3 must be denied for per-job networks to be the cross-job answer."
echo "leg 4 must pass, or the candidate costs the job the route it exists to have."
