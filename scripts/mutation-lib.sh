#!/usr/bin/env bash
# Shared mutation-receipt discipline for the delivery custody work (PR #1006).
#
# WHAT A RECEIPT HAS TO CARRY, and why each part is here.
#
# A mutation run is a claim about a SUITE: "delete this, and the suite says so". Three ways that
# claim goes wrong without anyone noticing:
#
#   1. THE SUITE NEVER RAN. A mutant that does not compile makes cargo exit non-zero, and a check
#      that only reads the exit status calls that "caught". It is the opposite: a mutant that
#      cannot build was never tested by anything. So a red run here must be a run whose failures
#      are NAMED TEST FAILURES with a NAMED ASSERTION in them, and whose log carries no compile
#      error.
#   2. THE COUNT DRIFTED. "Some assertion failed" is satisfied by an unrelated flake in the same
#      binary. The expected number of failing tests is stated up front and enforced, so a mutant
#      that starts failing MORE tests, or fewer, is a change in the evidence and stops the run.
#   3. THE TREE WAS NOT WHAT THE RECEIPT SAYS. The whole argument depends on exactly one edit being
#      present during the red run and absent during the green one. So every phase is bound to
#      hashes taken from the filesystem at that moment: the mutated file's digest, the ROOT TREE of
#      the entire worktree as git would record it, and the same pair again after restoration, which
#      must equal the pre-mutation pair.
#
# Sourced, not run. Callers set `root` and `receipts` before sourcing.
set -euo pipefail

if command -v sha256sum > /dev/null 2>&1; then
  sha_of() { sha256sum "$1" | awk '{print $1}'; }
else
  sha_of() { shasum -a 256 "$1" | awk '{print $1}'; }
fi

# The root tree of the WORKTREE AS IT STANDS, including the mutation, as a real git tree oid.
#
# Written through a throwaway index so the repository's own index is never touched: this script
# must not stage anything, and a receipt taken by mutating the developer's index would be a receipt
# that changed what it measured.
root_tree() {
  local idx
  idx="$(mktemp -t mutation-index)"
  GIT_INDEX_FILE="$idx" git -C "$root" read-tree HEAD > /dev/null
  GIT_INDEX_FILE="$idx" git -C "$root" add -A > /dev/null
  GIT_INDEX_FILE="$idx" git -C "$root" write-tree
  rm -f "$idx"
}

# Replace an anchor that must appear EXACTLY ONCE. A mutation whose anchor matches twice is a
# mutation in an unknown place, which is not evidence about anything.
mutate() {
  python3 - "$1" "$2" "$3" <<'PY'
import sys
path, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
body = open(path).read()
if body.count(old) != 1:
    sys.exit(f"mutation anchor appears {body.count(old)} times in {path}, expected exactly 1")
open(path, "w").write(body.replace(old, new))
PY
}

receipt() { printf '%s\n' "$*" >> "$receipts"; }

# EXTRA ANCHORS for one mutant, as a flat list of old/new pairs, consumed and cleared by the next
# `run_mutant`.
#
# A mutation is supposed to remove a MECHANISM, and a mechanism is not always one edit. Where a
# property is defended in two places, removing one of them proves nothing about the gate: the other
# still holds the line and the mutant survives for a reason that has nothing to do with the test's
# sensitivity. Those cases get one mutant that removes both sites at once, recorded as such.
extra_mutations=()

# PRISTINE COPIES, taken once before anything is edited, and the only source a restore ever reads.
#
# Restoring from `git checkout` would be wrong here: this branch's work is uncommitted often enough
# that a checkout could silently revert more than the mutation. The pristine copy is of the file as
# this run found it, whatever state that was, which is also what the hash receipts are taken
# against. Plain arrays and a directory, because macOS still ships bash 3.2 with no associative
# arrays.
backup_dir="$(mktemp -d -t mutation-backups)"
targets=()

register_target() {
  targets+=("$1")
  cp "$1" "$backup_dir/$(basename "$1")"
}

restore_targets() {
  local file
  for file in "${targets[@]}"; do
    cp "$backup_dir/$(basename "$file")" "$file"
  done
}

cleanup_mutations() { restore_targets; rm -rf "$backup_dir"; }
trap cleanup_mutations EXIT

# expect_red LABEL LOG EXPECTED_FAILURES ASSERTION_SUBSTRING TEST_NAME...
#
# Every condition below has to hold. Any one of them missing means this run is not evidence that
# the suite detects the mutation, and the script stops rather than reporting a catch.
expect_red() {
  local label="$1" log="$2" expected="$3" assertion="$4"
  shift 4
  local names=("$@")

  set +e
  "${suite[@]}" > "$log" 2>&1
  local status=$?
  set -e

  if [[ $status -eq 0 ]]; then
    echo "SURVIVED: $label — the suite PASSED against the mutant. $log"
    exit 1
  fi

  # A build failure is not a caught mutant. This is the check the previous version of this script
  # did not make, and the reason its receipts could not tell "the suite objected" from "nothing
  # ran".
  if grep -qE '^error\[E[0-9]+\]|^error: could not compile|^error: expected|^error: cannot' "$log"; then
    echo "NOT EVIDENCE: $label — the mutant failed to BUILD, so no test observed it. $log"
    grep -E '^error' "$log" | head -5
    exit 1
  fi

  # The named tests, each of them, reported by the harness as failing.
  local name
  for name in "${names[@]}"; do
    if ! grep -qE "^test .*${name} \.\.\. FAILED" "$log"; then
      echo "WRONG TEST: $label — expected '$name' to FAIL and it did not. $log"
      grep -E '^test .* \.\.\. (ok|FAILED)' "$log" | head -10
      exit 1
    fi
  done

  # The named assertion, not merely some panic: this is what ties the red to the ORACLE under test
  # rather than to a fixture that fell over while the mutant happened to be applied.
  if ! grep -qF "$assertion" "$log"; then
    echo "WRONG REASON: $label — no failure quoting the required assertion: '$assertion'. $log"
    grep -E '^thread .* panicked|assertion' "$log" | head -5
    exit 1
  fi

  # The count, enforced rather than printed.
  local failed
  failed="$(grep -cE '^test .* \.\.\. FAILED' "$log" || true)"
  if [[ "$failed" != "$expected" ]]; then
    echo "COUNT DRIFT: $label — $failed failing tests, receipt says $expected. $log"
    grep -E '^test .* \.\.\. FAILED' "$log" | head -10
    exit 1
  fi

  receipt "  red.failing_tests   = $failed (required $expected)"
  receipt "  red.assertion       = $assertion"
  receipt "  red.log_sha256      = $(sha_of "$log")"
  echo "CAUGHT:   $label — $failed named failure(s), required assertion present. $log"
}

expect_green() {
  local label="$1" log="$2"
  if ! "${suite[@]}" > "$log" 2>&1; then
    echo "CONTROL RED: $label — the unmutated suite fails, so the mutants above prove nothing. $log"
    grep -E '^test .* \.\.\. FAILED|^error' "$log" | head -10
    exit 1
  fi
  local passed
  passed="$(grep -cE '^test .* \.\.\. ok' "$log" || true)"
  receipt "  green.passing_tests = $passed"
  receipt "  green.log_sha256    = $(sha_of "$log")"
  echo "GREEN:    $label — $passed passing. $log"
}

# Bind one mutant's whole lifecycle to hashes: clean tree, mutant tree, restored tree.
#
# RED BEFORE GREEN, in this order, in one process: the mutation is applied to a tree whose hash is
# recorded, the suite goes red for the stated reason, the file is restored from the pristine copy,
# and the restored tree must hash to exactly what it was before. A receipt whose restored hashes
# differ from its pre hashes is reporting on a tree nobody can reconstruct, so it fails.
run_mutant() {
  local label="$1" target="$2" old="$3" new="$4" expected="$5" assertion="$6" log="$7"
  shift 7

  restore_targets
  local pre_source pre_tree
  pre_source="$(sha_of "$target")"
  pre_tree="$(root_tree)"

  receipt ""
  receipt "== $label"
  receipt "  head                = $(git -C "$root" rev-parse HEAD)"
  receipt "  target              = ${target#"$root"/}"
  receipt "  pre.source_sha256   = $pre_source"
  receipt "  pre.root_tree       = $pre_tree"

  mutate "$target" "$old" "$new"
  local extra=0
  while [[ $extra -lt ${#extra_mutations[@]} ]]; do
    mutate "$target" "${extra_mutations[$extra]}" "${extra_mutations[$((extra + 1))]}"
    extra=$((extra + 2))
  done
  receipt "  mutant.anchors      = $((1 + extra / 2))"
  extra_mutations=()
  receipt "  mutant.source_sha256= $(sha_of "$target")"
  receipt "  mutant.root_tree    = $(root_tree)"

  expect_red "$label" "$log" "$expected" "$assertion" "$@"

  restore_targets
  local post_source post_tree
  post_source="$(sha_of "$target")"
  post_tree="$(root_tree)"
  receipt "  restored.source_sha256 = $post_source"
  receipt "  restored.root_tree     = $post_tree"
  if [[ "$post_source" != "$pre_source" || "$post_tree" != "$pre_tree" ]]; then
    echo "RESTORATION MISMATCH: $label — the tree after restore is not the tree before mutation."
    exit 1
  fi
  receipt "  restored.matches_pre   = yes"
}
