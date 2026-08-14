/**
 * src/model/kinds.ts says: "a renumber stays a one-file change ... and a test
 * enforces it." This is that test. Without it the sentence was a claim the
 * codebase did not keep — and it was already broken, in the baker.
 */
import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import {
  ACCEPT, AWARD, CLAIM, FEEDBACK, HEARTBEAT, MAXPLAYER_TAGGED_KINDS, OFFER, PROFILE, RECEIPT, RESULT,
} from "../src/model/kinds.js";
import { PROFILE_KIND, TAGGED_KINDS } from "../scripts/bake-snapshot.mjs";

const APP = join(dirname(fileURLToPath(import.meta.url)), "..");

/** The canonical file, and the one documented exception (see its comment). */
const CANONICAL = join("src", "model", "kinds.ts");
const ALLOWED = new Set([join("scripts", "bake-snapshot.mjs")]);

function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) out.push(...sourceFiles(full));
    else if (/\.(ts|mjs)$/.test(entry)) out.push(full);
  }
  return out;
}

test("kind numbers appear in exactly one file", () => {
  // The distinctive ones only. PROFILE is 0, and scanning for a bare 0 would
  // match arithmetic everywhere — it gets the shape-based check below instead.
  const distinctive = [OFFER, CLAIM, RESULT, FEEDBACK, AWARD, ACCEPT, RECEIPT, HEARTBEAT];
  const pattern = new RegExp(`\\b(${distinctive.join("|")})\\b`);

  const offenders: string[] = [];
  for (const file of [...sourceFiles(join(APP, "src")), ...sourceFiles(join(APP, "scripts"))]) {
    const rel = relative(APP, file);
    if (rel === CANONICAL || ALLOWED.has(rel)) continue;
    const hit = readFileSync(file, "utf8").split("\n").findIndex((line) => pattern.test(line));
    if (hit >= 0) offenders.push(`${rel}:${hit + 1}`);
  }
  assert.deepEqual(offenders, [], `kind numbers belong in ${CANONICAL} — found literals in ${offenders.join(", ")}`);
});

test("a relay filter never names a kind by a bare number", () => {
  // Catches PROFILE (0), which the numeric scan above cannot see. `kinds: [0]`
  // is exactly how the baker's profile stream was written.
  const offenders: string[] = [];
  for (const file of [...sourceFiles(join(APP, "src")), ...sourceFiles(join(APP, "scripts"))]) {
    const rel = relative(APP, file);
    if (rel === CANONICAL) continue;
    for (const [i, line] of readFileSync(file, "utf8").split("\n").entries()) {
      if (/kinds:\s*\[\s*\d/.test(line)) offenders.push(`${rel}:${i + 1}`);
    }
  }
  assert.deepEqual(offenders, [], `a filter names kinds by constant, never by number: ${offenders.join(", ")}`);
});

test("the baker's duplicated kind list cannot drift from the canonical one", () => {
  // The one place a literal is allowed, because it runs without a TS loader.
  // Allowed, therefore pinned.
  assert.deepEqual([...TAGGED_KINDS], [...MAXPLAYER_TAGGED_KINDS], "the baker's tagged kinds match kinds.ts");
  assert.equal(PROFILE_KIND, PROFILE, "and its profile kind does too");
});
