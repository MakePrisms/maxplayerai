/**
 * #784 seat capability: the reader against the emitter's own signed output.
 *
 * `test/fixtures/golden-kind30340.json` is a REAL signed kind-30340, written by the emitter itself
 * (`MAXPLAYER_WRITE_GOLDEN_30340` in maxplayer-core's `heartbeat.rs`), not a hand-written shape.
 * That is the whole reason it is worth having, and it is why nothing here asserts on `id`, `sig`,
 * `pubkey` or `created_at` — those vary per emission, and pinning one would make the fixture
 * synthetic while still looking like evidence.
 *
 * ⚠ This file carries more weight than a normal fixture test. `parse_heartbeat` has NO Rust
 * production caller — every call site is inside the emitter's own test module — so the beat's real
 * readers are here, in JS. This fixture is the only artifact tying the emitter to its actual
 * consumer, and a green here is the only thing that says the two still agree.
 *
 * ⛔ It cannot detect emitter drift on its own. The golden is rewritten only when that env var is
 * set, so the emitter can move while the file stands still. The staleness guard is the emitter
 * side's half, running by default; this half proves only that the reader matches the artifact.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { readFileSync } from "node:fs";
import { harnessFamilyFromId, parseEvent, parseSeatCapability, wireFamilyFromId } from "../js/parse.js";
import { SEAT_DISPLAY_ONLY_TAGS, SEAT_FILTERABLE_TAGS } from "../js/kinds.js";
import { verifyAdvertisedHarnesses } from "../js/profiles.js";

const goldenRaw = readFileSync(new URL("./fixtures/golden-kind30340.json", import.meta.url), "utf8");
const golden = JSON.parse(goldenRaw);

test("the golden kind-30340 decodes to the capability the emitter signed", () => {
  const ev = parseEvent(golden);
  assert.equal(ev.role, "heartbeat", "a kind-30340 must parse as a heartbeat");
  const cap = ev.heartbeat.capability;

  assert.deepEqual(cap.filterable.harness_family, ["claude-code", "codex"]);
  assert.deepEqual(cap.filterable.capabilities, ["rust", "node"]);
  assert.deepEqual(cap.filterable.harness_model, [
    { family: "claude-code", model: "claude-opus-5" },
    { family: "codex", model: "gpt-5.6-sol[low]" },
  ]);
  assert.equal(cap.displayOnly.harness_variant, "my-fork");
  assert.equal(cap.displayOnly.hardware, "mac studio, 64GB");

  // The joined view a reader actually renders.
  assert.deepEqual(cap.harnesses, [
    { family: "claude-code", models: ["claude-opus-5"] },
    { family: "codex", models: ["gpt-5.6-sol[low]"] },
  ]);
  assert.deepEqual(cap.orphanModels, []);

  // `agents` is a THIRD vocabulary and is not the same as `harness_family`: the golden advertises
  // `["agents","claude"]` beside `["harness_family","claude-code","codex"]`. A reader that treats
  // the two as one list reports a roster no seat sent.
  assert.deepEqual(ev.heartbeat.agents, ["claude"]);
});

test("the fixture is a real emission: the volatile fields carry no assertions", () => {
  // Positive control on the claim in this file's header. Rewrite every per-emission field and the
  // capability reading must be untouched — if any assertion above depended on `id`, `sig`,
  // `pubkey` or `created_at`, this goes red and the fixture was synthetic in effect.
  const shifted = {
    ...golden,
    id: "f".repeat(64),
    sig: "0".repeat(128),
    pubkey: "a".repeat(64),
    created_at: golden.created_at + 86400,
  };
  const before = parseEvent(golden).heartbeat.capability;
  const after = parseEvent(shifted).heartbeat.capability;
  assert.deepEqual(after, before, "capability must be a function of the tags alone");
});

test("harness_model REPEATS: reading it as one list loses every model past the first", () => {
  // The trap this shape exists to spring. `harness_family` and `capabilities` are one tag holding N
  // values; `harness_model` is N tags each holding a PAIR. A reader that applies the list shape to
  // the pair tag sees "a family plus a model" in the first tag and never looks at the second.
  const tags = golden.tags;
  const modelTags = tags.filter((t) => t[0] === "harness_model");
  assert.equal(modelTags.length, 2, "the emitter sent one tag per model, not one tag for both");

  const asIfOneList = modelTags[0].slice(1); // the wrong reader, spelled out
  assert.deepEqual(asIfOneList, ["claude-code", "claude-opus-5"]);
  assert.equal(
    asIfOneList.length,
    2,
    "which is why it looks plausible: two strings, exactly like a two-value list",
  );

  const correct = parseSeatCapability(tags).filterable.harness_model;
  assert.equal(correct.length, 2, "the pair reader keeps both models");
  assert.notDeepEqual(correct.map((m) => m.model), ["claude-opus-5"], "the second model must survive");
  // And each pair carries its OWN family, so nothing is positional.
  assert.equal(correct[1].family, "codex");
  assert.equal(correct[1].model, "gpt-5.6-sol[low]");
});

test("a harness with no model reads as an empty model list, not as a shifted pairing", () => {
  // Charter acceptance: a harness with no model must render as ABSENT, never as a blank that looks
  // like a value. Three families, two models, and the MIDDLE family names none. Under a positional
  // encoding every pair after the gap re-attributes silently — cursor would inherit codex's model.
  const cap = parseSeatCapability([
    ["harness_family", "claude-code", "cursor", "codex"],
    ["harness_model", "claude-code", "claude-opus-5"],
    ["harness_model", "codex", "gpt-5.6-sol[low]"],
  ]);
  assert.deepEqual(cap.harnesses, [
    { family: "claude-code", models: ["claude-opus-5"] },
    { family: "cursor", models: [] },
    { family: "codex", models: ["gpt-5.6-sol[low]"] },
  ]);
  // The gap is an empty list and NOT a null, and not the next family's model.
  const cursor = cap.harnesses.find((h) => h.family === "cursor");
  assert.deepEqual(cursor.models, []);
  assert.notEqual(cursor.models, null, "an absent model list must still be a list to render from");
  assert.equal(cap.harnesses[2].models[0], "gpt-5.6-sol[low]", "codex keeps its own model");
});

test("a model naming an unadvertised family is surfaced, not dropped", () => {
  // Dropping it would make the reader disagree with the wire while looking complete.
  const cap = parseSeatCapability([
    ["harness_family", "claude-code"],
    ["harness_model", "claude-code", "claude-opus-5"],
    ["harness_model", "codex", "gpt-5.6-sol[low]"],
  ]);
  assert.deepEqual(cap.harnesses, [{ family: "claude-code", models: ["claude-opus-5"] }]);
  assert.deepEqual(cap.orphanModels, [{ family: "codex", model: "gpt-5.6-sol[low]" }]);
});

test("hardware is unreachable from the filterable surface", () => {
  // Charter acceptance: hardware renders but provably never reaches a filter predicate. Asserted
  // against the WHOLE filterable object rather than against named keys, so a field added later is
  // covered by construction — a test listing today's keys would silently stop covering a new one.
  const cap = parseSeatCapability([
    ["harness_variant", "my-fork"],
    ["hardware", "mac studio, 64GB"],
  ]);
  const filterableValues = Object.values(cap.filterable).flat();
  assert.deepEqual(
    filterableValues,
    [],
    "a seat stating ONLY display-only fields must expose nothing filterable",
  );
  // And the display-only values really were read — otherwise the emptiness above proves nothing.
  assert.equal(cap.displayOnly.hardware, "mac studio, 64GB");
  assert.equal(cap.displayOnly.harness_variant, "my-fork");

  // The two sets are disjoint, and neither is empty. A shared name would put a display-only field
  // inside the filter's input without any assertion above noticing.
  const overlap = SEAT_FILTERABLE_TAGS.filter((t) => SEAT_DISPLAY_ONLY_TAGS.includes(t));
  assert.deepEqual(overlap, []);
  assert.ok(SEAT_FILTERABLE_TAGS.length > 0 && SEAT_DISPLAY_ONLY_TAGS.length > 0);
  // Every filterable tag name is a key of the filterable shape, so the constant list and the parsed
  // shape cannot drift apart in silence.
  for (const name of SEAT_FILTERABLE_TAGS) {
    assert.ok(name in cap.filterable, `${name} is filterable but absent from the parsed shape`);
  }
  for (const name of SEAT_DISPLAY_ONLY_TAGS) {
    assert.ok(name in cap.displayOnly, `${name} is display-only but absent from the parsed shape`);
  }
});

test("value text is never parsed: brackets and commas survive verbatim", () => {
  // `gpt-5.6-sol[low]` carries brackets; `mac studio, 64GB` carries a comma AND a space. A
  // comma-splitting reader turns the hardware into two fields and looks like it worked.
  const cap = parseSeatCapability(golden.tags);
  assert.equal(cap.displayOnly.hardware, "mac studio, 64GB");
  assert.ok(cap.displayOnly.hardware.includes(", "), "the comma and space are part of one value");
  assert.equal(cap.displayOnly.hardware.split(", ").length, 2, "which is exactly what makes splitting tempting");
  assert.equal(cap.filterable.harness_model[1].model, "gpt-5.6-sol[low]");
  assert.ok(cap.filterable.harness_model[1].model.includes("["), "brackets are part of the model id");
});

test("a malformed harness_model pair is skipped, matching the emitter's own reader", () => {
  // The Rust reader skips rather than half-decodes: "A pair with an empty family would be a model
  // no buyer could attach to a harness." A half-decoded pair here would be indistinguishable in
  // the joined view from a family that advertises no model.
  const cap = parseSeatCapability([
    ["harness_family", "claude-code"],
    ["harness_model", "claude-code"], // no model
    ["harness_model", "", "claude-opus-5"], // no family
    ["harness_model", "claude-code", ""], // empty model
    ["harness_model", "claude-code", "claude-opus-5"], // the only well-formed one
  ]);
  assert.deepEqual(cap.filterable.harness_model, [
    { family: "claude-code", model: "claude-opus-5" },
  ]);
  assert.deepEqual(cap.orphanModels, [], "a skipped pair is not an orphan either");
});

/**
 * A minimal stand-in for the two DOM calls `el`/`text` make. This suite has no DOM, and the
 * property under test is what the cell BUILDS — tag, class and text — so a recording stub answers
 * it directly.
 *
 * ⚠ Bound worth stating: this is not a DOM. It proves the cell emits the class and the visible
 * string it claims to; it cannot prove anything about layout, or about how a browser renders what
 * was emitted. It is used only for assertions of that first kind.
 */
function domStub() {
  const mk = (tag) => ({
    tag,
    className: "",
    dataset: {},
    style: {},
    kids: [],
    setAttribute() {},
    append(c) {
      this.kids.push(c);
    },
  });
  return {
    createElement: (tag) => mk(tag),
    createTextNode: (s) => ({ tag: "#text", value: String(s), kids: [] }),
  };
}

/** Every string a built cell would show a human, concatenated in order. */
function shownText(node) {
  if (node.tag === "#text") return node.value;
  return node.kids.map(shownText).join("");
}

/** Every class name anywhere in the built cell. */
function classes(node) {
  const out = node.className ? [node.className] : [];
  for (const k of node.kids) out.push(...classes(k));
  return out;
}

function withDom(fn) {
  const prev = globalThis.document;
  globalThis.document = domStub();
  try {
    return fn();
  } finally {
    if (prev === undefined) delete globalThis.document;
    else globalThis.document = prev;
  }
}

test("RENDER: a harness with no model shows an explicit absence, not an empty cell", async () => {
  // Charter acceptance, at the render rather than in the data: "absent, not blank-that-looks-like-a
  // -value". An empty cell is exactly the blank this must not be.
  const { capabilityCell } = await import("../js/views.js");
  const cap = parseSeatCapability([
    ["harness_family", "claude-code", "cursor"],
    ["harness_model", "claude-code", "claude-opus-5"],
  ]);
  const cell = withDom(() => capabilityCell(cap));
  const shown = shownText(cell);

  assert.match(shown, /cursor · no model stated/, "the gap must say so in words");
  assert.match(shown, /claude-code · claude-opus-5/, "the stated model still shows");
  assert.ok(shown.trim().length > 0, "a cell that renders nothing is the failure this test exists for");
  assert.ok(
    classes(cell).includes("unidentified"),
    "the absence carries the muted style, so it cannot be misread as a value",
  );
});

test("RENDER: a seat with no capability tags at all says so", async () => {
  const { capabilityCell, displayOnlyCell } = await import("../js/views.js");
  const empty = parseSeatCapability([["d", "maxplayer-seller"]]);
  assert.equal(shownText(withDom(() => capabilityCell(empty))), "advertises none");
  assert.equal(shownText(withDom(() => displayOnlyCell(empty))), "not stated");
  // And a null capability — a beat this reader could not decode — is not the same statement.
  assert.equal(shownText(withDom(() => capabilityCell(null))), "no capability tags");
});

test("RENDER: hardware shows verbatim and lands in its own cell, apart from the claims", async () => {
  const { capabilityCell, displayOnlyCell } = await import("../js/views.js");
  const cap = parseSeatCapability(golden.tags);
  const hardwareShown = shownText(withDom(() => displayOnlyCell(cap)));
  assert.match(hardwareShown, /mac studio, 64GB/, "the comma and space survive to the screen");
  assert.match(hardwareShown, /my-fork/);

  // The hardware string must NOT appear in the capability cell. Same page, different column: a
  // reader who sees hardware beside the filterable capabilities reads it as selectable.
  const capabilityShown = shownText(withDom(() => capabilityCell(cap)));
  assert.ok(
    !capabilityShown.includes("mac studio"),
    "hardware in the capability cell would present a display-only field as a filterable one",
  );
  assert.match(capabilityShown, /claude-code · claude-opus-5/);
  assert.match(capabilityShown, /gpt-5\.6-sol\[low\]/, "brackets survive to the screen");
});

test("a beat with no capability tags reads as empty, never as null holes", () => {
  // Absence-stays-absence. Every field is present and empty so a renderer can distinguish "this
  // seat advertised nothing" from "this reader failed", and so `.flat()`/`.map()` downstream never
  // touch a null.
  const cap = parseSeatCapability([["d", "maxplayer-seller"], ["t", "maxplayer"]]);
  assert.deepEqual(cap.filterable.harness_family, []);
  assert.deepEqual(cap.filterable.capabilities, []);
  assert.deepEqual(cap.filterable.harness_model, []);
  assert.deepEqual(cap.harnesses, []);
  assert.deepEqual(cap.orphanModels, []);
  assert.equal(cap.displayOnly.hardware, null, "a single-value tag absent is null, not empty string");
  assert.equal(cap.displayOnly.harness_variant, null);
});

/* ═══════ verifying against harness_family: the roster the award decision reads ═══════ */

/** A seat's delivery receipt, reduced to the two fields the pairing reads. */
const receipt = (pubkey, harnessId) => ({
  role: "result",
  pubkey,
  result: { usage: { harness_id: harnessId } },
});

const SEAT = "11".repeat(32);

test("pairs a harness_family claim against a receipt in the same wire vocabulary", () => {
  // `claude-code` advertised, `claude-agent-acp` delivered. Three spellings of one harness — wire
  // family, adapter identity, and the `agents` preset label — and the comparison happens in the wire
  // vocabulary because that is the one the award decision reads.
  const v = verifyAdvertisedHarnesses([receipt(SEAT, "claude-agent-acp")], SEAT, ["claude"], ["claude-code"]);
  assert.equal(v.claims.length, 1);
  assert.equal(v.claims[0].advertised, "claude-code");
  assert.equal(v.claims[0].verdict, "agreed");
  assert.equal(v.claims[0].deliveredId, "claude-agent-acp");
  // ⚠ Asserting the MECHANISM, not just the verdict. `agreed` is reachable by two paths — the wire
  // vocabulary and the display shorthand fallback — so a test that checked only the verdict would
  // stay green with the wire comparison broken. Found exactly that way: a mutation to the wire
  // mapping left the suite green until this line existed.
  assert.equal(v.claims[0].on, "wire", "the wire vocabulary must be what matched, not the fallback");
});

test("goose is in the wire vocabulary and now reads as a family", () => {
  // The display shorthand never knew `goose`, so before this it read null and every goose seat was
  // incomparable. It reaches the wire without a code change once a goose preset is configured.
  const v = verifyAdvertisedHarnesses([receipt(SEAT, "goose-acp")], SEAT, [], ["goose"]);
  assert.equal(v.claims[0].verdict, "agreed", "a goose claim must be comparable, not incomparable");
  assert.equal(wireFamilyFromId("goose-acp"), "goose");
  assert.equal(harnessFamilyFromId("goose-acp"), null, "and the old shorthand still cannot read it");
});

test("DIVERGENCE: a preset with no wire family stays visible instead of vanishing", () => {
  // The one direction the two tags diverge. A custom [agents] entry carries its own NAME in `agents`
  // and contributes NOTHING to `harness_family`, because it has no family in the closed vocabulary.
  //
  // ⛔ Verifying only against `harness_family` would drop it from the panel entirely — the seat would
  // render as though it never advertised it. That is strictly worse than the `incomparable` it gets:
  // a claim we cannot check has to SAY so. Silence is not a verdict.
  const v = verifyAdvertisedHarnesses(
    [receipt(SEAT, "claude-agent-acp")],
    SEAT,
    ["claude", "my-llm"],
    ["claude-code"],
  );
  const labels = v.claims.map((c) => c.advertised);
  assert.ok(labels.includes("claude-code"), "the wire family is verified");
  assert.ok(labels.includes("my-llm"), "the unmappable preset MUST still appear");
  assert.equal(v.claims.find((c) => c.advertised === "my-llm").verdict, "incomparable");
  assert.equal(v.claims.find((c) => c.advertised === "claude-code").verdict, "agreed");
  // And it is not turned into an accusation.
  assert.deepEqual(v.contradictedBy, []);
});

test("DIVERGENCE, second case: the unlabelled hatch is absent from BOTH tags", () => {
  // `advertised()` returns serving NAMES and the --agent-argv hatch has none, so it is missing from
  // `agents` as well as `harness_family`. It therefore cannot appear here at all — and an empty
  // roster is "no claim made", never a contradiction, however the seat delivers.
  const v = verifyAdvertisedHarnesses([receipt(SEAT, "sh")], SEAT, [], []);
  assert.equal(v.advertisesNothing, true);
  assert.deepEqual(v.claims, []);
  assert.deepEqual(v.contradictedBy, [], "a seat that claimed nothing cannot be contradicted");
});

test("a seat sending no harness_family still pairs off agents — no un-verify cliff", () => {
  // A seat that has not upgraded sends `agents` and no capability tags. It must keep pairing exactly
  // as before, or this change silently un-verifies the entire existing fleet.
  const v = verifyAdvertisedHarnesses([receipt(SEAT, "claude-agent-acp")], SEAT, ["claude"], []);
  assert.equal(v.claims.length, 1);
  assert.equal(v.claims[0].advertised, "claude", "the agents label is what it advertised");
  assert.equal(v.claims[0].verdict, "agreed");
});
