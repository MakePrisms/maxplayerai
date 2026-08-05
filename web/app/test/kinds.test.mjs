import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import * as kinds from "../js/kinds.js";

const ROOT = fileURLToPath(new URL("..", import.meta.url));

/**
 * Kind numbers must live in exactly one file so a renumber is a one-file change.
 * Kind 0 is a NIP-01 standard that will not move and reads as an index
 * everywhere, so it is not gated by digits — it is still routed via PROFILE.
 */
const RENUMBERABLE = [3400, 3401, 3402, 3403, 3404, 3405, 30340, 31990];

/**
 * Retired DVM kinds from mobee's earlier protocol. The app is a clean cut from
 * that era and does not read it, so a stray digit is always a bug — including
 * in js/kinds.js itself.
 */
const RETIRED = [5109, 6109, 7000];

const stripComments = (src) =>
  src.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");

function sourceFiles(dir, acc = []) {
  for (const name of readdirSync(dir)) {
    const full = join(dir, name);
    if (statSync(full).isDirectory()) sourceFiles(full, acc);
    else if (name.endsWith(".js") || name.endsWith(".mjs")) acc.push(full);
  }
  return acc;
}

test("every kind the app touches is a named constant", () => {
  assert.equal(kinds.PROFILE, 0);
  assert.equal(kinds.RECEIPT, 3400);
  assert.equal(kinds.OFFER, 3401);
  assert.equal(kinds.CLAIM, 3402);
  assert.equal(kinds.RESULT, 3403);
  assert.equal(kinds.FEEDBACK, 3404);
  assert.equal(kinds.AWARD, 3405);
  assert.equal(kinds.HEARTBEAT, 30340);
  assert.equal(kinds.HANDLER, 31990);
});

test("no kind literal appears outside js/kinds.js", () => {
  const offenders = [];
  for (const file of sourceFiles(join(ROOT, "js"))) {
    if (file.endsWith(join("js", "kinds.js"))) continue;
    const src = stripComments(readFileSync(file, "utf8"));
    for (const kind of RENUMBERABLE) {
      if (new RegExp(`\\b${kind}\\b`).test(src)) offenders.push(`${file}: ${kind}`);
    }
  }
  assert.deepEqual(offenders, [], "import the named constant instead");
});

test("no retired kind appears anywhere in the app source", () => {
  const offenders = [];
  for (const file of sourceFiles(join(ROOT, "js"))) {
    const src = stripComments(readFileSync(file, "utf8"));
    for (const kind of RETIRED) {
      if (new RegExp(`\\b${kind}\\b`).test(src)) offenders.push(`${file}: ${kind}`);
    }
  }
  assert.deepEqual(offenders, [], "the app is a clean cut from the retired protocol");
});

test("gift-wrap is never requested", () => {
  const requested = new Set([...kinds.MAXPLAYER_TAGGED_KINDS, ...kinds.UNTAGGED_KINDS]);
  assert.equal(requested.has(1059), false, "gift-wrapped payment traffic stays dark");
});

test("every trade stage maps to a kind the client actually requests", () => {
  for (const kind of Object.keys(kinds.TRADE_STAGES)) {
    assert.ok(
      kinds.MAXPLAYER_TAGGED_KINDS.includes(Number(kind)),
      `kind ${kind} is staged but never fetched`,
    );
  }
});
