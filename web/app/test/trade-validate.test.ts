/**
 * The /trades validator must reach the trade CLI's verdict on every golden
 * case. Each case in fixtures/trade-rust.json is a real event signed by
 * crates/maxplayer-trade and judged there by `parse_lot` / `lifecycle`
 * (generator: crates/maxplayer-trade/examples/web_market_fixtures.rs on #1107).
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { checkLot, lifecycle, parseLot, validateAsset, verifyEvent, type NostrEvent } from "../src/trade/validate.js";
import { parseJson } from "../src/trade/json.js";
import { decodeNpub, npubEncode } from "../src/trade/nip19.js";

const here = dirname(fileURLToPath(import.meta.url));
interface LotCase { name: string; now: number; event: NostrEvent; rust: { ok: boolean; error?: string } }
interface ChainCase { name: string; lot: NostrEvent; statuses: NostrEvent[]; rust: { ok: boolean; status?: string; error?: string } }
const golden = JSON.parse(readFileSync(join(here, "fixtures", "trade-rust.json"), "utf8")) as { lots: LotCase[]; chains: ChainCase[] };

/** The one named divergence: nostr:// mints are accepted for display ahead of CLI support. */
const EXTENSIONS = new Set(["nostr npub mint"]);

test("the golden file is substantial and covers both verdicts", () => {
  assert.ok(golden.lots.length >= 70, `lots: ${golden.lots.length}`);
  assert.ok(golden.chains.length >= 30, `chains: ${golden.chains.length}`);
  for (const set of [golden.lots, golden.chains]) {
    assert.ok(set.some((c) => c.rust.ok) && set.some((c) => !c.rust.ok));
  }
});

for (const c of golden.lots) {
  test(`listing agrees with parse_lot: ${c.name}`, () => {
    const v = parseLot(c.event, c.now);
    if (EXTENSIONS.has(c.name)) {
      assert.equal(c.rust.ok, false, "the CLI still refuses it; drop the extension when it stops");
      assert.equal(v.ok, true, "the page accepts canonical nostr:// mints");
      return;
    }
    assert.equal(v.ok, c.rust.ok, `rust: ${c.rust.error ?? "ok"} / web: ${v.ok ? "ok" : v.error}`);
  });
}

for (const c of golden.chains) {
  test(`status chain agrees with lifecycle: ${c.name}`, () => {
    const v = lifecycle(c.lot, c.statuses);
    assert.equal(v.ok, c.rust.ok, `rust: ${c.rust.error ?? c.rust.status} / web: ${v.ok ? v.status : v.error}`);
    if (v.ok) assert.equal(v.status, c.rust.status);
  });
}

const valid = golden.lots.find((c) => c.name === "valid")!;

test("expiry is reported as history, not invalidity", () => {
  const e = valid.event;
  const live = parseLot(e, e.created_at + 86399);
  assert.equal(live.ok, true);
  const gone = parseLot(e, e.created_at + 86400);
  assert.ok(!gone.ok && gone.expired, "at expires_at the lot is expired");
  assert.equal(checkLot(e).ok, true, "and still structurally valid");
  const tampered = golden.lots.find((c) => c.name === "tampered content")!;
  const bad = parseLot(tampered.event, tampered.now);
  assert.ok(!bad.ok && !bad.expired, "a forged lot is never merely expired");
});

test("the parsed lot carries the amounts, mints and maker", () => {
  const v = parseLot(valid.event, valid.now);
  assert.ok(v.ok);
  assert.equal(v.lot.give.net, 64);
  assert.equal(v.lot.want.net, 48);
  assert.equal(v.lot.give.mint_url, "https://mint.minibits.cash/Bitcoin");
  assert.equal(v.lot.want.mint_url, "https://testnut.cashu.space");
  assert.equal(v.lot.maker, valid.event.pubkey);
  assert.equal(v.lot.expires_at, valid.event.created_at + 86400);
});

test("signature checks refuse malformed shapes without throwing", () => {
  assert.equal(verifyEvent(valid.event), true);
  for (const bad of [null, 5, {}, { ...valid.event, tags: [[]] }, { ...valid.event, tags: [[1]] },
    { ...valid.event, id: valid.event.id.toUpperCase() }, { ...valid.event, created_at: 1.5 },
    { ...valid.event, sig: "00".repeat(64) }]) {
    assert.equal(verifyEvent(bad), false);
  }
  assert.equal(parseLot(null as unknown as NostrEvent, 0).ok, false);
  assert.equal(lifecycle(valid.event, [null as unknown as NostrEvent]).ok, false);
});

test("a forged status that reuses a real id cannot pass as a duplicate", () => {
  const sold = golden.chains.find((c) => c.name === "sold")!;
  const real = sold.statuses.find((e) => e.content.includes('"sold"'))!;
  const first = sold.statuses.find((e) => e !== real)!;
  const forged = { ...real, content: real.content.replace('"sold"', '"cancelled"') };
  assert.notEqual(forged.content, real.content);
  assert.equal(lifecycle(sold.lot, [first, real]).ok, true);
  // The forgery comes first, so de-duplication cannot hide it behind the real event.
  assert.equal(lifecycle(sold.lot, [forged, first, real]).ok, false);
});

test("nostr mints: canonical npub only", () => {
  const npub = "npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
  assert.doesNotThrow(() => validateAsset({ mint_url: `nostr://${npub}`, unit: "sat" }));
  for (const bad of [`nostr://${npub}/`, `nostr://${npub}/v1`, `nostr://${npub.toUpperCase()}`, "nostr://npub1notakey",
    `nostr://${npub.slice(0, -1)}q`, `nostr://user@${npub}`]) {
    assert.throws(() => validateAsset({ mint_url: bad, unit: "sat" }), Error, bad);
  }
});

test("npub round-trips and rejects a bad checksum", () => {
  const hex = "7bdef7be22dd8e59f4e4b80b5bdbd0e6e6eb38d8c9b9fe9eb2d9fa4c8e1d1a9b";
  const npub = npubEncode(hex);
  assert.match(npub, /^npub1[02-9ac-hj-np-z]{58}$/);
  assert.equal(Buffer.from(decodeNpub(npub)!).toString("hex"), hex);
  assert.equal(decodeNpub(npub.slice(0, -1) + (npub.endsWith("q") ? "p" : "q")), null);
});

test("strict JSON matches serde on the cases JSON.parse gets wrong", () => {
  for (const bad of ['{"a":1} x', "01", "1.", '"\\ud800"', '"\\udc00"', "[1,]", "{,}", "\u00a0{}"]) {
    assert.throws(() => parseJson(bad), Error, bad);
  }
  assert.deepEqual(parseJson(' {"a":[1,-2.5e3,"\\u00e9\\ud83d\\ude00"]} \n'), {
    t: "obj",
    v: [["a", { t: "arr", v: [{ t: "num", raw: "1" }, { t: "num", raw: "-2.5e3" }, { t: "str", v: "é😀" }] }]],
  });
});
