/**
 * Seller attribution — the trust-critical join.
 *
 * A completed trade must credit the runner the BUYER signed for (award,
 * accept, receipt), never whichever claimant a relay page happened to list
 * first. The reported failure: a job Sage paid to Bolty rendered as "Sage paid
 * Wally", because a losing late claim was the first seller-bearing event the
 * join met and it stuck.
 *
 * These feed the same events in three arrival orders — chronological, reverse,
 * shuffled — and assert the resolved winner, the feed copy, and the earnings
 * are identical and correct in every one.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { OFFER, CLAIM, AWARD, RESULT, RECEIPT, ACCEPT } from "../src/model/kinds.js";
import { buildTrades } from "../src/market/trades.js";
import { sellerBoard } from "../src/market/participants.js";
import { feedLine } from "../src/ui/board.js";
import { parseEvent, type RawEvent } from "../src/model/events.js";
import type { MarketView } from "../src/market/engine.js";

const hex = (seed: string): string => seed.repeat(64).slice(0, 64);
const SAGE = hex("a");   // buyer
const BOLTY = hex("b");  // the runner that won and delivered
const WATT = hex("c");   // late losing claimant
const WALLY = hex("d");  // late losing claimant

const NAMES = new Map<string, string>([
  [SAGE, "Sage"], [BOLTY, "Bolty"], [WATT, "Watt"], [WALLY, "Wally"],
]);

const T0 = 1_700_000_000;
let idCounter = 0;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");
const ev = (kind: number, pubkey: string, created_at: number, tags: string[][] = []): RawEvent =>
  ({ id: id(), kind, pubkey, created_at, tags, content: "" });

const root = (offerId: string): string[] => ["e", offerId, "", "root"];
const offer = (amount: number): RawEvent => ev(OFFER, SAGE, T0, [["t", "maxplayer"], ["i", "do the thing"], ["amount", String(amount)]]);
const claim = (offerId: string, seller: string, at: number): RawEvent => ev(CLAIM, seller, at, [root(offerId)]);
const result = (offerId: string, seller: string, at: number): RawEvent => ev(RESULT, seller, at, [root(offerId)]);
// Buyer-authored: p-tags the bound seller. This is the authenticated winner.
const award = (offerId: string, seller: string, at: number): RawEvent => ev(AWARD, SAGE, at, [root(offerId), ["p", seller]]);
const accept = (offerId: string, seller: string, at: number): RawEvent => ev(ACCEPT, SAGE, at, [root(offerId), ["p", seller]]);
const receipt = (offerId: string, seller: string, at: number, amount: number): RawEvent =>
  ev(RECEIPT, SAGE, at, [root(offerId), ["p", seller], ["amount", String(amount)]]);

/** A minimal view carrying only what feedLine reads: names and the trade join. */
const viewFor = (events: RawEvent[]): MarketView =>
  ({ names: NAMES, trades: new Map(buildTrades(events).map((t) => [t.offerId, t])) }) as unknown as MarketView;

const lineFor = (events: RawEvent[], raw: RawEvent): string =>
  feedLine(viewFor(events), parseEvent(raw)!);

/** The reported scenario, as raw events. Bolty wins; Watt and Wally claim late. */
function scenario(o: RawEvent) {
  return {
    o,
    bClaim: claim(o.id, BOLTY, T0 + 1),
    bAward: award(o.id, BOLTY, T0 + 2),
    bResult: result(o.id, BOLTY, T0 + 5),
    bAccept: accept(o.id, BOLTY, T0 + 6),
    bReceipt: receipt(o.id, BOLTY, T0 + 7, 100),
    // Both claim only, and only AFTER Bolty had already delivered.
    wattClaim: claim(o.id, WATT, T0 + 10),
    wallyClaim: claim(o.id, WALLY, T0 + 11),
  };
}

function ordering(s: ReturnType<typeof scenario>, name: string): RawEvent[] {
  const chronological = [s.o, s.bClaim, s.bAward, s.bResult, s.bAccept, s.bReceipt, s.wattClaim, s.wallyClaim];
  // Newest-first is how relay history actually arrives — the order that made
  // Wally's late claim win before the fix.
  if (name === "reverse") return [...chronological].reverse();
  if (name === "shuffled") return [s.wallyClaim, s.bAward, s.o, s.bReceipt, s.wattClaim, s.bClaim, s.bAccept, s.bResult];
  return chronological;
}

for (const name of ["chronological", "reverse", "shuffled"]) {
  test(`attribution: winner, copy and earnings are correct in ${name} order`, () => {
    const s = scenario(offer(100));
    const events = ordering(s, name);

    const trades = buildTrades(events);
    assert.equal(trades.length, 1);
    const trade = trades[0]!;
    assert.equal(trade.seller, BOLTY, "the buyer-signed winner is Bolty, not a late claimant");
    assert.equal(trade.sellerConflict, false, "the records agree — no conflict");

    const paid = lineFor(events, s.bReceipt);
    assert.ok(paid.includes("Bolty"), `receipt names Bolty (got: ${paid})`);
    assert.ok(!paid.includes("Wally") && !paid.includes("Watt"), `receipt must not name a loser (got: ${paid})`);
    assert.match(paid, /paid/, "the receipt line reads 'paid'");

    const accepted = lineFor(events, s.bAccept);
    assert.ok(accepted.includes("Bolty") && accepted.includes("accepted the delivery"),
      `accept names Bolty (got: ${accepted})`);
    assert.ok(!accepted.includes("Wally") && !accepted.includes("Watt"), `accept must not name a loser (got: ${accepted})`);

    const board = sellerBoard(events, T0 + 1000);
    const earned = (pk: string): number => board.find((r) => r.pubkey === pk)?.satsEarned ?? 0;
    assert.equal(earned(BOLTY), 100, "Bolty earned the 100 sats");
    assert.equal(earned(WATT), 0, "Watt earned nothing");
    assert.equal(earned(WALLY), 0, "Wally earned nothing");
  });
}

test("attribution: buyer-signed records that disagree render as conflicted, not a guess", () => {
  // The award names Bolty but the accept names Wally: the winner is genuinely
  // undeterminable from the public record. The UI must say so, never pick one.
  const o = offer(100);
  const events = [
    o,
    claim(o.id, BOLTY, T0 + 1),
    award(o.id, BOLTY, T0 + 2),
    result(o.id, BOLTY, T0 + 5),
    accept(o.id, WALLY, T0 + 6),
    receipt(o.id, BOLTY, T0 + 7, 100),
  ];

  const trade = buildTrades(events)[0]!;
  assert.equal(trade.sellerConflict, true, "award vs accept disagree — flagged as a conflict");
  assert.equal(trade.seller, null, "a conflicted trade names no winner");

  const paid = lineFor(events, events[5]!);
  assert.ok(paid.includes("undetermined runner"), `conflict renders as undetermined (got: ${paid})`);
  assert.ok(!paid.includes("Bolty") && !paid.includes("Wally"), `conflict must not guess a name (got: ${paid})`);

  // No one is credited earnings on a trade whose winner cannot be trusted.
  const board = sellerBoard(events, T0 + 1000);
  assert.equal(board.find((r) => r.pubkey === BOLTY)?.satsEarned ?? 0, 0, "conflict withholds earnings");
  assert.equal(board.find((r) => r.pubkey === WALLY)?.satsEarned ?? 0, 0, "conflict withholds earnings");
});
