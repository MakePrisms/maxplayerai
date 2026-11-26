#!/usr/bin/env bash
#
# Wait until the npm registry SERVES a package version: the metadata npm install reads names a
# tarball for it, and that tarball downloads. Exit non-zero, naming the package, if it does not within the timeout.
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
#   CURL               the command to run (tests substitute a fake)
#
# The metadata read is the ABBREVIATED packument (Accept: application/vnd.npm.install-v1+json), the
# one npm install and npx fetch. The registry caches it separately from the full packument that
# `npm view` reads (Vary: accept, max-age=300), so `npm view` answering does not mean an install
# would find the version yet.

set -euo pipefail

die() { echo "wait-npm-published: $*" >&2; exit 1; }

[ "$#" -eq 2 ] || die "usage: $0 <package> <version>"
package="$1"
version="$2"
timeout="${WAIT_NPM_TIMEOUT:-2700}"
interval="${WAIT_NPM_INTERVAL:-15}"
registry="${WAIT_NPM_REGISTRY:-https://registry.npmjs.org}"
curl_cmd="${CURL:-curl}"
# A scoped name's slash is escaped in the registry path.
encoded="${package/\//%2f}"
install_accept='application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*'

started="$(date +%s)"
attempt=0
while :; do
    attempt=$((attempt + 1))
    packument="$("$curl_cmd" -fsSL --max-time 30 -H "Accept: $install_accept" "$registry/$encoded" 2>/dev/null || true)"
    tarball="$(printf '%s' "$packument" | VERSION="$version" node -e '
        let s = ""; process.stdin.on("data", (d) => (s += d)).on("end", () => {
          try { process.stdout.write(JSON.parse(s).versions[process.env.VERSION].dist.tarball || ""); } catch {}
        });' 2>/dev/null || true)"
    if [ -n "$tarball" ]; then
        if "$curl_cmd" -fsSL --max-time 60 -o /dev/null "$tarball"; then
            echo "ok: $package@$version is served by $registry (attempt $attempt, $(( $(date +%s) - started ))s)"
            exit 0
        fi
        why="its metadata names $tarball, but that tarball does not download yet"
    else
        why="the registry's install metadata does not list it yet"
    fi
    elapsed=$(( $(date +%s) - started ))
    if [ "$elapsed" -ge "$timeout" ]; then
        die "$package@$version is still not installable after ${elapsed}s ($why). Do not publish anything that depends on it; re-run this job once it is served."
    fi
    echo "waiting: $package@$version — $why (attempt $attempt, ${elapsed}s)"
    sleep "$interval"
done
