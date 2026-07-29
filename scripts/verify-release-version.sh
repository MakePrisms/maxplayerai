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
#     e.g. ./scripts/verify-release-version.sh 0.1.0 result/bin/mobee
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
    reported="$("$BINARY" version)" || die "$BINARY failed to report its version"
    [ "$reported" = "mobee $VERSION" ] \
        || die "$BINARY reports '$reported', expected 'mobee $VERSION' — this artifact was built from a different tree"
    echo "ok: artifact reports $reported"
fi

echo "PASS: everything states $VERSION"
