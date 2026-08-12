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
const RENUMBERABLE = [3400, 3401, 3402, 3403, 3404, 3405, 30340];

/**
 * Retired DVM kinds from maxplayer's earlier protocol. The app is a clean cut from
 * that era and does not read it, so a stray digit is always a bug — including
 * in js/kinds.js itself.
 */
const RETIRED = [5109, 6109, 7000, 31990];

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

/**
 * REGRESSION (#449): a kind the board BRANCHES ON must be a kind the client
 * asks the relay for.
 *
 * The seller board grew a `p.kind === PROFILE` arm when the seat name moved to
 * kind-0, and every other layer followed — the parser, the replaceable-slot
 * cache, the row field. The one thing nobody added was kind 0 to a requested
 * list, so the arm was live, correct and unreachable: every seller card fell
 * back to the short pubkey while kind-0 sat on the relay resolving fine.
 *
 * The branch list is read out of the source rather than written here on
 * purpose. A hand-kept list is updated by whoever remembers, which is the same
 * person who would have remembered to widen the subscription.
 */
test("REGRESSION: every kind the participant board branches on is requested", () => {
  const src = stripComments(readFileSync(join(ROOT, "js", "participants.js"), "utf8"));
  const branched = [...src.matchAll(/\bkind\s*===\s*([A-Z_][A-Z0-9_]*)/g)].map((m) => m[1]);
  assert.ok(branched.length > 0, "found no kind branches — the regex stopped matching the source");

  const requested = new Set([...kinds.MAXPLAYER_TAGGED_KINDS, ...kinds.UNTAGGED_KINDS]);
  for (const name of new Set(branched)) {
    const kind = kinds[name];
    assert.equal(typeof kind, "number", `${name} is branched on but is not a kind constant`);
    assert.ok(
      requested.has(kind),
      `the board reads ${name} (kind ${kind}) but the client never requests it`,
    );
  }
});
