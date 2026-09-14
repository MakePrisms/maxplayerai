#!/usr/bin/env bash
# gate5k — does an UNSUPPORTED daemon default runtime fail CLOSED?
#
# Maxie's ruling (9 Sep 2026): "Missing --runtime means daemon default, not
# guaranteed runc; test unsupported defaults fail closed."
#
# The correction lands on the product. `holder_argv()` deliberately passes NO
# `--runtime`, and `the_containment_plane_never_carries_the_jobs_runtime` locks
# it there, because a runsc holder's namespace is unusable: a job joining it
# sees `lo` only (gate2a). The design therefore ASSUMES the daemon default is
# runc, and nothing in the product verifies that assumption.
#
# Fail CLOSED = the job gets no usable egress, or never starts.
# Fail OPEN   = the job runs and REACHES a live listener with no containment.
# Only the second is a security defect; the first is an availability failure.
#
# v3. Two earlier drafts are committed as failures and kept:
#   v1 gate5k-CONFOUNDED-harness-defect-*.txt  — listener started with `docker
#      exec` into a runsc holder, which runsc refuses; target never served.
#   v2 gate5k-v2-daemon-default-runtime-*.txt  — root cause found: the sandbox
#      image has NO nc and NO wget, so every probe was doomed before it ran.
# This version uses the primitives the other gates already proved work in this
# image: `--entrypoint node` with inline JS. gate5f/5g/5h/5i never used nc or
# wget, which is why their SERVING/REACHED readings were real.
#
# Discipline: ONE gVisor container per namespace (gate5d: namespaces are
# single-use for gVisor), a fresh namespace per probe, a LIVE listener, and a
# liveness reading printed beside every leg so a silence is never mistaken for
# containment.
#
# Runs inside the disposable gvisor-repro VM ONLY: it edits
# /etc/docker/daemon.json and restarts docker.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
DAEMON_JSON=/etc/docker/daemon.json
BACKUP="/home/forge.guest/daemon.json.gate5k.bak"
NET="gate5k-net"
FAIL=0
LISTEN_ADDR=""

say() { printf '%s\n' "$*"; }
hr()  { printf -- '---- %s\n' "$*"; }

[ -f "${DAEMON_JSON}" ] || { say "ABORT: no ${DAEMON_JSON}"; exit 2; }
if command -v limactl >/dev/null 2>&1; then
  say "ABORT: limactl present — this looks like the HOST, not the disposable VM"; exit 2
fi

SERVER_JS='require("http").createServer((q,r)=>r.end("REACHED")).listen(8080,"0.0.0.0");'

LIVENESS_JS='const n=require("net");const s=n.connect({host:process.argv[1],port:8080,timeout:5000});
s.on("connect",()=>{console.log("SERVING");s.destroy();});
s.on("timeout",()=>{console.log("DEAD timeout");s.destroy();});
s.on("error",e=>console.log("DEAD "+e.code));'

PROBE_JS='const os=require("os"),http=require("http");
const v4=Object.values(os.networkInterfaces()).flat().filter(x=>x&&x.family==="IPv4"&&!x.internal);
process.stdout.write(v4.length?("HEALTHY "+v4[0].address):"SICK no-address");
const r=http.get({host:process.argv[1],port:8080,timeout:8000},res=>{let d="";
  res.on("data",c=>d+=c);res.on("end",()=>console.log(" | target="+d.trim()));});
r.on("timeout",()=>{console.log(" | target=TIMEOUT");r.destroy();});
r.on("error",e=>console.log(" | target="+e.code));'

default_runtime() { sudo docker info --format '{{.DefaultRuntime}}' 2>/dev/null; }

restore() {
  hr "restoring ${DAEMON_JSON}"
  if [ -f "${BACKUP}" ]; then
    sudo cp "${BACKUP}" "${DAEMON_JSON}"
    sudo systemctl restart docker || true
    sleep 5
    say "restored default runtime = $(default_runtime)"
  fi
  sudo docker rm -f gate5k-listener gate5k-ns1 gate5k-ns2 >/dev/null 2>&1
  sudo docker network rm "${NET}" >/dev/null 2>&1
}
trap restore EXIT

sudo cp "${DAEMON_JSON}" "${BACKUP}"
say "gate5k v3 — unsupported daemon default runtime"
say "kernel:  $(uname -srm)"
say "docker:  $(sudo docker version --format '{{.Server.Version}}')"
say "runsc:   $(runsc --version 2>/dev/null | head -1)"
say "ORIGINAL default runtime: $(default_runtime)"
say ""

sudo docker network create --driver bridge "${NET}" >/dev/null 2>&1

# Test infrastructure, pinned to runc so it keeps working when the default flips
# underneath it. gate5g's confound (a listener's own OUTPUT rules dropping its
# replies) cannot arise here: gate5k installs no netns plan at all.
sudo docker rm -f gate5k-listener >/dev/null 2>&1
sudo timeout 120 docker run --detach --name gate5k-listener --runtime runc \
  --network "${NET}" --entrypoint node "${IMAGE}" -e "${SERVER_JS}" >/dev/null 2>&1

serve() { # a daemon restart stops containers; revive and re-read the address
  sudo docker start gate5k-listener >/dev/null 2>&1
  sleep 4
  LISTEN_ADDR="$(sudo docker inspect -f "{{(index .NetworkSettings.Networks \"${NET}\").IPAddress}}" gate5k-listener 2>/dev/null)"
}

liveness() {
  sudo timeout 60 docker run --rm --runtime runc --network "${NET}" \
    --entrypoint node "${IMAGE}" -e "${LIVENESS_JS}" "${LISTEN_ADDR}" 2>&1 | tr -d '\r' | tail -1
}

probe() { # ns_name — product-SHAPED holder carrying NO --runtime, then ONE gVisor job
  local ns="$1"
  sudo docker rm -f "${ns}" >/dev/null 2>&1
  sudo timeout 120 docker run --detach --name "${ns}" --network "${NET}" \
    --entrypoint sleep "${IMAGE}" infinity >/dev/null 2>&1
  local started=$?
  sleep 4
  if ! sudo docker ps --format '{{.Names}}' | grep -qx "${ns}"; then
    say "  holder: DID NOT START (docker run exit=${started}) -> no job can run"
    return 3
  fi
  say "  holder runtime: $(sudo docker inspect -f '{{.HostConfig.Runtime}}' "${ns}" 2>/dev/null)"
  local out
  out="$(sudo timeout 120 docker run --rm --runtime runsc --network "container:${ns}" \
    --entrypoint node "${IMAGE}" -e "${PROBE_JS}" "${LISTEN_ADDR}" 2>&1 | tr -d '\r' | tail -2 | tr '\n' ' ')"
  if [ -z "${out}" ]; then
    say "  job: PRODUCED NO OUTPUT -> could not run in this namespace (fail closed by refusal)"
  else
    say "  job: ${out}"
  fi
}

hr "LEG 1 — control: daemon default is runc (the supported configuration)"
say "default runtime now: $(default_runtime)"
serve
L1="$(liveness)"
say "listener ${LISTEN_ADDR} liveness: ${L1}"
if [ "${L1}" != "SERVING" ]; then
  say "leg 1 target is dead — NO EVIDENCE"; FAIL=$((FAIL + 1))
fi
probe gate5k-ns1
say "  ^ expected here: HEALTHY <addr> and target=REACHED. This leg is the"
say "    positive control: it proves the harness CAN observe reachability, so"
say "    leg 2's silence means something."
say ""

hr "LEG 2 — daemon default switched to runsc (the UNSUPPORTED configuration)"
printf '{\n  "default-runtime": "runsc",\n  "runtimes": { "runsc": { "path": "/usr/local/bin/runsc" } }\n}\n' \
  | sudo tee "${DAEMON_JSON}" >/dev/null
sudo systemctl restart docker
sleep 7
NOW="$(default_runtime)"
say "default runtime now: ${NOW}"
if [ "${NOW}" != "runsc" ]; then
  say "could not switch the default runtime — leg 2 is NO EVIDENCE"
  FAIL=$((FAIL + 1))
else
  serve
  L2="$(liveness)"
  say "listener ${LISTEN_ADDR} liveness: ${L2}"
  if [ "${L2}" != "SERVING" ]; then
    say "leg 2 target is dead — NO EVIDENCE, not a fail-closed result"
    FAIL=$((FAIL + 1))
  fi
  probe gate5k-ns2
fi
say ""

hr "VERDICT"
say "With a SERVING listener in both legs: leg 1 REACHED and leg 2 SICK/denied"
say "means the unsupported default FAILS CLOSED — no egress path to contain, an"
say "availability failure rather than a containment bypass. Leg 2 printing"
say "target=REACHED would mean it FAILS OPEN and is a security defect."
say "failing_checks=${FAIL}  (non-zero means a leg above proved nothing)"
exit 0
