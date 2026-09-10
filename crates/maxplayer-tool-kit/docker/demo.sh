#!/usr/bin/env bash
# Linux container demonstration of the seller-level tool holder.
#
# What this shows, and the shape it shows it in:
#
#   * The seller daemon enrols the tool ONCE. Two sequential jobs run against that one login,
#     and the vendor's own counter is the witness.
#   * A job container is given exactly two things: its own directory and its own socket. It is
#     started with `--network none`, so it could not reach the vendor even holding a credential.
#   * The credential never enters a job container. The demo greps for it from inside one.
#   * Job A ending does not log the tool out; job B finds the session already live.
#   * Availability follows the daemon: stop it and the tool is gone; start it and the same
#     session comes back without a new login.
#
# The vendor is reachable from the HOST on a published loopback port, so every counter used as
# evidence is read from outside the system under test rather than from the holder's self-report.
#
# Synthetic throughout: the account exists only inside `vendor-service`. No live account, no
# spend, no egress beyond this machine.

set -euo pipefail

IMAGE="${IMAGE:-maxplayer-tool-kit:demo}"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
EV="$REPO_ROOT/evidence/$RUN_ID"
# Host scratch lives outside the repository: nothing credential-shaped can be committed by
# accident, and it is removed on exit.
HOSTDIR="$HOME/.mtk-demo/$RUN_ID"
# Host-side scratch for the credential-absence scan (advisor F1). It holds captured job-container
# archives and the extracted trees the scanner reads. It lives under HOSTDIR so the EXIT trap
# removes it, and it never enters a container.
RUN_TMP="$HOSTDIR/scan"

mkdir -p "$EV"
mkdir -p "$HOSTDIR"
chmod 700 "$HOSTDIR"
mkdir -p "$RUN_TMP"

SUFFIX="$$"
NET="mtk-net-$SUFFIX"
VENDOR="mtk-vendor-$SUFFIX"
HOLDER="mtk-holder-$SUFFIX"
VOL_STATE="mtk-state-$SUFFIX"
VOL_RUN="mtk-run-$SUFFIX"
VOL_SOCK_A="mtk-sock-a-$SUFFIX"
VOL_SOCK_B="mtk-sock-b-$SUFFIX"
VOL_WORK_A="mtk-work-a-$SUFFIX"
VOL_WORK_B="mtk-work-b-$SUFFIX"

PASSES=0
FAILURES=0
RESULTS="$EV/results.txt"
: > "$RESULTS"

note() { printf '%s\n' "$*" | tee -a "$RESULTS"; }

check() { # check <verdict-name> <expected> <actual>
  local name="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    PASSES=$((PASSES + 1))
    printf 'PASS  %-52s %s\n' "$name" "$actual" | tee -a "$RESULTS"
  else
    FAILURES=$((FAILURES + 1))
    printf 'FAIL  %-52s expected=%s actual=%s\n' "$name" "$expected" "$actual" | tee -a "$RESULTS"
  fi
}

check_contains() { # check_contains <verdict-name> <needle> <haystack>
  local name="$1" needle="$2" hay="$3"
  if [[ "$hay" == *"$needle"* ]]; then
    PASSES=$((PASSES + 1))
    printf 'PASS  %-52s contains %s\n' "$name" "$needle" | tee -a "$RESULTS"
  else
    FAILURES=$((FAILURES + 1))
    printf 'FAIL  %-52s missing %s in: %s\n' "$name" "$needle" "${hay:0:200}" | tee -a "$RESULTS"
  fi
}

cleanup() {
  set +e
  docker logs "$HOLDER" > "$EV/holder.log" 2>&1
  docker logs "$VENDOR" > "$EV/vendor.log" 2>&1
  docker rm -f "$VENDOR" "$HOLDER" >/dev/null 2>&1
  docker volume rm "$VOL_STATE" "$VOL_RUN" "$VOL_SOCK_A" "$VOL_SOCK_B" "$VOL_WORK_A" "$VOL_WORK_B" >/dev/null 2>&1
  docker network rm "$NET" >/dev/null 2>&1
  rm -rf "$HOSTDIR"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Synthetic credential, generated now, never on a command line.
# ---------------------------------------------------------------------------
SECRET="synthetic-demo-secret-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
umask 077
printf '{"client_id":"synthetic-seller-client","client_secret":"%s"}\n' "$SECRET" > "$HOSTDIR/cred.json"
chmod 600 "$HOSTDIR/cred.json"

note "run-id: $RUN_ID"
note "image:  $IMAGE"

docker network create "$NET" >/dev/null
for v in "$VOL_STATE" "$VOL_RUN" "$VOL_SOCK_A" "$VOL_SOCK_B" "$VOL_WORK_A" "$VOL_WORK_B"; do
  docker volume create "$v" >/dev/null
done

# ---------------------------------------------------------------------------
# Vendor. Published on loopback so the HOST can read its counters independently.
# ---------------------------------------------------------------------------
docker run -d --name "$VENDOR" --network "$NET" --network-alias vendor \
  -p 127.0.0.1:0:8080 \
  -v "$HOSTDIR/cred.json:/run/secrets/cred.json:ro" \
  "$IMAGE" vendor-service --listen 0.0.0.0:8080 --credential-file /run/secrets/cred.json >/dev/null

VENDOR_HOSTPORT="$(docker port "$VENDOR" 8080/tcp | head -1)"
VENDOR_URL="http://${VENDOR_HOSTPORT}"
note "vendor observable from host at $VENDOR_URL"

stats() { curl -fsS "$VENDOR_URL/admin/stats"; }
field() { # field <json> <name>
  printf '%s' "$1" | grep -o "\"$2\":[0-9]*" | head -1 | cut -d: -f2
}

for _ in $(seq 1 50); do
  stats >/dev/null 2>&1 && break
  sleep 0.2
done
stats > "$EV/stats-00-before-holder.json"
check "vendor_login_count_before_holder" "0" "$(field "$(stats)" login_count)"

# ---------------------------------------------------------------------------
# Holder. Per-job socket volumes are mounted at their own paths so each job container can be
# handed exactly one endpoint.
# ---------------------------------------------------------------------------
docker run -d --name "$HOLDER" --network "$NET" \
  -v "$HOSTDIR/cred.json:/run/secrets/cred.json:ro" \
  -v "$VOL_STATE:/var/lib/holder" \
  -v "$VOL_RUN:/run/holder" \
  -v "$VOL_SOCK_A:/run/holder/jobs/job-a" \
  -v "$VOL_SOCK_B:/run/holder/jobs/job-b" \
  -v "$VOL_WORK_A:/srv/jobs/job-a" \
  -v "$VOL_WORK_B:/srv/jobs/job-b" \
  "$IMAGE" tool-holderd \
    --config /etc/maxplayer/seller-tool-config.json \
    --state /var/lib/holder \
    --runtime /run/holder \
    --credential-file /run/secrets/cred.json \
    --vendor-cli /usr/local/bin/vendor-cli \
    --vendor-base-url http://vendor:8080 >/dev/null

hctl() { docker exec "$HOLDER" holderctl "$@" --socket /run/holder/holder.sock; }

for _ in $(seq 1 60); do
  hctl status >/dev/null 2>&1 && break
  sleep 0.25
done
hctl status > "$EV/holder-status-01-after-start.json"
check_contains "holder_healthy_at_start" '"healthy": true' "$(cat "$EV/holder-status-01-after-start.json")"
check "vendor_login_count_after_enrolment" "1" "$(field "$(stats)" login_count)"

# ---------------------------------------------------------------------------
# Job A.
# ---------------------------------------------------------------------------
docker exec "$HOLDER" sh -c 'printf "first job payload" > /srv/jobs/job-a/input.txt'
hctl attach --job-id job-a --job-root /srv/jobs/job-a > "$EV/attach-job-a.json"

mcp_drive() { # mcp_drive <sock-volume> <work-volume> <outfile>
  local sock="$1" work="$2" out="$3"; shift 3
  printf '%s\n' "$@" | docker run -i --rm --network none \
    -v "$sock:/run/holder" -v "$work:/work" \
    -e HOLDER_JOB_SOCKET=/run/holder/job.sock \
    "$IMAGE" tool-mcp-bridge > "$out"
}

mcp_drive "$VOL_SOCK_A" "$VOL_WORK_A" "$EV/job-a-mcp.jsonl" \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"transform-file","arguments":{"input":"input.txt","output":"out.txt","mode":"upper"}}}'

check_contains "job_a_mcp_initialize" '"protocolVersion":"2024-11-05"' "$(cat "$EV/job-a-mcp.jsonl")"
check_contains "job_a_mcp_tools_list" '"transform-file"' "$(cat "$EV/job-a-mcp.jsonl")"
check_contains "job_a_mcp_call_ok" '"isError":false' "$(cat "$EV/job-a-mcp.jsonl")"
check "job_a_output" "FIRST JOB PAYLOAD" "$(docker run --rm -v "$VOL_WORK_A:/work" "$IMAGE" cat /work/out.txt)"

# The job container's own view: no credential, no holder state, no vendor reachability.
docker run --rm --network none -v "$VOL_SOCK_A:/run/holder" -v "$VOL_WORK_A:/work" "$IMAGE" \
  sh -c '
    echo "--- what this container can see ---"
    ls -la /run/holder /work
    echo "--- credential paths ---"
    ls /run/secrets 2>&1 || true
    ls /var/lib/holder 2>&1 || true
    echo "--- search for a session token ---"
    grep -rl "sess-" /work /run /etc /tmp 2>/dev/null || echo "NO_TOKEN_FOUND"
    echo "--- the secret search runs on the host, see job-a-fs-scan.txt ---"
  ' > "$EV/job-a-container-view.txt" 2>&1 || true

# ---------------------------------------------------------------------------
# Credential absence: observed from the HOST, never by handing the container the secret.
#
# What this replaces (advisor F1). The previous version passed the live secret into the probe
# container as -e NEEDLE and grepped from inside. That was worthless three times over: it put
# the credential inside the very container whose cleanliness was the claim, so a positive would
# have been self-inflicted; `grep -rl ... || echo NOT_FOUND` printed NOT_FOUND for *any*
# non-match exit including a scan that never ran; and a trailing `|| true` swallowed docker
# failures, so an unstarted container also read as "absent". The check could not fail.
#
# The shape now: the container's filesystem is exported to the host with no secret anywhere in
# its environment, and the scan happens here, where the secret legitimately lives. Errors are
# fatal instead of absence. The scanner is a single function used by both the real check and a
# negative control, so the control actually exercises the code that makes the claim.
# ---------------------------------------------------------------------------

# Export the job container's searchable surface. No -e, no secret: this container is handed
# nothing but its own two mounts. A capture failure aborts rather than reporting a clean scan.
capture_job_fs() {
  _cap_sock="$1"; _cap_work="$2"; _cap_out="$3"
  # Exclude Unix sockets: the job's own socket lives under run/holder, and `tar` returns a
  # non-zero "socket ignored" status for it. A socket carries no file content, so it is not a
  # place a credential could be read from; skipping it removes a false capture failure while a
  # genuine failure (an unreadable directory, an empty archive) still aborts below.
  if ! docker run --rm --network none -v "$_cap_sock:/run/holder" -v "$_cap_work:/work" "$IMAGE" \
      tar --exclude='*.sock' -cf - -C / work run/holder etc/maxplayer usr/local/bin > "$_cap_out" 2>"$_cap_out.err"; then
    echo "FATAL: filesystem capture failed; see $(basename "$_cap_out").err" >&2
    return 1
  fi
  [ -s "$_cap_out" ] || { echo "FATAL: capture produced an empty archive" >&2; return 1; }
}

# Count occurrences of the secret in a captured archive. Prints an integer, or "SCAN_ERROR".
# grep -F takes the needle on stdin-adjacent state only: it is passed as an argument here on
# the host, where the value is already present in this shell, and never crosses into a container.
scan_capture_for_secret() {
  _scan_tar="$1"; _scan_dir="$2"
  rm -rf "$_scan_dir"; mkdir -p "$_scan_dir"
  if ! tar -xf "$_scan_tar" -C "$_scan_dir" 2>/dev/null; then
    echo "SCAN_ERROR"; return 0
  fi
  # -a treats every file as text so binaries are searched too, not skipped. grep's exit status is
  # load-bearing here: 0 means it found the secret, 1 means a clean no-match, and >=2 means a real
  # scanner error. A no-match is the EXPECTED absence result, so it must not be read as a failure
  # under `set -o pipefail`; a real error must not be read as "0 hits", which would manufacture a
  # false absence (the F1 anti-pattern). So the three cases are separated explicitly.
  set +e
  _hits="$(LC_ALL=C grep -r -a -F -l -- "$SECRET" "$_scan_dir" 2>/dev/null)"
  _rc=$?
  set -e
  if [ "$_rc" -gt 1 ]; then
    echo "SCAN_ERROR"; return 0
  fi
  if [ -z "$_hits" ]; then
    echo 0
  else
    printf '%s\n' "$_hits" | wc -l | tr -d ' '
  fi
}

capture_job_fs "$VOL_SOCK_A" "$VOL_WORK_A" "$EV/job-a-fs.tar" || exit 1
JOB_A_HITS=$(scan_capture_for_secret "$EV/job-a-fs.tar" "$RUN_TMP/scan-real")
{
  echo "scanned: job A container filesystem (/work, /run/holder, /etc/maxplayer, /usr/local/bin)"
  echo "archive_bytes: $(wc -c < "$EV/job-a-fs.tar" | tr -d ' ')"
  echo "files_in_archive: $(tar -tf "$EV/job-a-fs.tar" 2>/dev/null | wc -l | tr -d ' ')"
  echo "secret_in_container_env: no (container received no secret; scan is host-side)"
  echo "files_containing_secret: $JOB_A_HITS"
} > "$EV/job-a-fs-scan.txt"

check "credential_absent_from_job_container" "0" "$JOB_A_HITS"

# Negative control: the same scanner, the same archive, plus one planted file holding the real
# secret. If this reports 0 the scanner is blind and the check above means nothing, so a passing
# absence result is only trustworthy while this line also passes.
cp "$EV/job-a-fs.tar" "$RUN_TMP/planted.tar"
mkdir -p "$RUN_TMP/plant/work"
printf 'leaked=%s\n' "$SECRET" > "$RUN_TMP/plant/work/leaked.txt"
tar -rf "$RUN_TMP/planted.tar" -C "$RUN_TMP/plant" work/leaked.txt 2>/dev/null
PLANTED_HITS=$(scan_capture_for_secret "$RUN_TMP/planted.tar" "$RUN_TMP/scan-planted")
{
  echo "control: identical scanner over the same archive with one file containing the secret"
  echo "files_containing_secret: $PLANTED_HITS"
  echo "interpretation: 1 proves the scanner detects the credential when it IS present"
} > "$EV/job-a-fs-scan-negative-control.txt"

check "secret_scanner_detects_planted_credential" "1" "$PLANTED_HITS"
check_contains "holder_state_absent_from_job_container" "No such file or directory" "$(cat "$EV/job-a-container-view.txt")"

# Cross-job attempt: job A reaching for job B's directory by absolute path.
mcp_drive "$VOL_SOCK_A" "$VOL_WORK_A" "$EV/job-a-crossjob.jsonl" \
  '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"transform-file","arguments":{"input":"/srv/jobs/job-b/input.txt","output":"out.txt","mode":"upper"}}}'
check_contains "cross_job_absolute_path_refused" '"code":1003' "$(cat "$EV/job-a-crossjob.jsonl")"

# ---------------------------------------------------------------------------
# Job A ends. The tool must not be logged out.
# ---------------------------------------------------------------------------
hctl detach --job-id job-a > "$EV/detach-job-a.json"
check_contains "detach_reports_tool_still_enrolled" '"tool_still_enrolled": true' "$(cat "$EV/detach-job-a.json")"

# ---------------------------------------------------------------------------
# Job B, same seller, same offering, same login.
# ---------------------------------------------------------------------------
docker exec "$HOLDER" sh -c 'printf "second job payload" > /srv/jobs/job-b/input.txt'
hctl attach --job-id job-b --job-root /srv/jobs/job-b > "$EV/attach-job-b.json"

mcp_drive "$VOL_SOCK_B" "$VOL_WORK_B" "$EV/job-b-mcp.jsonl" \
  '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"transform-file","arguments":{"input":"input.txt","output":"out.txt","mode":"reverse"}}}'

check_contains "job_b_mcp_call_ok" '"isError":false' "$(cat "$EV/job-b-mcp.jsonl")"
check "job_b_output" "daolyap boj dnoces" "$(docker run --rm -v "$VOL_WORK_B:/work" "$IMAGE" cat /work/out.txt)"

# Same offering seen by both jobs — the WHOLE tool list, compared as parsed JSON.
#
# What this replaces (advisor F3). The previous version compared `grep -o '"tools":\[[^]]*\]'`,
# a regex that stops at the first ']' — which falls inside the schema's `enum`, so it compared a
# prefix of the list and never saw the operation schema at all. Here jq parses the full response,
# sorts object keys (-S) for a canonical form, and the entire tool array is compared: any
# difference anywhere in the schema is caught, not just up to the first bracket. The seller's own
# control view is compared too, so "seller-level offering" is checked against all three surfaces.
tools_of_jsonl() { jq -cS 'select(.result and .result.tools) | .result.tools' "$1" | head -1; }
A_TOOLS="$(tools_of_jsonl "$EV/job-a-mcp.jsonl")"
B_TOOLS="$(tools_of_jsonl "$EV/job-b-mcp.jsonl")"
hctl tools > "$EV/holder-tools-control.json"
CTL_TOOLS="$(jq -cS '.tools' "$EV/holder-tools-control.json")"

A_LEN="$(printf '%s' "$A_TOOLS" | jq 'length' 2>/dev/null || echo 0)"
check "operation_list_nonempty"                   "true"           "$([ "${A_LEN:-0}" -ge 1 ] && echo true || echo false)"
check "operation_list_identical_for_both_jobs"    "$A_TOOLS"       "$B_TOOLS"
check "operation_list_matches_seller_control_view" "$A_TOOLS"      "$CTL_TOOLS"
check "operation_present_transform_file"          "transform-file" "$(printf '%s' "$A_TOOLS" | jq -r '.[] | select(.name=="transform-file") | .name')"
check "operation_schema_is_closed"                "false"          "$(printf '%s' "$A_TOOLS" | jq -r '.[0].inputSchema.additionalProperties')"

stats > "$EV/stats-02-after-two-jobs.json"
S="$(stats)"
check "vendor_transform_count_after_two_jobs" "2" "$(field "$S" transform_count)"
check "vendor_login_count_after_two_jobs" "1" "$(field "$S" login_count)"
check "vendor_auth_failures_after_two_jobs" "0" "$(field "$S" auth_failures)"

# ---------------------------------------------------------------------------
# Restart: the persisted session is reused, not re-established — and the tool still serves a
# LIVE job afterwards.
#
# A restart drops the holder's in-memory attachments (it comes back with a control socket and an
# empty jobs map). That is exactly why job B must be RE-ATTACHED here before it can be called
# again. Doing so, and proving a live call on the fresh endpoint, is what lets the later stop
# below be a proof about the tool rather than about an endpoint that was already gone (advisor F3).
# ---------------------------------------------------------------------------
docker restart "$HOLDER" >/dev/null
for _ in $(seq 1 60); do
  hctl status >/dev/null 2>&1 && break
  sleep 0.25
done
hctl status > "$EV/holder-status-03-after-restart.json"
check_contains "restart_resumed_existing_session" '"resumed_existing_session": true' "$(cat "$EV/holder-status-03-after-restart.json")"
check_contains "restart_enrollments_this_process_zero" '"enrollments_this_process": 0' "$(cat "$EV/holder-status-03-after-restart.json")"
check "vendor_login_count_after_restart" "1" "$(field "$(stats)" login_count)"

# Re-establish job B's addressing and prove a live call on the persisted session (no new login).
hctl attach --job-id job-b --job-root /srv/jobs/job-b > "$EV/attach-job-b-after-restart.json"
docker exec "$HOLDER" sh -c 'printf "post restart payload" > /srv/jobs/job-b/input.txt'
mcp_drive "$VOL_SOCK_B" "$VOL_WORK_B" "$EV/job-b-after-restart.jsonl" \
  '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"transform-file","arguments":{"input":"input.txt","output":"out.txt","mode":"upper"}}}'
check_contains "restart_live_call_ok" '"isError":false' "$(cat "$EV/job-b-after-restart.jsonl")"
check "job_b_output_after_restart" "POST RESTART PAYLOAD" "$(docker run --rm -v "$VOL_WORK_B:/work" "$IMAGE" cat /work/out.txt)"
check "vendor_login_count_after_restart_call" "1" "$(field "$(stats)" login_count)"

# ---------------------------------------------------------------------------
# Vendor-side revocation must become visible seller state, then recover.
# ---------------------------------------------------------------------------
curl -fsS -X POST "$VENDOR_URL/admin/revoke" > "$EV/revoke.json"
set +e
hctl health > "$EV/holder-health-04-revoked.json" 2>&1
HEALTH_EXIT=$?
set -e
check "unhealthy_exit_code_nonzero" "1" "$HEALTH_EXIT"
check_contains "unhealthy_state_visible" "unhealthy" "$(cat "$EV/holder-health-04-revoked.json")"
check_contains "unhealthy_reason_visible" "rejected the stored session" "$(cat "$EV/holder-health-04-revoked.json")"

hctl reenroll > "$EV/holder-reenroll-05.json"
check_contains "reenrolment_restores_health" '"healthy"' "$(cat "$EV/holder-reenroll-05.json")"
check "vendor_login_count_after_reenrolment" "2" "$(field "$(stats)" login_count)"

# ---------------------------------------------------------------------------
# Availability follows the daemon: call -> loss -> restore, all on a LIVE attachment.
#
# Job B is attached and was just used, so the endpoint is genuinely live here. We prove one
# successful call on it immediately before the stop, prove that the SAME endpoint fails after the
# stop, then start the daemon, re-attach, and prove success again without a new login. A
# still-serving endpoint would make the "unavailable" check fail — which is the point.
# ---------------------------------------------------------------------------
docker exec "$HOLDER" sh -c 'printf "pre stop payload" > /srv/jobs/job-b/input.txt'
mcp_drive "$VOL_SOCK_B" "$VOL_WORK_B" "$EV/job-b-pre-stop.jsonl" \
  '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"transform-file","arguments":{"input":"input.txt","output":"out.txt","mode":"upper"}}}'
check_contains "live_endpoint_ok_before_stop" '"isError":false' "$(cat "$EV/job-b-pre-stop.jsonl")"

docker stop "$HOLDER" >/dev/null
set +e
mcp_drive "$VOL_SOCK_B" "$VOL_WORK_B" "$EV/job-b-after-stop.jsonl" \
  '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"transform-file","arguments":{"input":"input.txt","output":"out2.txt","mode":"upper"}}}'
set -e
check_contains "tool_unavailable_once_daemon_stops" "holder endpoint unavailable" "$(cat "$EV/job-b-after-stop.jsonl")"

docker start "$HOLDER" >/dev/null
for _ in $(seq 1 60); do
  hctl status >/dev/null 2>&1 && break
  sleep 0.25
done
check "vendor_login_count_after_daemon_restart" "2" "$(field "$(stats)" login_count)"

# Restore: re-attach and prove the same job works again on the resumed session, no new login.
hctl attach --job-id job-b --job-root /srv/jobs/job-b > "$EV/attach-job-b-after-daemon-start.json"
docker exec "$HOLDER" sh -c 'printf "restored payload" > /srv/jobs/job-b/input.txt'
mcp_drive "$VOL_SOCK_B" "$VOL_WORK_B" "$EV/job-b-restored.jsonl" \
  '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"transform-file","arguments":{"input":"input.txt","output":"out.txt","mode":"upper"}}}'
check_contains "call_restored_after_daemon_restart" '"isError":false' "$(cat "$EV/job-b-restored.jsonl")"
check "job_b_output_restored" "RESTORED PAYLOAD" "$(docker run --rm -v "$VOL_WORK_B:/work" "$IMAGE" cat /work/out.txt)"
check "vendor_login_count_after_restore" "2" "$(field "$(stats)" login_count)"
stats > "$EV/stats-06-final.json"

# ---------------------------------------------------------------------------
# Source-to-build receipt (advisor F4).
#
# The old manifest recorded the image TAG, which is mutable and does not identify what was built.
# This binds the run to: the source commit and the crate's own git tree object (source identity
# independent of the rest of the repo), the lockfile and Dockerfile hashes, the two base-image
# digests the Dockerfile pins, and the ACTUAL built image id. A reader can therefore check that
# the image tested was built from this exact source, not merely from a tag that happened to point
# somewhere on the day.
# ---------------------------------------------------------------------------
sha256() { shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'; }

BUILT_IMAGE_ID="$(docker inspect --format '{{.Id}}' "$IMAGE" 2>/dev/null || echo unknown)"
BUILT_REPO_DIGESTS="$(docker inspect --format '{{json .RepoDigests}}' "$IMAGE" 2>/dev/null || echo '[]')"
SRC_COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
CRATE_TREE="$(git -C "$REPO_ROOT" rev-parse 'HEAD:crates/maxplayer-tool-kit' 2>/dev/null || echo unknown)"
if [ -n "$(git -C "$REPO_ROOT" status --porcelain -- crates/maxplayer-tool-kit 2>/dev/null)" ]; then
  SRC_DIRTY=true
else
  SRC_DIRTY=false
fi
LOCK_SHA="$(sha256 "$SCRIPT_DIR/../Cargo.lock")"
DOCKERFILE_SHA="$(sha256 "$SCRIPT_DIR/Dockerfile")"
BUILD_BASE="$(grep -oE 'rust@sha256:[0-9a-f]+' "$SCRIPT_DIR/Dockerfile" | head -1)"
RUNTIME_BASE="$(grep -oE 'debian@sha256:[0-9a-f]+' "$SCRIPT_DIR/Dockerfile" | head -1)"

BUILD_RECEIPT="$(jq -n \
  --arg src_commit "$SRC_COMMIT" \
  --arg crate_tree "$CRATE_TREE" \
  --argjson src_dirty "$SRC_DIRTY" \
  --arg cargo_lock_sha256 "$LOCK_SHA" \
  --arg dockerfile_sha256 "$DOCKERFILE_SHA" \
  --arg build_base "$BUILD_BASE" \
  --arg runtime_base "$RUNTIME_BASE" \
  --arg built_image_id "$BUILT_IMAGE_ID" \
  --argjson built_repo_digests "$BUILT_REPO_DIGESTS" \
  '{source_commit:$src_commit, crate_tree_object:$crate_tree, source_tree_dirty:$src_dirty,
    cargo_lock_sha256:$cargo_lock_sha256, dockerfile_sha256:$dockerfile_sha256,
    build_base_image:$build_base, runtime_base_image:$runtime_base,
    built_image_id:$built_image_id, built_image_repo_digests:$built_repo_digests}')"

# Hash every raw capture so the manifest fixes the exact bytes of the transcripts it summarises.
CAPTURE_HASHES="{}"
for f in "$EV"/*.jsonl "$EV"/results.txt; do
  [ -e "$f" ] || continue
  CAPTURE_HASHES="$(printf '%s' "$CAPTURE_HASHES" | jq --arg k "$(basename "$f")" --arg v "$(sha256 "$f")" '. + {($k): $v}')"
done

# ---------------------------------------------------------------------------
# Evidence manifest.
# ---------------------------------------------------------------------------
FINAL="$(stats)"
cat > "$EV/manifest.json" <<JSON
{
  "run_id": "$RUN_ID",
  "image": "$IMAGE",
  "platform": "$(docker version --format '{{.Server.Os}}/{{.Server.Arch}}' 2>/dev/null)",
  "docker_server_version": "$(docker version --format '{{.Server.Version}}' 2>/dev/null)",
  "generated_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",

  "build": $BUILD_RECEIPT,
  "raw_capture_sha256": $CAPTURE_HASHES,

  "mechanism_only": true,
  "what_this_proves": "That the holder mechanism behaves as specified against a fake vendor and a CLI written for it.",
  "what_this_does_not_prove": [
    "Independent real-tool acceptance: the CLI and the vendor were both written for this contract and cannot falsify it.",
    "General onboarding acceptance for any unseen third-party tool.",
    "Production integration with maxplayer's seller execution path, which today attaches no MCP servers at all."
  ],
  "synthetic": {
    "credential": "generated per run, host-only, never on a command line, removed on exit",
    "live_account": false,
    "spend": false,
    "external_egress": false
  },

  "observers": {
    "vendor_counters": "read from the host over a published loopback port, outside the system under test",
    "job_container_network": "none",
    "holder_self_report": "recorded but never used as the sole basis for a call-count claim"
  },

  "vendor_final_counters": $FINAL,

  "checks": { "passed": $PASSES, "failed": $FAILURES },
  "verdict": "$( ((FAILURES == 0)) && echo PASS || echo FAIL )"
}
JSON

note ""
note "checks passed: $PASSES   failed: $FAILURES"
note "evidence: $EV"

if ((FAILURES > 0)); then
  exit 1
fi
