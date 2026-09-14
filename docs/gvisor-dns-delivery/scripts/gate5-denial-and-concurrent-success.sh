#!/usr/bin/env bash
# Gate 5: denial holds, and concurrent jobs still deliver.
#
# This is a REWRITE. The first version of this script produced a FAIL that is kept in
# evidence/, and it was unsound in three ways that the sub-experiments (gate5b–gate5f)
# then exposed. All three are fixed here, and naming them is part of the gate:
#
#   1. It probed addresses where NOTHING LISTENED and read the resulting timeouts as
#      denial. Absence and enforcement are indistinguishable that way — the same error
#      cost gate 2 its "metadata denied" line. Every denial leg below is either aimed at
#      a LIVE listener, or reported as NO EVIDENCE unless bare and ruled runs DIFFER.
#   2. It reused one namespace across gVisor probes. The namespace is single-use for
#      gVisor: runsc takes the addresses into its netstack and never gives them back, so
#      every later leg ran in a namespace with `lo` only and "passed" by being broken.
#      One gVisor container per namespace here, with a health check beside every leg.
#   3. It double-counted a request timeout and scored a SUCCESS as PUBLIC-FAIL.
#
# The rules under test are rendered by the PRODUCT (`--example render_net_plan` and
# `--example render_host_plan`), never transcribed here.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
PLAN_DIR="${PLAN_DIR:-$HOME/gate5-plans}"
RESOLVER="${RESOLVER:-1.1.1.1}"
HOST_LAN="${HOST_LAN:-192.168.5.15}"
HOST_PORT="${HOST_PORT:-49254}"
PUBLIC_REPO="${PUBLIC_REPO:-https://github.com/octocat/Hello-World.git}"
RESOLV_FILE="${RESOLV_FILE:-$HOME/gate5-resolv.conf}"
HOSTS_FILE="${HOSTS_FILE:-$HOME/gate5-hosts}"
OUT="${OUT:-$HOME/gate5-evidence.txt}"
DENIED_NAME="denied-lan.maxplayer.test"

exec > >(tee "${OUT}") 2>&1
CUR_NS=""
NS_SEQ=0
declare -a RULES_UP=()
FAILURES=0
note_fail() { FAILURES=$((FAILURES + 1)); }

for tag in d n c1 c2 c3; do
  for f in "plan-${tag}.txt" "host-${tag}.txt" "host-${tag}-teardown.txt"; do
    [ -s "${PLAN_DIR}/${f}" ] || { echo "MISSING rendered plan ${PLAN_DIR}/${f}"; exit 2; }
  done
done

# tag -> network, subnet, gateway, pinned address
net_of()    { echo "maxplayer-dns-gate5-$1"; }
subnet_of() { case "$1" in d) echo 172.31.30.0/24;; n) echo 172.31.34.0/24;; c1) echo 172.31.31.0/24;; c2) echo 172.31.32.0/24;; c3) echo 172.31.33.0/24;; esac; }
gw_of()     { case "$1" in d) echo 172.31.30.1;;   n) echo 172.31.34.1;;   c1) echo 172.31.31.1;;   c2) echo 172.31.32.1;;   c3) echo 172.31.33.1;;   esac; }
addr_of()   { case "$1" in d) echo 172.31.30.10;;  n) echo 172.31.34.10;;  c1) echo 172.31.31.10;;  c2) echo 172.31.32.10;;  c3) echo 172.31.33.10;;  esac; }

# The stale-plan guard from gate5f: a host plan keyed to the wrong address denies some
# other container and leaves this job open, and nothing in the run would show it.
for tag in d n c1 c2 c3; do
  grep -q -- "-s $(addr_of "${tag}")/32" "${PLAN_DIR}/host-${tag}.txt" ||
    { echo "REFUSING TO RUN: host-${tag}.txt is not keyed to $(addr_of "${tag}")"; exit 2; }
done

echo "=== gate5: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m) docker=$(sudo docker version --format '{{.Server.Version}}') runsc=$(runsc --version | head -1)"
echo "docker default-runtime=$(sudo docker info --format '{{.DefaultRuntime}}') · br_netfilter=$(cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>/dev/null || echo absent)"
BASE_DU="$(sudo iptables -S DOCKER-USER | wc -l)"; BASE_IN="$(sudo iptables -S INPUT | wc -l)"
echo "chain depth before anything: DOCKER-USER=${BASE_DU} INPUT=${BASE_IN}"

apply_host() { # tag install|teardown
  local tag="$1" verb="$2" file="${PLAN_DIR}/host-$1.txt"
  [ "${verb}" = teardown ] && file="${PLAN_DIR}/host-$1-teardown.txt"
  local applied expected
  applied="$(sudo timeout 120 docker run --rm --interactive --network host \
    --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
    "${NETFILTER_IMAGE}" < "${file}" 2>&1 | tr -d '\r' | tail -1)"
  expected="$(wc -l < "${file}" | tr -d ' ')"
  local landed; landed="$(( $(sudo iptables -S DOCKER-USER | grep -c -- "-s $(addr_of "${tag}")/32") + $(sudo iptables -S INPUT | grep -c -- "-s $(addr_of "${tag}")/32") ))"
  echo "  host policy ${verb} for $(addr_of "${tag}"): applier=${applied}/${expected}, host kernel now carries ${landed}"
  if [ "${verb}" = install ]; then RULES_UP+=("${tag}"); else
    local keep=(); local t; for t in "${RULES_UP[@]:-}"; do [ -n "${t}" ] && [ "${t}" != "${tag}" ] && keep+=("${t}"); done
    RULES_UP=("${keep[@]:-}")
  fi
}
drop_ns() { [ -n "${CUR_NS}" ] && { sudo docker rm -f "${CUR_NS}" >/dev/null 2>&1; CUR_NS=""; }; return 0; }
cleanup() {
  drop_ns
  local t; for t in "${RULES_UP[@]:-}"; do [ -n "${t}" ] && apply_host "${t}" teardown >/dev/null 2>&1; done
  sudo docker rm -f gate5-neigh-holder gate5-neigh $(sudo docker ps -aq --filter "name=gate5-") >/dev/null 2>&1 || true
  for t in d n c1 c2 c3; do sudo docker network rm "$(net_of "${t}")" >/dev/null 2>&1; done
  pkill -f "gate5-host-listener" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

rm -f "${RESOLV_FILE}" "${HOSTS_FILE}"
printf 'nameserver %s\noptions timeout:2 attempts:2\n' "${RESOLVER}" > "${RESOLV_FILE}"
# A name that resolves INTO a denied range, without depending on a third-party wildcard
# DNS service being up. The point of the leg is that reaching a denied address BY NAME is
# denied too, which no earlier gate measured.
printf '127.0.0.1 localhost\n%s %s\n' "${HOST_LAN}" "${DENIED_NAME}" > "${HOSTS_FILE}"
chmod 0444 "${RESOLV_FILE}" "${HOSTS_FILE}"

for t in d n c1 c2 c3; do
  sudo docker network create --subnet "$(subnet_of "${t}")" --gateway "$(gw_of "${t}")" "$(net_of "${t}")" >/dev/null
done

setsid python3 -c "
import socket
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',${HOST_PORT})); s.listen(16)   # gate5-host-listener
while True:
    c,_=s.accept(); c.sendall(b'REACHED'); c.close()
" >/dev/null 2>&1 </dev/null &
sleep 1

# A live listener in ANOTHER job's namespace, on its own network: a private destination
# reached by route, inside 172.16.0.0/12.
sudo timeout 120 docker run --detach --name gate5-neigh-holder --network "$(net_of n)" \
  --ip "$(addr_of n)" --read-only --cap-drop ALL --security-opt no-new-privileges \
  --user 65534:65534 --entrypoint sleep "${IMAGE}" infinity >/dev/null
sudo timeout 60 docker run --detach --name gate5-neigh --network "container:gate5-neigh-holder" \
  --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges --entrypoint node "${IMAGE}" \
  -e "require('http').createServer((q,r)=>r.end('REACHED')).listen(8080,'0.0.0.0')" >/dev/null
sleep 3
echo "live listeners: host ${HOST_LAN}:${HOST_PORT} · neighbour job $(addr_of n):8080 · name ${DENIED_NAME} -> ${HOST_LAN}"

in_ns() { # runtime script args...
  local rt="$1"; shift; local script="$1"; shift
  sudo timeout 180 docker run --rm --runtime "${rt}" --network "container:${CUR_NS}" \
    --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
    -v "${RESOLV_FILE}:/etc/resolv.conf:ro" -v "${HOSTS_FILE}:/etc/hosts:ro" \
    --entrypoint node "${IMAGE}" -e "${script}" "$@" 2>&1 | tr -d '\r' | tail -1
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
V6='
const os=require("os");
const v6=Object.values(os.networkInterfaces()).flat().filter(x=>x&&x.family==="IPv6"&&!x.internal);
console.log(v6.length?("HAS-V6 "+v6.map(x=>x.address).join(",")):"NO-V6");
'
fresh_ns() { # tag
  drop_ns
  NS_SEQ=$((NS_SEQ + 1)); CUR_NS="gate5-ns-$1-${NS_SEQ}"
  sudo timeout 120 docker run --detach --name "${CUR_NS}" --network "$(net_of "$1")" \
    --ip "$(addr_of "$1")" --read-only --cap-drop ALL --security-opt no-new-privileges \
    --user 65534:65534 --entrypoint sleep "${IMAGE}" infinity >/dev/null
  sudo timeout 120 docker run --rm --interactive --network "container:${CUR_NS}" \
    --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
    "${NETFILTER_IMAGE}" < "${PLAN_DIR}/plan-$1.txt" >/dev/null
}
probe_leg() { # label expect host port
  local label="$1" expect="$2" host="$3" port="$4"
  fresh_ns d
  local h; h="$(in_ns runc "${HEALTH}")"
  case "${h}" in
    HEALTHY*) ;;
    *) echo "  ${label}: UNSOUND — ${h}"; note_fail; return ;;
  esac
  local got; got="$(in_ns runsc "${PROBE}" "${host}" "${port}")"
  if [ "${expect}" = denied ]; then
    case "${got}" in
      REACHED) echo "  ${label}: ${got}   *** FAIL, expected denial ***"; note_fail ;;
      *)       echo "  ${label}: ${got}   (denied)" ;;
    esac
  else
    echo "  ${label}: ${got}"
  fi
}

echo
echo "=== gate5: 1. denial, BEFORE the host policy (so each leg has a baseline) ==="
probe_leg "runsc -> host ${HOST_LAN}:${HOST_PORT} (live)"        measure "${HOST_LAN}" "${HOST_PORT}"
probe_leg "runsc -> ${DENIED_NAME}:${HOST_PORT} (live, by name)" measure "${DENIED_NAME}" "${HOST_PORT}"
probe_leg "runsc -> neighbour job $(addr_of n):8080 (live)"      measure "$(addr_of n)" 8080
probe_leg "runsc -> 169.254.169.254:80 (nothing listens)"        measure 169.254.169.254 80

echo
echo "=== gate5: 2. the product's host policy, installed for the probe namespace ==="
apply_host d install

echo
echo "=== gate5: 3. denial, AFTER — every one of these MUST be denied ==="
probe_leg "runsc -> host ${HOST_LAN}:${HOST_PORT} (live)"        denied "${HOST_LAN}" "${HOST_PORT}"
probe_leg "runsc -> ${DENIED_NAME}:${HOST_PORT} (live, by name)" denied "${DENIED_NAME}" "${HOST_PORT}"
probe_leg "runsc -> neighbour job $(addr_of n):8080 (live)"      denied "$(addr_of n)" 8080
probe_leg "runsc -> 169.254.169.254:80 (nothing listens)"        denied 169.254.169.254 80
echo "  ^ read the metadata leg ONLY as the difference against section 1; with no listener,"
echo "    an identical result in both sections is NO EVIDENCE either way."

echo
echo "=== gate5: 4. IPv6, measured rather than assumed ==="
fresh_ns d
echo "  namespace v6: $(in_ns runc "${V6}")"
echo "  runsc v6:     $(in_ns runsc "${V6}")"
echo "  The host-side plan renders NO ip6tables rules (DOCKER-USER may not exist there)."
echo "  Where the job namespace has no global IPv6 there is nothing to deny; where it has,"
echo "  host-side v6 denial for a runsc job is UNPROVEN and must not be claimed."

echo
echo "=== gate5: 5. concurrent delivery, three jobs at once, all policies installed ==="
for t in c1 c2 c3; do apply_host "${t}" install; done
declare -a PIDS=()
for t in c1 c2 c3; do
  (
    ns="gate5-ns-${t}"
    sudo timeout 120 docker run --detach --name "${ns}" --network "$(net_of "${t}")" \
      --ip "$(addr_of "${t}")" --read-only --cap-drop ALL --security-opt no-new-privileges \
      --user 65534:65534 --entrypoint sleep "${IMAGE}" infinity >/dev/null
    sudo timeout 120 docker run --rm --interactive --network "container:${ns}" \
      --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
      "${NETFILTER_IMAGE}" < "${PLAN_DIR}/plan-${t}.txt" >/dev/null
    # ONE gVisor container in this namespace, doing all three things, because a second one
    # would find the namespace already emptied into the first one's netstack.
    out="$(sudo timeout 240 docker run --rm --runtime runsc --network "container:${ns}" \
      --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
      -v "${RESOLV_FILE}:/etc/resolv.conf:ro" --entrypoint sh "${IMAGE}" -c '
        export HOME=/tmp GIT_TERMINAL_PROMPT=0
        node -e '"'"'
          const dns=require("dns"),https=require("https");
          dns.lookup("relay.maxplayer.ai",(e,a)=>{
            if(e){console.log("FAIL dns "+e.code);process.exit(0);}
            let done=false; const say=(w)=>{if(!done){done=true;console.log(w);}};
            const r=https.request({host:"relay.maxplayer.ai",port:443,path:"/",method:"HEAD",timeout:20000},(res)=>{
              say((res.socket.authorized?"OK":"FAIL")+" dns="+a+" tls="+res.statusCode+" verified="+res.socket.authorized);
              res.resume(); r.destroy();
            });
            r.on("timeout",()=>{say("FAIL tls timeout");r.destroy();});
            r.on("error",(x)=>say("FAIL tls "+x.code));
            r.end();
          });
        '"'"'
        cd /tmp && git clone --quiet "$1" repo >/dev/null 2>&1 &&
          echo "OK git $(git -C repo rev-parse HEAD)" || echo "FAIL git"
      ' gate5 "${PUBLIC_REPO}" 2>&1 | tr -d '\r')"
    echo "  ${t}: $(echo "${out}" | paste -sd' | ' -)"
    sudo docker rm -f "${ns}" >/dev/null 2>&1
  ) &
  PIDS+=($!)
done
for p in "${PIDS[@]}"; do wait "${p}"; done
for t in c1 c2 c3; do apply_host "${t}" teardown; done

echo
echo "=== gate5: 6. teardown leaves the shared chains as it found them ==="
drop_ns
apply_host d teardown
AFTER_DU="$(sudo iptables -S DOCKER-USER | wc -l)"; AFTER_IN="$(sudo iptables -S INPUT | wc -l)"
echo "  DOCKER-USER=${AFTER_DU} (was ${BASE_DU}) · INPUT=${AFTER_IN} (was ${BASE_IN})"
if [ "${AFTER_DU}" = "${BASE_DU}" ] && [ "${AFTER_IN}" = "${BASE_IN}" ]; then
  echo "  CLEAN"
else
  echo "  LEAKED"; note_fail
fi

echo
echo "=== gate5: verdict ==="
echo "denial legs failing: ${FAILURES}"
echo "A concurrent line counts as delivery only if it reads OK dns=… tls=200 verified=true AND OK git <sha>."
[ "${FAILURES}" -eq 0 ] && echo "GATE5-DENIAL: PASS" || echo "GATE5-DENIAL: FAIL"
