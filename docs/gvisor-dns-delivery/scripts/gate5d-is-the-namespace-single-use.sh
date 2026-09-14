#!/usr/bin/env bash
# Gate 5d: does a runsc container leave the shared namespace usable behind it?
#
# gate5c's later legs all returned ENETUNREACH — including the runc CONTROL, which
# had worked minutes earlier in the same namespace, and including DNS to 1.1.1.1
# which no rule under test touched. A control that dies is not a control, so
# gate5c's verdicts on both candidate enforcement sites are void until this is
# settled. The same signature appeared in gate 5: the first runsc container
# resolved fine, the second could not resolve at all.
#
# The suspicion: runsc's netstack takes the veth's addresses and routes INTO the
# sandbox, and does not put them back when it exits — leaving the namespace
# stripped for whatever runs next. If so, the shared job namespace is SINGLE-USE
# for gVisor, and every multi-container measurement in this branch has to be read
# again with that in mind.
#
# The image has no iproute2, so the namespace is read with node's
# os.networkInterfaces() — the same way the earlier probes read it.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
NET="${NET:-maxplayer-dns-gate5d}"
SUBNET="${SUBNET:-172.31.16.0/24}"
GATEWAY="${GATEWAY:-172.31.16.1}"
RESOLVER="${RESOLVER:-1.1.1.1}"
PLAN_FILE="${PLAN_FILE:-$HOME/gate5d-plan.txt}"
RESOLV_FILE="${RESOLV_FILE:-$HOME/gate5d-resolv.conf}"
OUT="${OUT:-$HOME/gate5d-evidence.txt}"
HOLDER="gate5d-holder"

exec > >(tee "${OUT}") 2>&1

echo "=== gate5d: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m) docker=$(sudo docker version --format '{{.Server.Version}}') runsc=$(runsc --version | head -1)"

cleanup() {
  sudo docker rm -f "${HOLDER}" >/dev/null 2>&1 || true
  sudo docker network rm "${NET}" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

printf 'nameserver %s\noptions timeout:2 attempts:2\n' "${RESOLVER}" > "${RESOLV_FILE}"
chmod 0444 "${RESOLV_FILE}"
sudo docker network create --subnet "${SUBNET}" --gateway "${GATEWAY}" "${NET}" >/dev/null
sudo timeout 120 docker run --detach --name "${HOLDER}" --runtime runc --network "${NET}" \
  --read-only --cap-drop ALL --security-opt no-new-privileges --user 65534:65534 \
  --entrypoint sleep "${IMAGE}" infinity >/dev/null
sudo timeout 120 docker run --rm --interactive --runtime runc --network "container:${HOLDER}" \
  --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
  "${NETFILTER_IMAGE}" < "${PLAN_FILE}" >/dev/null
echo "namespace established, plan applied ($(grep -c . "${PLAN_FILE}") rules)"

READ_NS='
const os=require("os"),dns=require("dns");
const ifs=os.networkInterfaces();
const v4=Object.entries(ifs).flatMap(([n,a])=>(a||[]).filter(x=>x.family==="IPv4").map(x=>n+"="+x.address));
console.log("interfaces: "+(v4.join(" ")||"NONE"));
dns.lookup("relay.maxplayer.ai",(e,a)=>console.log("dns: "+(e?e.code:a)));
'
look() { # runtime label
  echo "  [$2 via $1] $(sudo timeout 60 docker run --rm --runtime "$1" --network "container:${HOLDER}" \
    --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
    -v "${RESOLV_FILE}:/etc/resolv.conf:ro" --entrypoint node "${IMAGE}" -e "${READ_NS}" 2>&1 | tr -d '\r' | tr '\n' ' ')"
}

echo
echo "=== gate5d: the namespace before any gVisor container has touched it ==="
look runc "before"

echo
echo "=== gate5d: one runsc container runs and exits ==="
look runsc "the gVisor job itself"

echo
echo "=== gate5d: the same namespace afterwards ==="
look runc "after, host runtime"
look runsc "after, a second gVisor job"

echo
echo "=== gate5d: read it off the two 'after' lines ==="
echo "If 'before' has an eth0 address and 'after' says NONE, the namespace is single-use"
echo "for gVisor, and gate5c's ENETUNREACH verdicts are void — a dead control, not a policy."
