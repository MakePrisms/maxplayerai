#!/usr/bin/env bash
# runsc-session-persistence.sh — does a parked session container keep its rootfs files across
# `docker stop` + `docker start` under this box's runsc runtime?
#
# WHY: a session job's container is parked between turns and resumed with `docker start -a -i`
# (seller_exec.rs `docker_start_argv`). gVisor's default `--overlay2=root:self` keeps rootfs writes in
# a sandbox-private overlay that is discarded when the container stops, so turn 2 lost the agent's
# transcript under ~/.claude ("bug 2"; reproduced on runsc 20261102.0, arm64). A runsc seat now
# advertises sessions only when its runtime has `--overlay2=none` (docs/specs/session-jobs.md §5.6).
#
# WHAT IT PROVES, for the runtime named by RUNTIME (default: runsc) on THIS docker daemon:
#   1. FLAG:   whether the runtime's registered runtimeArgs leave --overlay2 at `none` (the same
#              rule the seller's gate applies; last setting wins).
#   2. ROOTFS: whether a file the container writes to its own rootfs (/home/agent/.claude, the
#              path claude-agent-acp keeps transcripts under) survives docker stop + docker start.
#   3. /tmp (info only): runsc mounts its own tmpfs at /tmp, so files there are lost on stop/start
#              whatever --overlay2 says. The agent's transcript is not under /tmp.
# It uses the same create-once / start-again lifecycle and the same --user, --cap-drop ALL,
# no-new-privileges and --init flags a session container gets.
#
# WHAT IT DOES NOT PROVE: it runs no maxplayer binary, no ACP agent and no session/load; it uses
# busybox unless IMAGE names the sandbox image; it does not cover a docker daemon or host restart
# between turns (session GC expires those sessions, §5.5); and it says nothing about runtimes other
# than the one tested. RUNTIME=runc is a useful control (rootfs should survive, flag "n/a").
#
# Usage:   scripts/runsc-session-persistence.sh            # RUNTIME=runsc IMAGE=busybox:1.36
#          RUNTIME=runc scripts/runsc-session-persistence.sh
#          IMAGE=ghcr.io/makeprisms/maxplayer-sandbox:v0.7.0 scripts/runsc-session-persistence.sh
# Exit:    0 rootfs survived; 1 rootfs file lost; 4 setup error.
#          The flag verdict is printed; it does not change the exit code.
set -euo pipefail

RUNTIME="${RUNTIME:-runsc}"
IMAGE="${IMAGE:-busybox:1.36}"
NAME="maxplayer-persistence-check-$$"
UID_GID="$(id -u):$(id -g)"

say() { printf '%s\n' "$*"; }
die() { printf 'SETUP ERROR: %s\n' "$*" >&2; exit 4; }

# Invoked only via the EXIT trap below, which shellcheck cannot see (SC2317 false positive).
# shellcheck disable=SC2317
cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  if [ -n "${state_dir:-}" ]; then rm -rf "$state_dir"; fi
}
trap cleanup EXIT

command -v docker >/dev/null 2>&1 || die "docker is not on PATH"
command -v python3 >/dev/null 2>&1 || die "python3 is needed to read docker's runtime args"
docker info >/dev/null 2>&1 || die "the docker daemon did not answer \`docker info\`"

runtimes="$(docker info --format '{{json .Runtimes}}')"
# Prints: "<flag verdict>\t<runtimeArgs json>", or UNREGISTERED.
read_flag='
import json, sys
runtime = json.load(sys.stdin).get(sys.argv[1])
if not isinstance(runtime, dict):
    print("UNREGISTERED"); sys.exit()
args = [a for a in (runtime.get("runtimeArgs") or []) if isinstance(a, str)]
value, it = None, iter(args)
for arg in it:
    flag = arg[2:] if arg.startswith("--") else arg[1:] if arg.startswith("-") else None
    if flag is None: continue
    if flag.startswith("overlay2="): value = flag[len("overlay2="):]
    elif flag == "overlay2": value = next(it, None)
print(("yes" if (value or "").strip() == "none" else "no") + "\t" + json.dumps(args))
'
flag_line="$(printf '%s' "$runtimes" | python3 -c "$read_flag" "$RUNTIME")"
[ "$flag_line" != "UNREGISTERED" ] || die "the docker daemon registers no runtime named '$RUNTIME'"
flag="${flag_line%%$'\t'*}"
args="${flag_line#*$'\t'}"

say "runtime:      $RUNTIME"
say "runtimeArgs:  $args"
say "image:        $IMAGE"
say "container as: $UID_GID"
if [ "$flag" = "yes" ]; then
  say "FLAG  yes:    --overlay2=none is set on '$RUNTIME'"
else
  say "FLAG  no:     '$RUNTIME' runs without --overlay2=none"
  if [ "$RUNTIME" = "runsc" ]; then
    say "              -> a runsc seat with [sessions] enabled will NOT advertise sessions"
  fi
fi

docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull -q "$IMAGE" >/dev/null || die "cannot pull $IMAGE"

# The turn counter lives on a HOST bind mount, which survives any runtime's stop/start, so the
# second start always knows it is the second one. (The first version kept the counter in /tmp and
# reported a setup error on every runsc run: /tmp did not survive a stop/start even with
# --overlay2=none, consistent with runsc mounting its own tmpfs at /tmp. Field run 2026-11-06, Lima arm64, runsc 20261102.0.)
# The markers are written to the container ROOTFS (what --overlay2 governs) and, for information,
# to /tmp. ROOTFS decides the exit code; TMP is printed only.
state_dir="$(mktemp -d)"
chown "$UID_GID" "$state_dir" 2>/dev/null || true
chmod 0777 "$state_dir"

# Rootfs directory: /home/agent (the sandbox image's HOME, 0777) when it is writable, otherwise
# /mx-home, which only uid 0 can create on a stock image (run this script with sudo).
# Single-quoted on purpose: the variables expand INSIDE the container, not here (SC2016).
# shellcheck disable=SC2016
probe='
d=/home/agent; [ -d "$d" ] && [ -w "$d" ] || d=/mx-home
mkdir -p "$d/.claude" 2>/dev/null || { echo NOROOTFSDIR; exit 0; }
if [ -f /mx-state/turn ]; then
  [ -f "$d/.claude/rootfs-marker" ] && echo ROOTFS=kept || echo ROOTFS=lost
  [ -f /tmp/.mx-tmp-marker ] && echo TMP=kept || echo TMP=lost
else
  echo turn1 > /mx-state/turn
  echo turn1 > "$d/.claude/rootfs-marker"
  echo turn1 > /tmp/.mx-tmp-marker
  echo "WROTE $d"
fi'

docker create --name "$NAME" --runtime "$RUNTIME" \
  --init --security-opt no-new-privileges --cap-drop ALL \
  --user "$UID_GID" -v "$state_dir:/mx-state" \
  "$IMAGE" sh -c "$probe" >/dev/null || die "docker create failed for runtime $RUNTIME"

first="$(docker start -a "$NAME")" || die "first docker start failed"
case "$first" in
  *NOROOTFSDIR*) die "uid $UID_GID cannot create a rootfs directory in $IMAGE: run with sudo, or IMAGE=<sandbox image>" ;;
  *WROTE*) say "rootfs dir:   ${first#WROTE }/.claude" ;;
  *) die "the first start did not write its marker: $first" ;;
esac
# The container has exited on its own; stop is a no-op then, and is here to mirror a park.
docker stop -t 5 "$NAME" >/dev/null 2>&1 || true
second="$(docker start -a "$NAME")" || die "second docker start failed"

case "$second" in
  *TMP=kept*) say "info /tmp:    kept across stop + start" ;;
  *TMP=lost*) say "info /tmp:    LOST across stop + start (runsc's own /tmp tmpfs; not what --overlay2 controls)" ;;
esac
case "$second" in
  *ROOTFS=kept*)
    say "PASS rootfs:  a file in the container rootfs survived docker stop + start"
    status=0 ;;
  *ROOTFS=lost*)
    say "FAIL rootfs:  a file in the container rootfs was DISCARDED by docker stop + start"
    say "     fix: in /etc/docker/daemon.json under \"runtimes\" set"
    say "     \"$RUNTIME\": {\"path\": \"<runsc path>\", \"runtimeArgs\": [\"--overlay2=none\"]},"
    say "     then restart docker (this stops running containers) and re-run this script"
    status=1 ;;
  *) die "unexpected second-start output: $second" ;;
esac
say "exit $status"
exit "$status"
