/**
 * The seat capability advertisement (#784), from wire tag to Profile row.
 *
 * Two things are under test and they fail differently. The READ is a spelling
 * contract with the publisher: the tag names below are typed as raw literals,
 * never imported, so a change to what `events.ts` looks for goes red here
 * rather than silently rendering an empty Profile. The DISPLAY is a claim about
 * what a buyer sees — which values wear "operator-declared" — and that is
 * asserted against rendered markup, because a source-text grep cannot tell a
 * marker that reaches the page from one that is built and dropped.
 *
 * ⚠ SCOPE OF THE SPELLING PIN. It pins THIS side only. Nothing in web/app can
 * go red on a rename in `crates/maxplayer-core/src/heartbeat.rs`, so a
 * publisher-side rename still lands as blank rows and a green suite. That
 * cross-check belongs beside the emitter and cannot be written until the
 * emitter is on main — see the PR.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import { parseEvent, type RawEvent } from "../src/model/events.js";
import { HEARTBEAT } from "../src/model/kinds.js";
import { sellerBoard } from "../src/market/participants.js";
import { capabilityRows, profileRowHtml } from "../src/ui/docks.js";

const SELLER = "b".repeat(64);
// parseEvent memoizes by id, so every fixture event needs its own.
let idCounter = 0x7840;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");

const beat = (created_at: number, tags: string[][]): RawEvent =>
  ({ id: id(), kind: HEARTBEAT, pubkey: SELLER, created_at, tags: [["d", "maxplayer-seller"], ...tags] });

const T0 = 1_700_000_000;
const rowOf = (rows: ReturnType<typeof capabilityRows>, label: string) =>
  rows.find(([k]) => k === label);

test("every #784 field is read off the heartbeat under its wire spelling", () => {
  // Literals on purpose. Importing the names events.ts uses would make this
  // assert that a string equals itself.
  const p = parseEvent(beat(T0, [
    ["harness_family", "claude-code", "codex"],
    ["harness_model", "claude-code", "claude-opus-4"],
    ["capabilities", "node", "rust"],
    ["harness_variant", "fork-of-something"],
    ["hardware", "mac studio, 64GB"],
  ]));

  assert.deepEqual(p?.harnessFamilies, ["claude-code", "codex"]);
  assert.deepEqual(p?.harnessModels, [{ family: "claude-code", model: "claude-opus-4" }]);
  assert.deepEqual(p?.capabilities, ["node", "rust"]);
  assert.equal(p?.harnessVariant, "fork-of-something");
  assert.equal(p?.hardware, "mac studio, 64GB");
});

test("a seat serving two harnesses keeps BOTH models", () => {
  // `harness_model` is one tag per pair, repeated. The tag reader used by every
  // other multi-value field returns the cells of the FIRST tag with a name, so
  // reading models that way drops all but one — and the loss is invisible: one
  // model renders, and a seat with two harnesses looks like a seat with one.
  const p = parseEvent(beat(T0, [
    ["harness_model", "claude-code", "claude-opus-4"],
    ["harness_model", "codex", "gpt-5"],
  ]));
  assert.deepEqual(p?.harnessModels, [
    { family: "claude-code", model: "claude-opus-4" },
    { family: "codex", model: "gpt-5" },
  ]);
});

test("a half-written model pair is dropped, not rendered under a guessed family", () => {
  const p = parseEvent(beat(T0, [
    ["harness_model", "claude-code"],
    ["harness_model", "", "gpt-5"],
    ["harness_model", "codex", "gpt-5"],
  ]));
  assert.deepEqual(p?.harnessModels, [{ family: "codex", model: "gpt-5" }]);
});

test("an unstated capability produces no row at all", () => {
  // A seat may honestly state nothing — the stock Docker runtime image proves
  // no tokens. An empty row would read as a measured zero instead of silence.
  const p = parseEvent(beat(T0, []));
  assert.deepEqual(p?.capabilities, []);
  assert.deepEqual(p?.harnessFamilies, []);
  assert.deepEqual(p?.harnessModels, []);
  assert.equal(p?.harnessVariant, null);
  assert.equal(p?.hardware, null);

  const stated = capabilityRows({ capabilities: [], harnessFamilies: [], harnessModels: [] })
    .filter(([, v]) => v != null && v !== "");
  assert.deepEqual(stated, []);
});

test("the row set comes from ONE beat, never merged across beats", () => {
  // Merged, a row would show yesterday's harness beside today's probe and say
  // nothing about the seam. The newest beat replaces the whole advertisement.
  const rows = sellerBoard([
    beat(T0, [["capabilities", "rust"], ["hardware", "old box"], ["harness_family", "codex"]]),
    beat(T0 + 60, [["capabilities", "node"]]),
  ], T0 + 120);

  const seat = rows.find((r) => r.pubkey === SELLER);
  assert.deepEqual(seat?.capabilities, ["node"]);
  assert.equal(seat?.hardware, null, "the newer beat states no hardware — the older value must not survive");
  assert.deepEqual(seat?.harnessFamilies, []);
});

test("an older beat arriving late never overwrites the current advertisement", () => {
  // Relay pages do not promise order, so the out-of-order case is the real one.
  const rows = sellerBoard([
    beat(T0 + 60, [["capabilities", "node"]]),
    beat(T0, [["capabilities", "rust"]]),
  ], T0 + 120);
  assert.deepEqual(rows.find((r) => r.pubkey === SELLER)?.capabilities, ["node"]);
});

test("operator-typed rows are marked and probed rows are not", () => {
  const rows = capabilityRows({
    harnessFamilies: ["codex"],
    harnessModels: [{ family: "codex", model: "gpt-5" }],
    capabilities: ["rust"],
    harnessVariant: "some fork",
    hardware: "mac studio",
  });

  for (const label of ["Harness variant", "Hardware"]) {
    const row = rowOf(rows, label);
    assert.equal(row?.[2]?.mark, "operator-declared", `${label} is operator-typed and must say so`);
    assert.match(profileRowHtml(row!), /class="unverified">operator-declared</);
  }

  for (const label of ["Capabilities", "Harness family", "Harness model"]) {
    const row = rowOf(rows, label);
    assert.equal(row?.[2]?.mark, undefined, `${label} is machine-sourced — marking it would say the opposite`);
    assert.doesNotMatch(profileRowHtml(row!), /unverified/);
  }
});

test("a capability token carries the contract it actually buys", () => {
  // The token means its probe command resolved in the job environment. A buyer
  // commits sats on this row, so "necessary, not sufficient" is on the page and
  // not left to the reader's inference.
  const row = rowOf(capabilityRows({ capabilities: ["rust"] }), "Capabilities");
  assert.match(row?.[2]?.title ?? "", /not sufficient/);
  assert.match(profileRowHtml(row!), /title="[^"]*not sufficient/);
});

test("hostile free text in an unverified field cannot escape its row", () => {
  // `hardware` and `harness_variant` are arbitrary text from an open relay.
  // They are the injection path an enum-bound token is not.
  const payload = '"><img src=x onerror=alert(1)>';
  const p = parseEvent(beat(T0, [["hardware", payload]]));
  assert.equal(p?.hardware, payload, "the model preserves free text verbatim");

  const html = profileRowHtml(rowOf(capabilityRows({ hardware: payload }), "Hardware")!);
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
});
