/**
 * Generated buyer names (src/model/names.ts).
 *
 * A buyer with no kind-0 name used to render as a bare key prefix. It now gets
 * a name derived from its public key, in the retired Try it style. These pin:
 * the word lists (frozen once live: any edit renames every generated buyer),
 * determinism, the output grammar, kind-0 precedence, sellers left alone, and
 * bad input never throwing.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { createHash } from "node:crypto";
import {
  NAME_SPACE, characters, designations, displayName, models, nameForPubkey, validName,
} from "../src/model/names.js";
import { createEngine, type MarketView } from "../src/market/engine.js";
import { AWARD, CLAIM, OFFER, PROFILE, RESULT } from "../src/model/kinds.js";
import { parseEvent, type RawEvent } from "../src/model/events.js";
import { feedLine, participantName } from "../src/ui/board.js";

/** The 40 + 41 + 5 words of the Try it generator (5f112e3f^:web/app/src/try/wire.ts). */
const TRY_IT_MODELS = "vantor stradale apex aerion velora radian corsair solaro torven caldera virelli ignis chicane slipstream redline camber aero apexion valtor revora ventaro torqen astera voltane cravon serran veyra falcar valden aleron zenora meridan orvex savaro draeven varion thalor kestrel pinion kerbline".split(" ");
const TRY_IT_CHARACTERS = "nero veloce corsa comet sprint rosso tempest vector falcon spectre foudre strada fulmine rapida nocturne ember thunder lightning carbon titanium silver crimson scarlet obsidian onyx cobalt graphite jet sonic swift rapid fierce blazing charged nimble dusk dawn stealth storm fire frost".split(" ");

const SWARM_BUYER = "94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5";
const hex = (seed: string): string => seed.repeat(64).slice(0, 64);

test("word lists: ≥80 models and characters, the 5 designations, every Try it word kept", () => {
  assert.ok(models.length >= 80, `models: ${models.length}`);
  assert.ok(characters.length >= 80, `characters: ${characters.length}`);
  assert.deepEqual([...designations], ["gt", "gtr", "rs", "gts", "rr"]);
  assert.deepEqual(models.slice(0, TRY_IT_MODELS.length), TRY_IT_MODELS, "original models kept, in order");
  assert.deepEqual(characters.slice(0, TRY_IT_CHARACTERS.length), TRY_IT_CHARACTERS, "original characters kept, in order");
});

test("word lists: lowercase a-z only, no duplicates within or across lists", () => {
  const all = [...models, ...characters, ...designations];
  for (const w of all) assert.match(w, /^[a-z]+$/, `bad word ${JSON.stringify(w)}`);
  assert.equal(new Set(all).size, all.length, "a word appears twice");
});

test("word lists: nothing that reads as an official account", () => {
  const banned = ["maxie", "maxplayer", "admin", "nemo", "worker", "support", "team", "official", "staff", "mod", "system", "bot"];
  for (const w of [...models, ...characters, ...designations]) {
    for (const b of banned) assert.ok(!w.includes(b), `${w} contains ${b}`);
  }
});

test("word lists are frozen: changing them renames every generated buyer", () => {
  // Update this hash ONLY with the team's sign-off that renaming everyone is intended.
  const digest = createHash("sha256")
    .update([models.join(" "), characters.join(" "), designations.join(" ")].join("|"))
    .digest("hex");
  assert.equal(digest, "fcf7a069a1774b3726e784c4dc6f2ba94cb582492c0210fb1e3ff399ca9a9144");
  assert.ok(Object.isFrozen(models) && Object.isFrozen(characters) && Object.isFrozen(designations));
});

test("name space: M·C + M·D + M·C·D", () => {
  assert.equal(NAME_SPACE, 80 * 80 + 80 * 5 + 80 * 80 * 5);
  assert.equal(NAME_SPACE, 38_800);
});

test("deterministic: fixed keys map to fixed names, every call", () => {
  const expected: [string, string][] = [
    [SWARM_BUYER, "aero-veloce"],          // model-character
    [hex("0"), "veyra-alloy"],
    [hex("f"), "revora-rr"],               // model-designation
    [hex("a"), "sarvoni-dawn-gts"],        // model-character-designation
    [hex("c"), "orvex-rapida-rr"],
  ];
  for (const [key, name] of expected) {
    assert.equal(nameForPubkey(key), name, key);
    assert.equal(nameForPubkey(key), nameForPubkey(key));
    assert.equal(nameForPubkey(key.toUpperCase()), name, "case-insensitive");
  }
});

test("every generated name parses with validName, and all three forms occur", () => {
  const forms = new Set<number>();
  const seen = new Set<string>();
  for (let i = 0; i < 5000; i++) {
    const key = createHash("sha256").update(`buyer-${i}`).digest("hex");
    const name = nameForPubkey(key)!;
    assert.ok(validName(name), `${key} -> ${name}`);
    assert.match(name, /^[a-z]+(-[a-z]+){1,2}$/);
    const parts = name.split("-");
    forms.add(parts.length === 3 ? 2 : designations.includes(parts[1]!) ? 1 : 0);
    seen.add(name);
  }
  assert.deepEqual([...forms].sort(), [0, 1, 2]);
  // The form is a uniform 1-in-3 pick, so ~1667 of these keys share the 400
  // model-designation names; ~3500 distinct is the expected count, not a bug.
  assert.ok(seen.size > 3300, `only ${seen.size} distinct names in 5000 keys`);
  assert.equal(validName("maxie-gt"), false);
  assert.equal(validName("aero"), false);
});

test("invalid input returns null and never throws", () => {
  for (const bad of ["", "abc", hex("a").slice(0, 63), hex("a") + "0", "g".repeat(64), ` ${hex("a")}`,
    "npub1jjvf7aj6d8ju8ykkqv2qa4eu0lnpuyelhfm8ee2f2k85alzw2l2sf7haqz", undefined, null, 42, {}]) {
    assert.doesNotThrow(() => nameForPubkey(bad));
    assert.equal(nameForPubkey(bad), null, JSON.stringify(bad));
  }
  assert.equal(displayName(null, "short", true), null, "an unnamed bad key still falls back to the key");
});

test("displayName: kind-0 wins; buyers fall back to a generated name; sellers do not", () => {
  assert.equal(displayName("Sage", SWARM_BUYER, true), "Sage");
  assert.equal(displayName("Sage", SWARM_BUYER, false), "Sage");
  assert.equal(displayName(null, SWARM_BUYER, true), "aero-veloce");
  assert.equal(displayName("", SWARM_BUYER, true), "aero-veloce");
  assert.equal(displayName(null, SWARM_BUYER, false), null);
});

/* ---- wired into the board ---- */

const T0 = 1_700_000_000;
let idCounter = 0x9000;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");
const ev = (kind: number, pubkey: string, created_at: number, tags: string[][] = [], content = ""): RawEvent =>
  ({ id: id(), kind, pubkey, created_at, tags, content });
const root = (offerId: string): string[] => ["e", offerId, "", "root"];

function viewOf(events: RawEvent[]): MarketView {
  const engine = createEngine({ windowKey: "all", now: () => T0 + 600 });
  for (const e of events) engine.ingest(e);
  engine.flush();
  return engine.view()!;
}

function trade(buyer: string, seller: string) {
  const o = ev(OFFER, buyer, T0, [["t", "maxplayer"], ["i", "do the thing"], ["amount", "500", "sat"]]);
  return {
    o,
    events: [o, ev(CLAIM, seller, T0 + 1, [root(o.id)]), ev(AWARD, buyer, T0 + 2, [root(o.id), ["p", seller]]),
      ev(RESULT, seller, T0 + 3, [root(o.id), ["p", buyer]])],
  };
}

test("board: an unnamed buyer shows its generated name; an unnamed seller still shows its key", () => {
  const buyer = SWARM_BUYER, seller = hex("b");
  const { o, events } = trade(buyer, seller);
  const view = viewOf(events);
  const offerLine = feedLine(view, parseEvent(o)!);
  assert.match(offerLine, /<span class="person">aero-veloce<\/span>/, offerLine);
  assert.doesNotMatch(offerLine, new RegExp(`<code>${buyer.slice(0, 8)}</code>`));
  const claimLine = feedLine(view, parseEvent(events[1]!)!);
  assert.match(claimLine, new RegExp(`^<code>${seller.slice(0, 8)}</code> claimed a job from <span class="person">aero-veloce</span>$`), claimLine);
  assert.equal(participantName(view, buyer, true), "aero-veloce");
  assert.equal(participantName(view, buyer), null, "the same key outside a buyer slot shows its key");
  assert.equal(participantName(view, seller), null, "a pure seller is unchanged");
});

test("board: a buyer's kind-0 name takes precedence over the generated one", () => {
  const buyer = SWARM_BUYER, seller = hex("b");
  const { o, events } = trade(buyer, seller);
  const named = viewOf([...events, ev(PROFILE, buyer, T0 + 5, [], JSON.stringify({ name: "Sage" }))]);
  assert.match(feedLine(named, parseEvent(o)!), /<span class="person">Sage<\/span>/);
  assert.equal(participantName(named, buyer, true), "Sage");
  assert.equal(participantName(named, buyer), "Sage", "kind-0 names every slot");
  const displayOnly = viewOf([...events, ev(PROFILE, buyer, T0 + 5, [], JSON.stringify({ display_name: "Sage D" }))]);
  assert.equal(participantName(displayOnly, buyer, true), "Sage D", "display_name counts as a kind-0 name");
  const blank = viewOf([...events, ev(PROFILE, buyer, T0 + 5, [], JSON.stringify({ name: "  ", display_name: 7 }))]);
  assert.equal(participantName(blank, buyer, true), "aero-veloce", "a blank or non-string name is no name");
});

test("board: a key that also sells keeps its key in seller slots", () => {
  // DUAL buys from SELLER and sells to OTHER.
  const DUAL = hex("c"), seller = hex("b"), other = hex("d");
  const bought = trade(DUAL, seller), sold = trade(other, DUAL);
  const view = viewOf([...bought.events, ...sold.events]);
  assert.match(feedLine(view, parseEvent(bought.o)!), /<span class="person">orvex-rapida-rr<\/span>/, "named as a buyer");
  const claim = feedLine(view, parseEvent(sold.events[1]!)!);
  assert.match(claim, new RegExp(`^<code>${DUAL.slice(0, 8)}</code> claimed a job from `), claim);
  const award = feedLine(view, parseEvent(sold.events[2]!)!);
  assert.match(award, new RegExp(`awarded the job to <code>${DUAL.slice(0, 8)}</code>$`), award);
  assert.equal(participantName(view, DUAL), null, "seller role: unchanged");
  assert.equal(participantName(view, DUAL, true), "orvex-rapida-rr", "buyer role: generated");
});

test("board: a buyer slot names a key even with no buyer activity in the cache", () => {
  const view = viewOf([]);
  assert.equal(participantName(view, SWARM_BUYER, true), "aero-veloce");
  assert.equal(participantName(view, SWARM_BUYER), null);
  assert.doesNotThrow(() => participantName(view, "not-a-key", true));
  assert.equal(participantName(view, "not-a-key", true), null);
});
