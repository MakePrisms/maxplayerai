/**
 * The seat capability advertisement (#784), from wire tag to Profile row.
 *
 * Two things are under test and they fail differently. The READ is a spelling
 * contract with the publisher: the tag names below are typed as raw literals,
 * never imported, so a change to what `events.ts` looks for goes red here
 * rather than silently rendering an empty Profile. The DISPLAY is a claim about
 * what a buyer sees — which mark each value wears — and that is asserted
 * against rendered markup, because a source-text grep cannot tell a marker that
 * reaches the page from one that is built and dropped.
 *
 * ⚠ SCOPE OF THE SPELLING PIN. It pins THIS side only. Nothing in web/app goes
 * red on a rename in `crates/maxplayer-core/src/heartbeat.rs`, so a
 * publisher-side rename lands as blank rows over a green suite. That
 * cross-check belongs beside the emitter and is tracked as #888.
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

test("every capability row wears the mark its provenance earns", () => {
  const rows = capabilityRows({
    harnessFamilies: ["codex"],
    harnessModels: [{ family: "codex", model: "gpt-5" }],
    capabilities: ["rust"],
    harnessVariant: "some fork",
    hardware: "mac studio",
  });

  // Operator-typed: nothing measured these and no filter reads them.
  for (const label of ["Harness variant", "Hardware"]) {
    const row = rowOf(rows, label);
    assert.equal(row?.[2]?.mark, "operator-declared", `${label} is operator-typed and must say so`);
    assert.match(profileRowHtml(row!), /class="unverified">operator-declared</);
  }

  // Machine-sourced, and NOT interchangeable. Protocol §4.5.3: one enforcement,
  // one echo, one silence. A reader must not read three grades of one proof.
  // Tuple-typed, not inferred: destructuring a `string[][]` yields
  // `string | undefined` under noUncheckedIndexedAccess.
  const filterable: [string, string][] = [
    ["Harness family", "enforced at dispatch"],
    ["Harness model", "last observed"],
    ["Capabilities", "as of seat start"],
  ];
  for (const [label, mark] of filterable) {
    const row = rowOf(rows, label);
    assert.equal(row?.[2]?.mark, mark, `${label} must state its own provenance`);
    assert.match(profileRowHtml(row!), new RegExp(`class="provenance">${mark}<`));
    // The operator marker on a measured value would say the opposite.
    assert.doesNotMatch(profileRowHtml(row!), /unverified/);
  }

  // The trichotomy is the point: the three must DIFFER. One mark shared across
  // them satisfies every assertion above and still presents the equal-grades
  // reading the protocol forbids, so the distinctness is asserted separately.
  const marks = filterable.map(([label]) => rowOf(rows, label)?.[2]?.mark);
  assert.equal(new Set(marks).size, 3, "the three filterable rows must not share a mark");
});

test("the capability row states its staleness bound and its over-claim direction", () => {
  // §4.5.4: probed ONCE at seat start and republished on every beat since, so
  // the bound is the seat's uptime and a recent beat proves nothing about when
  // it was measured. The drift is not symmetric — a REMOVED toolchain keeps
  // being advertised, nothing on the filter path catches it, and a buyer
  // commits sats on this row.
  const html = profileRowHtml(rowOf(capabilityRows({ capabilities: ["rust"] }), "Capabilities")!);
  assert.match(html, /seat started/);
  assert.match(html, /NOT evidence of a recent measurement/);
  assert.match(html, /over-claim/);
  assert.match(html, /not sufficient/);
});

test("the model row never states an execution fact", () => {
  // The emitter names this defect family: any wording that upgrades this
  // SELF-REPORT into an EXECUTION FACT is the same error, and it has been
  // written in all three tenses already. The tense was never the error.
  //
  // ⚠ The forbidden list below is an INCLUSION filter. It pins the three shapes
  // the emitter enumerated and CANNOT prove a new wording is safe — a novel
  // overclaim passes it. The load-bearing assertion is the positive one: the
  // title must say what the value actually is.
  const row = rowOf(capabilityRows({ harnessModels: [{ family: "codex", model: "gpt-5" }] }), "Harness model");
  const title = row?.[2]?.title ?? "";
  assert.notEqual(title, "", "positive control: an empty title would pass every doesNotMatch below");

  for (const shape of [/will use/, /is serving/, /actually ran/, /guarantees/]) {
    assert.doesNotMatch(title, shape, `states an execution fact no code here supports: ${shape}`);
  }
  assert.match(title, /last asked/, "it must say what the value actually is");
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

test("a hostile beat normalizes all five fields to stated-or-absent", () => {
  // §4.5.2: an all-whitespace value is UNSTATED, and a reader must treat it
  // exactly like a missing tag. Read raw, a `"   "` renders as a stated-but-blank
  // claim no operator typed, and for the three filterable fields it is what a
  // buyer's award filter consumes — so the display would show tokens the award
  // path does not. Matches `heartbeat.rs`'s own hostile-tag test.
  const p = parseEvent(beat(T0, [
    ["harness_family", "   ", " codex "],
    ["capabilities", "\t", " rust "],
    ["harness_model", " ", "gpt-x"],
    ["harness_model", "claude-code", "   "],
    ["harness_model", " claude-code ", "  opus  "],
    ["harness_variant", "  "],
    ["hardware", " \t "],
  ]));

  assert.deepEqual(p?.harnessFamilies, ["codex"], "a blank family states nothing; a padded one must match what a buyer names");
  assert.deepEqual(p?.capabilities, ["rust"], "a blank token would be a capability no seat can be held to");
  assert.deepEqual(p?.harnessModels, [{ family: "claude-code", model: "opus" }],
    "either half blank drops the WHOLE pair — a stated model under a blank family is not a partial answer");
  assert.equal(p?.harnessVariant, null, "present-but-blank is not a state this field has");
  assert.equal(p?.hardware, null, "a tab padded with spaces states nothing either");
});

test("padded values normalize but INTERIOR whitespace is content and survives", () => {
  // The positive control for the test above. Without it every assertion there is
  // satisfied by a reader that returns nothing at all — which would hide every
  // seat instead of only the blank fields.
  const p = parseEvent(beat(T0, [
    ["harness_family", " claude-code "],
    ["capabilities", "  rust  ", " node "],
    ["harness_variant", "  pro fork  "],
    ["hardware", " mac studio, 64GB "],
  ]));

  assert.deepEqual(p?.harnessFamilies, ["claude-code"]);
  assert.deepEqual(p?.capabilities, ["rust", "node"]);
  assert.equal(p?.harnessVariant, "pro fork", "the space between words is content");
  assert.equal(p?.hardware, "mac studio, 64GB", "only the edges are noise");
});

test("a beat stating only blanks produces no capability rows at all", () => {
  // End to end, because the row is what a buyer reads. An unstated field must
  // render as silence, not as a row containing a space.
  const rows = sellerBoard([beat(T0, [
    ["harness_family", "   "],
    ["capabilities", " "],
    ["harness_model", "  ", " "],
    ["harness_variant", "\t"],
    ["hardware", "  "],
  ])], T0 + 60);

  const seat = rows.find((r) => r.pubkey === SELLER);
  const shown = capabilityRows(seat ?? null).filter(([, v]) => v != null && v !== "");
  assert.deepEqual(shown, [], "every field is unstated, so the Profile shows no capability rows");
});

test("normalization is capability-scoped — agents and accepted_mints are untouched", () => {
  // ⛔ THE CONSTRAINT, not a nicety. `tagValues` and `firstTag` are shared with
  // the mint list, and a mint string is matched elsewhere; changing how it parses
  // as a side effect of a capability fix is a different change with a different
  // blast radius. `stated` is applied at the five #784 readers ONLY, and this
  // test fails if someone later "tidies" it into the shared helper.
  const p = parseEvent(beat(T0, [
    ["accepted_mints", "  https://mint.a  ", "", "https://mint.b"],
    ["agents", " claude ", "  "],
    ["capabilities", " rust "],
  ]));

  assert.deepEqual(p?.acceptedMints, ["  https://mint.a  ", "https://mint.b"],
    "mint values keep their padding — only the zero-length cell drops, as before");
  assert.deepEqual(p?.agents, [" claude ", "  "],
    "agents keeps both padding AND its whitespace-only cell, exactly as before");
  assert.deepEqual(p?.capabilities, ["rust"], "positive control: the capability reader IS normalizing in the same parse");
});
