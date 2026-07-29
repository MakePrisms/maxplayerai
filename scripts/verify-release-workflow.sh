#!/usr/bin/env bash
#
# Assert the release workflow cannot publish by accident.
#
# The properties below are the reason the workflow is safe to merge with no token configured, and
# every one of them is a single edit away from being lost — moving a gate, adding a trigger, or
# copying an `npm publish` into another job. None of that breaks anything visibly: the workflow keeps
# working, and the next release is simply published from somewhere it should not have been. This
# script is what makes such an edit fail.
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

// The token half of the gate cannot be a job-level `if:` (the secrets context is unavailable there),
// so it must be a step that stops the job when the secret is empty.
const publish = jobs.get("publish").join("\n");
if (!/NPM_TOKEN/.test(publish) || !/-z\s+"\$\{NPM_TOKEN/.test(publish)) {
  fail("the publish job does not fail closed on an empty NPM_TOKEN");
}

console.log("ok: release and publish are gated on a tag push");
console.log("ok: 'npm publish' appears only in the publish job");
console.log("ok: the publish job fails closed without NPM_TOKEN");
NODE

echo "PASS: $WORKFLOW publishes nothing without both a pushed tag and the secret"
