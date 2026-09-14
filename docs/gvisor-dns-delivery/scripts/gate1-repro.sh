#!/usr/bin/env bash
# Gate 1: reproduce the named-bridge DNS failure under runsc, with runc as a
# DIAGNOSTIC CONTROL only. Runs inside the disposable VM. Bounded: every docker
# run carries a timeout and the network is removed on exit.
#
# runc appears here to isolate the variable. It is never a fallback path for
# production jobs.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NET="${NET:-maxplayer-dns-repro}"
HOST_TARGET="${HOST_TARGET:-relay.maxplayer.ai}"
OUT="${OUT:-$HOME/gate1-evidence.txt}"

exec > >(tee "${OUT}") 2>&1

echo "=== gate1: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m)"
. /etc/os-release && echo "os=${PRETTY_NAME}"
echo "docker=$(sudo docker version --format '{{.Server.Version}}')"
echo "runsc=$(runsc --version | head -1)"
echo "image=${IMAGE}"
echo "image_digest=$(sudo docker image inspect "${IMAGE}" --format '{{index .RepoDigests 0}}')"
echo "image_arch=$(sudo docker image inspect "${IMAGE}" --format '{{.Architecture}}/{{.Os}}')"

cleanup() { sudo docker network rm "${NET}" >/dev/null 2>&1 || true; }
trap cleanup EXIT
sudo docker network rm "${NET}" >/dev/null 2>&1 || true
sudo docker network create "${NET}" >/dev/null
echo "network=${NET} subnet=$(sudo docker network inspect "${NET}" --format '{{(index .IPAM.Config 0).Subnet}}')"

# One probe body, run identically under both runtimes: resolve, then report the
# resolver the container was actually handed.
PROBE='const dns=require("dns");const fs=require("fs");
console.log("resolv.conf:", fs.readFileSync("/etc/resolv.conf","utf8").trim().replace(/\n/g,"|"));
dns.lookup(process.argv[1],(e,a)=>{console.log("lookup:", e?("ERR "+e.code):("OK "+a));process.exitCode=e?1:0});'

run_probe() {
  local runtime="$1"
  echo
  echo "=== gate1: dns lookup under --runtime ${runtime} ==="
  sudo timeout 90 docker run --rm --runtime "${runtime}" --network "${NET}" \
    --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
    --entrypoint node "${IMAGE}" -e "${PROBE}" "${HOST_TARGET}"
  echo "exit=$?"
}

run_probe runsc
run_probe runc

echo
echo "=== gate1: raw udp/53 to the embedded resolver under runsc ==="
sudo timeout 90 docker run --rm --runtime runsc --network "${NET}" \
  --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
  --entrypoint node "${IMAGE}" -e '
const dgram=require("dgram");const s=dgram.createSocket("udp4");
const q=Buffer.from("abcd01000001000000000000057265" +
  "6c6179096d6178706c61796572026169000001" + "0001","hex");
const t=setTimeout(()=>{console.log("udp53: TIMEOUT (no answer from 127.0.0.11)");s.close();process.exitCode=1;},8000);
s.on("message",(m)=>{clearTimeout(t);console.log("udp53: ANSWER "+m.length+" bytes");s.close();});
s.on("error",(e)=>{clearTimeout(t);console.log("udp53: ERR "+e.code);s.close();process.exitCode=1;});
s.send(q,53,"127.0.0.11");'
echo "exit=$?"

echo
echo "=== gate1: done ==="
