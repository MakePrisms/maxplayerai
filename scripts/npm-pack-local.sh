#!/usr/bin/env bash
#
# Build the npm wrapper packages and prove `npx maxplayer` launches the native binary — entirely
# locally. Nothing here publishes, and nothing here reaches the public registry.
#
# The shape is the esbuild/swc one: `maxplayer` is a tiny JS launcher, and the executable lives in a
# per-platform package listed under optionalDependencies so an install pulls one platform's binary
# rather than all of them. The alternative — a postinstall downloader — breaks under
# --ignore-scripts, which security-conscious users and many CI setups set by default, so this script
# installs WITH --ignore-scripts to keep that property honest.
#
# Usage:
#   ./scripts/npm-pack-local.sh [path-to-static-binary] [path-to-aarch64-binary]
#     default: result/bin/maxplayer
#     nix build .#buyer-static            # produces the x86_64 payload
#     nix build .#buyer-static-aarch64    # optional second argument
#
# The aarch64 payload is packed and structurally checked only — see the note at that step.

set -euo pipefail

BINARY="${1:-result/bin/maxplayer}"
BINARY_ARM64="${2:-}"
PKG_MAIN="npm/mobee"
PKG_PLATFORM="npm/cli-linux-x64"

die() { echo "npm-pack-local: $*" >&2; exit 1; }

command -v npm  >/dev/null 2>&1 || die "npm not found"
command -v node >/dev/null 2>&1 || die "node not found"
command -v ldd  >/dev/null 2>&1 || die "ldd not found"
[ -f "$BINARY" ]          || die "no binary at $BINARY — run: nix build .#buyer-static"
[ -f "$PKG_MAIN/package.json" ]     || die "run from the repo root ($PKG_MAIN missing)"
[ -f "$PKG_PLATFORM/package.json" ] || die "run from the repo root ($PKG_PLATFORM missing)"

# ── The payload must be the portable artifact, not a dev build ───────────────────────────────────
# Shipping a dynamically linked binary would produce a package that works only on machines
# resembling the builder. Assert rather than assume.
head -c 4 "$BINARY" | grep -q $'\x7fELF' || die "$BINARY is not an ELF binary"
case "$(ldd "$BINARY" 2>&1 || true)" in
    *"statically linked"* | *"not a dynamic executable"*) ;;
    *) die "$BINARY is dynamically linked — package the output of .#buyer-static" ;;
esac
echo "ok: payload is a static ELF"

# ── Every package declares the project's licence ─────────────────────────────────────────────────
# The SPDX `license` FIELD and the licence FILES are two independent things a package must get right,
# and a new platform package can satisfy one while contradicting the other — shipping LICENSE-APACHE
# in `files` while declaring plain "MIT" says two different things about the same tarball. The files
# are asserted after packing; the field is asserted here.
#
# Every directory under npm/ is checked, not only the ones this run packs, so a package added but not
# yet wired up (the next platform) is covered too. The field is read as JSON rather than grepped:
# `"license"` also matches a `licenseFile` key or a nested string.
EXPECTED_LICENSE="MIT OR Apache-2.0"
for manifest in npm/*/package.json; do
    declared="$(node -e 'process.stdout.write(String(require(process.argv[1]).license))' "$PWD/$manifest")"
    [ "$declared" = "$EXPECTED_LICENSE" ] \
        || die "$manifest declares license '$declared', expected '$EXPECTED_LICENSE'"
done
echo "ok: every npm package declares $EXPECTED_LICENSE"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
STAGE="$WORK/stage"
DIST="$WORK/dist"
PROJECT="$WORK/project"
mkdir -p "$STAGE" "$DIST" "$PROJECT"

# ── Stage and pack ──────────────────────────────────────────────────────────────────────────────
# The binary is copied in at pack time and is deliberately NOT committed: it is ~38MB of build
# output, and it belongs to a release, not to the source tree.
cp -R "$PKG_MAIN"     "$STAGE/mobee"
cp -R "$PKG_PLATFORM" "$STAGE/cli-linux-x64"

# Every published package carries both licence texts. Apache-2.0 section 4(a) requires a copy of the
# License to travel with distributed copies, and npm cannot reference paths above a package
# directory, so the files are copied in here rather than listed from the repo root.
stage_licenses() {
    cp LICENSE-MIT LICENSE-APACHE "$1/" \
        || die "could not stage the licence files into $1"
}
stage_licenses "$STAGE/mobee"
stage_licenses "$STAGE/cli-linux-x64"
mkdir -p "$STAGE/cli-linux-x64/bin"
cp -L "$BINARY" "$STAGE/cli-linux-x64/bin/maxplayer"
chmod 755 "$STAGE/cli-linux-x64/bin/maxplayer"

( cd "$STAGE/mobee"          && npm pack --silent --pack-destination "$DIST" >/dev/null )
( cd "$STAGE/cli-linux-x64"  && npm pack --silent --pack-destination "$DIST" >/dev/null )

TGZ_MAIN="$(find "$DIST" -name 'maxplayer-*.tgz' | head -1)"
TGZ_PLATFORM="$(find "$DIST" -name 'maxplayerai-linux-x64-*.tgz' | head -1)"
[ -n "$TGZ_MAIN" ]     || die "main tarball not produced"
[ -n "$TGZ_PLATFORM" ] || die "platform tarball not produced"
echo "ok: packed $(basename "$TGZ_MAIN") + $(basename "$TGZ_PLATFORM")"

# The platform tarball must actually contain the executable — a `files` typo fails silently
# otherwise, producing a package that installs fine and then cannot run anything.
#
# Read the listing into a variable first. Under `set -o pipefail`, `tar … | grep -q` reports FAILURE
# on a successful match: grep exits at the first hit, tar dies of SIGPIPE, and pipefail surfaces
# tar's death. That inverts the check — it fails loudest when the file is present.
TARBALL_LISTING="$(tar -tzf "$TGZ_PLATFORM")"
grep -qx 'package/bin/maxplayer' <<<"$TARBALL_LISTING" \
    || die "platform tarball does not contain package/bin/maxplayer"
echo "ok: platform tarball carries bin/maxplayer"

# ── Optional: the aarch64 payload package ───────────────────────────────────────────────────────
# Packed and structurally checked, nothing more. npm skips it here on the os/cpu mismatch, and an
# aarch64 binary cannot execute on x86_64, so proving it RUNS belongs to verify-arm64-artifact.sh on
# an arm64 host. What is worth asserting here is that the tarball exists, carries the executable, and
# carries the right architecture — a payload package that shipped the wrong binary would install
# cleanly and fail only at run time, on the one platform we cannot test.
TGZ_ARM64=""
if [ -n "$BINARY_ARM64" ]; then
    [ -f "$BINARY_ARM64" ] || die "no aarch64 binary at $BINARY_ARM64 — run: nix build .#buyer-static-aarch64"

    cp -R npm/cli-linux-arm64 "$STAGE/cli-linux-arm64"
    stage_licenses "$STAGE/cli-linux-arm64"
    mkdir -p "$STAGE/cli-linux-arm64/bin"
    cp -L "$BINARY_ARM64" "$STAGE/cli-linux-arm64/bin/maxplayer"
    chmod 755 "$STAGE/cli-linux-arm64/bin/maxplayer"
    ( cd "$STAGE/cli-linux-arm64" && npm pack --silent --pack-destination "$DIST" >/dev/null )

    TGZ_ARM64="$(find "$DIST" -name 'maxplayerai-linux-arm64-*.tgz' | head -1)"
    [ -n "$TGZ_ARM64" ] || die "aarch64 tarball not produced"
    ARM_LISTING="$(tar -tzf "$TGZ_ARM64")"
    grep -qx 'package/bin/maxplayer' <<<"$ARM_LISTING" \
        || die "aarch64 tarball does not contain package/bin/maxplayer"

    ARM_MACHINE="$(node "$(dirname "$0")/elf-info.mjs" "$BINARY_ARM64" | sed -n 's/^machine=//p')"
    [ "$ARM_MACHINE" = "AArch64" ] \
        || die "aarch64 payload is $ARM_MACHINE, not AArch64 — the wrong binary was staged"
    echo "ok: packed $(basename "$TGZ_ARM64") carrying an AArch64 binary"
fi

# Both licence texts must be inside EVERY published tarball, the aarch64 payload included — it is a
# published package like any other, and Apache-2.0 4(a) does not exempt it. A `files` typo or a missed
# staging step fails silently: the package installs cleanly and is simply missing the licence it
# claims to be under.
for tgz in "$TGZ_MAIN" "$TGZ_PLATFORM" ${TGZ_ARM64:+"$TGZ_ARM64"}; do
    listing="$(tar -tzf "$tgz")"
    for lic in LICENSE-MIT LICENSE-APACHE; do
        grep -qx "package/$lic" <<<"$listing" \
            || die "$(basename "$tgz") does not contain $lic — Apache-2.0 4(a) requires it to ship"
    done
done
echo "ok: every tarball carries LICENSE-MIT and LICENSE-APACHE"

# ── Install into a clean project, scripts disabled ──────────────────────────────────────────────
cat > "$PROJECT/package.json" <<JSON
{ "name": "maxplayer-wrapper-proof", "version": "0.0.0", "private": true }
JSON
( cd "$PROJECT" && npm install --silent --ignore-scripts "$TGZ_PLATFORM" "$TGZ_MAIN" >/dev/null )

# `npx maxplayer` must reach the LAUNCHER. The platform package deliberately names its own bin
# `maxplayer-linux-x64` so it cannot win the `maxplayer` name and silently bypass the launcher.
# That bin name intentionally does NOT track the package name: the package is `@maxplayerai/linux-x64`,
# and npm's default bin name for it would be the scope-stripped `linux-x64` — far too generic for a
# command installed into a shared `.bin`, so the key stays spelled out.
LINK="$PROJECT/node_modules/.bin/maxplayer"
[ -e "$LINK" ] || die "npm did not link a 'maxplayer' bin"
head -c 4 "$(readlink -f "$LINK")" | grep -q $'\x7fELF' \
    && die "'maxplayer' resolves straight to the ELF — the launcher is being bypassed"
echo "ok: 'maxplayer' resolves to the JS launcher"

# ── Prove it launches ───────────────────────────────────────────────────────────────────────────
# A scratch home, always: this OVERRIDES whatever MOBEE_HOME the caller had. With MOBEE_HOME unset
# maxplayer falls back to ~/.mobee — a real wallet home on a developer machine — and inheriting a
# caller's home would be just as wrong, so the value is forced rather than checked.
export MOBEE_HOME="$WORK/home"
mkdir -p "$MOBEE_HOME"

VERSION_OUT="$( cd "$PROJECT" && npx --no-install maxplayer version )" \
    || die "npx maxplayer version failed"
echo "$VERSION_OUT" | grep -Eq '^maxplayer [0-9]+\.[0-9]+\.[0-9]+$' \
    || die "unexpected version output: $VERSION_OUT"
echo "ok: npx maxplayer version -> $VERSION_OUT"

# The real target: the buyer MCP server over stdio, which is how an MCP client launches it.
REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"npm-wrapper-proof","version":"0"}}}'
MCP_OUT="$( cd "$PROJECT" && printf '%s\n' "$REQ" | timeout 30 npx --no-install maxplayer mcp 2>/dev/null )" \
    || die "npx maxplayer mcp failed"
echo "$MCP_OUT" | grep -q '"serverInfo":{"name":"maxplayer"' \
    || die "no MCP initialize result; got: $(echo "$MCP_OUT" | head -2)"
echo "ok: npx maxplayer mcp -> initialize answered by serverInfo.name=maxplayer"

# ── Negative control ────────────────────────────────────────────────────────────────────────────
# Without this, the launcher's "no binary for this platform" branch is unreachable code that has
# never been observed working — and a missing payload would surface as a confusing crash.
rm -rf "$PROJECT/node_modules/@maxplayerai/linux-x64"
if ( cd "$PROJECT" && npx --no-install maxplayer version >/dev/null 2>&1 ); then
    die "control failed: launcher still succeeded with the platform package removed"
fi
CONTROL_ERR="$( cd "$PROJECT" && npx --no-install maxplayer version 2>&1 >/dev/null || true )"
echo "$CONTROL_ERR" | grep -q 'no binary for' \
    || die "control produced no actionable message: $CONTROL_ERR"
echo "ok: control -> platform package removed gives a clear error, non-zero"

echo "PASS: npx maxplayer launches the native binary, and fails cleanly without it"
