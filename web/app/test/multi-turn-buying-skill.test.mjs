import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// The Mode B rsync line keeps every undotted name collect renames out of the mirror. A name added to
// AGENT_CONTROL_FILES and not to the line would be deleted from buyer clones by --delete
// (worker-fable review of #1307, finding 4).
const REPO = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

test("Mode B rsync excludes every undotted name collect renames", () => {
  const rust = readFileSync(join(REPO, "crates/maxplayer-core/src/collect.rs"), "utf8");
  const block = /const AGENT_CONTROL_FILES: &\[&str\] = &\[([\s\S]*?)\];/.exec(rust);
  assert.ok(block, "AGENT_CONTROL_FILES not found in collect.rs");
  const names = [...block[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]);
  assert.ok(names.length >= 10, `only ${names.length} names parsed`);
  const skill = readFileSync(
    join(REPO, "web/app/.well-known/skills/multi-turn-buying/skill.md"), "utf8");
  const excluded = new Set(
    [...skill.matchAll(/--exclude=([^\s'"\\]+)/g)].map((m) => m[1].toLowerCase()));
  for (const name of names) {
    assert.ok(excluded.has(name), `multi-turn-buying Mode B rsync does not exclude ${name}`);
  }
  assert.match(skill, /--exclude-from=/, "the line protects the paths the settle renamed");
  assert.doesNotMatch(skill, /--delete-excluded\s+[^`]*results\//, "never --delete-excluded");
});
