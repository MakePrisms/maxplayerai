#!/usr/bin/env bash
#
# CI gate for docs/protocol-v1.md "## 12. Reserved Paths" (#636).
#
# checks::validate_against_base (crates/maxplayer-core/src/checks.rs) enforces this rule at
# RUNTIME against a delivery's BASE tree. Until this script existed, nothing enforced it against
# OUR OWN tree — the rule was prose plus a function exercised only against a synthetic fixture.
# #635 (a paid market delivery) briefly had MAXPLAYER_EXECUTION_SENTINEL committed at the tree
# root; only a human reading the diff caught it. This script is the automated version of that
# read, run on every PR.
#
# The seller writes MAXPLAYER_EXECUTION_SENTINEL into every DELIVERED tree on purpose — it is
# required there and forbidden here. Two trees, opposite requirements: this script is what keeps
# "here" honest.
#
# Usage: ./scripts/verify-reserved-paths.sh   (run from the repo root; needs bash/grep/sed/git only)

set -euo pipefail

die() { echo "verify-reserved-paths: $*" >&2; exit 1; }

CHECKS_RS="crates/maxplayer-core/src/checks.rs"
SENTINEL_RS="crates/maxplayer-core/src/delivery_sentinel.rs"

[ -f "$CHECKS_RS" ] || die "$CHECKS_RS not found — run this from the repo root"
[ -f "$SENTINEL_RS" ] || die "$SENTINEL_RS not found — run this from the repo root"

# ── Enumerate reserved names FROM the source constants ─────────────────────────────────────────
# Never hand-type a reserved filename here. Extract the string literal each `..._FILE` constant is
# DEFINED to equal, straight from the Rust source, so a changed value — or a THIRD reserved-path
# constant added later, as long as it follows the same `pub const X_FILE: &str = "...";` naming —
# is picked up automatically and this script cannot silently drift from checks::validate_against_base.
# The `_FILE:` suffix deliberately excludes DECLARATION_PATH (".maxplayer/checks.toml", a nested
# config path, not a reserved root path) and CHECKS_ATTESTATION_MARKER (not a path at all).
names="$(grep -hoE 'pub const [A-Z_]+_FILE: &str = "[^"]+"' "$CHECKS_RS" "$SENTINEL_RS" \
    | sed -E 's/.*"([^"]+)"/\1/' || true)"

[ -n "$names" ] || die "enumerated ZERO reserved path names from $CHECKS_RS / $SENTINEL_RS — the extraction pattern no longer matches the source. This is an INSTRUMENT FAILURE: fix the pattern above, do not let an empty enumeration report a false PASS."

# ── Assert none is a TRACKED file at the tree root ──────────────────────────────────────────────
# `git ls-files` (tracked/staged, matching what would actually merge) — not a raw directory listing,
# which would also flag an untracked scratch file nobody is about to commit.
tracked="$(git ls-files)"
[ -n "$tracked" ] || die "git ls-files returned ZERO tracked files — the tree read failed, so this check has verified NOTHING. Fail loud rather than report a false PASS for a repo it never actually read."

fail=0
while IFS= read -r name; do
    [ -n "$name" ] || continue
    # Exact whole-line match: the protocol reserves ROOT paths only (see docs/protocol-v1.md
    # "## 12. Reserved Paths" — "Two root paths in a delivered tree"). A same-named file nested in
    # a subdirectory is not what the protocol reserves and must not trip this check.
    if grep -qxF "$name" <<<"$tracked"; then
        echo "verify-reserved-paths: PROTOCOL-RESERVED PATH IS TRACKED AT ROOT: $name (see docs/protocol-v1.md '## 12. Reserved Paths', and #635)" >&2
        fail=1
    fi
done <<<"$names"

[ "$fail" -eq 0 ] || die "one or more protocol-reserved paths are tracked at the tree root — remove them before merging (see #636, #635)"

echo "verify-reserved-paths: PASS — reserved path(s) [$(tr '\n' ' ' <<<"$names")] absent from the tracked tree root"
