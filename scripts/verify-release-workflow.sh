#!/usr/bin/env bash
#
# Assert the release workflow cannot publish by accident.
#
# The properties below are the reason the workflow is safe to merge before any trusted publisher is
# configured, and every one of them is a single edit away from being lost — moving a gate, adding a
# trigger, or copying an `npm publish` into another job. None of that breaks anything visibly: the
# workflow keeps working, and the next release is simply published from somewhere it should not have
# been. This script is what makes such an edit fail.
#
# ★ This checks the workflow's STRUCTURE, which is what can be checked without GitHub. It cannot
#   evaluate the `if:` expressions the way Actions does — observing the publish job actually skip on
#   a `workflow_dispatch` run is a separate, live confirmation, and RELEASE.md records it as the
#   remaining step before the first real tag.
#
# Usage:
#   ./scripts/verify-release-workflow.sh [path-to-workflow]

set -euo pipefail

WORKFLOW="${1:-.github/workflows/release.yml}"

die() { echo "verify-release-workflow: $*" >&2; exit 1; }

[ -f "$WORKFLOW" ] || die "no workflow at $WORKFLOW"
command -v node >/dev/null 2>&1 || die "node not found"

# ── No trigger a pull request can reach ─────────────────────────────────────────────────────────
# A `pull_request` trigger would let any fork's branch start the release workflow.
if grep -Eq '^\s*pull_request(_target)?\s*:' "$WORKFLOW"; then
    die "$WORKFLOW has a pull_request trigger — a pull request must not be able to start a release"
fi
echo "ok: no pull_request trigger"

# ── No credential in the tree ───────────────────────────────────────────────────────────────────
# The token belongs in repository secrets. `npm_` followed by a long token body is the shape of a
# real npm automation token; `//registry` with an auth line is the shape of a committed .npmrc.
if grep -Eq 'npm_[A-Za-z0-9]{30,}|_authToken\s*=' "$WORKFLOW"; then
    die "$WORKFLOW looks like it contains a literal npm credential"
fi
echo "ok: no credential literal in the workflow"

# ── No `run:` continues a plain scalar with a backslash ─────────────────────────────────────────
# A `run:` written as a plain scalar folds its newline into a space, so a trailing `\` reaches the
# shell as `\ ` — an escaped space, which joins onto the following word instead of continuing the
# line. The next argument silently gains a leading space. Nothing about the YAML looks wrong and the
# step runs; it just receives a path that cannot exist. That cost a full three-platform dry-run
# (30476367671), where the argument arrived as ` target/x86_64-unknown-linux-musl/release/maxplayer`.
# Every such command belongs in a `run: |` block.
#
# ★ actionlint is silent on this shape — it reports zero findings on the exact file that failed — so
#   this is not covered by the linting already in CI.
#
# Checked across every workflow, not only the release one: the mistake is not release-specific, and a
# guard that watches one file while the class recurs next door is a guard in name only.
#
# `- run:` on one line is as common as `run:` under its own `- name:`, so both forms are anchored. An
# expression that only allowed the second passed a broken workflow written in the first.
folded=$(grep -nE '^[[:space:]]*(-[[:space:]]+)?run:[[:space:]]*[^|>[:space:]].*\\[[:space:]]*$' \
    "$(dirname "$WORKFLOW")"/*.yml "$(dirname "$WORKFLOW")"/*.yaml 2>/dev/null || true)
if [ -n "$folded" ]; then
    echo "$folded" >&2
    die "a plain-scalar 'run:' is continued with a backslash (see above) — use 'run: |'"
fi
echo "ok: every backslash-continued run: is a block scalar"

# ── Both surfaces are still built, and each is verified by its own script ───────────────────────
# A release that publishes only the racer assets is not a broken release — it is a smaller one, and
# nothing downstream notices: the assets it does publish build, verify, checksum and install exactly
# as they should. The first report is a seller running `install.sh --seller` against a 404. So the
# pair is asserted here, where dropping one fails the pull request that does it rather than the tag
# that ships it.
#
# The verifier scripts are checked for existence too. A workflow naming a script that is not there
# fails at the tag, after six builds — and the `if:`-free step would be the last thing to run before
# packaging, so the artifact would already exist.
for surface in racer seller; do
    verifier="scripts/verify-$surface-surface.sh"
    grep -qF -- "$verifier" "$WORKFLOW" \
        || die "$WORKFLOW does not run $verifier — the $surface artifact would ship with its feature set unasserted"
    [ -x "$verifier" ] \
        || die "$WORKFLOW runs $verifier, which is missing or not executable"
done
grep -qE "^ +asset: maxplayer$" "$WORKFLOW" \
    || die "$WORKFLOW builds no asset named 'maxplayer' — the buyer artifact is what every install and every npm payload resolves"
grep -qE "^ +asset: maxplayer-seller$" "$WORKFLOW" \
    || die "$WORKFLOW builds no 'maxplayer-seller' asset — sellers would have nothing to install"
echo "ok: both surfaces are built, each verified by its own script"

# ── The privileged jobs are gated, and nothing else publishes ───────────────────────────────────
# Job boundaries are found by their two-space-indented keys rather than parsed as YAML, so that this
# check needs nothing installed. It fails closed: if the jobs cannot be located, it does not pass.
node - "$WORKFLOW" <<'NODE'
const fs = require("fs");
const path = process.argv[2];
const lines = fs.readFileSync(path, "utf8").split("\n");

const jobsStart = lines.findIndex((l) => /^jobs:\s*$/.test(l));
if (jobsStart < 0) { console.error("could not find the jobs: block"); process.exit(1); }

// Collect each job's own lines, keyed by name.
const jobs = new Map();
let current = null;
for (const line of lines.slice(jobsStart + 1)) {
  const header = line.match(/^ {2}([A-Za-z][\w-]*):\s*$/);
  if (header) { current = header[1]; jobs.set(current, []); continue; }
  if (current) jobs.get(current).push(line);
}
if (jobs.size === 0) { console.error("no jobs found — the file's shape is not what this check understands"); process.exit(1); }

const fail = (msg) => { console.error(msg); process.exit(1); };

// Anything that writes to the outside world has to be gated on a tag PUSH. Both halves matter: a
// workflow_dispatch can be aimed at a tag ref, so the ref type alone does not imply a push.
for (const name of ["release", "publish"]) {
  const body = jobs.get(name);
  if (!body) fail(`expected a '${name}' job — this check is out of date with the workflow`);
  const text = body.join("\n");
  const gate = text.match(/^ {4}if:(.*)$/m);
  if (!gate) fail(`job '${name}' has no if: gate, so it would run on every trigger`);
  const expr = gate[1];
  if (!expr.includes("github.event_name == 'push'")) {
    fail(`job '${name}' is not gated on a push event: a workflow_dispatch aimed at a tag would reach it`);
  }
  if (!expr.includes("github.ref_type == 'tag'")) {
    fail(`job '${name}' is not gated on a tag ref`);
  }
}

// `npm publish` must live in exactly one job. A copy anywhere else would not be covered by the gate
// checked above.
for (const [name, body] of jobs) {
  if (name === "publish") continue;
  const at = body.findIndex((l) => l.includes("npm publish"));
  if (at >= 0) fail(`job '${name}' runs 'npm publish' — publishing belongs only in the gated publish job`);
}
if (!jobs.get("publish").some((l) => l.includes("npm publish"))) {
  fail("the publish job does not run 'npm publish' — this check is out of date with the workflow");
}

// ── The publish job cannot skip quietly ─────────────────────────────────────────────────────────
// Authentication is npm trusted publishing (OIDC), so there is no secret to be empty and nothing to
// check for emptiness. The property that guard existed to protect is unchanged and still checked
// here: a publish that cannot happen must FAIL, never pass quietly. A silent skip is
// indistinguishable from a successful release, and stays that way until somebody tries to install
// what was never published.
//
// Under OIDC that property has two halves — the job must be able to authenticate at all, and
// nothing may swallow the failure when it cannot.
const publishBody = jobs.get("publish");

// Half one. Without `id-token: write` no OIDC token is minted, npm falls back to looking for a
// credential that no longer exists, and the job fails for a reason that reads like a registry
// problem. The permissions block is matched at its own indentation so that a permissions block
// belonging to some other job cannot satisfy this.
if (!/^ {4}permissions:\s*$/m.test(publishBody.join("\n")) ||
    !/^ {6}id-token:\s*write\s*$/m.test(publishBody.join("\n"))) {
  fail("the publish job does not request 'id-token: write' — trusted publishing has no token to exchange");
}

// Half two. Comment-only lines are dropped first: this half searches for shapes that would suppress
// a failure, and the prose around them necessarily describes those same shapes. A check that reads
// its own explanation is a check that fails on documentation.
const publishCode = publishBody.filter((l) => !/^\s*#/.test(l)).join("\n");

// An early return reports success for a job that published nothing — the exact shape the deleted
// token guard was careful NOT to have (it exited 1, not 0).
if (/\bexit\s+0\b/.test(publishCode)) {
  fail("the publish job can 'exit 0' early — a job that returns success without publishing is the silent skip");
}
// A step-level `if:` conditionally skips its step, and a skipped step is reported green. The job's
// one legitimate gate is the job-level `if:` checked above, at four spaces; step keys sit at eight.
if (/^ {8}if:/m.test(publishCode)) {
  fail("a step in the publish job carries an 'if:' — a skipped publish step is green, which is the silent skip");
}
// `continue-on-error` turns a failed publish into a passing job, which is the same failure wearing
// a different hat.
if (/continue-on-error/.test(publishCode)) {
  fail("the publish job uses 'continue-on-error' — a failed publish would be reported as a success");
}
// Nothing should be reaching for the retired token. Its presence means auth quietly went back to a
// secret, and with it the expiry that trusted publishing was adopted to remove.
if (/NODE_AUTH_TOKEN|secrets\.NPM_TOKEN/.test(publishCode)) {
  fail("the publish job still references an npm token — authentication is trusted publishing (OIDC)");
}

console.log("ok: release and publish are gated on a tag push");
console.log("ok: 'npm publish' appears only in the publish job");
console.log("ok: the publish job requests id-token: write and has no silent-skip path");
NODE

echo "PASS: $WORKFLOW publishes nothing without a pushed tag, and cannot skip publishing quietly"
