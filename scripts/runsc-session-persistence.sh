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
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
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

# One script for both starts: the first start writes, the second reports what it finds. HOME is
# pointed at a path the job uid can create, like the sandbox image's 0777 /home/agent.
# Single-quoted on purpose: $HOME must expand INSIDE the container, not here (SC2016).
# shellcheck disable=SC2016
probe='
mkdir -p "$HOME/.claude"
if [ -f /tmp/.mx-turn ]; then
  [ -f "$HOME/.claude/rootfs-marker" ] && echo ROOTFS=kept || echo ROOTFS=lost
else
  echo turn1 > /tmp/.mx-turn
  echo turn1 > "$HOME/.claude/rootfs-marker"
  echo WROTE
fi'

docker create --name "$NAME" --runtime "$RUNTIME" \
  --init --security-opt no-new-privileges --cap-drop ALL \
  --user "$UID_GID" -e HOME=/tmp/agent \
  "$IMAGE" sh -c "$probe" >/dev/null || die "docker create failed for runtime $RUNTIME"

first="$(docker start -a "$NAME")" || die "first docker start failed"
case "$first" in
  *WROTE*) ;;
  *) die "the first start did not write its marker: $first" ;;
esac
# The container has exited on its own; stop is a no-op then, and is here to mirror a park.
docker stop -t 5 "$NAME" >/dev/null 2>&1 || true
second="$(docker start -a "$NAME")" || die "second docker start failed"

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
