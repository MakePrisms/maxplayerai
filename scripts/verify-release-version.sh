#!/usr/bin/env bash
#
# Assert that ONE version is stated everywhere a release states a version, and that the artifact
# names the COMMIT it was built from.
#
# A release tag is the only input a human supplies, and nothing else moves with it automatically.
# Tag `v0.2.0` against a tree still saying `0.1.0` and the failure is quiet in the worst way: the
# GitHub Release is labelled 0.2.0 while npm publishes 0.1.0 — either colliding with the version
# already on the registry (the publish fails late, after the Release exists) or, once a 0.2.0 tag is
# re-cut, shipping a tarball whose contents belong to a different commit than its name suggests.
#
# The `optionalDependencies` pin is the subtlest of the four. The launcher package resolves its
# payload by exact version, so a pin left behind means `maxplayer@0.2.0` installs the 0.1.0 binary — an
# install that succeeds and silently runs last release's code.
#
# ── The build stamp (#818) ───────────────────────────────────────────────────────────────────────
# A version alone does not identify a build. #818 measured a deployed seat whose commit could only be
# recovered by bracketing its crate version against the window that produced it — a method that
# worked because no commit happened to land in the gap, and which returns several candidates or none
# the moment two releases are cut in a day. So the artifact now prints
# `maxplayer <version> (<40-hex commit sha>)`, and the check below is the issue's own acceptance
# predicate turned into a release gate: the sha must RESOLVE (`git cat-file -t` says `commit`) and
# must be the commit this tree is at. The resolution half is the load-bearing one — #818 documents a
# binary carrying a plausible 40-hex string that was an object in no repository, which looks fixed
# and is not — and it is why `(unknown)` fails a release closed here even though the binary is
# allowed to print it from a build path that genuinely had no git to read.
#
# Usage:
#   ./scripts/verify-release-version.sh <version> [path-to-binary]
#     e.g. ./scripts/verify-release-version.sh 0.1.0 result/bin/maxplayer
#
# Pass the version WITHOUT a leading `v` — the workflow strips it from the tag.

set -euo pipefail

VERSION="${1:-}"
BINARY="${2:-}"

die() { echo "verify-release-version: $*" >&2; exit 1; }

[ -n "$VERSION" ] || die "usage: verify-release-version.sh <version> [path-to-binary]"
case "$VERSION" in
    v*) die "pass the version without a leading 'v' (got '$VERSION')" ;;
esac
[ -f Cargo.toml ] || die "run from the repo root (Cargo.toml missing)"

# Fail closed on the tools rather than skipping a check: a version check that silently did not run
# is indistinguishable from one that passed.
command -v cargo >/dev/null 2>&1 || die "cargo not found — needed to read the workspace version"
command -v node  >/dev/null 2>&1 || die "node not found — needed to read the npm manifests"

# ── The crate version ───────────────────────────────────────────────────────────────────────────
# Read through cargo rather than grepping Cargo.toml: the crates inherit
# `version.workspace = true`, so the literal lives in one section and a grep would also match
# `[workspace.dependencies]` entries. `--no-deps` keeps this local and offline.
CRATE_VERSION="$(cargo metadata --no-deps --format-version 1 \
    | node -e 'const m=JSON.parse(require("fs").readFileSync(0,"utf8"));const p=m.packages.find(p=>p.name==="maxplayer");if(!p){process.stderr.write("no maxplayer package in cargo metadata\n");process.exit(1)}process.stdout.write(p.version)')"

[ "$CRATE_VERSION" = "$VERSION" ] \
    || die "crate maxplayer is $CRATE_VERSION but the release is $VERSION — bump [workspace.package] version in Cargo.toml before tagging"
echo "ok: crate maxplayer $CRATE_VERSION"

# ── The npm manifests ───────────────────────────────────────────────────────────────────────────
# Every directory under npm/ is checked, including any not yet wired into a build, so a package
# added ahead of its platform cannot drift out of step unnoticed.
for manifest in npm/*/package.json; do
    declared="$(node -e 'process.stdout.write(String(require(process.argv[1]).version))' "$PWD/$manifest")"
    [ "$declared" = "$VERSION" ] \
        || die "$manifest is version $declared, expected $VERSION"
done
echo "ok: every npm package is version $VERSION"

# ── The payload pins ────────────────────────────────────────────────────────────────────────────
# Asserted as an exact match, not a range: the launcher must resolve the payload built from THIS
# commit. A caret or a stale pin both resolve to something else.
PIN_REPORT="$(node -e '
const pkg = require(process.argv[1]);
const deps = pkg.optionalDependencies || {};
const names = Object.keys(deps);
if (names.length === 0) { console.error("npm/maxplayer/package.json declares no optionalDependencies — the launcher has no payload to resolve"); process.exit(1); }
const wrong = names.filter((n) => deps[n] !== process.argv[2]);
if (wrong.length) { console.error("stale payload pins: " + wrong.map((n) => n + "@" + deps[n]).join(", ")); process.exit(1); }
process.stdout.write(names.join(", "));
' "$PWD/npm/maxplayer/package.json" "$VERSION")" \
    || die "npm/maxplayer/package.json payload pins are not all $VERSION"
echo "ok: payload pins at $VERSION -> $PIN_REPORT"

# ── The release notes ───────────────────────────────────────────────────────────────────────────
# RELEASE_NOTES.md is version-specific prose living at a permanent path, and the release job passes
# it to `--notes-file` unconditionally. Absence already fails closed there — `gh` errors and no
# Release is created — but staleness had no guard at all: an unrewritten file publishes the PREVIOUS
# release's notes under this release's title, green and silent, and a Release cannot be un-published.
#
# The reason it survived unnoticed is that it does not misfire on the next release. A stale note
# stays accidentally true for as long as the limitation it describes is unfixed, so every look at it
# is reassuring. The first release the file is WRONG for is the release that FIXES the limitation —
# which would ship "not supported" while announcing support.
#
# Checked here rather than beside `--notes-file` because CI runs this script on every pull request
# with the crate version, so a version bump that forgets the notes fails on the bump PR, before a tag
# exists. At `--notes-file` the earliest possible failure is after the tag has been pushed.
#
# Dots are escaped so they cannot match any character, and the digit boundaries keep 0.5.1 from being
# satisfied by a file that only mentions 0.5.10.
[ -f RELEASE_NOTES.md ] \
    || die "RELEASE_NOTES.md is missing — the release job passes it to --notes-file and would fail after the tag exists"
VERSION_RE="$(printf '%s' "$VERSION" | sed 's/\./\\./g')"
grep -qE "(^|[^0-9.])${VERSION_RE}([^0-9]|$)" RELEASE_NOTES.md \
    || die "RELEASE_NOTES.md does not name $VERSION — rewrite it for this release; it still describes the previous one"
echo "ok: RELEASE_NOTES.md names $VERSION"

# ── The built binary ────────────────────────────────────────────────────────────────────────────
# The only check that looks at the artifact instead of the tree. It catches the case the others
# structurally cannot: an asset carried over from an earlier build.
if [ -n "$BINARY" ]; then
    [ -x "$BINARY" ] || die "no executable at $BINARY"
    # The expected name is the name this artifact SHIPS AS — the basename of the path the workflow
    # built and is about to package — rather than a literal. Derived for two reasons. The binary name
    # and the crate name are deliberately different (`[[bin]] maxplayer` inside package `maxplayer`), so a
    # literal here silently commits to one of them and goes stale the moment either moves; that is
    # exactly how this check came to expect `maxplayer` from a binary that correctly answers to
    # `maxplayer`. And an asset published under a name it does not answer to is the same hazard the
    # constructed asset filename guards against upstream — a runner shipped under the racer's name
    # would install, run, and be the wrong program. A binary must announce the name it is invoked by.
    SHIPPED_AS="$(basename "$BINARY")"
    # Both forms, `--version` first. They are separate dispatch arms, so a check that asks only the
    # subcommand measures the path next to the one a stranger takes: it passes an artifact whose
    # `--version` prints usage and exits 1, which is the state this repo actually shipped in. Asking
    # the flag alone would trade one blind spot for the other, so assert that both answer, and that
    # they answer the same thing.
    #
    # `<version> (<stamp>)` since #818. The stamp is the whole parenthesised field: a 40-lowercase-hex
    # commit sha, optionally followed by a space and the commit timestamp when the build path knew
    # it. Matched as that grammar rather than as a prefix, so a suffix glued straight onto the sha —
    # `<sha>-dirty`, which is what nix hands a flake built from an uncommitted tree — is a failure
    # here rather than something that slips through on its first forty characters. A release is cut
    # from a committed tree or it is not a release.
    reported_first=""
    for form in --version version; do
        reported="$("$BINARY" "$form")" \
            || die "$BINARY $form did not report a version — every release artifact must answer both '$SHIPPED_AS --version' and '$SHIPPED_AS version'"
        if [ -z "$reported_first" ]; then
            reported_first="$reported"
        elif [ "$reported" != "$reported_first" ]; then
            die "$BINARY --version reports '$reported_first' but $BINARY $form reports '$reported' — the two dispatch arms disagree, so one of them is not the line this release ships"
        fi
        # The name/version half stays an exact string comparison — a regex over `$VERSION` would let
        # its dots match any character — and only the parenthesised stamp is matched as a pattern.
        # The pattern lives in a variable: bash's `[[ =~ ]]` parses an inline `)` as the end of the
        # conditional, so an unquoted regex carrying one is a syntax error, not a match failure.
        PREFIX="$SHIPPED_AS $VERSION "
        STAMP_RE='^\(([0-9a-f]{40})( [^)]*)?\)$'
        stamp="${reported#"$PREFIX"}"
        [ "$stamp" != "$reported" ] && [[ "$stamp" =~ $STAMP_RE ]] \
            || die "$BINARY $form reports '$reported', expected '$SHIPPED_AS $VERSION (<40-hex commit sha>)' — either this artifact is not the one this release is packaging, or it carries no build stamp (#818)"
        stamp_sha="${BASH_REMATCH[1]}"
        echo "ok: artifact reports $reported ($form)"
    done

    # The half that cannot be faked by the format alone, and the reason #818 was filed: a sha that is
    # an object in NO repository reads exactly like one that is. `git` is a build/CI tool here, not a
    # product path, so using it is fine — issue #55's ban is on the shipped binary shelling out.
    command -v git >/dev/null 2>&1 \
        || die "git not found — needed to resolve the artifact's build stamp; a stamp that was never resolved is indistinguishable from one that does not resolve"
    git rev-parse --git-dir >/dev/null 2>&1 \
        || die "not inside a git repository — the artifact's build stamp cannot be resolved here"
    [ "$(git cat-file -t "$stamp_sha" 2>/dev/null || true)" = commit ] \
        || die "the artifact's build stamp $stamp_sha is not a commit in this repository — this is exactly the #818 failure: a plausible sha that resolves to nothing looks fixed and identifies no build"
    head_sha="$(git rev-parse HEAD)"
    [ "$stamp_sha" = "$head_sha" ] \
        || die "the artifact was built from $stamp_sha but this tree is at $head_sha — the binary and the source being packaged are different commits"
    echo "ok: build stamp $stamp_sha resolves to a commit and is this tree's HEAD"
fi

echo "PASS: everything states $VERSION"
