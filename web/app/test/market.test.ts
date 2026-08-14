/**
 * Core market semantics — the invariants that survived the redesign, pinned
 * against the TypeScript rebuild. Fixture events are hand-built raw events;
 * ids/pubkeys must be 64-char lowercase hex or parseEvent rejects them (that
 * rejection is itself under test).
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { parseEvent } from "../src/model/events.js";
import { OFFER, CLAIM, AWARD, RESULT, RECEIPT, ACCEPT, FEEDBACK, HEARTBEAT, PROFILE } from "../src/model/kinds.js";
import { createCache } from "../src/store/cache.js";
import { buildTrades, marketMetrics } from "../src/market/trades.js";
import { buyerBoard, sellerBoard, inProgressJobs, participantActivity, relatedActivity, JOB_OVERDUE, JOB_WORKING, withinWindow } from "../src/market/participants.js";
import { activeTradeJobs, createEngine, rankClimbs, ACTIVE_GRACE_SECONDS } from "../src/market/engine.js";
import type { RawEvent } from "../src/model/events.js";

const SRC = join(dirname(fileURLToPath(import.meta.url)), "..", "src");

const hex = (seed: string): string => seed.repeat(64).slice(0, 64);
const BUYER = hex("a");
const SELLER = hex("b");
const SELLER2 = hex("c");
const T0 = 1_700_000_000;

let idCounter = 0;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");

function ev(kind: number, pubkey: string, created_at: number, tags: string[][] = [], content = ""): RawEvent {
  return { id: id(), kind, pubkey, created_at, tags, content };
}

function offer(created_at: number, opts: { amount?: number; deadline?: number; self?: boolean } = {}): RawEvent {
  const tags: string[][] = [["t", "maxplayer"], ["i", "do the thing"]];
  if (opts.amount != null) tags.push(["amount", String(opts.amount)]);
  if (opts.deadline != null) tags.push(["param", "deadline", String(opts.deadline)]);
  if (opts.self) tags.push(["t", "self-trade"]);
  return ev(OFFER, BUYER, created_at, tags);
}

const claim = (offerId: string, seller: string, created_at: number): RawEvent =>
  ev(CLAIM, seller, created_at, [["e", offerId, "", "root"]]);
const award = (offerId: string, seller: string, created_at: number): RawEvent =>
  ev(AWARD, BUYER, created_at, [["e", offerId, "", "root"], ["p", seller]]);
const result = (offerId: string, seller: string, created_at: number): RawEvent =>
  ev(RESULT, seller, created_at, [["e", offerId, "", "root"]]);
const receipt = (offerId: string, created_at: number, amount: number): RawEvent =>
  ev(RECEIPT, BUYER, created_at, [["e", offerId, "", "root"], ["amount", String(amount)]]);
const accept = (offerId: string, created_at: number): RawEvent =>
  ev(ACCEPT, BUYER, created_at, [["e", offerId, "", "root"]]);
const feedback = (offerId: string, seller: string, created_at: number, tags: string[][] = [], content = ""): RawEvent =>
  ev(FEEDBACK, seller, created_at, [["e", offerId, "", "root"], ...tags], content);

test("parseEvent rejects non-hex ids and pubkeys at the boundary", () => {
  assert.equal(parseEvent({ id: "nope", kind: OFFER, pubkey: BUYER, created_at: T0 }), null);
  assert.equal(parseEvent({ id: id(), kind: OFFER, pubkey: "NOT-HEX", created_at: T0 }), null);
  assert.ok(parseEvent(offer(T0)));
});

test("cache dedupes, resolves replaceable slots newest-wins, reports eviction", () => {
  const cache = createCache();
  const beat1 = ev(HEARTBEAT, SELLER, T0, [["d", "seat"]]);
  const beat2 = ev(HEARTBEAT, SELLER, T0 + 10, [["d", "seat"]]);
  assert.equal(cache.ingest(beat1).stored, true);
  assert.equal(cache.ingest(beat1).stored, false);
  const second = cache.ingest(beat2);
  assert.equal(second.stored, true);
  assert.equal(second.evictedId, beat1.id, "eviction is reported so the DB can follow");
  assert.equal(cache.size, 1, "superseded heartbeat is gone");
  // Ties go to the incumbent.
  const beat3 = ev(HEARTBEAT, SELLER, T0 + 10, [["d", "seat"]]);
  assert.equal(cache.ingest(beat3).reason, "superseded");
});

test("trades key on the root offer and take the earliest stamp per stage", () => {
  const o = offer(T0, { amount: 100 });
  const dup = { ...receipt(o.id, T0 + 50, 100) };
  const later = { ...receipt(o.id, T0 + 90, 100), id: id() };
  const trades = buildTrades([o, later, dup]);
  assert.equal(trades.length, 1);
  assert.equal(trades[0]!.at.receipt, T0 + 50, "earliest receipt stamp wins");
});

test("a receipt amount is a FLOOR — relay order never lowers it", () => {
  // The stamp test above uses two receipts of the SAME amount, so it cannot
  // see this: the amount was a plain assignment, making the figure whichever
  // receipt the relay happened to page last. Two orders of the same three
  // events must not produce two different numbers.
  const o = offer(T0, { amount: 1000 });
  const high = receipt(o.id, T0 + 5, 1000);
  const low = receipt(o.id, T0 + 9, 1);

  assert.equal(buildTrades([o, high, low])[0]!.receiptAmount, 1000, "a later low receipt cannot lower the floor");
  assert.equal(buildTrades([o, low, high])[0]!.receiptAmount, 1000, "and the reverse order agrees");
  // The figure the page actually renders.
  assert.equal(marketMetrics([o, high, low]).satsInReceipts, 1000);
  assert.equal(marketMetrics([o, low, high]).satsInReceipts, 1000);
});

test("self-trades are excluded from metrics and counted, never silent", () => {
  const arms = offer(T0, { amount: 10 });
  const self = offer(T0 + 1, { amount: 10, self: true });
  const m = marketMetrics([arms, self, receipt(arms.id, T0 + 2, 10), receipt(self.id, T0 + 3, 10)]);
  assert.equal(m.funnel.posted, 1);
  assert.equal(m.selfTrades, 1);
  assert.equal(m.satsInReceipts, 10, "the self-trade's receipt is not in the floor");
});

test("boards rank deterministically with pubkey tiebreaks", () => {
  const o1 = offer(T0, { amount: 5 });
  const o2 = offer(T0 + 1, { amount: 5 });
  const events = [o1, o2, claim(o1.id, SELLER, T0 + 2), claim(o2.id, SELLER2, T0 + 3)];
  const a = sellerBoard(events, T0 + 10);
  const b = sellerBoard([...events].reverse(), T0 + 10);
  assert.deepEqual(a.map((r) => r.pubkey), b.map((r) => r.pubkey), "arrival order must not affect rank");
});

test("inProgressJobs: awarded-undelivered is working, blown deadline is overdue, delivery ends it", () => {
  const deadline = T0 + 1000;
  const o = offer(T0, { deadline });
  const events = [o, claim(o.id, SELLER, T0 + 1), award(o.id, SELLER, T0 + 2)];
  const working = inProgressJobs(events, T0 + 100);
  assert.equal(working.length, 1);
  assert.equal(working[0]!.state, JOB_WORKING);
  const overdue = inProgressJobs(events, deadline + 400);
  assert.equal(overdue[0]!.state, JOB_OVERDUE);
  const done = inProgressJobs([...events, result(o.id, SELLER, T0 + 50)], T0 + 100);
  assert.equal(done.length, 0, "delivery ends the job");
  assert.throws(() => inProgressJobs(events, NaN), /now is required/);
});

test("activeTradeJobs: racer active from offer, runner from claim, losses and payment end it", () => {
  const o = offer(T0, { deadline: T0 + 5000 });
  const both = [o, claim(o.id, SELLER, T0 + 1), claim(o.id, SELLER2, T0 + 2)];
  let active = activeTradeJobs(both, T0 + 10);
  assert.ok(active.byBuyer.get(BUYER)?.length, "racer active from posting");
  assert.ok(active.bySeller.get(SELLER)?.length, "claimer active from claiming");
  assert.ok(active.bySeller.get(SELLER2)?.length, "second claimer too");

  const awarded = [...both, award(o.id, SELLER, T0 + 3)];
  active = activeTradeJobs(awarded, T0 + 10);
  assert.ok(active.bySeller.get(SELLER)?.length, "winner stays active");
  assert.ok(!active.bySeller.get(SELLER2)?.length, "loser stops the moment they lose");

  // Accepting the delivery ends the work: the buyer signed off on it. The
  // receipt (payment) is a separate, optional announcement, so the lamp must
  // not wait on it — that kept it flashing long after each completed job.
  const accepted = [...awarded, result(o.id, SELLER, T0 + 4), accept(o.id, T0 + 5)];
  active = activeTradeJobs(accepted, T0 + 10);
  assert.ok(!active.bySeller.get(SELLER)?.length, "accept ends the runner's work");
  assert.ok(!active.byBuyer.get(BUYER)?.length, "and the racer's");

  // A receipt with no prior accept also ends it — the money landed.
  const paid = [...awarded, result(o.id, SELLER, T0 + 4), receipt(o.id, T0 + 6, 5)];
  active = activeTradeJobs(paid, T0 + 10);
  assert.ok(!active.byBuyer.get(BUYER)?.length, "the receipt ends the racer's activity");
  assert.ok(!active.bySeller.get(SELLER)?.length, "and the runner's");
});

test("activeTradeJobs: only TERMINAL feedback ends a job — progress notes never do (§7.2)", () => {
  const o = offer(T0, { deadline: T0 + 5000 });
  const base = [o, claim(o.id, SELLER, T0 + 1), award(o.id, SELLER, T0 + 2)];

  // A routine progress note carries a status tag and readable content. It must
  // NOT read as a decline: feedbackReason() always returns text, so gating on
  // the reason alone once ended lamps on every progress update.
  const progressing = [...base, feedback(o.id, SELLER, T0 + 3, [["status", "progress"]], "progress: halfway there")];
  let active = activeTradeJobs(progressing, T0 + 10);
  assert.ok(active.bySeller.get(SELLER)?.length, "progress keeps the runner working");
  assert.ok(active.byBuyer.get(BUYER)?.length, "and the racer");
  assert.equal(buildTrades(progressing.map(parseEvent).filter((e) => e != null))[0]!.declineReason, null,
    "a progress note is not a decline");

  // A classified terminal code ends it.
  const released = [...base, feedback(o.id, SELLER, T0 + 4, [["reason_code", "execution_failed"]], "execution_failed: no dice")];
  active = activeTradeJobs(released, T0 + 10);
  assert.ok(!active.bySeller.get(SELLER)?.length, "terminal feedback ends the runner's job");
  assert.ok(!active.byBuyer.get(BUYER)?.length, "and the racer's");

  // Unclassified feedback (no tags) is conservative: NOT terminal.
  const vague = [...base, feedback(o.id, SELLER, T0 + 5, [], "hm")];
  active = activeTradeJobs(vague, T0 + 10);
  assert.ok(active.bySeller.get(SELLER)?.length, "unclassified feedback leaves the job running");
});

test("activeTradeJobs: delivered-but-never-receipted expires after the grace period", () => {
  const o = offer(T0, { deadline: T0 + 5000 });
  const events = [o, claim(o.id, SELLER, T0 + 1), award(o.id, SELLER, T0 + 2), result(o.id, SELLER, T0 + 3)];
  const fresh = activeTradeJobs(events, T0 + 100);
  assert.ok(fresh.bySeller.get(SELLER)?.length, "awaiting payment reads active");
  const stale = activeTradeJobs(events, T0 + 3 + ACTIVE_GRACE_SECONDS + 1);
  assert.ok(!stale.bySeller.get(SELLER)?.length, "receipts are optional, so activity must expire");
});

test("rankClimbs diffs all-time standings now vs 24h ago; new entrants are not climbers", () => {
  const o1 = offer(T0, { amount: 5 });
  const day = 86400;
  const t = T0 + day + 100;
  const events = [
    o1, claim(o1.id, SELLER, T0 + 1), award(o1.id, SELLER, T0 + 2), result(o1.id, SELLER, T0 + 3),
  ];
  // Yesterday SELLER2 had one delivery, SELLER had none → today SELLER passes.
  const o0 = offer(T0 - 10, { amount: 5 });
  const o2 = offer(T0 - 9, { amount: 5 });
  const history = [
    o0, claim(o0.id, SELLER2, T0 - 8), award(o0.id, SELLER2, T0 - 7), result(o0.id, SELLER2, T0 - 6),
    o2, claim(o2.id, SELLER, T0 - 5),
  ];
  const all = [...history, ...events, result(o1.id, SELLER, t - 50), result(o2.id, SELLER, t - 40)];
  const climbs = rankClimbs((evts, now) => sellerBoard(evts, now), all, t);
  assert.ok((climbs.get(SELLER) ?? 0) >= 1, "seller passed someone since yesterday");
  assert.equal(climbs.get(SELLER2), undefined, "the seller passed does not climb");
});

test("windowed views include only in-window events", () => {
  const oldEvent = offer(T0);
  const newEvent = offer(T0 + 100_000);
  const t = T0 + 100_100;
  const day = withinWindow([oldEvent, newEvent], "24h", t);
  assert.deepEqual(day.map((e) => e.id), [newEvent.id]);
  const all = withinWindow([oldEvent, newEvent], "all", t);
  assert.equal(all.length, 2);
});

test("engine recomputes on ingest (coalesced), exposes a coherent view", async () => {
  const t = T0 + 50;
  const engine = createEngine({ windowKey: "all", now: () => t });
  const o = offer(T0, { amount: 42 });
  const views: number[] = [];
  engine.subscribe((view) => views.push(view.buyers.length));
  engine.ingest(o);
  engine.ingest(claim(o.id, SELLER, T0 + 1));
  engine.ingest(o); // duplicate — must not schedule anything
  await new Promise((resolve) => setTimeout(resolve, 80));
  assert.equal(views.length, 1, "a burst coalesces to one recompute");
  const view = engine.view();
  assert.ok(view);
  assert.equal(view.buyers[0]?.pubkey, BUYER);
  assert.equal(view.buyers[0]?.posted, 1);
  assert.ok(view.trades.get(o.id), "counterparty join is on the view");
  assert.ok(view.activeByBuyer.get(BUYER)?.length, "racer is active from the offer");
});

test("buyer board counts receipts as the floor and prices as medians", () => {
  const o1 = offer(T0, { amount: 10 });
  const o2 = offer(T0 + 1, { amount: 30 });
  const events = [o1, o2, receipt(o1.id, T0 + 5, 10)];
  const board = buyerBoard(events, T0 + 10);
  assert.equal(board.length, 1);
  assert.equal(board[0]!.posted, 2);
  assert.equal(board[0]!.receipted, 1);
  assert.equal(board[0]!.satsPaid, 10);
  assert.equal(board[0]!.medianPrice, 20);
});

test("a losing claimant's refusal ends only its OWN claim, not the awarded runner's job", () => {
  // Two runners claim one offer; the racer picks the second. The FIRST one then
  // refuses. Nothing about that should touch the winner — but the trade join
  // took the first seller it saw in array order, and the decline was recorded
  // trade-wide, so the loser's refusal darkened the winner's lamp AND the
  // racer's, and the release was charged to whichever seat came first in the
  // array. Both orders of the same events must agree.
  const o = offer(T0, { deadline: T0 + 5000 });
  const events = [
    o,
    claim(o.id, SELLER, T0 + 1),   // claims, loses, refuses
    claim(o.id, SELLER2, T0 + 2),  // claims, wins, still working
    award(o.id, SELLER2, T0 + 3),
    feedback(o.id, SELLER, T0 + 4, [["reason_code", "execution_failed"]], "execution_failed: not mine"),
  ];

  for (const [label, arr] of [["in order", events], ["reversed", [...events].reverse()]] as const) {
    const trade = buildTrades(arr.map(parseEvent).filter((e) => e != null))[0]!;
    assert.equal(trade.seller, SELLER2, `${label}: the buyer-signed award names the runner, not the first claim`);
    assert.equal(trade.sellerConflict, false, `${label}: one award, so nothing to conflict`);
    assert.equal(trade.declineReason, null, `${label}: a loser's refusal is not the trade's decline`);
    assert.deepEqual(trade.releasedBy, [SELLER], `${label}: it is the loser's own record`);

    const active = activeTradeJobs(arr, T0 + 10);
    assert.ok(active.bySeller.get(SELLER2)?.length, `${label}: the awarded runner is still working`);
    assert.ok(active.byBuyer.get(BUYER)?.length, `${label}: and the racer's lamp stays on`);
    assert.ok(!active.bySeller.get(SELLER)?.length, `${label}: the runner that refused is done`);

    const board = sellerBoard(arr, T0 + 10);
    const won = board.find((r) => r.pubkey === SELLER2);
    const lost = board.find((r) => r.pubkey === SELLER);
    assert.ok(won, `${label}: the awarded runner has a row`);
    assert.ok(lost, `${label}: the second claimant has a row of its own`);
    assert.equal(lost!.claimed, 1, `${label}: every claim is counted`);
    assert.equal(won!.claimed, 1);
    assert.equal(lost!.released, 1, `${label}: the release is charged to whoever refused`);
    assert.equal(won!.released, 0, `${label}: never to the runner still working`);
  }
});

test("the boards COUNT self-trades, and no copy claims the page excludes them", () => {
  // Two halves of one invariant, because the defect was the gap between them.
  //
  // Ruled by bob: the boards keep counting self-trades and the misleading
  // sentence goes. So this is a DECISION pinned here, not an oversight — if
  // someone later "fixes" it by filtering the boards, the standings move and
  // this goes red on purpose.
  const arms = offer(T0, { amount: 10 });
  const self = offer(T0 + 1, { amount: 10, self: true });

  const board = buyerBoard([arms, self], T0 + 10);
  assert.equal(board[0]!.posted, 2, "the boards include self-trades");
  // The stats figures are the ones that genuinely exclude them, which is why
  // the note in board.ts saying so is accurate and stays.
  assert.equal(marketMetrics([arms, self]).funnel.posted, 1, "metrics still exclude them");

  // Therefore no copy may tell a reader the whole page excludes them. The
  // boards are on that page.
  const docks = readFileSync(join(SRC, "ui", "docks.ts"), "utf8");
  assert.ok(
    !/excluded from the figures on this page/.test(docks),
    "the self-trade note must not claim a page-wide exclusion the boards do not honour",
  );
});

test("profiles enrich rows but never create them", () => {
  const profile = ev(PROFILE, hex("d"), T0, [], JSON.stringify({ name: "lurker" }));
  const board = sellerBoard([profile], T0 + 10);
  assert.equal(board.length, 0, "a kind-0 alone earns no seller row");
});

test("same-second events sort by lifecycle order, not by random id", () => {
  // Offer and claim published in the SAME second — the coarse timestamp
  // cannot order them, the stage must.
  const o = offer(T0, { amount: 5 });
  const c = claim(o.id, SELLER, T0);
  const r = receipt(o.id, T0, 5);
  const history = relatedActivity([c, r, o], o.id);
  assert.deepEqual(history.map((e) => e.stage), ["offer", "claim", "receipt"],
    "job history reads oldest-first in lifecycle order");
  const activity = participantActivity([c, r, o], BUYER);
  assert.deepEqual(activity.map((e) => e.stage), ["receipt", "claim", "offer"],
    "participant activity reads newest-first in reverse lifecycle order");
});
