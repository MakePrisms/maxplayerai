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
#   ./scripts/package-release-asset.sh <binary> <platform> <version> <outdir> [name]
#     e.g. ./scripts/package-release-asset.sh result/bin/maxplayer linux-x64 0.1.0 dist
#
# Writes <outdir>/<name>-<version>-<platform>.tar.gz and prints its path.
#
# `name` defaults to `maxplayer` and names the ARCHIVE — the tarball and the directory inside it.
# The release passes nothing and takes that default: since #510 there is one shipped asset stem, and
# naming it at the call site would only add a second place for it to drift. The argument stays for
# local and experimental packaging, where an archive built beside a real one needs a name of its own.
#
# The executable inside is named after the binary being packaged, never after the archive. A binary
# must answer to the name it is invoked by — `verify-release-version.sh` holds every artifact to
# that — and `maxplayer` reports `maxplayer <version>` whichever feature set it was built with. An
# archive-derived name would ship a command that disagrees with its own `version` output.

set -euo pipefail

BINARY="${1:-}"
PLATFORM="${2:-}"
VERSION="${3:-}"
OUTDIR="${4:-}"
# `${5-...}` and not `${5:-...}`: the colon form would also substitute the default for an argument
# that was passed but EMPTY, which is how an unset caller-side variable arrives. Defaulting there
# would quietly produce a `maxplayer`-named archive for a caller that believed it had named
# something else.
# Omitted means default; empty means the caller is broken, and the check below says so.
NAME="${5-maxplayer}"

die() { echo "package-release-asset: $*" >&2; exit 1; }

[ -n "$BINARY" ] && [ -n "$PLATFORM" ] && [ -n "$VERSION" ] && [ -n "$OUTDIR" ] \
    || die "usage: package-release-asset.sh <binary> <platform> <version> <outdir> [name]"
case "$NAME" in
    # The name lands in a filesystem path and a tar member, so a slash or an empty value would
    # silently write outside the staging directory rather than fail.
    ""|*/*) die "name must be non-empty and contain no slash (got '$NAME')" ;;
esac
[ -f "$BINARY" ] || die "no binary at $BINARY"
[ -f LICENSE-MIT ] && [ -f LICENSE-APACHE ] \
    || die "run from the repo root — LICENSE-MIT and LICENSE-APACHE must both be present"

mkdir -p "$OUTDIR"
OUTDIR="$(cd "$OUTDIR" && pwd)"

BIN="$(basename "$BINARY")"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
ROOT="$STAGE/$NAME-$VERSION-$PLATFORM"
mkdir -p "$ROOT"

# `cp -L` because a nix build output is a symlink into the store: archiving the link would produce a
# tarball containing a dangling path instead of an executable.
cp -L "$BINARY" "$ROOT/$BIN"
chmod 755 "$ROOT/$BIN"
cp LICENSE-MIT LICENSE-APACHE "$ROOT/"

TARBALL="$OUTDIR/$NAME-$VERSION-$PLATFORM.tar.gz"

# Fixed uid/gid/mtime so the archive is a function of its contents. Without this, two archives built
# from identical inputs differ, and any reproducibility comparison downstream compares timestamps
# instead of code.
#
# Every flag below is GNU tar's. macOS ships BSD tar, which rejects `--sort` outright and spells the
# ownership options differently, so `$TAR` lets the caller name a GNU tar (`gtar`) on platforms whose
# default is not one. The archive format stays identical everywhere as a result — branching the FLAGS
# per platform would instead leave each platform with its own archive semantics, which reads as a fix
# while quietly reducing the determinism guarantee to same-platform-only.
TAR="${TAR:-tar}"

# Compression is a separate step, and `gzip -n` is the reason: gzip writes the source name and a
# timestamp into its header, and `tar -z` delegates to whichever gzip is on PATH, so the archive's
# determinism silently depended on that implementation choosing to omit them. GNU gzip does; the one
# on the macOS runner does not, which made darwin archives differ between two runs over identical
# inputs while linux archives matched. `-n` states the requirement instead of inheriting it.
"$TAR" --sort=name \
    --owner=0 --group=0 --numeric-owner \
    --mtime='UTC 1970-01-01' \
    -cf - \
    -C "$STAGE" "$NAME-$VERSION-$PLATFORM" \
    | gzip -n > "$TARBALL"

# Assert the archive actually holds all three files. A staging slip produces a well-formed tarball
# that is simply missing something, and the licence omission is the one nobody notices until it
# matters.
#
# The listing is read into a variable first: under `set -o pipefail`, `tar … | grep -q` FAILS on a
# successful match, because grep exits at the first hit and tar then dies of SIGPIPE.
LISTING="$("$TAR" -tzf "$TARBALL")"
for entry in "$BIN" LICENSE-MIT LICENSE-APACHE; do
    grep -qx "$NAME-$VERSION-$PLATFORM/$entry" <<<"$LISTING" \
        || die "$TARBALL is missing $entry"
done

echo "ok: $(basename "$TARBALL") carries $BIN, LICENSE-MIT, LICENSE-APACHE"
echo "$TARBALL"
