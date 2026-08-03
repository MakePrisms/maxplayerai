#!/usr/bin/env bash
#
# Assert the `[package.metadata.binstall]` templates resolve to assets a real release actually
# publishes — and to the real path inside them.
#
# Usage:
#   ./scripts/verify-binstall-metadata.sh <version> [path-to-manifest]
#     e.g. ./scripts/verify-binstall-metadata.sh 0.0.1
#
# ── What makes this more than a spell-check ─────────────────────────────────────────────────────
# The failure mode of binstall metadata is silent: a wrong `pkg-url` 404s and binstall quietly falls
# back to compiling from source, so the user gets a working install and nobody learns the metadata is
# broken. A wrong `bin-dir` is worse — it fails only on the machines that took the download path.
#
# So neither field is checked against a hand-written expectation. Both are resolved against the
# published release itself:
#
#   pkg-url → compared to the asset names in the release's own SHA256SUMS
#   bin-dir → compared to the member list of the DOWNLOADED tarball (`tar -tzf`)
#
# ★ `bin-dir` is checked against the archive rather than against `package-release-asset.sh`. Reading
#   the packaging script would only prove the two files agree with each other; both can be wrong
#   together, and the thing binstall opens is the archive.
#
# ── Why the manifest is read with a header tracker, not a TOML parser ──────────────────────────
# There is no TOML parser on this box that does not involve running cargo, and hand-rolling one for a
# security-adjacent check trades a real dependency for a fake parser. What this does instead is track
# the current `[table]` header line by line and take the `key = "value"` lines that fall under a
# `package.metadata.binstall` table — which is not a parse of TOML, but it is an honest read of the
# only shape these tables are ever written in, and it ignores comments explicitly.

set -euo pipefail

VERSION=""
MANIFEST=""
# Targets whose asset this particular release is known not to publish. Nothing is skipped silently:
# an override whose asset is absent FAILS unless the caller names the target here, and naming it
# prints a loud UNVERIFIED line instead of an ok. See the waiver rules below.
UNRELEASED=""

REPO_HOST_PATH="github.com/MakePrisms/maxplayerai"
# The supported platform set, which must be the same one install.sh enforces. Keeping the two in step
# is the point: a target released here but refused there (or the reverse) is a user finding out from a
# 404 which install paths were really meant to work.
#
# `x86_64-apple-darwin` is deliberately absent — the matrix builds Apple Silicon only, and install.sh
# refuses an Intel mac for exactly that reason.
SUPPORTED_TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin)

die() { echo "verify-binstall-metadata: $*" >&2; exit 1; }
usage() {
    echo "usage: verify-binstall-metadata.sh <version> [path-to-manifest] [--unreleased <target>[,<target>…]]" >&2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --unreleased)
            shift; [ $# -gt 0 ] || { usage; die "--unreleased needs a target"; }
            UNRELEASED="$UNRELEASED $(printf '%s' "$1" | tr ',' ' ')" ;;
        --unreleased=*)
            UNRELEASED="$UNRELEASED $(printf '%s' "${1#--unreleased=}" | tr ',' ' ')" ;;
        -h | --help) usage; exit 0 ;;
        -*) usage; die "unknown option '$1'" ;;
        *)
            if [ -z "$VERSION" ]; then VERSION="$1"
            elif [ -z "$MANIFEST" ]; then MANIFEST="$1"
            else usage; die "unexpected argument '$1'"
            fi ;;
    esac
    shift
done

[ -n "$VERSION" ] || { usage; die "a version is required"; }
[ -n "$MANIFEST" ] || MANIFEST="crates/mobee/Cargo.toml"
case "$VERSION" in
    v*) die "pass the version without a leading 'v' (got '$VERSION')" ;;
esac
[ -f "$MANIFEST" ] || die "no manifest at $MANIFEST"

is_waived() {
    case " $UNRELEASED " in *" $1 "*) return 0 ;; esac
    return 1
}

# A waiver naming something outside the supported set is a typo, and a typo'd waiver waives nothing
# while looking like it waived something.
for t in $UNRELEASED; do
    found=0
    for supported in "${SUPPORTED_TARGETS[@]}"; do
        [ "$t" != "$supported" ] || found=1
    done
    [ "$found" -eq 1 ] || die "--unreleased names '$t', which is not in the supported set (${SUPPORTED_TARGETS[*]})"
done

# Fail closed on tools rather than skipping a check.
for t in curl tar awk; do
    command -v "$t" >/dev/null 2>&1 || die "$t not found — needed to resolve the templates against the real release"
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# ── Read the binstall tables ────────────────────────────────────────────────────────────────────
# Emits one `<table>|<key>|<value>` line per setting. Comments are dropped before anything else, so a
# commented-out pkg-url can never be read as live configuration.
read_binstall() {
    awk '
        { line = $0 }
        # A leading-# line is a comment outright. (Trailing comments after a quoted value are not
        # stripped: none are used here, and guessing at where a # is inside a string is how a reader
        # like this starts lying.)
        line ~ /^[[:space:]]*#/ { next }
        line ~ /^[[:space:]]*\[/ {
            table = line
            sub(/^[[:space:]]*\[/, "", table)
            sub(/\][[:space:]]*$/, "", table)
            next
        }
        table ~ /^package\.metadata\.binstall/ && line ~ /^[[:space:]]*[a-z-]+[[:space:]]*=/ {
            key = line; sub(/[[:space:]]*=.*$/, "", key); gsub(/[[:space:]]/, "", key)
            val = line; sub(/^[^=]*=[[:space:]]*/, "", val)
            gsub(/^"|"[[:space:]]*$/, "", val)
            print table "|" key "|" val
        }
    ' "$1"
}

SETTINGS="$(read_binstall "$MANIFEST")"
[ -n "$SETTINGS" ] || die "$MANIFEST declares no [package.metadata.binstall] settings at all"

# Substitute the templates. `{ version }` is written with the spaces binstall's own docs use, but the
# form without them is equally valid, so both are accepted — otherwise a legitimate edit to the
# manifest would make this check silently stop matching and report nothing.
expand() {
    printf '%s' "$1" | sed -e "s/{[[:space:]]*version[[:space:]]*}/$VERSION/g"
}

# ── pkg-fmt ─────────────────────────────────────────────────────────────────────────────────────
fmt="$(printf '%s\n' "$SETTINGS" | awk -F'|' '$1 == "package.metadata.binstall" && $2 == "pkg-fmt" { print $3 }')"
[ "$fmt" = tgz ] || die "pkg-fmt is '${fmt:-<absent>}', but the release publishes .tar.gz — expected tgz"
echo "ok: pkg-fmt = tgz"

# ── Ground truth: what the release actually published ────────────────────────────────────────────
# SHA256SUMS is the release's own statement of its assets, so the asset names come from the release
# rather than from this script's idea of them.
sums_url="https://$REPO_HOST_PATH/releases/download/v$VERSION/SHA256SUMS"
curl -fsSL --proto '=https' -o "$WORK/SHA256SUMS" "$sums_url" \
    || die "could not fetch $sums_url — is v$VERSION published?"
awk '{ n = $2; sub(/^\*/, "", n); if (n != "") print n }' "$WORK/SHA256SUMS" > "$WORK/published"
[ -s "$WORK/published" ] || die "SHA256SUMS for v$VERSION lists no assets"
echo "ok: v$VERSION publishes:"
sed 's/^/     /' "$WORK/published"

# ── Every supported target has both settings, and they resolve ───────────────────────────────────
# Two outcomes are possible per target and they are NOT the same result:
#
#   VERIFIED   — the asset is in this release's SHA256SUMS, downloaded, and bin-dir is a real member.
#   UNVERIFIED — this release publishes no such asset. Permitted only when the caller named the
#                target in --unreleased, and reported as UNVERIFIED, never as ok.
#
# ★ The reason for the waiver rather than a silent skip: `aarch64-apple-darwin` is real and supported,
#   but v0.0.1 was cut from a linux-only matrix, so there is no darwin asset to resolve against yet. A
#   check that quietly passed on the absent one would report three verified targets while having
#   touched two — the shape where a skip is indistinguishable from a pass.
verified=0
unverified=""
for target in "${SUPPORTED_TARGETS[@]}"; do
    table="package.metadata.binstall.overrides.$target"

    pkg_url="$(printf '%s\n' "$SETTINGS" | awk -F'|' -v t="$table" '$1 == t && $2 == "pkg-url" { print $3 }')"
    bin_dir="$(printf '%s\n' "$SETTINGS" | awk -F'|' -v t="$table" '$1 == t && $2 == "bin-dir" { print $3 }')"

    [ -n "$pkg_url" ] || die "no pkg-url for $target — binstall would fall back to building from source on that target"
    [ -n "$bin_dir" ] || die "no bin-dir for $target — binstall cannot find the executable inside the archive"

    url="$(expand "$pkg_url")"
    dir="$(expand "$bin_dir")"

    # An unexpanded brace means a template variable this release-time substitution does not know
    # about. Left alone it would reach binstall, which may or may not know it — and if it does not,
    # the URL 404s and binstall silently compiles from source instead.
    case "$url$dir" in
        *'{'* | *'}'*) die "$target: a template variable survived substitution: url='$url' bin-dir='$dir'" ;;
    esac

    # The host, positively. `{ repo }` would expand to the manifest's `repository` — the buzzrelay
    # mirror, which publishes no release assets — and any other host drift is the same class of bug.
    case "$url" in
        "https://$REPO_HOST_PATH/releases/download/v$VERSION/"*) ;;
        *) die "$target: pkg-url resolves to '$url', which is not a v$VERSION release asset URL on $REPO_HOST_PATH" ;;
    esac

    asset="${url##*/}"

    if ! grep -qxF "$asset" "$WORK/published"; then
        # Absent from the release. Only a named waiver makes this anything other than a failure.
        is_waived "$target" \
            || die "$target: pkg-url resolves to '$asset', which v$VERSION does not publish. It publishes: $(tr '\n' ' ' < "$WORK/published"). If this release genuinely predates that platform, say so explicitly: --unreleased $target"
        echo "⚠ UNVERIFIED: $target"
        echo "     pkg-url -> $asset — v$VERSION publishes NO such asset (waived via --unreleased)"
        echo "     bin-dir -> $dir — NOT checked against any archive; nothing here proves it"
        unverified="$unverified $target"
        continue
    fi

    # A waiver for a target the release DOES publish is stale, and a stale waiver is permanent
    # blindness: it would keep excusing a real failure long after the reason expired. Fail so the
    # waiver has to be removed on the first release that carries the asset.
    if is_waived "$target"; then
        die "$target: --unreleased says v$VERSION has no '$asset', but the release publishes it — drop the waiver so this target is actually checked"
    fi

    # ── bin-dir against the real archive ────────────────────────────────────────────────────────
    curl -fsSL --proto '=https' -o "$WORK/$asset" "$url" \
        || die "$target: pkg-url '$url' could not be downloaded"
    tar -tzf "$WORK/$asset" > "$WORK/$asset.list" \
        || die "$target: $asset is not a readable gzip tarball"
    grep -qxF "$dir" "$WORK/$asset.list" \
        || die "$target: bin-dir resolves to '$dir', which is not a member of $asset. Members: $(tr '\n' ' ' < "$WORK/$asset.list")"

    echo "ok: $target"
    echo "     pkg-url -> $asset (published)"
    echo "     bin-dir -> $dir (present in the archive)"
    verified=$((verified + 1))
done

# ── Nothing is configured for a target we do not release ─────────────────────────────────────────
# An override for an unreleased target would point binstall at an asset that does not exist, and the
# resulting 404 is invisible — binstall just compiles from source. The check is a set comparison so
# that ADDING a target without releasing it fails here, rather than at some user's terminal.
declared="$(printf '%s\n' "$SETTINGS" \
    | awk -F'|' '$1 ~ /^package\.metadata\.binstall\.overrides\./ { sub(/^package\.metadata\.binstall\.overrides\./, "", $1); print $1 }' \
    | sort -u)"
for target in $declared; do
    found=0
    for supported in "${SUPPORTED_TARGETS[@]}"; do
        [ "$target" != "$supported" ] || found=1
    done
    [ "$found" -eq 1 ] \
        || die "the manifest declares an override for $target, which is not in this release's platform set (${SUPPORTED_TARGETS[*]}) — binstall would 404 and silently build from source"
done
echo "ok: overrides cover exactly ${SUPPORTED_TARGETS[*]}"

# ── The denominator, always ─────────────────────────────────────────────────────────────────────
# The verdict states how many targets were actually resolved against the release, out of how many
# exist. A bare "PASS" over a waived target would be the claim this whole waiver mechanism exists to
# avoid making.
total="${#SUPPORTED_TARGETS[@]}"
if [ -n "$unverified" ]; then
    echo "PARTIAL: $verified/$total targets resolved against v$VERSION assets that exist, at paths inside them."
    echo "         NOT verified (v$VERSION publishes no asset for them):$unverified"
    echo "         Their templates are shape-checked only — re-run against a release that carries them."
else
    echo "PASS: all $total targets resolve to v$VERSION assets that exist, at paths that exist inside them"
fi
