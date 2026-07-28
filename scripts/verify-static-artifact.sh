#!/usr/bin/env bash
#
# Portability verifier for a release artifact — asserts the binary runs on a machine that has
# neither nix nor a rust toolchain.
#
# A nix-built binary normally hardcodes an ELF interpreter under /nix/store, so it runs only on a
# machine with that store. `packages.<system>.buyer-static` links statically against musl and has
# no interpreter at all. This script is what proves that property, and it is what a release
# workflow should call before publishing an asset.
#
# ★ It proves PORTABILITY ONLY — that the artifact starts and answers `version`. It does NOT test
#   any feature-gated surface, and it cannot: `version` succeeds in every build regardless of which
#   features were compiled in. A capability gate has to drive the real subcommand instead.
#
# Usage:
#   ./scripts/verify-static-artifact.sh [path-to-binary]        # default: result/bin/mobee

set -euo pipefail

BINARY="${1:-result/bin/mobee}"

# Two images on purpose: alpine is musl, debian is glibc. Passing on both shows the artifact is
# libc-independent rather than merely alpine-compatible.
IMAGES=("alpine:3" "debian:bookworm-slim")

die() { echo "verify-static-artifact: $*" >&2; exit 1; }

[ -f "$BINARY" ] || die "no binary at $BINARY — build one with: nix build .#buyer-static"

# ── Fail closed ─────────────────────────────────────────────────────────────────────────────────
# A missing tool has to stop the run. Skipping the only check able to observe the failure would
# leave behind a pass that means nothing. (Deliberately not readelf/objdump/file: none of the three
# is present on a stock NixOS host, and a check that silently never runs is worse than no check.)
command -v ldd    >/dev/null 2>&1 || die "ldd not found"
command -v docker >/dev/null 2>&1 || die "docker not found — cannot verify without a nix-free container"

# ── Linkage ─────────────────────────────────────────────────────────────────────────────────────
# Confirm it is an ELF binary before asking ldd about it. ldd reports "statically linked" for a
# shell script too, so without this the linkage check passes on things that are not binaries.
head -c 4 "$BINARY" | grep -q $'\x7fELF' || die "$BINARY is not an ELF binary"

LINKAGE="$(ldd "$BINARY" 2>&1 || true)"
case "$LINKAGE" in
    *"statically linked"* | *"not a dynamic executable"*) ;;
    *) die "$BINARY is dynamically linked, so it depends on this machine's libraries:"$'\n'"$LINKAGE" ;;
esac
echo "ok: statically linked"

# ── Run it where no /nix exists ─────────────────────────────────────────────────────────────────
# This is the decisive check, and it subsumes inspecting the ELF headers: a binary that wants an
# interpreter under /nix/store cannot exec at all inside these images.
#
# Copying the binary out of the store is the point. Run it in place and a store path it still needs
# can be satisfied by the build machine's own store, so a broken artifact would pass.
SHIPDIR="$(mktemp -d)"
trap 'rm -rf "$SHIPDIR"' EXIT
cp -L "$BINARY" "$SHIPDIR/mobee"
chmod 755 "$SHIPDIR/mobee"

for image in "${IMAGES[@]}"; do
    # Assert the premise instead of assuming it: an image that shipped a /nix could satisfy a
    # needed store path, and the run below would prove nothing.
    docker run --rm "$image" sh -c '! test -e /nix' \
        || die "$image contains /nix — it cannot show the artifact runs without nix"

    out="$(docker run --rm -v "$SHIPDIR:/b:ro" "$image" /b/mobee version)" \
        || die "$image: the artifact failed to run"
    [ -n "$out" ] || die "$image: ran but printed nothing"
    echo "ok: $image -> $out"
done

# ── Negative control ────────────────────────────────────────────────────────────────────────────
# Without this the successes above are unfalsified: a binary that exited 0 on everything would
# pass every check so far.
if docker run --rm -v "$SHIPDIR:/b:ro" "${IMAGES[0]}" /b/mobee not-a-subcommand >/dev/null 2>&1; then
    die "control failed: the artifact exits 0 on an unknown subcommand, so rc=0 above proves nothing"
fi
echo "ok: control -> unknown subcommand exits nonzero"

echo "PASS: $BINARY runs with no nix and no toolchain present"
