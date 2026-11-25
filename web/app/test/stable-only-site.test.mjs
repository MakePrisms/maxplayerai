/**
 * The website names stable releases only: an RC bump leaves it alone, a stable bump moves it.
 *
 * 1. The version every pinned page must name (see site-version.mjs): Cargo.toml's when stable,
 *    otherwise the newest stable release-notes section.
 * 2. Nothing served mentions or relies on a release candidate: no `@rc` / `--tag rc` npm install,
 *    no `-rcN` version, no "pre-release" or "release candidate" guidance, anywhere under
 *    `public/`, `.well-known/` or `src/`. No served file needs an exemption today, so there is no
 *    allow-list; add one here, with the reason, only for historical or legal text.
 *
 *   node --test web/app/test/stable-only-site.test.mjs
 *
 * Node builtins only.
 */
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, extname, join, relative, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { expectedSiteVersion, latestStable, siteVersion } from "./site-version.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");   // web/app
const REPO = resolve(root, "..", "..");                             // repository root

// --- which version the site names --------------------------------------------

const NOTES = ["# Release notes", "", "## v0.8.2-rc2", "x", "## v0.8.2-rc1", "x", "## v0.8.1", "x",
  "## v0.8.1-rc2", "x", "## v0.8.0", "x", "### v9.9.9 is a subheading, not a release", ""].join("\n");

test("a prerelease tree names the newest stable release, not the RC", () => {
  assert.equal(latestStable(NOTES), "0.8.1");
  assert.equal(expectedSiteVersion("0.8.2-rc2", NOTES), "0.8.1");
  assert.equal(expectedSiteVersion("0.9.0-rc1", `## v0.9.0-rc1\n${NOTES}`), "0.8.1");
});

test("a stable tree names its own version, so the stable cut moves the pins", () => {
  assert.equal(expectedSiteVersion("0.8.2", `## v0.8.2\n${NOTES}`), "0.8.2");
  assert.equal(expectedSiteVersion("0.8.1", NOTES), "0.8.1");
});

test("a prerelease with no stable behind it, or one already released, is refused", () => {
  assert.throws(() => expectedSiteVersion("0.8.2-rc1", "## v0.8.2-rc1\n"), /no stable section/);
  assert.throws(() => expectedSiteVersion("0.8.2-rc1", `## v0.8.2\n${NOTES}`),
    /must build toward an unreleased version/);
});

test("this tree's site version is a stable release", () => {
  assert.match(siteVersion(REPO), /^\d+\.\d+\.\d+$/);
});

// --- nothing served mentions a release candidate ------------------------------

const SERVED = ["public", ".well-known", "src"].map((dir) => join(root, dir));
const TEXT = new Set([".html", ".md", ".txt", ".json", ".css", ".svg", ".xml", ".js", ".mjs", ".ts"]);

/** Each pattern names one way a page can point a reader at a release candidate. */
const RC_GUIDANCE = [
  [/@rc\b/i, "an @rc npm install"],
  [/(?:--tag|dist-?tags?)\W{0,3}rc\b|\brc\W{0,3}(?:dist-?tag|npm|channel)\b/i, "the rc npm dist-tag"],
  [/\bnpm\b[^\n]{0,80}[\s@:"'`]rc\b/i, "an npm rc install"],
  [/\d+\.\d+\.\d+-rc/i, "a release-candidate version"],
  [/-rc\.?\d+\b/i, "a -rcN suffix"],
  [/\bpre-?releases?\b/i, "pre-release guidance"],
  [/\brelease[- ]candidates?\b/i, "release-candidate guidance"],
];

/** Release-candidate mentions in a text, one per offending line. */
function rcMentions(text) {
  const found = [];
  text.split("\n").forEach((line, index) => {
    for (const [pattern, what] of RC_GUIDANCE) {
      if (pattern.test(line)) {
        found.push(`${index + 1}: ${what}: ${line.trim().slice(0, 160)}`);
        break;
      }
    }
  });
  return found;
}

function walk(dir) {
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walk(full));
    else if (TEXT.has(extname(entry.name).toLowerCase())) out.push(full);
  }
  return out;
}

test("the scanner catches each way a page can name a release candidate", () => {
  for (const bad of [
    "npm install -g maxplayer@rc",
    "npx maxplayer@rc --version",
    "npm i -g maxplayer --tag rc",
    "the rc dist-tag carries the newest build",
    "npm install -g maxplayer rc",
    "This page matches maxplayer 0.8.2-rc1.",
    "git checkout v0.8.2-rc1",
    "ghcr.io/makeprisms/maxplayer-sandbox:v0.8.2-rc.1",
    "Install the pre-release to try it.",
    "the newest prerelease",
    "Try the release candidate first.",
  ]) {
    assert.equal(rcMentions(bad).length, 1, `the scanner missed: ${bad}`);
  }
  for (const fine of [
    "npm install -g maxplayer",
    "git checkout v0.8.1",
    "the source (src/) and the rcfile",
    "a sorcerer's arc",
    "Release notes for v0.8.1",
  ]) {
    assert.deepEqual(rcMentions(fine), [], `the scanner flagged a stable line: ${fine}`);
  }
});

test("no served page mentions or relies on a release candidate", () => {
  let scanned = 0;
  const problems = [];
  for (const dir of SERVED) {
    for (const file of walk(dir)) {
      scanned += 1;
      for (const hit of rcMentions(readFileSync(file, "utf8"))) {
        problems.push(`${relative(root, file)}:${hit}`);
      }
    }
  }
  assert.ok(scanned > 20, `only ${scanned} served text files were scanned; the walk is broken`);
  assert.deepEqual(problems, [],
    "the website names stable releases only; release candidates belong in RELEASE_NOTES.md and "
    + `GitHub Releases:\n${problems.join("\n")}`);
});
