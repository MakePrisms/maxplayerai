#!/bin/zsh -f
# run-detached.sh — run one long generative design job so it survives the caller's exit.
#
# A real model turn authoring, rendering and iterating on a design takes longer than a
# single supervised foreground step, and a job killed halfway proves nothing. This wrapper
# runs the job to completion and records the exit code in a marker file, so the outcome can
# be read back later instead of inferred.
#
# Usage: scripts/run-detached.sh <out-dir> [extra args to design-run.mjs...]
set -u
here=${0:A:h}
root=${here:h}
out=${1:?usage: run-detached.sh <out-dir> [args...]}
shift
mkdir -p "$out"
cd "$root" || exit 3
: > "$out/detached.log"
node agent/design-run.mjs --out "$out" "$@" >> "$out/detached.log" 2>&1
code=$?
print -r -- "$code" > "$out/exit-code"
print -r -- "finished $(date -u +%Y-%m-%dT%H:%M:%SZ) exit=$code" >> "$out/detached.log"
exit $code
