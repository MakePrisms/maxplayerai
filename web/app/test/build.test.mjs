/**
 * The build's shipped-surface invariants: the agent-facing URLs (/skill.md,
 * the discovery index) and the cache-stamped asset references. These pin what
 * a deploy publishes, not how the app behaves — that's market.test.ts.
 */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
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
  assert.equal(discoveryIndex.skills.length, legacyIndex.skills.length);
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

test("every asset URL in shipped HTML and CSS carries the deploy stamp", () => {
  const { stamp } = JSON.parse(readFileSync(join(root, "dist", ".buildstamp"), "utf8"));
  assert.match(stamp, /^[0-9a-f]{12}$/);

  const html = readFileSync(join(root, "dist", "index.html"), "utf8");
  for (const asset of ["styles.css", "fonts.css", "terminal.js"]) {
    assert.ok(html.includes(`./${asset}?v=${stamp}`), `${asset} is stamped in index.html`);
    assert.ok(!html.includes(`"./${asset}"`), `no unstamped ${asset} reference remains`);
  }
  // Font preload URLs must match fonts.css URLs byte-for-byte or the browser
  // fetches every preloaded font twice.
  const fontsCss = readFileSync(join(root, "dist", "fonts.css"), "utf8");
  const preloads = [...html.matchAll(/href="\.\/(fonts\/[^"?]+)\?v=([0-9a-f]{12})"/g)];
  assert.ok(preloads.length >= 2, "font preloads exist and are stamped");
  for (const m of preloads) {
    assert.equal(m[2], stamp, `${m[1]} preload carries the stamp`);
    assert.ok(fontsCss.includes(`url('./${m[1]}?v=${stamp}')`), `${m[1]} stamped identically in fonts.css`);
  }
  assert.ok(!/url\((['"])\.\/fonts\/[^'"?]+\1\)/.test(fontsCss), "no unstamped font URL remains in fonts.css");
});

test("the bundle ships as one module and the snapshot stays out of git", () => {
  assert.ok(existsSync(join(root, "dist", "terminal.js")));
  // A local bake writes public/snapshot.json (that's fine); git must ignore
  // it — 2.5MB of market data would rot in history and churn every refresh.
  const gitignore = readFileSync(join(root, ".gitignore"), "utf8");
  assert.match(gitignore, /^public\/snapshot\.json$/m, "snapshot.json is baked at deploy, never committed");
});
