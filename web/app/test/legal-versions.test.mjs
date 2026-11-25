// Legal versions manifest (/legal/versions.json). The registration service accepts a
// terms/privacy version it does not know when this file lists it, and `maxplayer account link`
// offers `current`. So publishing a new version is: add the dated page, its rewrite, and its date
// here. These checks keep the file in step with the pages that actually exist.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const manifest = JSON.parse(readFileSync(join(root, "public", "legal", "versions.json"), "utf8"));
const vercel = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
const DOCS = ["terms", "privacy"];

test("the manifest has exactly current, terms and privacy, all as 8-digit dates", () => {
  assert.deepEqual(Object.keys(manifest).sort(), ["current", "privacy", "terms"]);
  assert.deepEqual(Object.keys(manifest.current).sort(), ["privacy", "terms"]);
  for (const doc of DOCS) {
    const list = manifest[doc];
    assert.ok(Array.isArray(list) && list.length > 0, doc);
    for (const v of list) assert.match(v, /^\d{8}$/, `${doc} ${v}`);
    assert.deepEqual([...list].sort(), list, `${doc} is sorted oldest first`);
    assert.equal(new Set(list).size, list.length, `${doc} has no duplicates`);
    assert.equal(manifest.current[doc], list.at(-1), `current ${doc} is the newest listed`);
  }
});

test("every listed version has its page and rewrite, and every dated page is listed", () => {
  const pages = readdirSync(join(root, "public"));
  for (const doc of DOCS) {
    for (const v of manifest[doc]) {
      assert.ok(pages.includes(`${doc}-${v}.html`), `public/${doc}-${v}.html exists`);
      assert.ok(
        vercel.rewrites.some((r) => r.source === `/${doc}/${v}` && r.destination === `/${doc}-${v}`),
        `vercel.json rewrites /${doc}/${v}`,
      );
    }
    const dated = pages.filter((p) => new RegExp(`^${doc}-\\d{8}\\.html$`).test(p)).map((p) => p.slice(doc.length + 1, -5));
    assert.deepEqual(dated.sort(), manifest[doc], `every public/${doc}-<date>.html is listed`);
  }
});

test("/terms and /privacy redirect to the current version", () => {
  for (const doc of DOCS) {
    const r = vercel.redirects.find((r) => r.source === `/${doc}`);
    assert.equal(r?.destination, `/${doc}/${manifest.current[doc]}`, doc);
  }
});
