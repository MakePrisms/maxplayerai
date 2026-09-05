/**
 * Free jobs — an offer that settles with no payment.
 *
 * Bob, 2026-09-04: "on free jobs, the last event is 'Accept'". A free job can
 * never publish a 3400 receipt, so the base board filed every completed free
 * job under "delivered with no receipt" — an unpaid delivery, forever. The
 * predicate is `settlesWithoutPayment` in model/events.ts, read from the
 * offer's own `["param","payment","none"]` tag; a zero amount corroborates but
 * never decides, and a MISSING amount tag is a different thing entirely.
 *
 * Imported through a namespace with a fallback so this file loads at the base
 * sha (where the predicate does not exist) and goes red on behaviour.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { activeTradeJobs, createEngine } from "../src/market/engine.js";
import { buyerBoard, inProgressJobs, sellerBoard } from "../src/market/participants.js";
import { buildTrades } from "../src/market/trades.js";
import { ACCEPT, AWARD, CLAIM, OFFER, RESULT } from "../src/model/kinds.js";
import * as events from "../src/model/events.js";
import { feedLine } from "../src/ui/board.js";
import type { RawEvent } from "../src/model/events.js";

const settlesWithoutPayment: (e: RawEvent) => boolean =
  (events as { settlesWithoutPayment?: (e: RawEvent) => boolean }).settlesWithoutPayment ?? (() => false);

const hex = (seed: string): string => seed.repeat(64).slice(0, 64);
const BUYER = hex("a");
const SELLER = hex("b");
const T0 = 1_700_000_000;
const NOW = T0 + 600;

let idCounter = 0x1000;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");
function ev(kind: number, pubkey: string, created_at: number, tags: string[][] = []): RawEvent {
  return { id: id(), kind, pubkey, created_at, tags, content: "" };
}

/** Tag shapes as they appear on the live market (snapshot.json, 2026-09-04). */
const freeOffer = (created_at: number): RawEvent =>
  ev(OFFER, BUYER, created_at, [["t", "maxplayer"], ["i", "do it for nothing"], ["amount", "0", "sat"],
    ["param", "deadline", String(created_at + 3600)], ["param", "payment", "none"]]);
const paidOffer = (created_at: number, amount = 500): RawEvent =>
  ev(OFFER, BUYER, created_at, [["t", "maxplayer"], ["i", "do it for sats"], ["amount", String(amount), "sat"],
    ["param", "deadline", String(created_at + 3600)]]);
const unpricedOffer = (created_at: number): RawEvent =>
  ev(OFFER, BUYER, created_at, [["t", "maxplayer"], ["i", "price unstated"], ["param", "deadline", String(created_at + 3600)]]);
const zeroWithoutTag = (created_at: number): RawEvent =>
  ev(OFFER, BUYER, created_at, [["t", "maxplayer"], ["i", "zero, but nobody said free"], ["amount", "0", "sat"]]);
// The two malformed shapes the classifier must fail CLOSED on (free-job-lane.md
// §1.3: the amount tag "is unchanged and stays required", and payment=none is
// what makes ITS zero mean "no payment leg"). Either half alone is not free.
const noneWithoutAmount = (created_at: number): RawEvent =>
  ev(OFFER, BUYER, created_at, [["t", "maxplayer"], ["i", "says free, states no amount"], ["param", "payment", "none"]]);
const noneWithNonzeroAmount = (created_at: number): RawEvent =>
  ev(OFFER, BUYER, created_at, [["t", "maxplayer"], ["i", "says free, prices 500"], ["amount", "500", "sat"], ["param", "payment", "none"]]);
/** `payment=none` with an amount string the protocol reader would reject. */
const noneWithAmountString = (created_at: number, amount: string): RawEvent =>
  ev(OFFER, BUYER, created_at, [["t", "maxplayer"], ["i", `says free, amount ${JSON.stringify(amount)}`], ["amount", amount, "sat"], ["param", "payment", "none"]]);

const claim = (o: RawEvent, at: number): RawEvent => ev(CLAIM, SELLER, at, [["t", "maxplayer"], ["e", o.id, "", "root"]]);
const award = (o: RawEvent, at: number): RawEvent => ev(AWARD, BUYER, at, [["t", "maxplayer"], ["e", o.id, "", "root"], ["p", SELLER]]);
const result = (o: RawEvent, at: number): RawEvent => ev(RESULT, SELLER, at, [["t", "maxplayer"], ["e", o.id, "", "root"]]);
const accept = (o: RawEvent, at: number): RawEvent => ev(ACCEPT, BUYER, at, [["t", "maxplayer"], ["e", o.id, "", "root"], ["p", SELLER]]);

/** offer → claim → award → result → accept. The last event of a free job. */
const lifecycle = (o: RawEvent): RawEvent[] => [o, claim(o, T0 + 1), award(o, T0 + 2), result(o, T0 + 3), accept(o, T0 + 4)];

test("free requires BOTH `param payment=none` AND `amount 0 sat` — either half alone is not free", () => {
  assert.equal(settlesWithoutPayment(freeOffer(T0)), true, "payment=none with amount 0 sat is free");
  assert.equal(settlesWithoutPayment(paidOffer(T0)), false);
  assert.equal(settlesWithoutPayment(unpricedOffer(T0)), false, "no amount tag at all is NOT silently free");
  assert.equal(settlesWithoutPayment(zeroWithoutTag(T0)), false, "a zero amount alone is not the test");
  assert.equal(events.parseEvent(freeOffer(T0))?.free, true, "parsed once, at the edge");
  assert.equal(events.parseEvent(unpricedOffer(T0))?.free, false);
  assert.equal(buildTrades(lifecycle(freeOffer(T0)))[0]!.free, true, "the trade join carries it");
});

test("control: `payment=none` with NO amount tag is NOT free — predicate, parsed offer, trade join", () => {
  const o = noneWithoutAmount(T0);
  assert.equal(settlesWithoutPayment(o), false, "the mode tag alone does not make an offer free");
  assert.equal(events.parseEvent(o)?.free, false, "parseEvent().free fails closed");
  assert.equal(buildTrades(lifecycle(o))[0]!.free, false, "buildTrades()[0].free fails closed");
  assert.equal(buyerBoard(lifecycle(o), NOW)[0]!.unpaidDeliveries, 1, "so its delivery is still an open question");
});

test("control: `payment=none` with a NONZERO amount is NOT free — predicate, parsed offer, trade join", () => {
  const o = noneWithNonzeroAmount(T0);
  assert.equal(settlesWithoutPayment(o), false, "a priced offer is not free whatever its mode tag says");
  assert.equal(events.parseEvent(o)?.free, false, "parseEvent().free fails closed");
  assert.equal(buildTrades(lifecycle(o))[0]!.free, false, "buildTrades()[0].free fails closed");
  assert.equal(buyerBoard(lifecycle(o), NOW)[0]!.unpaidDeliveries, 1, "so its delivery is still an open question");
});

test("control: `payment=none` with `amount 0junk sat` is NOT free — a numeric prefix is not a number", () => {
  // `parseFloat("0junk")` is 0. The protocol reader (`parse_offer`,
  // crates/maxplayer-core/src/gateway.rs) parses the amount as a whole-string
  // u64 and rejects this offer outright; the site must not call it free.
  const o = noneWithAmountString(T0, "0junk");
  assert.equal(settlesWithoutPayment(o), false, "a malformed amount string is not zero");
  assert.equal(events.parseEvent(o)?.free, false, "parseEvent().free fails closed");
  assert.equal(buildTrades(lifecycle(o))[0]!.free, false, "buildTrades()[0].free fails closed");
  assert.equal(buyerBoard(lifecycle(o), NOW)[0]!.unpaidDeliveries, 1, "so its delivery is still an open question");
});

test("the amount half is parsed whole-string by the protocol reader's unsigned-integer rule (`^\\+?[0-9]+$`)", () => {
  for (const ok of ["0", "00", "+0"]) {
    assert.equal(settlesWithoutPayment(noneWithAmountString(T0, ok)), true, `${JSON.stringify(ok)} is zero`);
  }
  for (const bad of [" 0", "0.0", "0e0", "-0", "0x0", ""]) {
    assert.equal(settlesWithoutPayment(noneWithAmountString(T0, bad)), false, `${JSON.stringify(bad)} is not accepted as zero`);
  }
});

test("a free job that reached ACCEPT is complete and settled: zero unpaid deliveries, zero active jobs, no payment wording", () => {
  const all = lifecycle(freeOffer(T0));
  const buyer = buyerBoard(all, NOW)[0]!;
  assert.equal(buyer.unpaidDeliveries, 0, `a free job is not a delivery awaiting money (counted ${buyer.unpaidDeliveries})`);
  assert.equal(buyer.receipted, 0, "and it is not counted as paid either");
  assert.equal(buyer.satsPaid, 0);
  assert.equal(buyer.awarded, 1, "the work itself still counts");

  const seller = sellerBoard(all, NOW)[0]!;
  assert.equal(seller.delivered, 1, "the runner's delivery counts");
  assert.equal(seller.receipted, 0, "with no receipt invented for it");
  assert.equal(seller.satsEarned, 0);

  const active = activeTradeJobs(all, NOW);
  assert.equal(active.byBuyer.get(BUYER)?.length ?? 0, 0, "the racer's lamp is out on accept");
  assert.equal(active.bySeller.get(SELLER)?.length ?? 0, 0, "the runner's lamp is out on accept");
  assert.equal(inProgressJobs(all, NOW).length, 0);

  // What the feed says about the offer: named free, not priced at zero, and
  // nothing about payment owed.
  const engine = createEngine({ windowKey: "all", now: () => NOW });
  for (const e of all) engine.ingest(e);
  engine.flush();
  const view = engine.view()!;
  const line = feedLine(view, events.parseEvent(all[0])!);
  assert.match(line, />free</, "the offer line names the job free");
  assert.doesNotMatch(line, /\$0\.00|0 sat/, "and does not price it at zero");
  for (const e of all) {
    const text = feedLine(view, events.parseEvent(e)!);
    assert.doesNotMatch(text, /unpaid|awaiting|owed/i, `no payment-owed wording on the ${events.parseEvent(e)!.stage} line`);
  }
});

test("a PAID job delivered with no receipt is still an unpaid delivery — not collateral damage", () => {
  const o = paidOffer(T0);
  const all = [o, claim(o, T0 + 1), award(o, T0 + 2), result(o, T0 + 3)];
  const buyer = buyerBoard(all, NOW)[0]!;
  assert.equal(buyer.unpaidDeliveries, 1, "delivered, no receipt, sats involved: still an open question");
  const accepted = buyerBoard([...all, accept(o, T0 + 4)], NOW)[0]!;
  assert.equal(accepted.unpaidDeliveries, 1, "an accept does not settle a PAID job's receipt question");
  const engine = createEngine({ windowKey: "all", now: () => NOW });
  for (const e of all) engine.ingest(e);
  engine.flush();
  assert.match(feedLine(engine.view()!, events.parseEvent(o)!), /sats/, "a paid offer is still priced");
  assert.doesNotMatch(feedLine(engine.view()!, events.parseEvent(o)!), />free</);
});

test("an offer with NO amount tag is not classed as free anywhere downstream", () => {
  const o = unpricedOffer(T0);
  const all = [o, claim(o, T0 + 1), award(o, T0 + 2), result(o, T0 + 3), accept(o, T0 + 4)];
  assert.equal(buildTrades(all)[0]!.free, false);
  assert.equal(buyerBoard(all, NOW)[0]!.unpaidDeliveries, 1, "price unstated is still a delivery with no receipt");
  const engine = createEngine({ windowKey: "all", now: () => NOW });
  for (const e of all) engine.ingest(e);
  engine.flush();
  assert.doesNotMatch(feedLine(engine.view()!, events.parseEvent(o)!), />free</);
});
