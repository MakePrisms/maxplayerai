import assert from "node:assert/strict";
import { test } from "node:test";

import { parseEvent } from "../js/model.js";
import { buildTrades, marketMetrics } from "../js/trades.js";
import { AWARD, CLAIM, MOBEE_TAG, OFFER, RECEIPT, RESULT, SELF_TRADE_TAG } from "../js/kinds.js";

// A buyer paying its own seller is real work but not market demand, and after
// the fact its receipt is indistinguishable from an arms-length one. The
// disclosure in the job text is for humans; this tag is the machine predicate.
// Nothing here matches prose — that would be an inference dressed as a fact.

const HEX = (n) => String(n).padStart(64, "0");
const pk = (c) => c.repeat(64);
const ev = (kind, { id, pubkey = pk("a"), at = 1000, tags = [] }) =>
  ({ id, kind, pubkey, created_at: at, tags, content: "" });
const root = (offerId) => ["e", offerId, "", "root"];

function trade(offerId, { sats = 10, selfTrade = false, buyer = pk("b"), seller = pk("c") } = {}) {
  const tags = [["amount", String(sats), "sat"], ["t", MOBEE_TAG]];
  if (selfTrade) tags.push(["t", SELF_TRADE_TAG]);
  return [
    ev(OFFER, { id: offerId, pubkey: buyer, at: 1000, tags }),
    ev(CLAIM, { id: offerId.slice(0, 63) + "1", pubkey: seller, at: 1010, tags: [root(offerId)] }),
    ev(AWARD, { id: offerId.slice(0, 63) + "2", pubkey: buyer, at: 1020, tags: [root(offerId)] }),
    ev(RESULT, { id: offerId.slice(0, 63) + "3", pubkey: seller, at: 1030, tags: [root(offerId)] }),
    ev(RECEIPT, { id: offerId.slice(0, 63) + "4", at: 1040, tags: [root(offerId), ["amount", String(sats), "sat"]] }),
  ];
}

test("the self-trade marker is a t-tag, read structurally", () => {
  const plain = parseEvent(ev(OFFER, { id: HEX(1), tags: [["t", MOBEE_TAG]] }));
  assert.equal(plain.selfTrade, false);

  const marked = parseEvent(ev(OFFER, { id: HEX(2), tags: [["t", MOBEE_TAG], ["t", SELF_TRADE_TAG]] }));
  assert.equal(marked.selfTrade, true, "both t values coexist; the mobee filter still matches");
});

test("prose in the job text is NOT the predicate", () => {
  // Rocky's first self-commissioned job disclosed in the description only.
  // A human reads that; a counting rule must not, or rewording silently breaks
  // it and a quotation silently triggers it.
  const prose = parseEvent(ev(OFFER, {
    id: HEX(3),
    tags: [["t", MOBEE_TAG], ["i", "NOTE: this is an internal self-commissioned review, NOT an arms-length trade"]],
  }));
  assert.equal(prose.selfTrade, false, "prose disclosure must not set the machine flag");
  assert.match(prose.description, /self-commissioned/, "but the text is preserved for the reader");
});

test("a self-trade is excluded from every market figure", () => {
  const m = marketMetrics([...trade(HEX(10), { sats: 7 }), ...trade(HEX(20), { sats: 500, selfTrade: true })]);
  assert.deepEqual(m.funnel, { posted: 1, claimed: 1, awarded: 1, delivered: 1, receipted: 1 });
  assert.equal(m.receiptsOnRecord, 1);
  assert.equal(m.satsInReceipts, 7, "the 500-sat self-trade must not inflate settled value");
  assert.equal(m.tradesTracked, 1);
});

test("...and is counted, never silently dropped", () => {
  const m = marketMetrics([...trade(HEX(10)), ...trade(HEX(20), { selfTrade: true }), ...trade(HEX(30), { selfTrade: true })]);
  assert.equal(m.selfTrades, 2, "the exclusion is reportable, so a reader can recover the full picture");
  assert.equal(m.tradesTracked, 1);
});

test("no self-trades means no exclusion and no noise", () => {
  const m = marketMetrics(trade(HEX(10)));
  assert.equal(m.selfTrades, 0);
  assert.equal(m.tradesTracked, 1);
});

test("the flag rides on the trade, not just the event", () => {
  const [t] = buildTrades(trade(HEX(40), { selfTrade: true }));
  assert.equal(t.selfTrade, true, "so any view can badge it without re-parsing the offer");
});
