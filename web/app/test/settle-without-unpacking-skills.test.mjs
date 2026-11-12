import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// Spec docs/specs/settle-without-unpacking.md §4 test 5: a settled git delivery stays in the buyer
// store, so no buyer skill may send the agent into results/ for one without the `delivery` branch,
// and mode B must fetch the commit with git rather than sync files out of results/.
const REPO = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const read = (p) => readFileSync(join(REPO, p), "utf8");

const SKILLS = [
  "web/app/.well-known/skills/buyer-operate/skill.md",
  "web/app/.well-known/skills/multi-turn-buying/skill.md",
  "web/app/.well-known/skills/muse-buyer/references/settlement.md",
  "web/app/.well-known/skills/grok-bot-operate/skill.md",
];
// Also checked for the old "materialize/materialise" wording (#1311 fable M5).
const WORDING = [
  ...SKILLS,
  "web/app/.well-known/skills/muse-buyer/skill.md",
  "web/app/.well-known/skills/muse-buyer/references/verification.md",
  "docs/BUYER-QUICKSTART.md",
  "crates/maxplayer/src/mcp.rs",
];

test("buyer skills point at read_delivery and never at materialized results", () => {
  for (const path of SKILLS) {
    const text = read(path);
    assert.match(text, /read_delivery/, `${path} must name read_delivery`);
  }
  for (const path of WORDING) {
    const text = read(path).replace(/^\s*\/\/.*$/gm, "");
    assert.doesNotMatch(text, /materiali[sz]/i, `${path} still says the delivery is materialized`);
  }
  const operate = read(SKILLS[0]);
  assert.match(operate, /`"store"`/, "buyer-operate explains delivery: store");
  assert.match(operate, /extract_delivery/, "buyer-operate names extract_delivery");
  assert.match(operate, /do not start an agent session/i, "the one rule left after extract");
});

test("Mode B fetches the delivered commit from the store instead of syncing results/", () => {
  const skill = read(SKILLS[1]);
  const modeB = skill.slice(skill.indexOf("### Mode B"), skill.indexOf("## The promotion rule"));
  assert.match(modeB, /STORE="\$\{MAXPLAYER_HOME:-\$HOME\/\.maxplayer\}\/store"/, "store path has a default");
  assert.match(modeB, /fetch "\$STORE"/, "Mode B fetches from the buyer store");
  assert.match(modeB, /refs\/maxplayer\/deliveries\/<commit_oid>/, "by the retained ref");
  assert.match(modeB, /merge --ff-only/, "and merges only after review");
  // #1311 fable H1/M3/M4: the seeding turn has its own review, a failed review stops, a non-ff
  // merge has a safe next step, and the review names what to look at.
  assert.match(modeB, /hash-object -t tree \/dev\/null\)" review\//, "seeding review diffs against the empty tree");
  assert.match(modeB, /If any review command fails, stop: do not merge/);
  assert.match(modeB, /not a fast-forward/);
  assert.match(modeB, /--summary/);
  assert.match(modeB, /\.github\/workflows\//);
  assert.doesNotMatch(modeB, /rsync/, "no file sync out of results/");
  assert.doesNotMatch(modeB, /results\/<job_id>\//, "Mode B never reads results/<job_id>/");
});
