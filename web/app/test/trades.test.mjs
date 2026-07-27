import assert from "node:assert/strict";
import { test } from "node:test";

import { buildTrades, conversionRates, marketMetrics } from "../js/trades.js";
import { AWARD, CLAIM, FEEDBACK, OFFER, RECEIPT, RESULT } from "../js/kinds.js";

const pk = (c) => c.repeat(64);
const ev = (kind, { id, pubkey = pk("a"), at = 1000, tags = [] }) =>
  ({ id, kind, pubkey, created_at: at, tags, content: "" });
const rootTag = (offerId) => ["e", offerId, "", "root"];

/** offer -> claim -> award -> result -> receipt, all rooted on the offer. */
function fullTrade(offerId, { sats = 21, buyer = pk("b"), seller = pk("c"), t0 = 1000 } = {}) {
  return [
    ev(OFFER, { id: offerId, pubkey: buyer, at: t0, tags: [["amount", String(sats), "sat"]] }),
    ev(CLAIM, { id: offerId + "-claim", pubkey: seller, at: t0 + 10, tags: [rootTag(offerId)] }),
    ev(AWARD, { id: offerId + "-award", pubkey: buyer, at: t0 + 20, tags: [rootTag(offerId)] }),
    ev(RESULT, { id: offerId + "-result", pubkey: seller, at: t0 + 30, tags: [rootTag(offerId)] }),
    ev(RECEIPT, { id: offerId + "-receipt", at: t0 + 40, tags: [rootTag(offerId), ["amount", String(sats), "sat"]] }),
  ];
}

test("a trade's events join into one record under the offer id", () => {
  const trades = buildTrades(fullTrade("offer1"));
  assert.equal(trades.length, 1);
  const [t] = trades;
  assert.equal(t.offerId, "offer1");
  assert.equal(t.buyer, pk("b"));
  assert.equal(t.seller, pk("c"));
  assert.equal(t.receiptAmount, 21);
  assert.deepEqual(Object.keys(t.at).sort(), ["award", "claim", "offer", "receipt", "result"]);
});

test("a re-delivered event does not move a stage's clock", () => {
  const events = fullTrade("offer1");
  const later = { ...events[1], created_at: events[1].created_at + 500 };
  const [t] = buildTrades([...events, later]);
  assert.equal(t.at.claim, 1010, "earliest timestamp wins");
});

test("the funnel counts only trades whose offer we actually saw", () => {
  // One complete trade, plus a receipt rooted on an offer outside our window.
  const orphan = ev(RECEIPT, { id: "r-orphan", at: 2000, tags: [rootTag("unseen-offer"), ["amount", "9", "sat"]] });
  const m = marketMetrics([...fullTrade("offer1"), orphan]);

  assert.deepEqual(m.funnel, { posted: 1, claimed: 1, awarded: 1, delivered: 1, receipted: 1 });
  assert.equal(m.rootedElsewhere, 1, "the orphan is reported, not silently dropped");
  assert.equal(m.receiptsOnRecord, 2, "settlement counts include trades we only saw settle");
  assert.equal(m.satsInReceipts, 30);
});

test("settlement figures are a floor: no receipt means invisible, not zero", () => {
  // A trade that delivered but published no receipt — real, and uncountable here.
  const noReceipt = fullTrade("offer2").slice(0, 4);
  const m = marketMetrics([...fullTrade("offer1"), ...noReceipt]);
  assert.equal(m.funnel.delivered, 2);
  assert.equal(m.receiptsOnRecord, 1, "only the announced settlement is countable");
  assert.equal(m.satsInReceipts, 21);
});

test("an award e-tagging the winning claim still keys off the offer", () => {
  // The award references BOTH the offer (root) and the claim. Taking the first
  // e-tag blindly would key this trade off the claim id and split it in two.
  const events = fullTrade("offer1");
  events[2] = ev(AWARD, {
    id: "award-1", pubkey: pk("b"), at: 1020,
    tags: [["e", "offer1-claim"], rootTag("offer1")],
  });
  const trades = buildTrades(events);
  assert.equal(trades.length, 1, "one trade, not two");
  assert.ok(trades[0].at.award);
});

test("participants and span are derived from the joined trades", () => {
  const m = marketMetrics([
    ...fullTrade("o1", { buyer: pk("b"), seller: pk("c"), t0: 86400 }),
    ...fullTrade("o2", { buyer: pk("d"), seller: pk("c"), t0: 86400 * 3 }),
  ]);
  assert.equal(m.buyers, 2);
  assert.equal(m.sellers, 1);
  assert.equal(m.daysActive, 3);
  assert.equal(m.tradesTracked, 2);
});

test("a feedback reason is captured against its trade", () => {
  const events = [
    ...fullTrade("o1").slice(0, 2),
    ev(FEEDBACK, { id: "f1", at: 1015, tags: [rootTag("o1")] }),
  ];
  events[2].content = "claim_released: seller withdrew";
  const [t] = buildTrades(events);
  assert.equal(t.declineReason, "claim_released");
});

test("conversion rates are stage-over-previous and never divide by zero", () => {
  const r = conversionRates({ posted: 100, claimed: 50, awarded: 40, delivered: 20, receipted: 10 });
  assert.equal(r.claimed, 0.5);
  assert.equal(r.awarded, 0.8);
  assert.equal(r.delivered, 0.5);
  assert.equal(r.receipted, 0.5);
  assert.deepEqual(
    conversionRates({ posted: 0, claimed: 0, awarded: 0, delivered: 0, receipted: 0 }),
    { claimed: 0, awarded: 0, delivered: 0, receipted: 0 },
  );
});
