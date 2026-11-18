#!/usr/bin/env bash
#
# Pack the maxplayer-mint npm packages (launcher + this host's payload) and prove that
# `npx maxplayer-mint --version` launches the native binary through the launcher — locally, with
# install scripts disabled. Nothing here publishes. npm may still query the registry for the other
# platforms' optional payloads (a 404 for an unpublished version is expected). The mint's
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
( cd "$PROJECT" && npm install --silent --ignore-scripts --no-audit --no-fund "$TGZ_PLATFORM" "$TGZ_MAIN" >/dev/null ) \
    || die "npm install of the packed tarballs failed (see npm's output above)"

# `maxplayer-mint` must reach the LAUNCHER; the payload's own bin is `maxplayer-mint-<platform>`.
LINK="$PROJECT/node_modules/.bin/maxplayer-mint"
[ -e "$LINK" ] || die "npm did not link a 'maxplayer-mint' bin"
head -c 2 "$(readlink -f "$LINK")" | grep -q '#!' || die "'maxplayer-mint' does not resolve to the JS launcher"
echo "ok: 'maxplayer-mint' resolves to the JS launcher"

# A scratch home, forced: the mint's default home is ~/.maxplayer.
export MAXPLAYER_HOME="$WORK/home"
OUT="$( cd "$PROJECT" && npx --no-install maxplayer-mint --version )" || die "npx maxplayer-mint --version failed"
# The same stamp grammar verify-release-version.sh holds a release to: 40-hex sha, optionally a
# space and a timestamp. `-dirty` is refused there and here.
PREFIX="maxplayer-mint $VERSION "
STAMP="${OUT#"$PREFIX"}"
STAMP_RE='^\(([0-9a-f]{40})( [^)]*)?\)$'
[ "$STAMP" != "$OUT" ] && [[ "$STAMP" =~ $STAMP_RE ]] \
    || die "unexpected version output: '$OUT' (want 'maxplayer-mint $VERSION (<40-hex commit>)'; build from a committed tree)"
echo "ok: npx maxplayer-mint --version -> $OUT"

# The launcher must hand the exit status through: an unknown subcommand is the binary's usage exit 2.
set +e
( cd "$PROJECT" && npx --no-install maxplayer-mint not-a-subcommand >/dev/null 2>&1 ); rc=$?
set -e
[ "$rc" -eq 2 ] || die "the launcher returned $rc for an unknown subcommand, expected the binary's 2"
echo "ok: the launcher passes the binary's exit status through"

# Every argument must reach the binary: `--version extra` is not the version arm, so the binary
# answers with usage (exit 2). A launcher forwarding only argv[2] would print the version and exit 0.
set +e
( cd "$PROJECT" && npx --no-install maxplayer-mint --version extra >/dev/null 2>&1 ); rc=$?
set -e
[ "$rc" -eq 2 ] || die "the launcher returned $rc for '--version extra', expected 2 — it does not forward every argument"
echo "ok: the launcher forwards every argument"

# Signals sent to the LAUNCHER must reach the binary (#1339 review M3): a supervisor that signals
# only the main PID would otherwise kill node and leave `maxplayer-mint run` running with no clean
# stop. A stand-in payload records which signal reached it; the real payload is restored after.
PAYLOAD_BIN="$PROJECT/node_modules/@maxplayerai/mint-$PLATFORM/bin/maxplayer-mint"
cp "$PAYLOAD_BIN" "$WORK/real-payload"
MARK="$WORK/signal-marker"
cat > "$PAYLOAD_BIN" <<STUB
#!/bin/sh
trap 'echo TERM > "$MARK"; exit 0' TERM
echo ready > "$MARK"
while :; do sleep 1; done
STUB
chmod 755 "$PAYLOAD_BIN"
( cd "$PROJECT" && exec node node_modules/maxplayer-mint/bin/maxplayer-mint.js run ) &
LAUNCHER_PID=$!
for _ in $(seq 1 100); do [ -s "$MARK" ] && break; sleep 0.1; done
[ "$(cat "$MARK" 2>/dev/null)" = ready ] || die "the stand-in payload never started under the launcher"
kill -TERM "$LAUNCHER_PID"
set +e; wait "$LAUNCHER_PID"; rc=$?; set -e
[ "$(cat "$MARK")" = TERM ] || die "SIGTERM to the launcher did not reach the binary — it would be left running"
[ "$rc" -eq 0 ] || die "the launcher exited $rc after the binary stopped cleanly on SIGTERM, expected the binary's 0"
cp "$WORK/real-payload" "$PAYLOAD_BIN"
echo "ok: SIGTERM to the launcher reaches the binary, and its clean exit passes through"

# Negative control: without the payload the launcher must fail clearly, not crash.
rm -rf "$PROJECT/node_modules/@maxplayerai/mint-$PLATFORM"
ERR="$( cd "$PROJECT" && npx --no-install maxplayer-mint --version 2>&1 >/dev/null )" \
    && die "control failed: the launcher still succeeded with the payload package removed"
grep -q 'no binary for' <<<"$ERR" || die "control produced no actionable message: $ERR"
echo "ok: control -> payload removed gives a clear error, non-zero"

echo "PASS: npx maxplayer-mint launches the native binary, and fails cleanly without it"
