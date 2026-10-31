#!/usr/bin/env bash
# smoke.sh — the ONE documented gate for this seller package.
#
#   cd sellers/ui-ux-designer && npm ci && bash scripts/smoke.sh
#
# It exercises the real agent path (spawn + ACP handshake + prompt turn), renders the sample,
# captures screenshots, runs accessibility checks, and verifies every artifact from the outside.
#
# It is built to FAIL LOUDLY, including when the checks themselves stop working. Three negative
# controls are run on purpose, and each must exit NONZERO:
#   - an empty page that axe scores as 0 violations must be rejected by the load-proof guard
#   - a 404 URL must not produce a screenshot
#   - two identical images must not pass a "something changed" diff
# If a negative control passes, the gate fails. A suite that cannot fail proves nothing.
#
# Exit codes: 0 all good · 1 a check failed · 2 the environment is not usable.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
PKG="$PWD"
JOB="${1:-jobs/sample-review.json}"
BRIEF="${BRIEF:-briefs/nova-status-brief.md}"
FAILURES=0
STEP=0

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { printf '   \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '   \033[31mFAIL\033[0m %s\n' "$*"; FAILURES=$((FAILURES + 1)); }

# Run a command that MUST succeed.
must() {
  STEP=$((STEP + 1))
  local label="$1"; shift
  if "$@"; then pass "$label"; else fail "$label (exit $?)"; fi
}

# Run a command that MUST fail. This is how the gate proves its own guards are alive.
must_fail() {
  STEP=$((STEP + 1))
  local label="$1"; shift
  if "$@" >/dev/null 2>&1; then
    fail "$label — the command SUCCEEDED but had to fail; a guard is not working"
  else
    pass "$label (correctly nonzero)"
  fi
}

cleanup() {
  if [[ -n "${PREVIEW_PID:-}" ]] && kill -0 "$PREVIEW_PID" 2>/dev/null; then
    kill "$PREVIEW_PID" 2>/dev/null || true
    wait "$PREVIEW_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

say "0. Environment"
command -v node >/dev/null || { echo "node not on PATH"; exit 2; }
node -e 'const v=+process.versions.node.split(".")[0]; if (v<20) { console.error("node >= 20 required, have "+process.version); process.exit(2); }' || exit 2
[[ -d node_modules/playwright ]] || { echo "dependencies missing — run: npm ci"; exit 2; }
printf '   node %s\n' "$(node -v)"
# Harness ACCESS, not binary presence: launch the browser and ask it its version.
node -e '
  const {chromium} = await import("playwright");
  const exe = chromium.executablePath();
  const b = await chromium.launch();
  console.log(`   chromium ${b.version()}`);
  console.log(`   executable ${exe}`);
  await b.close();
' || { echo "Chromium could not be launched from the package — this is a BLOCKER, not a warning"; exit 2; }

say "1. Design tokens and measured contrast"
must "contrast ledger regenerates and every contracted pair passes" \
  node tools/tokens.mjs

say "2. Image pipeline"
must "sharp/libvips loaded with svg+png support" \
  node tools/svg.mjs --check

say "3. THE GENERATIVE PATH — a natural-language brief, NO candidate supplied"
# This is the heart of the gate. A real model harness is driven through the ACTUAL maxplayer
# local driver and must AUTHOR the page, render it, read its own screenshots and audit output,
# and iterate to zero accessibility violations at both viewports. Nothing here supplies a
# candidate to edit; if the model produces nothing, this check fails.
#
# It costs a real model turn and takes several minutes. That is the price of proving a
# designer can design, and it is not skippable: a gate that only checks a deterministic
# rewrite is exactly the failure this package was corrected for.
GEN_OUT="runs/generative-smoke"
rm -rf "$GEN_OUT"
must "the model authors, renders and audits its own design (several minutes)" \
  node agent/design-run.mjs --brief "$BRIEF" --out "$GEN_OUT"

say "3b. THE REVIEW PIPELINE AS TOOLING — deterministic, and only ever a tool"
# Retained from the reviewed package, but demoted: it inspects a supplied candidate. It is
# not the designer and is never allowed to stand in for one.
must "review tooling completes a review-redesign turn with stopReason=end_turn" \
  node tools/drive-agent.mjs --brief "$JOB"

RESULT_JSON="$(node -e '
  const {readFileSync} = await import("node:fs");
  const b = JSON.parse(readFileSync(process.argv[1], "utf8"));
  const name = b.name || "sample";
  process.stdout.write(`evidence/${name}-agent-result.json`);
' "$JOB")"

say "4. Verify the evidence from outside the agent"
must "every manifest artifact exists on disk with a matching size and sha256" \
  node tools/verify-manifest.mjs --result "$RESULT_JSON" --out evidence/MANIFEST.md

say "5. Negative controls — each of these MUST fail"
# A preview server for the two URL-based controls.
node tools/preview.mjs > /tmp/ui-ux-smoke-preview.$$ 2>&1 &
PREVIEW_PID=$!
for _ in $(seq 1 50); do
  grep -q '^origin ' /tmp/ui-ux-smoke-preview.$$ 2>/dev/null && break
  sleep 0.1
done
ORIGIN="$(sed -n 's/^origin //p' /tmp/ui-ux-smoke-preview.$$ | head -1)"
[[ -n "$ORIGIN" ]] || { echo "preview server did not report an origin"; exit 2; }
printf '   preview at %s\n' "$ORIGIN"

must_fail "empty page scoring 0 axe violations is rejected by load proof" \
  node tools/a11y.mjs --url "$ORIGIN/tools/fixtures/blank.html" --name smoke-blank --max 0 \
    --out /tmp/ui-ux-smoke-a11y.$$

must_fail "a 404 URL does not yield a screenshot" \
  node tools/shoot.mjs --url "$ORIGIN/samples/does-not-exist/" --name smoke-404 \
    --out /tmp/ui-ux-smoke-shots.$$

CAND="evidence/screenshots/$(node -e '
  const {readFileSync} = await import("node:fs");
  const b = JSON.parse(readFileSync(process.argv[1], "utf8"));
  process.stdout.write(`${b.name || "sample"}-candidate-desktop.png`);
' "$JOB")"
must_fail "two identical images do not pass a required-change diff" \
  node tools/vdiff.mjs --a "$CAND" --b "$CAND" --out /tmp/ui-ux-smoke-diff.$$.png --min-ratio 0.02

# A generative run whose deliverable has been removed must NOT be reported as a pass. This
# proves the generative check is reading real artifacts rather than trusting its own summary.
SABOTAGE="runs/generative-sabotage.$$"
rm -rf "$SABOTAGE"
if [[ -d "$GEN_OUT" ]]; then
  cp -R "$GEN_OUT" "$SABOTAGE"
  rm -f "$SABOTAGE/workspace/status.html"
  must_fail "a generative run with its deliverable deleted is not reported as a pass" \
    node agent/design-run.mjs --brief "$BRIEF" --out "$SABOTAGE" --verify-only
  rm -rf "$SABOTAGE"
else
  fail "no generative run directory to sabotage — the generative check did not produce evidence"
fi

say "Summary"
printf '   %d checks run, %d failed\n' "$STEP" "$FAILURES"
if [[ "$FAILURES" -ne 0 ]]; then
  printf '\n\033[31mSMOKE GATE FAILED\033[0m — %d of %d checks failed.\n' "$FAILURES" "$STEP"
  exit 1
fi
printf '\n\033[32mSMOKE GATE PASSED\033[0m — evidence manifest: %s/evidence/MANIFEST.md\n' "$PKG"
