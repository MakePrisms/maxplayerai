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

test("buyer skills point at read_delivery and never at materialized results", () => {
  for (const path of SKILLS) {
    const text = read(path);
    assert.match(text, /read_delivery/, `${path} must name read_delivery`);
    assert.doesNotMatch(text, /materializ/i, `${path} still says the delivery is materialized`);
  }
  const operate = read(SKILLS[0]);
  assert.match(operate, /`"store"`/, "buyer-operate explains delivery: store");
  assert.match(operate, /extract_delivery/, "buyer-operate names extract_delivery");
  assert.match(operate, /do not start an agent session/i, "the one rule left after extract");
});

test("Mode B fetches the delivered commit from the store instead of syncing results/", () => {
  const skill = read(SKILLS[1]);
  const modeB = skill.slice(skill.indexOf("### Mode B"), skill.indexOf("## The promotion rule"));
  assert.match(modeB, /fetch "\$MAXPLAYER_HOME\/store"/, "Mode B fetches from the buyer store");
  assert.match(modeB, /refs\/maxplayer\/deliveries\/<commit_oid>/, "by the retained ref");
  assert.match(modeB, /merge --ff-only/, "and merges only after review");
  assert.doesNotMatch(modeB, /rsync/, "no file sync out of results/");
  assert.doesNotMatch(modeB, /results\/<job_id>\//, "Mode B never reads results/<job_id>/");
});
