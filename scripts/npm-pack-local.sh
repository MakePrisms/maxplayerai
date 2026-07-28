#!/usr/bin/env bash
#
# Build the npm wrapper packages and prove `npx mobee` launches the native binary — entirely
# locally. Nothing here publishes, and nothing here reaches the public registry.
#
# The shape is the esbuild/swc one: `mobee` is a tiny JS launcher, and the executable lives in a
# per-platform package listed under optionalDependencies so an install pulls one platform's binary
# rather than all of them. The alternative — a postinstall downloader — breaks under
# --ignore-scripts, which security-conscious users and many CI setups set by default, so this script
# installs WITH --ignore-scripts to keep that property honest.
#
# Usage:
#   ./scripts/npm-pack-local.sh [path-to-static-binary]     # default: result/bin/mobee
#     nix build .#buyer-static     # produces result/bin/mobee

set -euo pipefail

BINARY="${1:-result/bin/mobee}"
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
mkdir -p "$STAGE/cli-linux-x64/bin"
cp -L "$BINARY" "$STAGE/cli-linux-x64/bin/mobee"
chmod 755 "$STAGE/cli-linux-x64/bin/mobee"

( cd "$STAGE/mobee"          && npm pack --silent --pack-destination "$DIST" >/dev/null )
( cd "$STAGE/cli-linux-x64"  && npm pack --silent --pack-destination "$DIST" >/dev/null )

TGZ_MAIN="$(find "$DIST" -name 'mobee-*.tgz' -not -name 'mobee-cli-*' | head -1)"
TGZ_PLATFORM="$(find "$DIST" -name 'mobee-cli-linux-x64-*.tgz' | head -1)"
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
grep -qx 'package/bin/mobee' <<<"$TARBALL_LISTING" \
    || die "platform tarball does not contain package/bin/mobee"
echo "ok: platform tarball carries bin/mobee"

# ── Install into a clean project, scripts disabled ──────────────────────────────────────────────
cat > "$PROJECT/package.json" <<JSON
{ "name": "mobee-wrapper-proof", "version": "0.0.0", "private": true }
JSON
( cd "$PROJECT" && npm install --silent --ignore-scripts "$TGZ_PLATFORM" "$TGZ_MAIN" >/dev/null )

# `npx mobee` must reach the LAUNCHER. The platform package deliberately names its own bin
# `mobee-linux-x64` so it cannot win the `mobee` name and silently bypass the launcher.
LINK="$PROJECT/node_modules/.bin/mobee"
[ -e "$LINK" ] || die "npm did not link a 'mobee' bin"
head -c 4 "$(readlink -f "$LINK")" | grep -q $'\x7fELF' \
    && die "'mobee' resolves straight to the ELF — the launcher is being bypassed"
echo "ok: 'mobee' resolves to the JS launcher"

# ── Prove it launches ───────────────────────────────────────────────────────────────────────────
# A scratch home, always: this OVERRIDES whatever MOBEE_HOME the caller had. With MOBEE_HOME unset
# mobee falls back to ~/.mobee — a real wallet home on a developer machine — and inheriting a
# caller's home would be just as wrong, so the value is forced rather than checked.
export MOBEE_HOME="$WORK/home"
mkdir -p "$MOBEE_HOME"

VERSION_OUT="$( cd "$PROJECT" && npx --no-install mobee version )" \
    || die "npx mobee version failed"
echo "$VERSION_OUT" | grep -Eq '^mobee [0-9]+\.[0-9]+\.[0-9]+$' \
    || die "unexpected version output: $VERSION_OUT"
echo "ok: npx mobee version -> $VERSION_OUT"

# The real target: the buyer MCP server over stdio, which is how an MCP client launches it.
REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"npm-wrapper-proof","version":"0"}}}'
MCP_OUT="$( cd "$PROJECT" && printf '%s\n' "$REQ" | timeout 30 npx --no-install mobee mcp 2>/dev/null )" \
    || die "npx mobee mcp failed"
echo "$MCP_OUT" | grep -q '"serverInfo":{"name":"mobee"' \
    || die "no MCP initialize result; got: $(echo "$MCP_OUT" | head -2)"
echo "ok: npx mobee mcp -> initialize answered by serverInfo.name=mobee"

# ── Negative control ────────────────────────────────────────────────────────────────────────────
# Without this, the launcher's "no binary for this platform" branch is unreachable code that has
# never been observed working — and a missing payload would surface as a confusing crash.
rm -rf "$PROJECT/node_modules/@mobee/cli-linux-x64"
if ( cd "$PROJECT" && npx --no-install mobee version >/dev/null 2>&1 ); then
    die "control failed: launcher still succeeded with the platform package removed"
fi
CONTROL_ERR="$( cd "$PROJECT" && npx --no-install mobee version 2>&1 >/dev/null || true )"
echo "$CONTROL_ERR" | grep -q 'no binary for' \
    || die "control produced no actionable message: $CONTROL_ERR"
echo "ok: control -> platform package removed gives a clear error, non-zero"

echo "PASS: npx mobee launches the native binary, and fails cleanly without it"
