#!/usr/bin/env bash
#
# Pack the maxplayer-mint npm packages (launcher + this host's payload) and prove that
# `npx maxplayer-mint --version` launches the native binary through the launcher — locally, with
# install scripts disabled. Nothing here publishes or reaches the public registry. The mint's
# counterpart of npm-pack-local.sh; CI runs it in the maxplayer-mint job on every pull request.
#
# Usage:
#   ./scripts/npm-pack-mint-local.sh <path-to-maxplayer-mint-binary>
#     e.g. ./scripts/npm-pack-mint-local.sh crates/maxplayer-mint/target/debug/maxplayer-mint
#
# The binary must be for THIS host (it is executed); its payload package is chosen from
# `node -p process.platform-process.arch`, exactly as the launcher resolves it at run time.

set -euo pipefail

BINARY="${1:-}"
PKG_MAIN="npm/maxplayer-mint"

die() { echo "npm-pack-mint-local: $*" >&2; exit 1; }

command -v npm  >/dev/null 2>&1 || die "npm not found"
command -v node >/dev/null 2>&1 || die "node not found"
[ -n "$BINARY" ] && [ -f "$BINARY" ] || die "usage: npm-pack-mint-local.sh <path-to-maxplayer-mint-binary>"
[ -f "$PKG_MAIN/package.json" ] || die "run from the repo root ($PKG_MAIN missing)"

PLATFORM="$(node -p 'process.platform + "-" + process.arch')"
PKG_PLATFORM="npm/mint-$PLATFORM"
[ -f "$PKG_PLATFORM/package.json" ] || die "no mint payload package for this host ($PKG_PLATFORM)"
VERSION="$(node -p "require('./$PKG_MAIN/package.json').version")"

EXPECTED_LICENSE="MIT OR Apache-2.0"
for manifest in "$PKG_MAIN/package.json" npm/mint-*/package.json; do
    declared="$(node -e 'process.stdout.write(String(require(process.argv[1]).license))' "$PWD/$manifest")"
    [ "$declared" = "$EXPECTED_LICENSE" ] || die "$manifest declares license '$declared', expected '$EXPECTED_LICENSE'"
done
echo "ok: every mint npm package declares $EXPECTED_LICENSE"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
STAGE="$WORK/stage"; DIST="$WORK/dist"; PROJECT="$WORK/project"
mkdir -p "$STAGE" "$DIST" "$PROJECT"

# Staged exactly as the release's publish job stages them: licences copied in, the binary under
# bin/maxplayer-mint at mode 755.
cp -R "$PKG_MAIN" "$STAGE/launcher"
cp -R "$PKG_PLATFORM" "$STAGE/payload"
cp LICENSE-MIT LICENSE-APACHE "$STAGE/launcher/"
cp LICENSE-MIT LICENSE-APACHE "$STAGE/payload/"
mkdir -p "$STAGE/payload/bin"
cp -L "$BINARY" "$STAGE/payload/bin/maxplayer-mint"
chmod 755 "$STAGE/payload/bin/maxplayer-mint"

( cd "$STAGE/launcher" && npm pack --silent --pack-destination "$DIST" >/dev/null )
( cd "$STAGE/payload"  && npm pack --silent --pack-destination "$DIST" >/dev/null )
TGZ_MAIN="$DIST/maxplayer-mint-$VERSION.tgz"
TGZ_PLATFORM="$DIST/maxplayerai-mint-$PLATFORM-$VERSION.tgz"
[ -f "$TGZ_MAIN" ]     || die "launcher tarball not produced ($(ls "$DIST"))"
[ -f "$TGZ_PLATFORM" ] || die "payload tarball not produced ($(ls "$DIST"))"

# A `files` typo produces a package that installs and then runs nothing, or ships without licences.
check_listing() {  # <tgz> <entry>...
    local tgz="$1" listing; shift
    listing="$(tar -tzf "$tgz")"
    for entry in "$@"; do
        grep -qx "package/$entry" <<<"$listing" || die "$(basename "$tgz") does not contain $entry"
    done
}
check_listing "$TGZ_MAIN" bin/maxplayer-mint.js README.md LICENSE-MIT LICENSE-APACHE
check_listing "$TGZ_PLATFORM" bin/maxplayer-mint LICENSE-MIT LICENSE-APACHE
echo "ok: packed $(basename "$TGZ_MAIN") + $(basename "$TGZ_PLATFORM") with binary, launcher and licences"

echo '{ "name": "maxplayer-mint-wrapper-proof", "version": "0.0.0", "private": true }' > "$PROJECT/package.json"
( cd "$PROJECT" && npm install --silent --ignore-scripts "$TGZ_PLATFORM" "$TGZ_MAIN" >/dev/null )

# `maxplayer-mint` must reach the LAUNCHER; the payload's own bin is `maxplayer-mint-<platform>`.
LINK="$PROJECT/node_modules/.bin/maxplayer-mint"
[ -e "$LINK" ] || die "npm did not link a 'maxplayer-mint' bin"
head -c 2 "$(readlink -f "$LINK")" | grep -q '#!' || die "'maxplayer-mint' does not resolve to the JS launcher"
echo "ok: 'maxplayer-mint' resolves to the JS launcher"

# A scratch home, forced: the mint's default home is ~/.maxplayer.
export MAXPLAYER_HOME="$WORK/home"
OUT="$( cd "$PROJECT" && npx --no-install maxplayer-mint --version )" || die "npx maxplayer-mint --version failed"
[[ "$OUT" =~ ^maxplayer-mint\ ${VERSION//./\\.}\ \([0-9a-f]{40}(-dirty)?\)$ ]] \
    || die "unexpected version output: '$OUT' (want 'maxplayer-mint $VERSION (<commit>)')"
echo "ok: npx maxplayer-mint --version -> $OUT"

# The launcher must hand the exit status through: an unknown subcommand is the binary's usage exit 2.
set +e
( cd "$PROJECT" && npx --no-install maxplayer-mint not-a-subcommand >/dev/null 2>&1 ); rc=$?
set -e
[ "$rc" -eq 2 ] || die "the launcher returned $rc for an unknown subcommand, expected the binary's 2"
echo "ok: the launcher passes the binary's exit status through"

# Negative control: without the payload the launcher must fail clearly, not crash.
rm -rf "$PROJECT/node_modules/@maxplayerai/mint-$PLATFORM"
ERR="$( cd "$PROJECT" && npx --no-install maxplayer-mint --version 2>&1 >/dev/null )" \
    && die "control failed: the launcher still succeeded with the payload package removed"
grep -q 'no binary for' <<<"$ERR" || die "control produced no actionable message: $ERR"
echo "ok: control -> payload removed gives a clear error, non-zero"

echo "PASS: npx maxplayer-mint launches the native binary, and fails cleanly without it"
