/**
 * The relay is untrusted input and anyone can publish to a relay. Ids and
 * pubkeys are rendered into markup and into data- attributes, so a value that
 * is not really an id is an injection path. These assert the boundary holds.
 *
 * The split under test: the MODEL preserves hostile-looking free text
 * verbatim (sanitising there would corrupt legitimate descriptions containing
 * < or &), and the RENDERER escapes everything through esc(). Interpolated
 * attributes must be double-quoted — esc() escapes `"` but not `'`.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import { isHex32, parseEvent, rootOfferId } from "../src/model/events.js";
import { buildTrades } from "../src/market/trades.js";
import { sellerBoard } from "../src/market/participants.js";
import { OFFER, RECEIPT, RESULT } from "../src/model/kinds.js";
import { esc } from "../src/ui/format.js";

const HEX = "a".repeat(64);
// parseEvent memoizes by event id (sound on the wire: an id is the hash of
// the content, so distinct events cannot share one). Fixtures must respect
// that or a later parse returns the FIRST event cached under the reused id.
let uidCounter = 0xf000;
const uid = (): string => (uidCounter++).toString(16).padStart(64, "0");
const PAYLOADS = [
  '"><img src=x onerror=alert(1)>',
  "<script>alert(1)</script>",
  "javascript:alert(1)",
  "' onmouseover='alert(1)",
  HEX + "extra",
  HEX.toUpperCase(),
  "",
  "../../etc/passwd",
];

test("isHex32 accepts only 32 bytes of lowercase hex", () => {
  assert.equal(isHex32(HEX), true);
  for (const bad of PAYLOADS) assert.equal(isHex32(bad), false, `must reject: ${bad}`);
  for (const bad of [null, undefined, 12345, {}, []]) assert.equal(isHex32(bad), false);
});

test("an event with an injected id or pubkey is dropped, not rendered", () => {
  for (const payload of PAYLOADS) {
    assert.equal(
      parseEvent({ id: payload, kind: OFFER, pubkey: HEX, created_at: 1, tags: [] }), null,
      `id must be rejected: ${payload}`,
    );
    assert.equal(
      parseEvent({ id: uid(), kind: OFFER, pubkey: payload, created_at: 1, tags: [] }), null,
      `pubkey must be rejected: ${payload}`,
    );
  }
});

test("an injected e-tag cannot become an offer id", () => {
  for (const payload of PAYLOADS) {
    assert.equal(rootOfferId({ id: HEX, kind: OFFER, pubkey: HEX, created_at: 1, tags: [["e", payload]] }), null,
      `e-tag must be rejected: ${payload}`);
    assert.equal(rootOfferId({ id: HEX, kind: OFFER, pubkey: HEX, created_at: 1, tags: [["e", payload, "", "root"]] }), null,
      "even marked root");
  }
  // A real id among junk still resolves.
  assert.equal(rootOfferId({ id: HEX, kind: OFFER, pubkey: HEX, created_at: 1, tags: [["e", "<script>"], ["e", HEX, "", "root"]] }), HEX);
});

test("a trade is never keyed by something that is not an event id", () => {
  const trades = buildTrades([
    { id: uid(), kind: RECEIPT, pubkey: HEX, created_at: 5, tags: [["e", '"><img src=x>'], ["amount", "10"]] },
  ]);
  assert.deepEqual(trades, [], "an unusable root means no trade, not a trade with a poisoned key");
});

test("free text from the relay stays free text — escaped at render, not sanitised in the model", () => {
  const p = parseEvent({
    id: uid(), kind: OFFER, pubkey: HEX, created_at: 1,
    tags: [["i", "compare a<b && c>d in <script>"]],
  });
  assert.equal(p!.description, "compare a<b && c>d in <script>");
});

test("a hostile harness or seller name survives to the renderer intact", () => {
  const board = sellerBoard([
    { id: uid(), kind: RESULT, pubkey: HEX, created_at: 5,
      tags: [["e", HEX, "", "root"], ["harness", "<img src=x onerror=alert(1)>"]], content: "" },
  ], 10);
  assert.equal(board[0]!.harness, "<img src=x onerror=alert(1)>", "preserved, for the renderer to escape");
});

test("esc neutralises every payload for text nodes and double-quoted attributes", () => {
  for (const payload of PAYLOADS) {
    const out = esc(payload);
    assert.ok(!/[<>]/.test(out), `no raw angle brackets: ${payload}`);
    assert.ok(!out.includes('"'), `no raw double quote (attributes are double-quoted): ${payload}`);
  }
  assert.equal(esc('<b a="1">&\''), "&lt;b a=&quot;1&quot;&gt;&amp;'");
});
