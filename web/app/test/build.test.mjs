import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
execFileSync(process.execPath, ["scripts/build.mjs"], { cwd: root });

const legacyIndex = JSON.parse(
  readFileSync(join(root, ".well-known", "skills", "index.json"), "utf8"),
);
const discoveryIndex = JSON.parse(
  readFileSync(join(root, "dist", ".well-known", "agent-skills", "index.json"), "utf8"),
);

test("build publishes the RFC v0.2.0 discovery index for every legacy skill", () => {
  assert.equal(
    discoveryIndex.$schema,
    "https://schemas.agentskills.io/discovery/0.2.0/schema.json",
  );
  assert.equal(discoveryIndex.skills.length, 5);
  assert.deepEqual(
    discoveryIndex.skills.map(({ name, description }) => ({ name, description })),
    legacyIndex.skills.map(({ name, description }) => ({ name, description })),
  );

  for (const skill of discoveryIndex.skills) {
    const legacy = legacyIndex.skills.find(({ name }) => name === skill.name);
    assert.equal(skill.type, "skill-md");
    assert.equal(skill.url, `https://www.maxplayer.ai${legacy.path}`);
    assert.deepEqual(Object.keys(skill), ["name", "type", "description", "url", "digest"]);
  }
});

test("discovery digests are lowercase SHA-256 hashes of the raw artifacts", () => {
  for (const skill of discoveryIndex.skills) {
    const artifactPath = new URL(skill.url).pathname;
    const artifact = readFileSync(join(root, artifactPath.slice(1)));
    const digest = `sha256:${createHash("sha256").update(artifact).digest("hex")}`;
    assert.match(skill.digest, /^sha256:[0-9a-f]{64}$/);
    assert.equal(skill.digest, digest);
  }
});

test("the homepage skill names and links all four companion skills", () => {
  const homepage = readFileSync(
    join(root, ".well-known", "skills", "default", "skill.md"),
    "utf8",
  );
  for (const name of ["buyer-operate", "seller-operate", "debug-buying", "debug-selling"]) {
    assert.match(homepage, new RegExp(`\\[${name}\\]\\(/\\.well-known/skills/${name}/skill\\.md\\)`));
  }
});

test("the root skill alias is byte-identical to the canonical homepage skill", () => {
  assert.deepEqual(
    readFileSync(join(root, "dist", "skill.md")),
    readFileSync(join(root, ".well-known", "skills", "default", "skill.md")),
  );
});
