#!/usr/bin/env bash
# Gate 4: REAL git delivery originating INSIDE the job sandbox — not a mock, not a
# host-side upload. Two legs, both from the job container under runsc, non-root,
# cap-drop ALL, no-new-privileges, inside the shared job namespace:
#
#   READ  — clone a real public repository over HTTPS, and prove the delivered
#           commit is the remote's real one by comparing against `git ls-remote`
#           taken independently OUTSIDE the sandbox.
#   WRITE — commit in the container and PUSH to a disposable bare remote that
#           lives outside the container, then prove the remote's ref now holds
#           exactly the hash the container produced.
#
# The write remote is reached at the namespace gateway on a port inside the
# policy's proxy pinhole — the one host-facing hole the design already opens —
# so the push crosses the job's egress policy rather than sidestepping it.
#
# ⛔ No credential is used, needed, or logged. A container-side push to a
#    credentialed remote would require putting a secret inside a stranger's
#    sandbox, which is the one thing the whole containment design exists to
#    prevent; the write leg therefore uses an unauthenticated disposable remote.
#    Named as a limitation in the runlog rather than papered over.
set -uo pipefail

IMAGE="${IMAGE:-ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8}"
NETFILTER_IMAGE="${NETFILTER_IMAGE:-ghcr.io/makeprisms/maxplayer-netfilter:v0.5.8}"
NET="${NET:-maxplayer-dns-gate4}"
SUBNET="${SUBNET:-172.31.8.0/24}"
GATEWAY="${GATEWAY:-172.31.8.1}"
RESOLVER="${RESOLVER:-1.1.1.1}"
# Inside the proxy pinhole range the rendered plan opens (49200-49299).
REMOTE_PORT="${REMOTE_PORT:-49250}"
PUBLIC_REPO="${PUBLIC_REPO:-https://github.com/octocat/Hello-World.git}"
PLAN_FILE="${PLAN_FILE:-$HOME/gate4-plan.txt}"
RESOLV_FILE="${RESOLV_FILE:-$HOME/gate4-resolv.conf}"
REMOTE_ROOT="${REMOTE_ROOT:-$HOME/gate4-remote}"
OUT="${OUT:-$HOME/gate4-evidence.txt}"
HOLDER="gate4-holder"
HOLDER_RUNTIME="${HOLDER_RUNTIME:-runc}"
JOB_RUNTIME="${JOB_RUNTIME:-runsc}"

exec > >(tee "${OUT}") 2>&1
fail=0

echo "=== gate4: environment ==="
date -u +"utc=%Y-%m-%dT%H:%M:%SZ"
echo "kernel=$(uname -r) arch=$(uname -m)"
. /etc/os-release && echo "os=${PRETTY_NAME}"
echo "docker=$(sudo docker version --format '{{.Server.Version}}')"
echo "runsc=$(runsc --version | head -1)"
echo "holder_runtime=${HOLDER_RUNTIME} job_runtime=${JOB_RUNTIME}"
echo "image_digest=$(sudo docker image inspect "${IMAGE}" --format '{{index .RepoDigests 0}}')"
echo "container_git=$(sudo docker run --rm --entrypoint git "${IMAGE}" --version)"
echo "public_repo=${PUBLIC_REPO}"

cleanup() {
  sudo docker rm -f "${HOLDER}" >/dev/null 2>&1 || true
  sudo docker network rm "${NET}" >/dev/null 2>&1 || true
  pkill -f "git-daemon.*${REMOTE_PORT}" >/dev/null 2>&1 || true
  pkill -f "git daemon.*${REMOTE_PORT}" >/dev/null 2>&1 || true
  rm -rf "${REMOTE_ROOT}"
}
trap cleanup EXIT
cleanup

cat > "${RESOLV_FILE}" <<EOF
# Written by maxplayer for a contained job. Docker's embedded resolver is unreachable
# from a gVisor sandbox, so this file names upstream resolvers directly and the job's
# egress policy opens port 53 to exactly these addresses.
nameserver ${RESOLVER}
options timeout:2 attempts:2
EOF
chmod 0444 "${RESOLV_FILE}"
echo "plan=${PLAN_FILE} rules=$(grep -c . "${PLAN_FILE}") sha256=$(sha256sum "${PLAN_FILE}" | cut -d' ' -f1)"

sudo docker network create --subnet "${SUBNET}" --gateway "${GATEWAY}" "${NET}" >/dev/null
echo "network=${NET} subnet=${SUBNET} gateway=${GATEWAY}"

# The CONTROL for the read leg, taken outside the sandbox: what the remote really holds.
echo
echo "=== gate4: control — the remote's real HEAD, read from the VM (outside any container) ==="
CONTROL_HEAD="$(git ls-remote "${PUBLIC_REPO}" HEAD | awk '{print $1}')"
echo "control_head=${CONTROL_HEAD}"
[ -n "${CONTROL_HEAD}" ] || { echo "GATE4: FAIL (no control hash)"; exit 1; }

# The disposable write remote: a bare repo outside every container, served on the
# gateway address at a port inside the policy's proxy pinhole.
echo
echo "=== gate4: disposable write remote ==="
mkdir -p "${REMOTE_ROOT}"
git init --bare --quiet "${REMOTE_ROOT}/answer.git"
git --git-dir="${REMOTE_ROOT}/answer.git" config http.receivepack true
setsid git daemon --reuseaddr --listen="${GATEWAY}" --port="${REMOTE_PORT}" \
  --base-path="${REMOTE_ROOT}" --export-all --enable=receive-pack \
  >/dev/null 2>&1 </dev/null &
sleep 2
echo "write_remote=git://${GATEWAY}:${REMOTE_PORT}/answer.git (disposable, unauthenticated, torn down on exit)"
echo "remote_refs_before=$(git --git-dir="${REMOTE_ROOT}/answer.git" show-ref | wc -l)"

echo
echo "=== gate4: establish the shared job namespace ==="
sudo timeout 120 docker run --detach --name "${HOLDER}" --runtime "${HOLDER_RUNTIME}" \
  --network "${NET}" --read-only --cap-drop ALL --security-opt no-new-privileges \
  --user 65534:65534 --entrypoint sleep "${IMAGE}" infinity >/dev/null
echo "holder_started=$?"
sudo timeout 120 docker run --rm --interactive --runtime "${HOLDER_RUNTIME}" \
  --network "container:${HOLDER}" --cap-drop ALL --cap-add NET_ADMIN \
  --security-opt no-new-privileges "${NETFILTER_IMAGE}" < "${PLAN_FILE}"
echo "sidecar_exit=$?"

# One payload, both legs, run as the job: clone over https, commit, push to the
# remote outside the container, and print the hashes for comparison.
PAYLOAD='
set -e
export HOME=/tmp GIT_TERMINAL_PROMPT=0
cd /tmp
echo "--- read leg: clone over https from inside the sandbox ---"
# A FULL clone, not --depth 1: a shallow history cannot be pushed on ("shallow update
# not allowed"), and the write leg is half the gate. The repository is deliberately tiny.
git clone --quiet "$1" work
cd work
echo "delivered_head=$(git rev-parse HEAD)"
echo "--- write leg: commit here, push to the remote outside this container ---"
git config user.email job@sandbox.invalid
git config user.name "sandbox job"
date -u +%s > answer.txt
git add answer.txt
git commit --quiet -m "answer from the sandboxed job"
echo "answer_commit=$(git rev-parse HEAD)"
git push --quiet "$2" HEAD:refs/heads/answer
echo "push_exit=$?"
'

echo
echo "=== gate4: job container (runsc, non-root, cap-drop ALL) ==="
JOB_OUT="$(sudo timeout 180 docker run --rm --runtime "${JOB_RUNTIME}" \
  --network "container:${HOLDER}" --user 65534:65534 --cap-drop ALL \
  --security-opt no-new-privileges -v "${RESOLV_FILE}:/etc/resolv.conf:ro" \
  --entrypoint sh "${IMAGE}" -c "${PAYLOAD}" gate4 \
  "${PUBLIC_REPO}" "git://${GATEWAY}:${REMOTE_PORT}/answer.git" 2>&1)"
echo "${JOB_OUT}"
echo "job_exit=$?"

DELIVERED_HEAD="$(printf '%s\n' "${JOB_OUT}" | sed -n 's/^delivered_head=//p')"
ANSWER_COMMIT="$(printf '%s\n' "${JOB_OUT}" | sed -n 's/^answer_commit=//p')"

echo
echo "=== gate4: verdict ==="
echo "control_head=${CONTROL_HEAD}"
echo "delivered_head=${DELIVERED_HEAD}"
if [ -n "${DELIVERED_HEAD}" ] && [ "${DELIVERED_HEAD}" = "${CONTROL_HEAD}" ]; then
  echo "READ: PASS — the sandbox delivered the remote's real HEAD"
else
  echo "READ: FAIL — delivered hash does not match the remote's real HEAD"
  fail=1
fi

# --verify, so a MISSING ref is empty rather than the string "refs/heads/answer" — which
# would otherwise be compared against a hash and merely look like a mismatch.
REMOTE_HASH="$(git --git-dir="${REMOTE_ROOT}/answer.git" rev-parse --verify -q refs/heads/answer 2>/dev/null)"
echo "answer_commit=${ANSWER_COMMIT}"
echo "remote_hash=${REMOTE_HASH}"
if [ -n "${ANSWER_COMMIT}" ] && [ "${ANSWER_COMMIT}" = "${REMOTE_HASH}" ]; then
  echo "WRITE: PASS — the commit made inside the sandbox reached the remote, hash matches"
else
  echo "WRITE: FAIL — the remote does not hold the container's commit"
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "GATE4: PASS"
else
  echo "GATE4: FAIL"
fi
exit "${fail}"
