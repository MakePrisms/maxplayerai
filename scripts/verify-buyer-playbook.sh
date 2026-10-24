#!/usr/bin/env bash
#
# CI gate: docs/BUYER-PLAYBOOK.md must stay in sync with the plugin skill.
#
# The skill (npm/plugin/skills/buyer/SKILL.md) is the SOURCE OF TRUTH. The docs page exists only so
# that hosts which speak MCP but not plugins are not orphaned — same playbook, no frontmatter. Two
# copies of the same normative text is exactly the shape that rots silently: an edit lands in one,
# the other keeps teaching the old shape, and nothing complains. This script is what complains.
#
# The docs page is DERIVED, never hand-edited. It is the skill with its YAML frontmatter stripped
# and a generated-file banner prepended, and this script owns that transform so the check cannot
# drift from the generator — there is only one.
#
# Usage:
#   ./scripts/verify-buyer-playbook.sh           verify (CI); exits non-zero on drift
#   ./scripts/verify-buyer-playbook.sh --write   regenerate the docs page from the skill

set -euo pipefail

die() { echo "verify-buyer-playbook: $*" >&2; exit 1; }

SKILL="npm/plugin/skills/buyer/SKILL.md"
DOCS="docs/BUYER-PLAYBOOK.md"

[ -f "$SKILL" ] || die "$SKILL not found — run this from the repo root"

# ── The skill must actually be a skill ──────────────────────────────────────────────────────────
# A truncated or frontmatter-less SKILL.md would still "generate" a docs page, and the comparison
# below would pass against it. Assert the shape first so a broken source fails loud rather than
# quietly certifying two copies of the same garbage.
head -n 1 "$SKILL" | grep -qxF -- '---' \
    || die "$SKILL does not open with YAML frontmatter — refusing to generate from it"
grep -qE '^name: *buyer *$' "$SKILL" \
    || die "$SKILL frontmatter has no 'name: buyer' — refusing to generate from it"

# ── Generate: strip frontmatter, prepend the banner ─────────────────────────────────────────────
# awk state machine rather than sed line numbers: the frontmatter is delimited, not fixed-length.
body="$(awk '
    NR == 1 && $0 == "---" { in_fm = 1; next }
    in_fm && $0 == "---"   { in_fm = 0; seen = 1; next }
    in_fm                  { next }
    seen                   { print }
' "$SKILL")"

[ -n "$body" ] || die "generated an EMPTY body from $SKILL — the frontmatter strip consumed the whole file. This is an INSTRUMENT FAILURE: fix the awk above, do not let an empty generation report a PASS."

generated="$(printf '%s\n%s\n' \
    "<!-- GENERATED FROM npm/plugin/skills/buyer/SKILL.md — DO NOT EDIT.
     Regenerate with: ./scripts/verify-buyer-playbook.sh --write -->" \
    "$body")"

# ── Write or verify ─────────────────────────────────────────────────────────────────────────────
if [ "${1:-}" = "--write" ]; then
    printf '%s\n' "$generated" > "$DOCS"
    echo "verify-buyer-playbook: WROTE $DOCS from $SKILL"
    exit 0
fi

[ -f "$DOCS" ] || die "$DOCS is missing — regenerate it with: ./scripts/verify-buyer-playbook.sh --write"

if ! diff -u "$DOCS" <(printf '%s\n' "$generated") > /dev/null; then
    echo "verify-buyer-playbook: DRIFT between $SKILL and $DOCS" >&2
    diff -u "$DOCS" <(printf '%s\n' "$generated") | head -40 >&2
    die "the docs page no longer matches the skill. The SKILL is the source of truth — edit it, then run: ./scripts/verify-buyer-playbook.sh --write"
fi

echo "verify-buyer-playbook: PASS — $DOCS matches $SKILL ($(wc -l < "$DOCS") lines)"
