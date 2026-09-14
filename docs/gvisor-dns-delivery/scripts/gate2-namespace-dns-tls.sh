#!/usr/bin/env bash
# Gate 2: DNS **and** certificate-validated TLS from inside the REAL shared job
# namespace — holder + sidecar-applied policy + job — under runsc, non-root,
# cap-drop ALL, no-new-privileges. Proven on a fresh namespace and again after
# the namespace is destroyed and recreated.
#
# Runs inside the disposable gvisor-repro VM. Bounded: every docker run carries a
# timeout, and every container and the network are removed on exit.
#
# The iptables plan is NOT transcribed here. It is rendered by the product's own
# `NetPolicy` (cargo run -p maxplayer-core --example render_net_plan) and passed
# in via PLAN_FILE, so this gate cannot pass against a firewall the product no
# longer builds.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
NET="${NET:-maxplayer-dns-gate2}"
HOST_TARGET="${HOST_TARGET:-relay.maxplayer.ai}"
RESOLVER="${RESOLVER:-1.1.1.1}"
SUBNET="${SUBNET:-172.31.7.0/24}"
GATEWAY="${GATEWAY:-172.31.7.1}"
PLAN_FILE="${PLAN_FILE:-$HOME/gate2-plan.txt}"
RESOLV_FILE="${RESOLV_FILE:-$HOME/gate2-resolv.conf}"
OUT="${OUT:-$HOME/gate2-evidence.txt}"
HOLDER="gate2-holder"
# The containment plane (holder + sidecar) runs on the HOST runtime; only the job runs
# under gVisor. Measured, not preference: a runsc container joining a runsc holder's
# network namespace sees `lo` ONLY — no eth0, no route, every lookup EAI_AGAIN — because
# a gVisor sandbox's netstack lives inside that sandbox and cannot be entered by a second
# one. Joining a runc holder, a runsc job gets the holder's own interface and address
# (172.31.11.2 in both, measured), so the host kernel's rules govern its traffic. The
# sidecar needs the host runtime for a second reason: iptables-nft inside gVisor fails
# `Failed to initialize nft: Protocol not supported`.
HOLDER_RUNTIME="${HOLDER_RUNTIME:-runc}"
JOB_RUNTIME="${JOB_RUNTIME:-runsc}"

exec > >(tee "${OUT}") 2>&1

fail=0

echo "=== gate2: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m)"
. /etc/os-release && echo "os=${PRETTY_NAME}"
echo "docker=$(sudo docker version --format '{{.Server.Version}}')"
echo "runsc=$(runsc --version | head -1)"
echo "holder_runtime=${HOLDER_RUNTIME} job_runtime=${JOB_RUNTIME}"
echo "image=${IMAGE}"
echo "image_digest=$(sudo docker image inspect "${IMAGE}" --format '{{index .RepoDigests 0}}')"
echo "netfilter_digest=$(sudo docker image inspect "${NETFILTER_IMAGE}" --format '{{index .RepoDigests 0}}')"
echo "resolver=${RESOLVER}"

# The resolver file the product would write, in the product's format.
cat > "${RESOLV_FILE}" <<EOF
# Written by maxplayer for a contained job. Docker's embedded resolver is unreachable
# from a gVisor sandbox, so this file names upstream resolvers directly and the job's
# egress policy opens port 53 to exactly these addresses.
nameserver ${RESOLVER}
options timeout:2 attempts:2
EOF
chmod 0444 "${RESOLV_FILE}"
echo "resolv_conf=${RESOLV_FILE} sha256=$(sha256sum "${RESOLV_FILE}" | cut -d' ' -f1)"
echo "plan=${PLAN_FILE} rules=$(grep -c . "${PLAN_FILE}") sha256=$(sha256sum "${PLAN_FILE}" | cut -d' ' -f1)"

teardown_ns() {
  sudo docker rm -f "${HOLDER}" >/dev/null 2>&1 || true
}
cleanup() {
  teardown_ns
  sudo docker network rm "${NET}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# A FIXED subnet, so the gateway in the rendered plan and the gateway of the network
# the job actually joins are the same address. Letting docker pick would render the
# proxy pinhole for one address while the job reaches the host at another — rules
# that look right in every log and route nothing.
sudo docker network rm "${NET}" >/dev/null 2>&1 || true
sudo docker network create --subnet "${SUBNET}" --gateway "${GATEWAY}" "${NET}" >/dev/null
echo "network=${NET} subnet=$(sudo docker network inspect "${NET}" --format '{{(index .IPAM.Config 0).Subnet}}') gateway=${GATEWAY}"

# The job probe: resolve, then complete a TLS handshake whose certificate chain is
# VERIFIED against the image's own trust store. `rejectUnauthorized` stays default
# (true) and the peer certificate is printed, so a passing gate cannot be a
# handshake that skipped verification.
PROBE='const dns=require("dns"),https=require("https"),fs=require("fs");
const host=process.argv[1];
console.log("resolv.conf:", fs.readFileSync("/etc/resolv.conf","utf8").trim().split("\n").filter(l=>!l.startsWith("#")).join("|"));
dns.lookup(host,(e,a)=>{
  if(e){console.log("lookup: ERR "+e.code);process.exit(1);}
  console.log("lookup: OK "+a);
  const req=https.request({host,port:443,path:"/",method:"HEAD",timeout:15000},(res)=>{
    const c=res.socket.getPeerCertificate();
    console.log("tls: "+res.statusCode+" cert-verified subject="+(c&&c.subject&&c.subject.CN)+" issuer="+(c&&c.issuer&&c.issuer.CN)+" authorized="+res.socket.authorized);
    process.exit(res.socket.authorized?0:1);
  });
  req.on("timeout",()=>{console.log("tls: TIMEOUT");process.exit(1);});
  req.on("error",(err)=>{console.log("tls: ERR "+err.code+" "+err.message);process.exit(1);});
  req.end();
});'

# A second probe proving the containment the DNS pinhole must not have widened:
# the cloud metadata address stays denied while public egress works.
DENY_PROBE='const net=require("net");
const s=net.connect({host:"169.254.169.254",port:80,timeout:6000});
s.on("connect",()=>{console.log("metadata: REACHED (containment broken)");process.exit(1);});
s.on("timeout",()=>{console.log("metadata: denied (timeout)");process.exit(0);});
s.on("error",(e)=>{console.log("metadata: denied ("+e.code+")");process.exit(0);});'

establish_namespace() {
  local label="$1"
  echo
  echo "=== gate2/${label}: establish the shared job namespace ==="
  # Holder: owns the namespace, holds no capability, runs as nobody, read-only.
  sudo timeout 120 docker run --detach --name "${HOLDER}" --runtime "${HOLDER_RUNTIME}" \
    --network "${NET}" --read-only --cap-drop ALL --security-opt no-new-privileges \
    --user 65534:65534 --entrypoint sleep "${IMAGE}" infinity >/dev/null
  echo "holder=${HOLDER} started=$?"

  # Sidecar: the ONLY container handed NET_ADMIN, scoped to the holder's namespace,
  # gone before the job starts. Same runtime as the holder, so it writes into the netns
  # the job will actually join.
  echo "--- sidecar applies the rendered plan ---"
  sudo timeout 120 docker run --rm --interactive --runtime "${HOLDER_RUNTIME}" \
    --network "container:${HOLDER}" --cap-drop ALL --cap-add NET_ADMIN \
    --security-opt no-new-privileges "${NETFILTER_IMAGE}" < "${PLAN_FILE}"
  echo "sidecar_exit=$?"

  # Read the rules back out of the namespace with a DIFFERENT container running a
  # DIFFERENT verb, because the question is what the netstack holds and not whether
  # the installer believes it succeeded.
  echo "--- readback (iptables -S) ---"
  sudo timeout 60 docker run --rm --runtime "${HOLDER_RUNTIME}" --network "container:${HOLDER}" \
    --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
    --entrypoint iptables "${NETFILTER_IMAGE}" -S OUTPUT
  echo "readback_exit=$?"
}

run_job() {
  local label="$1"
  echo
  echo "=== gate2/${label}: job in the shared namespace — dns + verified tls ==="
  sudo timeout 120 docker run --rm --runtime "${JOB_RUNTIME}" --network "container:${HOLDER}" \
    --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
    -v "${RESOLV_FILE}:/etc/resolv.conf:ro" \
    --entrypoint node "${IMAGE}" -e "${PROBE}" "${HOST_TARGET}"
  local rc=$?
  echo "job_exit=${rc}"
  [ "${rc}" -eq 0 ] || fail=1

  echo "--- containment still holds: metadata address ---"
  sudo timeout 60 docker run --rm --runtime "${JOB_RUNTIME}" --network "container:${HOLDER}" \
    --user 65534:65534 --cap-drop ALL --security-opt no-new-privileges \
    -v "${RESOLV_FILE}:/etc/resolv.conf:ro" \
    --entrypoint node "${IMAGE}" -e "${DENY_PROBE}"
  local drc=$?
  echo "metadata_exit=${drc}"
  [ "${drc}" -eq 0 ] || fail=1
}

establish_namespace fresh
run_job fresh

echo
echo "=== gate2: destroy the namespace and rebuild it ==="
teardown_ns
sleep 2
establish_namespace recreated
run_job recreated

echo
echo "=== gate2: verdict ==="
if [ "${fail}" -eq 0 ]; then
  echo "GATE2: PASS (fresh and recreated namespaces both resolved and completed verified TLS)"
else
  echo "GATE2: FAIL"
fi
exit "${fail}"
