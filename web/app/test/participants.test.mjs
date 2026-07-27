import assert from "node:assert/strict";
import { test } from "node:test";

import {
  DEFAULT_WINDOW, LIVE_WITHIN_SECONDS, WINDOWS,
  buyerBoard, participantDetail, sellerBoard, withinWindow, windowSeconds,
} from "../js/participants.js";
import { AWARD, CLAIM, FEEDBACK, HANDLER, HEARTBEAT, OFFER, RECEIPT, RESULT } from "../js/kinds.js";


/** Fixtures use readable labels; the wire uses 32 bytes of hex. Map one to the other. */
const _ids = new Map();
const H = (label) => {
  if (!_ids.has(label)) _ids.set(label, (_ids.size + 1).toString(16).padStart(64, "0"));
  return _ids.get(label);
};

const pk = (c) => c.repeat(64);
const NOW = 1_800_000_000;
const ev = (kind, { id, pubkey = pk("a"), at = NOW, tags = [], content = "" }) =>
  ({ id: H(id), kind, pubkey, created_at: at, tags, content });
const root = (offerId) => ["e", H(offerId), "", "root"];

function trade(offerId, { buyer = pk("b"), seller = pk("c"), sats = 10, t0 = NOW - 3600, receipt = true } = {}) {
  const out = [
    ev(OFFER, { id: offerId, pubkey: buyer, at: t0, tags: [["amount", String(sats), "sat"]] }),
    ev(CLAIM, { id: offerId + "c", pubkey: seller, at: t0 + 60, tags: [root(offerId)] }),
    ev(AWARD, { id: offerId + "a", pubkey: buyer, at: t0 + 70, tags: [root(offerId)] }),
    ev(RESULT, { id: offerId + "r", pubkey: seller, at: t0 + 120, tags: [root(offerId)] }),
  ];
  if (receipt) out.push(ev(RECEIPT, { id: offerId + "p", at: t0 + 130, tags: [root(offerId), ["amount", String(sats), "sat"]] }));
  return out;
}

test("the default window is a week and every window is selectable", () => {
  assert.equal(DEFAULT_WINDOW, "week");
  assert.deepEqual(WINDOWS.map((w) => w.key), ["24h", "week", "all"]);
  assert.equal(windowSeconds("24h"), 86400);
  assert.equal(windowSeconds("all"), null, "all time is unbounded");
});

test("a window filters events, and all-time filters nothing", () => {
  const events = [ev(OFFER, { id: "recent", at: NOW - 3600 }), ev(OFFER, { id: "old", at: NOW - 86400 * 10 })];
  assert.deepEqual(withinWindow(events, "24h", NOW).map((e) => e.id), [H("recent")]);
  assert.deepEqual(withinWindow(events, "week", NOW).map((e) => e.id), [H("recent")]);
  assert.equal(withinWindow(events, "all", NOW).length, 2);
});

test("buyers rank by sats paid and carry their posting history", () => {
  const big = pk("1"), small = pk("2");
  const board = buyerBoard([
    ...trade("o1", { buyer: big, sats: 50 }),
    ...trade("o2", { buyer: big, sats: 30 }),
    ...trade("o3", { buyer: small, sats: 5 }),
  ], NOW);

  assert.equal(board.length, 2);
  assert.equal(board[0].pubkey, big, "highest spend first");
  assert.equal(board[0].satsPaid, 80);
  assert.equal(board[0].posted, 2);
  assert.equal(board[0].receipted, 2);
  assert.equal(board[0].medianPrice, 40);
  assert.equal(board[1].satsPaid, 5);
});

test("a delivery with no receipt is an open question, not a debt", () => {
  const [row] = buyerBoard(trade("o1", { receipt: false }), NOW);
  assert.equal(row.receipted, 0);
  assert.equal(row.satsPaid, 0);
  assert.equal(row.unpaidDeliveries, 1, "surfaced, because settlement can happen unannounced");
});

test("sellers carry a completion rate and a median delivery time", () => {
  const s = pk("9");
  const board = sellerBoard([
    ...trade("o1", { seller: s, sats: 10, t0: NOW - 7200 }),
    ...trade("o2", { seller: s, sats: 20, t0: NOW - 3600 }),
  ], NOW);

  const [row] = board;
  assert.equal(row.pubkey, s);
  assert.equal(row.claimed, 2);
  assert.equal(row.delivered, 2);
  assert.equal(row.satsEarned, 30);
  assert.equal(row.completionRate, 1);
  assert.equal(row.medianDeliverSeconds, 60, "claim at +60, result at +120");
});

test("a claim that produced feedback but no delivery counts as released", () => {
  const s = pk("9");
  const events = [
    ...trade("o1", { seller: s }).slice(0, 2),
    ev(FEEDBACK, { id: "f1", pubkey: s, at: NOW - 3000, tags: [root("o1")], content: "claim_released: withdrew" }),
  ];
  const [row] = sellerBoard(events, NOW);
  assert.equal(row.released, 1);
  assert.equal(row.delivered, 0);
  assert.equal(row.completionRate, 0);
});

test("online means a recent heartbeat, not merely recent trading", () => {
  const fresh = pk("1"), stale = pk("2");
  const events = [
    ...trade("o1", { seller: fresh, t0: NOW - 3600 }),
    ...trade("o2", { seller: stale, t0: NOW - 3600 }),
    ev(HEARTBEAT, { id: "hb1", pubkey: fresh, at: NOW - 10, tags: [["d", "seat"]] }),
    ev(HEARTBEAT, { id: "hb2", pubkey: stale, at: NOW - LIVE_WITHIN_SECONDS - 60, tags: [["d", "seat"]] }),
  ];
  const board = sellerBoard(events, NOW);
  const byKey = Object.fromEntries(board.map((r) => [r.pubkey, r]));

  assert.equal(byKey[fresh].online, true);
  assert.equal(byKey[stale].online, false, "a stale heartbeat is not availability");
  assert.ok(byKey[stale].lastSeen > 0, "but they are still known to exist");
  assert.equal(board[0].pubkey, fresh, "online sellers sort first");
});

test("a capability advert attaches to its seller", () => {
  const s = pk("9");
  const events = [
    ...trade("o1", { seller: s }),
    ev(HANDLER, { id: "h1", pubkey: s, at: NOW - 100, tags: [["d", "code"]], content: '{"name":"code review"}' }),
  ];
  const [row] = sellerBoard(events, NOW);
  assert.deepEqual(row.capabilities, ["code review"]);
});

test("a participant detail gathers both roles and their trades", () => {
  const who = pk("5");
  const events = [
    ...trade("o1", { buyer: who }),
    ...trade("o2", { seller: who }),
    ...trade("o3", { buyer: pk("7"), seller: pk("8") }),
  ];
  const d = participantDetail(events, who, NOW);
  assert.equal(d.buyer.posted, 1);
  assert.equal(d.seller.delivered, 1);
  assert.equal(d.trades.length, 2, "only trades this participant took part in");
});

test("boards are empty, not broken, with no events", () => {
  assert.deepEqual(buyerBoard([], NOW), []);
  assert.deepEqual(sellerBoard([], NOW), []);
  assert.equal(participantDetail([], pk("1"), NOW).trades.length, 0);
});
