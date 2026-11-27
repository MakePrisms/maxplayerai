#!/usr/bin/env bash
#
# Publish one npm package directory, treating "this version is already on the registry" as done.
#
# Why: a re-run of the release's publish job must get past packages an earlier run already
# published. The workflow first asks `npm view`, but that read can be answered by a cache that
# predates the publish (the registry's CDN keeps metadata for up to 5 minutes, and the earlier run's
# own `npm view` can be what filled it), or still 404 right after a publish. `npm publish` then
# refuses with E403 "You cannot publish over the previously published versions" and the re-run dies
# before it reaches the wait loop. The registry's own refusal is the authoritative answer, so it is
# read here: that conflict means the package is already published; any other failure fails the job.
#
# Usage:
#   ./scripts/npm-publish-once.sh <package-dir> [npm publish args...]
#
# Environment (optional):
#   NPM   the command to run (default npm; tests substitute a fake)

set -euo pipefail

[ "$#" -ge 1 ] || { echo "usage: $0 <package-dir> [npm publish args...]" >&2; exit 2; }
dir="$1"
shift
npm_cmd="${NPM:-npm}"
name="$(cd "$dir" && node -p 'const p = require("./package.json"); p.name + "@" + p.version')"

status=0
out="$(cd "$dir" && "$npm_cmd" publish "$@" 2>&1)" || status=$?
printf '%s\n' "$out"
[ "$status" -eq 0 ] && exit 0
case "$out" in
    *"cannot publish over the previously published version"*|*EPUBLISHCONFLICT*)
        echo "$name is already on the registry (npm refused a second publish of it); continuing"
        exit 0
        ;;
esac
echo "npm-publish-once: publishing $name failed (exit $status)" >&2
exit "$status"
