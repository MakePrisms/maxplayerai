#!/usr/bin/env bash
#
# Assemble one downloadable release asset: the binary plus the licences that have to travel with it.
#
# A binary attached to a GitHub Release is a distribution of the work like any other, so
# Apache-2.0 section 4(a) applies to it exactly as it applies to the npm tarballs — a bare executable
# published on its own ships the code without the licence it is offered under. Packaging the asset as
# an archive is what makes room for the licence files; it is not presentation.
#
# Usage:
#   ./scripts/package-release-asset.sh <binary> <platform> <version> <outdir>
#     e.g. ./scripts/package-release-asset.sh result/bin/mobee linux-x64 0.1.0 dist
#
# Writes <outdir>/mobee-<version>-<platform>.tar.gz and prints its path.

set -euo pipefail

BINARY="${1:-}"
PLATFORM="${2:-}"
VERSION="${3:-}"
OUTDIR="${4:-}"

die() { echo "package-release-asset: $*" >&2; exit 1; }

[ -n "$BINARY" ] && [ -n "$PLATFORM" ] && [ -n "$VERSION" ] && [ -n "$OUTDIR" ] \
    || die "usage: package-release-asset.sh <binary> <platform> <version> <outdir>"
[ -f "$BINARY" ] || die "no binary at $BINARY"
[ -f LICENSE-MIT ] && [ -f LICENSE-APACHE ] \
    || die "run from the repo root — LICENSE-MIT and LICENSE-APACHE must both be present"

mkdir -p "$OUTDIR"
OUTDIR="$(cd "$OUTDIR" && pwd)"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
ROOT="$STAGE/mobee-$VERSION-$PLATFORM"
mkdir -p "$ROOT"

# `cp -L` because a nix build output is a symlink into the store: archiving the link would produce a
# tarball containing a dangling path instead of an executable.
cp -L "$BINARY" "$ROOT/mobee"
chmod 755 "$ROOT/mobee"
cp LICENSE-MIT LICENSE-APACHE "$ROOT/"

TARBALL="$OUTDIR/mobee-$VERSION-$PLATFORM.tar.gz"

# Fixed uid/gid/mtime so the archive is a function of its contents. Without this, two archives built
# from identical inputs differ, and any reproducibility comparison downstream compares timestamps
# instead of code.
tar --sort=name \
    --owner=0 --group=0 --numeric-owner \
    --mtime='UTC 1970-01-01' \
    -czf "$TARBALL" \
    -C "$STAGE" "mobee-$VERSION-$PLATFORM"

# Assert the archive actually holds all three files. A staging slip produces a well-formed tarball
# that is simply missing something, and the licence omission is the one nobody notices until it
# matters.
#
# The listing is read into a variable first: under `set -o pipefail`, `tar … | grep -q` FAILS on a
# successful match, because grep exits at the first hit and tar then dies of SIGPIPE.
LISTING="$(tar -tzf "$TARBALL")"
for entry in mobee LICENSE-MIT LICENSE-APACHE; do
    grep -qx "mobee-$VERSION-$PLATFORM/$entry" <<<"$LISTING" \
        || die "$TARBALL is missing $entry"
done

echo "ok: $(basename "$TARBALL") carries mobee, LICENSE-MIT, LICENSE-APACHE"
echo "$TARBALL"
