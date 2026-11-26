#!/usr/bin/env bash
#
# Wait until the npm registry SERVES a package version: its metadata names a tarball, and that
# tarball downloads. Exit non-zero, naming the package, if it does not within the timeout.
#
# Why: `npm publish` returning is not the package being installable. After 0.8.2-rc2's publish,
# @maxplayerai/linux-x64 answered 404 for about 23 minutes while the launcher that pins it by exact
# version was already live. npm skips a missing optional dependency SILENTLY, so every install and
# reinstall in that window got a launcher with no binary and said nothing. The release publishes the
# launcher only after this script has seen every payload package served.
#
# Usage:
#   ./scripts/wait-npm-published.sh <package> <version>
#
# Environment (all optional):
#   WAIT_NPM_TIMEOUT   seconds to keep trying (default 2700, 45 min)
#   WAIT_NPM_INTERVAL  seconds between tries (default 15)
#   WAIT_NPM_REGISTRY  registry URL (default https://registry.npmjs.org)
#   NPM, CURL          the commands to run (tests substitute fakes)

set -euo pipefail

die() { echo "wait-npm-published: $*" >&2; exit 1; }

[ "$#" -eq 2 ] || die "usage: $0 <package> <version>"
package="$1"
version="$2"
timeout="${WAIT_NPM_TIMEOUT:-2700}"
interval="${WAIT_NPM_INTERVAL:-15}"
registry="${WAIT_NPM_REGISTRY:-https://registry.npmjs.org}"
npm_cmd="${NPM:-npm}"
curl_cmd="${CURL:-curl}"

started="$(date +%s)"
attempt=0
while :; do
    attempt=$((attempt + 1))
    # --prefer-online: npm's own metadata cache must not answer for the registry.
    tarball="$("$npm_cmd" view "$package@$version" dist.tarball --registry "$registry" --prefer-online 2>/dev/null || true)"
    if [ -n "$tarball" ]; then
        if "$curl_cmd" -fsSL --max-time 60 -o /dev/null "$tarball"; then
            echo "ok: $package@$version is served by $registry (attempt $attempt, $(( $(date +%s) - started ))s)"
            exit 0
        fi
        why="its metadata names $tarball, but that tarball does not download yet"
    else
        why="the registry does not list it yet"
    fi
    elapsed=$(( $(date +%s) - started ))
    if [ "$elapsed" -ge "$timeout" ]; then
        die "$package@$version is still not installable after ${elapsed}s ($why). Do not publish anything that depends on it; re-run this job once 'npm view $package@$version' answers."
    fi
    echo "waiting: $package@$version — $why (attempt $attempt, ${elapsed}s)"
    sleep "$interval"
done
