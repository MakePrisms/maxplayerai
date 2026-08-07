#!/usr/bin/env bash
#
# Assert the release surface is ONE platform set, not several that happen to agree by hand.
#
# #249: the set of platforms a release ships was written out independently in the build matrix, in
# RELEASE_PLATFORMS, in the launcher's pinned dependencies and in each payload package. Nothing tied
# them together, so they drifted — a platform could be built and never published (that was #446), or
# pinned by the launcher with no payload behind it — and a dry run could not see any of it, because
# the only cross-check lived in the tag-gated publish job, after `gh release create`.
#
# The set now has ONE source: .github/release-platforms.json. The workflow DERIVES its build matrix
# and RELEASE_PLATFORMS from it (via the plan job). The copies npm needs to exist statically — the
# payload packages under npm/cli-*, the launcher's optionalDependencies, the launcher's runtime
# resolver — cannot read a workflow output, so this script holds each of them to that same file. It
# runs in the verify-surface job on every build, a dry run included, and gates release and publish.
#
# What is proved, and why each is a failure the others cannot see:
#   - the plan's derived platform list matches the source (so the matrix and RELEASE_PLATFORMS,
#     which are built from that list, cannot silently derive a different set than the file names);
#   - the built artifacts match the source (a build job that failed for one platform still lets the
#     others upload — fail-fast is off — so "some artifacts arrived" must not read as complete);
#   - the npm payload directories match the source (a payload with no platform, or a platform with
#     no payload, is the #446 shape);
#   - the launcher's pins AND its runtime resolver match the source (a pin with no payload installs
#     nothing; a resolver missing a published platform refuses a binary that exists).
#
# Set equality is reported in BOTH directions, by name: a count cannot tell "missing linux-arm64"
# from "shipped a stray freebsd-x64", and #249 is exactly such a mismatch.
#
# Usage:
#   ./scripts/verify-release-surface.sh <dist-dir> <version> [space-separated plan platforms]
#   ./scripts/verify-release-surface.sh --no-artifacts <version> [plan platforms]
#
# `--no-artifacts` in place of the dist dir skips only the built-artifact check, so the tree-side
# surfaces can be held to the source on an ordinary pull request (ci.yml), where nothing is built.

set -euo pipefail

DIST="${1:-}"
VERSION="${2:-}"
PLAN_PLATFORMS="${3:-}"

MANIFEST=".github/release-platforms.json"
LAUNCHER_PKG="npm/maxplayer/package.json"
LAUNCHER_JS="npm/maxplayer/bin/maxplayer.js"

die() { echo "verify-release-surface: $*" >&2; exit 1; }

[ -n "$DIST" ] || die "usage: verify-release-surface.sh <dist-dir|--no-artifacts> <version> [plan platforms]"
# The version only names the artifacts the built-artifact check looks for, so it is required only
# when that check runs. --no-artifacts (the tree-only ci run) needs none.
if [ "$DIST" != "--no-artifacts" ]; then
  [ -n "$VERSION" ] || die "no version given"
fi
command -v node >/dev/null 2>&1 || die "node not found"
[ -f "$MANIFEST" ]     || die "no platform source at $MANIFEST"
[ -f "$LAUNCHER_PKG" ] || die "no launcher manifest at $LAUNCHER_PKG"
[ -f "$LAUNCHER_JS" ]  || die "no launcher at $LAUNCHER_JS"

# Compare two sets given as newline-separated lists; name what is missing and what is extra. Both
# inputs are sorted, de-duplicated and stripped of blank lines here, so callers pass raw lists.
assert_set_eq() {
  local label="$1" expected="$2" actual="$3" missing extra
  missing="$(comm -23 <(printf '%s\n' "$expected" | grep -v '^$' | sort -u) \
                      <(printf '%s\n' "$actual"   | grep -v '^$' | sort -u))"
  extra="$(comm -13 <(printf '%s\n' "$expected" | grep -v '^$' | sort -u) \
                    <(printf '%s\n' "$actual"   | grep -v '^$' | sort -u))"
  if [ -n "$missing" ] || [ -n "$extra" ]; then
    [ -n "$missing" ] && echo "  MISSING from $label (named by the source, absent here): $(echo $missing)" >&2
    [ -n "$extra" ]   && echo "  EXTRA in $label (present here, not named by the source): $(echo $extra)" >&2
    die "$label does not match the release platform source ($MANIFEST) — reconcile it"
  fi
  echo "ok: $label matches the source [$(echo $actual | tr '\n' ' ' | sed 's/  */ /g;s/ $//')]"
}

# ── The source ──────────────────────────────────────────────────────────────────────────────────
# The manifest is read for its platform NAMES; its shape is validated so an empty or malformed file
# fails loudly rather than making every "subset" check below vacuously pass.
source_list="$(node -e '
  const fs = require("fs");
  const p = JSON.parse(fs.readFileSync("./.github/release-platforms.json", "utf8"));
  if (!Array.isArray(p) || p.length === 0) { console.error("release-platforms.json is not a non-empty array"); process.exit(1); }
  for (const e of p) {
    if (!e || typeof e.platform !== "string" || !e.platform) { console.error("a manifest entry has no platform name"); process.exit(1); }
    process.stdout.write(e.platform + "\n");
  }
')"
[ -n "$source_list" ] || die "the platform source is empty"
echo "source ($MANIFEST): [$(echo $source_list | tr '\n' ' ' | sed 's/ $//')]"

# ── The plan's derived list ─────────────────────────────────────────────────────────────────────
# Passed by verify-surface as needs.plan.outputs.platforms. Checking it here catches a plan
# derivation that drifted from the file directly, rather than only through the build it would have
# mis-driven. Omitted on the tree-only ci run, where there is no plan job.
if [ -n "$PLAN_PLATFORMS" ]; then
  assert_set_eq "the plan's derived platform list" "$source_list" "$(printf '%s\n' $PLAN_PLATFORMS)"
fi

# ── The built artifacts ─────────────────────────────────────────────────────────────────────────
if [ "$DIST" = "--no-artifacts" ]; then
  echo "ok: skipping the built-artifact check (--no-artifacts) — holding the tree surfaces only"
else
  [ -d "$DIST" ] || die "no artifact directory at $DIST"
  built="$(
    shopt -s nullglob
    for f in "$DIST"/maxplayer-"$VERSION"-*.tar.gz; do
      b="$(basename "$f" .tar.gz)"
      printf '%s\n' "${b#maxplayer-$VERSION-}"
    done
  )"
  assert_set_eq "the built artifacts in $DIST" "$source_list" "$built"
fi

# ── The npm payload directories ─────────────────────────────────────────────────────────────────
npm_dirs="$(
  shopt -s nullglob
  for d in npm/cli-*/; do
    b="$(basename "$d")"
    printf '%s\n' "${b#cli-}"
  done
)"
assert_set_eq "the npm payload packages (npm/cli-*)" "$source_list" "$npm_dirs"

# ── The launcher's pinned dependencies ──────────────────────────────────────────────────────────
# optionalDependencies is what npm consults to install one platform's binary; a pin with no payload
# published leaves `npm i maxplayer` resolving a package that is not on the registry.
opt_deps="$(node -e '
  const fs = require("fs");
  const p = JSON.parse(fs.readFileSync("./npm/maxplayer/package.json", "utf8"));
  const od = p.optionalDependencies || {};
  const pref = "@maxplayerai/";
  for (const k of Object.keys(od)) process.stdout.write((k.startsWith(pref) ? k.slice(pref.length) : k) + "\n");
')"
assert_set_eq "the launcher optionalDependencies ($LAUNCHER_PKG)" "$source_list" "$opt_deps"

# ── The launcher's runtime resolver ─────────────────────────────────────────────────────────────
# PLATFORM_PACKAGES maps `${process.platform}-${process.arch}` to the payload it resolves at run
# time. A platform missing here is refused a binary that was in fact published; one present with no
# payload behind it is dead weight. The file is parsed, not executed — running it launches maxplayer.
resolver="$(node -e '
  const fs = require("fs");
  const src = fs.readFileSync("./npm/maxplayer/bin/maxplayer.js", "utf8");
  const m = src.match(/PLATFORM_PACKAGES\s*=\s*\{([\s\S]*?)\}/);
  if (!m) { console.error("could not find PLATFORM_PACKAGES in the launcher"); process.exit(1); }
  const keys = [...m[1].matchAll(/["\x27]([\w-]+)["\x27]\s*:/g)].map(x => x[1]);
  if (keys.length === 0) { console.error("PLATFORM_PACKAGES has no entries"); process.exit(1); }
  for (const k of keys) process.stdout.write(k + "\n");
')"
assert_set_eq "the launcher resolver PLATFORM_PACKAGES ($LAUNCHER_JS)" "$source_list" "$resolver"

echo "PASS: every release surface — built, published and launcher — matches $MANIFEST"
