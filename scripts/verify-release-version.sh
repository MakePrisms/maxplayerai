#!/usr/bin/env bash
#
# Assert that ONE version is stated everywhere a release states a version.
#
# A release tag is the only input a human supplies, and nothing else moves with it automatically.
# Tag `v0.2.0` against a tree still saying `0.1.0` and the failure is quiet in the worst way: the
# GitHub Release is labelled 0.2.0 while npm publishes 0.1.0 — either colliding with the version
# already on the registry (the publish fails late, after the Release exists) or, once a 0.2.0 tag is
# re-cut, shipping a tarball whose contents belong to a different commit than its name suggests.
#
# The `optionalDependencies` pin is the subtlest of the four. The launcher package resolves its
# payload by exact version, so a pin left behind means `mobee@0.2.0` installs the 0.1.0 binary — an
# install that succeeds and silently runs last release's code.
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
    | node -e 'const m=JSON.parse(require("fs").readFileSync(0,"utf8"));const p=m.packages.find(p=>p.name==="mobee");if(!p){process.stderr.write("no mobee package in cargo metadata\n");process.exit(1)}process.stdout.write(p.version)')"

[ "$CRATE_VERSION" = "$VERSION" ] \
    || die "crate mobee is $CRATE_VERSION but the release is $VERSION — bump [workspace.package] version in Cargo.toml before tagging"
echo "ok: crate mobee $CRATE_VERSION"

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
if (names.length === 0) { console.error("npm/mobee/package.json declares no optionalDependencies — the launcher has no payload to resolve"); process.exit(1); }
const wrong = names.filter((n) => deps[n] !== process.argv[2]);
if (wrong.length) { console.error("stale payload pins: " + wrong.map((n) => n + "@" + deps[n]).join(", ")); process.exit(1); }
process.stdout.write(names.join(", "));
' "$PWD/npm/mobee/package.json" "$VERSION")" \
    || die "npm/mobee/package.json payload pins are not all $VERSION"
echo "ok: payload pins at $VERSION -> $PIN_REPORT"

# ── The built binary ────────────────────────────────────────────────────────────────────────────
# The only check that looks at the artifact instead of the tree. It catches the case the others
# structurally cannot: an asset carried over from an earlier build.
if [ -n "$BINARY" ]; then
    [ -x "$BINARY" ] || die "no executable at $BINARY"
    # The expected name is the name this artifact SHIPS AS — the basename of the path the workflow
    # built and is about to package — rather than a literal. Derived for two reasons. The binary name
    # and the crate name are deliberately different (`[[bin]] maxplayer` inside package `mobee`), so a
    # literal here silently commits to one of them and goes stale the moment either moves; that is
    # exactly how this check came to expect `mobee` from a binary that correctly answers to
    # `maxplayer`. And an asset published under a name it does not answer to is the same hazard the
    # constructed asset filename guards against upstream — a runner shipped under the racer's name
    # would install, run, and be the wrong program. A binary must announce the name it is invoked by.
    SHIPPED_AS="$(basename "$BINARY")"
    EXPECTED="$SHIPPED_AS $VERSION"
    # Both forms, `--version` first. They are separate dispatch arms, so a check that asks only the
    # subcommand measures the path next to the one a stranger takes: it passes an artifact whose
    # `--version` prints usage and exits 1, which is the state this repo actually shipped in. Asking
    # the flag alone would trade one blind spot for the other, so assert that both answer, and that
    # they answer the same thing.
    for form in --version version; do
        reported="$("$BINARY" "$form")" \
            || die "$BINARY $form did not report a version — every release artifact must answer both '$SHIPPED_AS --version' and '$SHIPPED_AS version'"
        [ "$reported" = "$EXPECTED" ] \
            || die "$BINARY $form reports '$reported', expected '$EXPECTED' — this artifact is not the one this release is packaging"
        echo "ok: artifact reports $reported ($form)"
    done
fi

echo "PASS: everything states $VERSION"
