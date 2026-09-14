#!/usr/bin/env bash
# Gate 5c (rewritten): if the netns OUTPUT chain does not bind a gVisor job, what does?
#
# The first version of this script measured a dead namespace and had to be thrown
# away: gate5d showed a runsc container takes the namespace's addresses into its
# netstack and never returns them, so the namespace is usable exactly ONCE and
# every leg after the first gVisor container was reading a corpse.
#
# This version obeys the rule that finding forced:
#   * ONE gVisor container per namespace — every probe gets a FRESH holder+plan;
#   * a health check immediately before any leg meant to be evidence, and a leg
#     whose namespace was already sick is reported UNSOUND, never as a denial;
#   * every denial targets a LIVE listener, because a timeout to an address where
#     nothing listens is indistinguishable from enforcement.
#
# Candidates, both on the far side of the veth where the host kernel handles the
# packet whatever produced it:
#   (a) DOCKER-USER (root-netns FORWARD path) keyed to the namespace's address;
#   (b) a per-job network instead of the one shared bridge all jobs share today.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
NET_A="${NET_A:-maxplayer-dns-gate5c-a}"
NET_B="${NET_B:-maxplayer-dns-gate5c-b}"
SUBNET_A="${SUBNET_A:-172.31.17.0/24}"
GATEWAY_A="${GATEWAY_A:-172.31.17.1}"
SUBNET_B="${SUBNET_B:-172.31.18.0/24}"
GATEWAY_B="${GATEWAY_B:-172.31.18.1}"
RESOLVER="${RESOLVER:-1.1.1.1}"
PUBLIC_REPO="${PUBLIC_REPO:-https://github.com/octocat/Hello-World.git}"
PLAN_FILE="${PLAN_FILE:-$HOME/gate5c-plan.txt}"
RESOLV_FILE="${RESOLV_FILE:-$HOME/gate5c-resolv.conf}"
OUT="${OUT:-$HOME/gate5c-evidence.txt}"
HOLDER_SAME="gate5c-holder-same"
HOLDER_FAR="gate5c-holder-far"
NEIGH_SAME="gate5c-neighbour-same"
NEIGH_FAR="gate5c-neighbour-far"

exec > >(tee "${OUT}") 2>&1
NS_SEQ=0
CUR_NS=""
CUR_RULE=""

echo "=== gate5c: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m) docker=$(sudo docker version --format '{{.Server.Version}}') runsc=$(runsc --version | head -1)"
echo "br_netfilter=$(cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>/dev/null || echo absent)"
echo "rule: one gVisor container per namespace; health check before every evidential leg"

drop_ns() {
  [ -n "${CUR_RULE}" ] && { sudo iptables -D DOCKER-USER ${CUR_RULE} >/dev/null 2>&1; CUR_RULE=""; }
  [ -n "${CUR_NS}" ] && { sudo docker rm -f "${CUR_NS}" >/dev/null 2>&1; CUR_NS=""; }
  return 0
}
cleanup() {
  drop_ns
  sudo docker rm -f "${HOLDER_SAME}" "${HOLDER_FAR}" "${NEIGH_SAME}" "${NEIGH_FAR}" >/dev/null 2>&1 || true
  sudo docker network rm "${NET_A}" "${NET_B}" >/dev/null 2>&1 || true
  # Belt and braces: any stray rule this script could have left in DOCKER-USER.
  while sudo iptables -S DOCKER-USER 2>/dev/null | grep -q -- "-d 172.16.0.0/12 -j DROP"; do
    sudo iptables -D DOCKER-USER $(sudo iptables -S DOCKER-USER | grep -m1 -- "-d 172.16.0.0/12 -j DROP" | sed 's/^-A DOCKER-USER //') || break
  done
}
trap cleanup EXIT
cleanup

printf 'nameserver %s\noptions timeout:2 attempts:2\n' "${RESOLVER}" > "${RESOLV_FILE}"
chmod 0444 "${RESOLV_FILE}"
sudo docker network create --subnet "${SUBNET_A}" --gateway "${GATEWAY_A}" "${NET_A}" >/dev/null
sudo docker network create --subnet "${SUBNET_B}" --gateway "${GATEWAY_B}" "${NET_B}" >/dev/null

holder_on() { # name network
  sudo timeout 120 docker run --detach --name "$1" --runtime runc --network "$2" \
    --read-only --cap-drop ALL --security-opt no-new-privileges --user 65534:65534 \
    --entrypoint sleep "${IMAGE}" infinity >/dev/null
}
addr_of() { sudo docker inspect "$1" --format "{{(index .NetworkSettings.Networks \"$2\").IPAddress}}"; }

# The live neighbours. Both are runc-only, so no gVisor container ever enters their
# namespaces and they stay alive for the whole run.
holder_on "${HOLDER_SAME}" "${NET_A}"
holder_on "${HOLDER_FAR}" "${NET_B}"
for pair in "${NEIGH_SAME} ${HOLDER_SAME}" "${NEIGH_FAR} ${HOLDER_FAR}"; do
  set -- ${pair}
  sudo timeout 60 docker run --detach --name "$1" --runtime runc --network "container:$2" \
    --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
    --entrypoint node "${IMAGE}" \
    -e "require('http').createServer((q,r)=>r.end('REACHED')).listen(8080,'0.0.0.0')" >/dev/null
done
sleep 3
SAME_ADDR="$(addr_of "${HOLDER_SAME}" "${NET_A}")"
FAR_ADDR="$(addr_of "${HOLDER_FAR}" "${NET_B}")"
echo "live neighbours: same_bridge=${SAME_ADDR}:8080 other_bridge=${FAR_ADDR}:8080"

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
dns.lookup("relay.maxplayer.ai",(e,a)=>console.log(e?("SICK dns-"+e.code):("HEALTHY "+v4[0].address+" dns="+a)));
'
PROBE='
const net=require("net");
const s=net.connect({host:process.argv[1],port:Number(process.argv[2]),timeout:8000});
let said=false; const say=(w)=>{ if(!said){said=true;console.log(w);} s.destroy(); };
s.on("connect",()=>say("REACHED"));
s.on("timeout",()=>say("timeout"));
s.on("error",(e)=>say(e.code));
'

fresh_ns() { # with_rule("rule"|"")
  drop_ns
  NS_SEQ=$((NS_SEQ + 1))
  CUR_NS="gate5c-ns-${NS_SEQ}"
  holder_on "${CUR_NS}" "${NET_A}"
  sudo timeout 120 docker run --rm --interactive --runtime runc \
    --network "container:${CUR_NS}" --cap-drop ALL --cap-add NET_ADMIN \
    --security-opt no-new-privileges "${NETFILTER_IMAGE}" < "${PLAN_FILE}" >/dev/null
  NS_ADDR="$(addr_of "${CUR_NS}" "${NET_A}")"
  if [ "$1" = "rule" ]; then
    CUR_RULE="-s ${NS_ADDR}/32 -d 172.16.0.0/12 -j DROP"
    sudo iptables -I DOCKER-USER ${CUR_RULE}
  fi
}

leg() { # label with_rule runtime host port
  fresh_ns "$2"
  local h; h="$(in_ns runc "${HEALTH}")"
  case "${h}" in
    HEALTHY*) echo "  ${1}: $(in_ns "$3" "${PROBE}" "$4" "$5")   [ns=${NS_ADDR} rule=${CUR_RULE:-none} health=${h}]" ;;
    *)        echo "  ${1}: UNSOUND — namespace was already sick before the probe (${h})" ;;
  esac
}

echo
echo "=== gate5c: baseline — today's behaviour, no host-side rule ==="
leg "runc  -> live neighbour, same bridge" ""     runc  "${SAME_ADDR}" 8080
leg "runsc -> live neighbour, same bridge" ""     runsc "${SAME_ADDR}" 8080

echo
echo "=== gate5c: (b) per-job network — the neighbour is on a DIFFERENT bridge ==="
leg "runsc -> live neighbour, other bridge" ""    runsc "${FAR_ADDR}" 8080

echo
echo "=== gate5c: (a) DOCKER-USER keyed to the namespace's own address ==="
leg "runc  -> live neighbour, same bridge" rule   runc  "${SAME_ADDR}" 8080
leg "runsc -> live neighbour, same bridge" rule   runsc "${SAME_ADDR}" 8080

echo
echo "=== gate5c: does (a) cost the public route the job must keep? ==="
fresh_ns rule
H="$(in_ns runc "${HEALTH}")"
echo "  health before the public leg: ${H}"
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

fresh_ns rule
echo "  health before the git leg: $(in_ns runc "${HEALTH}")"
sudo timeout 180 docker run --rm --runtime runsc --network "container:${CUR_NS}" \
  --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
  -v "${RESOLV_FILE}:/etc/resolv.conf:ro" --entrypoint sh "${IMAGE}" -c '
    export HOME=/tmp GIT_TERMINAL_PROMPT=0
    cd /tmp && git clone --quiet "$1" repo 2>&1 && echo "PUBLIC-PASS git $(git -C repo rev-parse HEAD)"
  ' gate5c "${PUBLIC_REPO}" 2>&1 | tr -d '\r' | sed 's/^/  /'

echo
echo "=== gate5c: how to read this ==="
echo "(a) binds runsc only if BOTH rule-legs are denied AND both public legs still pass."
echo "(b) binds runsc only if the other-bridge leg is denied from a HEALTHY namespace."
