import assert from "node:assert/strict";
import { test } from "node:test";

import {
  DEFAULT_WINDOW, LIVE_WITHIN_SECONDS, WINDOWS,
  buyerBoard, participantDetail, sellerBoard, withinWindow, windowSeconds,
} from "../js/participants.js";
import { AWARD, CLAIM, FEEDBACK, HANDLER, HEARTBEAT, OFFER, PROFILE, RECEIPT, RESULT } from "../js/kinds.js";


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
});

test("sellers rank by track record, and being online does not lift them", () => {
  const veteran = pk("1"), steady = pk("2"), flaky = pk("3"), newcomer = pk("4");
  const events = [
    // Two finished jobs, paid, but not around right now.
    ...trade("o-vet-1", { seller: veteran, sats: 10 }),
    ...trade("o-vet-2", { seller: veteran, sats: 10 }),
    // One finished job each, unpaid, so sats cannot break the tie — the
    // difference is that flaky also walked away from a claim.
    ...trade("o-steady", { seller: steady, receipt: false }),
    ...trade("o-flaky", { seller: flaky, receipt: false }),
    ...trade("o-flaky-open", { seller: flaky }).slice(0, 2),
    // Live this minute, has never finished anything.
    ...trade("o-new", { seller: newcomer }).slice(0, 2),
    ev(HEARTBEAT, { id: "hb-new", pubkey: newcomer, at: NOW - 10, tags: [["d", "seat"]] }),
  ];

  const board = sellerBoard(events, NOW);
  assert.deepEqual(board.map((r) => r.pubkey), [veteran, steady, flaky, newcomer]);
  assert.equal(board[0].delivered, 2, "most delivered leads");
  assert.equal(board[1].completionRate, 1, "equal deliveries break on completion rate");
  assert.equal(board[2].completionRate, 0.5);
  assert.equal(board[3].online, true, "online, and still last — it is not a ranking signal");
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

test("the seat name resolves from kind-0, not the 31990 handler (#275)", () => {
  const s = pk("e");
  const events = [
    ...trade("o1", { seller: s }),
    ev(PROFILE, { id: "p1", pubkey: s, at: NOW - 100, content: '{"name":"frogger"}' }),
    // A 31990 handler processed AFTER kind-0, carrying a STALE name, must NOT override it —
    // kind-0 metadata is the single publisher (§6.1 / #275). Handler is last in the array on
    // purpose: with the old `r.name = p.name || r.name` handler read still present this row
    // would read "STALE", so this red-proves the removal, not just the kind-0 fall-through.
    ev(HANDLER, { id: "h1", pubkey: s, at: NOW - 50, tags: [["d", "code"]], content: '{"name":"STALE"}' }),
  ];
  const [row] = sellerBoard(events, NOW);
  assert.equal(row.name, "frogger", "kind-0 name is authoritative; the 31990 name is ignored for display");
});

/**
 * REGRESSION: a kind-0 ENRICHES a participant, it never creates one.
 *
 * When kind-0 first began arriving, the seller board's PROFILE arm called the
 * same row-creating getter as the heartbeat and advert arms, so every profile
 * on the relay became a runner: 13 of 24 rows were strangers with no claim, no
 * delivery, no advert and no heartbeat — including a pubkey whose only activity
 * was buying.
 */
test("REGRESSION: a kind-0 alone never creates a seller row", () => {
  const stranger = pk("9");
  const events = [
    ...trade("o1", { seller: pk("c") }),
    ev(PROFILE, { id: "pstranger", pubkey: stranger, at: NOW - 100, content: '{"name":"bob"}' }),
  ];
  const board = sellerBoard(events, NOW);
  assert.equal(board.some((r) => r.pubkey === stranger), false,
    "publishing profile metadata is not selling");
  assert.equal(board.length, 1, "only the seat that actually delivered holds a row");
});

test("REGRESSION: a buyer's kind-0 never creates a seller row", () => {
  const buyer = pk("d");
  const events = [
    ...trade("o1", { buyer, seller: pk("c") }),
    ev(PROFILE, { id: "pbuyer", pubkey: buyer, at: NOW - 100, content: '{"name":"sage"}' }),
  ];
  assert.equal(sellerBoard(events, NOW).some((r) => r.pubkey === buyer), false,
    "buying is not selling, whoever owns the profile");
  const [row] = buyerBoard(events, NOW);
  assert.equal(row.pubkey, buyer);
  assert.equal(row.name, "sage", "the buyer is named from its own kind-0");
});

/**
 * Relay order is not ours to choose, so naming must not depend on it.
 *
 * The case that discriminates is a seat earning its row from a HEARTBEAT or an
 * advert — evidence read in the same pass as kind-0, not in the trades pass
 * that runs before it. Most live seats are exactly this: heartbeating, adverts
 * up, nothing delivered yet. Enriching in-loop with a plain row lookup passes
 * when the profile happens to arrive second and drops the name when it arrives
 * first, which is why the names are applied after every row-creating pass.
 */
test("REGRESSION: a heartbeat-only seat is named whichever order kind-0 arrives in", () => {
  const s = pk("e");
  const profile = ev(PROFILE, { id: "pe", pubkey: s, at: NOW - 100, content: '{"name":"cherry"}' });
  const beat = ev(HEARTBEAT, { id: "hb", pubkey: s, at: NOW - 30, tags: [["d", "seller"]] });

  const rowFrom = (events) => sellerBoard(events, NOW).find((r) => r.pubkey === s);
  assert.equal(rowFrom([profile, beat])?.name, "cherry", "kind-0 before the heartbeat");
  assert.equal(rowFrom([beat, profile])?.name, "cherry", "kind-0 after the heartbeat");
  assert.equal(rowFrom([profile, beat])?.delivered, 0, "still a seat with no deliveries");
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
