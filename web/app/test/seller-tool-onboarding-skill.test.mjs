/**
 * The served seller-tool-onboarding skill: its shipped invariants.
 *
 * 1. The page pins THIS tree's version at each site a reader acts on: the version sentence, the
 *    checkout tag, and the sandbox image tag.
 * 2. The page states no other version, except the fixed floor: the first release that knows the
 *    two tool tables.
 * 3. The skill index lists the page, with the frontmatter's name and description.
 *
 *   node --test web/app/test/seller-tool-onboarding-skill.test.mjs
 *
 * Node builtins only. No network, no docker, no daemon. This is a page-and-index gate, not an
 * acceptance run. A release cut bumps the pins, as it does for the Muse buyer bundle. This test
 * makes a forgotten bump fail CI. Without it, the page would name the previous release.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");   // web/app
const REPO = resolve(root, "..", "..");                             // repository root
const SKILL_PATH = "/.well-known/skills/seller-tool-onboarding/skill.md";
const SKILL_FILE = join(root, SKILL_PATH.slice(1));

/**
 * The first release whose daemon knows `[[sandbox.held_tools]]` and `[[sandbox.mcp_tools]]`
 * (pull request #1004, shipped in v0.5.9). A fixed fact: it does not move with the tree.
 */
const FLOOR = "0.5.9";

const version = /^version\s*=\s*"([0-9.]+)"/m.exec(readFileSync(join(REPO, "Cargo.toml"), "utf8"))[1];
const page = readFileSync(SKILL_FILE, "utf8");
const escaped = (v) => v.replace(/\./g, "\\.");

test("the page pins this tree's version at every site a reader acts on", () => {
  assert.match(page, new RegExp(`This page matches maxplayer ${escaped(version)},`),
    `the version sentence must name ${version}`);
  assert.match(page, new RegExp(`git checkout v${escaped(version)}\\b`),
    `the clone step must check out v${version}`);
  assert.match(page, new RegExp("ghcr\\.io/makeprisms/maxplayer-sandbox:v" + escaped(version) + "`"),
    `the Public route must name the v${version} sandbox image`);
});

test("the page states no version other than this tree's and the fixed floor", () => {
  let checked = 0;
  page.split("\n").forEach((line, index) => {
    for (const match of line.matchAll(/\b(\d+\.\d+\.\d+)\b/g)) {
      checked += 1;
      const stated = match[1];
      assert.ok(stated === version || stated === FLOOR,
        `skill.md:${index + 1} states version ${stated}; this tree is ${version} and the floor is `
        + `${FLOOR}: ${line.trim()}`);
    }
  });
  assert.ok(checked > 0, "no version statement was found to check; the scan is broken");
});

test("the skill index lists the page, with the frontmatter's name and description", () => {
  const index = JSON.parse(readFileSync(join(root, ".well-known", "skills", "index.json"), "utf8"));
  const entry = index.skills.find(({ path }) => path === SKILL_PATH);
  assert.ok(entry, `index.json lists ${SKILL_PATH}`);
  const front = /^---\n([\s\S]*?)\n---/.exec(page)[1];
  const name = /^name:\s*(.+)$/m.exec(front)[1].trim();
  const description = /^description:\s*(.+)$/m.exec(front)[1].trim();
  assert.equal(entry.name, name, "the index entry carries the frontmatter's name");
  assert.equal(entry.description, description, "the index entry carries the frontmatter's description");
});
