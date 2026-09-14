#!/usr/bin/env bash
# Gate 5b: does the egress plan actually BIND a gVisor job?
#
# Gate 5 turned up something that cannot be waved away: from a runsc job inside a
# namespace carrying the full 26-rule plan, a container at 172.31.x.x — squarely
# inside the `172.16.0.0/12 -j DROP` rule — was REACHED.
#
# Every other "denial" in gate 5 was a timeout or a refusal to an address where
# NOTHING IS LISTENING. Absence looks exactly like enforcement. So the only honest
# test is a destination that is (a) inside a DROPped range and (b) has a real
# listener, approached from the same namespace by two runtimes:
#
#     runsc job -> neighbour   vs   runc job -> neighbour
#
# If runc is dropped and runsc gets through, the plan is not binding the gVisor job
# and the containment story for this branch is wrong as written.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
NET="${NET:-maxplayer-dns-gate5b}"
SUBNET="${SUBNET:-172.31.13.0/24}"
GATEWAY="${GATEWAY:-172.31.13.1}"
RESOLVER="${RESOLVER:-1.1.1.1}"
PLAN_FILE="${PLAN_FILE:-$HOME/gate5b-plan.txt}"
OUT="${OUT:-$HOME/gate5b-evidence.txt}"
HOLDER_A="gate5b-holder-a"
HOLDER_B="gate5b-holder-b"
NEIGHBOUR="gate5b-neighbour"

exec > >(tee "${OUT}") 2>&1

echo "=== gate5b: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m)"
echo "docker=$(sudo docker version --format '{{.Server.Version}}') runsc=$(runsc --version | head -1)"

cleanup() {
  sudo docker rm -f "${HOLDER_A}" "${HOLDER_B}" "${NEIGHBOUR}" >/dev/null 2>&1 || true
  sudo docker network rm "${NET}" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

sudo docker network create --subnet "${SUBNET}" --gateway "${GATEWAY}" "${NET}" >/dev/null
echo "network=${NET} subnet=${SUBNET} plan_rules=$(grep -c . "${PLAN_FILE}")"

for holder in "${HOLDER_A}" "${HOLDER_B}"; do
  sudo timeout 120 docker run --detach --name "${holder}" --runtime runc --network "${NET}" \
    --read-only --cap-drop ALL --security-opt no-new-privileges --user 65534:65534 \
    --entrypoint sleep "${IMAGE}" infinity >/dev/null
done
# Only namespace A carries the plan; B is just where the neighbour lives.
sudo timeout 120 docker run --rm --interactive --runtime runc \
  --network "container:${HOLDER_A}" --cap-drop ALL --cap-add NET_ADMIN \
  --security-opt no-new-privileges "${NETFILTER_IMAGE}" < "${PLAN_FILE}"
echo "plan_applied_rules_reported=$?"

B_ADDR="$(sudo docker inspect "${HOLDER_B}" --format "{{(index .NetworkSettings.Networks \"${NET}\").IPAddress}}")"
sudo timeout 60 docker run --detach --name "${NEIGHBOUR}" --runtime runc \
  --network "container:${HOLDER_B}" --user 65534:65534 --cap-drop ALL \
  --security-opt no-new-privileges --entrypoint node "${IMAGE}" \
  -e "require('http').createServer((q,r)=>r.end('REACHED')).listen(8080,'0.0.0.0')" >/dev/null
sleep 3
echo "neighbour=${B_ADDR}:8080 running=$(sudo docker inspect -f '{{.State.Running}}' "${NEIGHBOUR}")"
echo "neighbour_is_inside_a_dropped_range=172.16.0.0/12"

PROBE='
const net=require("net");
const s=net.connect({host:process.argv[1],port:8080,timeout:8000});
s.on("connect",()=>{console.log("RESULT=REACHED");s.destroy();});
s.on("timeout",()=>{console.log("RESULT=timeout");s.destroy();});
s.on("error",(e)=>{console.log("RESULT="+e.code);s.destroy();});
'

echo
echo "=== gate5b: the same probe, the same namespace, two runtimes ==="
for rt in runc runsc; do
  r="$(sudo timeout 60 docker run --rm --runtime "${rt}" --network "container:${HOLDER_A}" \
        --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
        --entrypoint node "${IMAGE}" -e "${PROBE}" "${B_ADDR}" 2>&1 | tr -d '\r')"
  echo "job_runtime=${rt} ${r}"
  eval "res_${rt}=\"${r}\""
done

# Is the rule even visible from inside the namespace? Ask the host runtime, which can see it.
echo
echo "=== gate5b: the rule as installed, read back from the namespace ==="
sudo timeout 60 docker run --rm --runtime runc --network "container:${HOLDER_A}" \
  --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
  --entrypoint iptables "${NETFILTER_IMAGE}" -S OUTPUT 2>&1 | grep -n "172.16.0.0/12" || true

echo
echo "=== gate5b: verdict ==="
echo "runc_result=${res_runc}"
echo "runsc_result=${res_runsc}"
if [ "${res_runc}" = "RESULT=REACHED" ]; then
  echo "INCONCLUSIVE: the plan did not stop the host runtime either — the harness, not the runtime, is wrong"
  exit 2
fi
if [ "${res_runsc}" = "RESULT=REACHED" ]; then
  echo "FINDING: the plan binds a runc job and DOES NOT BIND a runsc job."
  echo "gVisor's netstack emits packets to the veth itself; the host kernel's OUTPUT chain"
  echo "in that netns never sees them, so per-job egress policy is not enforced for the job"
  echo "it is written for. Containment for gVisor jobs cannot live in the netns OUTPUT chain."
  exit 1
fi
echo "NO FINDING: both runtimes were denied; gate 5's REACHED was a harness artifact"
exit 0
