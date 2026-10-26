/**
 * Session jobs on the board — the buyer's FOLLOW-UP (kind 3412).
 *
 * Bob, 2026-10-26: multi-turn jobs read as three RESULT → ACCEPT → RECEIPT
 * rounds with nothing between them. The buyer's reply is a public event
 * (docs/specs/session-jobs.md §2.1), so the feed shows it as REPLY, the job
 * works again until that turn is answered, and the rounds carry their turn.
 *
 * Event shapes follow the live relay (job e51df310…, three turns, 2026-10-26):
 * a turn-n result e-tags its follow-up `reply`; a session accept and every
 * receipt e-tag the result they settle `reply`; turn 1's accept names none.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { activeTradeJobs } from "../src/market/engine.js";
import { inProgressJobs } from "../src/market/participants.js";
import { buildTrades, openFollowUp } from "../src/market/trades.js";
import { ACCEPT, AWARD, CLAIM, FOLLOW_UP, MAXPLAYER_TAGGED_KINDS, OFFER, RECEIPT, RESULT, TRADE_STAGES } from "../src/model/kinds.js";
import { parseEvent, type RawEvent } from "../src/model/events.js";
import type { MarketView } from "../src/market/engine.js";
import { feedLine } from "../src/ui/board.js";

const hex = (seed: string): string => seed.repeat(64).slice(0, 64);
const BUYER = hex("a");
const SELLER = hex("b");
const NAMES = new Map([[BUYER, "sage"], [SELLER, "chef-flay"]]);
const T0 = 1_793_035_600;

let idCounter = 0x5000;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");
const ev = (kind: number, pubkey: string, created_at: number, tags: string[][] = []): RawEvent =>
  ({ id: id(), kind, pubkey, created_at, tags: [["t", "maxplayer"], ...tags], content: "" });

const root = (o: RawEvent): string[] => ["e", o.id, "", "root"];
const reply = (e: RawEvent): string[] => ["e", e.id, "", "reply"];

const offer = (at: number) => ev(OFFER, BUYER, at, [["i", "write notes.md"], ["amount", "100", "sat"],
  ["param", "deadline", String(at + 3600)]]);
const claim = (o: RawEvent, at: number) => ev(CLAIM, SELLER, at, [root(o), ["p", BUYER], ["session", "1"]]);
const award = (o: RawEvent, c: RawEvent, at: number) => ev(AWARD, BUYER, at, [root(o), ["e", c.id], ["p", BUYER], ["p", SELLER]]);
const result = (o: RawEvent, at: number, followUp?: RawEvent) =>
  ev(RESULT, SELLER, at, [root(o), ["p", BUYER], ["amount", "100", "sat"], ...(followUp ? [reply(followUp)] : [])]);
const accept = (o: RawEvent, c: RawEvent, at: number, r?: RawEvent) =>
  ev(ACCEPT, BUYER, at, [root(o), ["e", c.id], ["p", BUYER], ["p", SELLER], ...(r ? [reply(r)] : [])]);
const receipt = (o: RawEvent, r: RawEvent, at: number, amount = 100) =>
  ev(RECEIPT, BUYER, at, [["amount", String(amount), "sat"], root(o), reply(r), ["p", BUYER], ["p", SELLER]]);
const turn = (o: RawEvent, prev: RawEvent, n: number, at: number, extra: string[][] = []) =>
  ev(FOLLOW_UP, BUYER, at, [root(o), reply(prev), ["status", "turn"], ["turn", String(n)],
    ["param", "deadline", String(at + 900)], ["p", SELLER], ...extra]);
const close = (o: RawEvent, at: number) => ev(FOLLOW_UP, BUYER, at, [root(o), ["status", "close"], ["p", SELLER]]);

const viewFor = (events: RawEvent[]): MarketView =>
  ({ names: NAMES, trades: new Map(buildTrades(events).map((t) => [t.offerId, t])) }) as unknown as MarketView;
const lineFor = (events: RawEvent[], raw: RawEvent): string => feedLine(viewFor(events), parseEvent(raw)!);

/** A three-turn public session job, closed at the end. */
function threeTurns() {
  const o = offer(T0);
  const c = claim(o, T0 + 1);
  const a = award(o, c, T0 + 2);
  const r1 = result(o, T0 + 10);
  const acc1 = accept(o, c, T0 + 11);
  const rc1 = receipt(o, r1, T0 + 12);
  const f2 = turn(o, r1, 2, T0 + 20, [["i", "add 2 more fruits"]]);
  const r2 = result(o, T0 + 30, f2);
  const acc2 = accept(o, c, T0 + 31, r2);
  const rc2 = receipt(o, r2, T0 + 32);
  const f3 = turn(o, r2, 3, T0 + 40, [["i", "now write number.txt"]]);
  const r3 = result(o, T0 + 50, f3);
  const acc3 = accept(o, c, T0 + 51, r3);
  const rc3 = receipt(o, r3, T0 + 52);
  const end = close(o, T0 + 60);
  return { o, c, a, r1, acc1, rc1, f2, r2, acc2, rc2, f3, r3, acc3, rc3, end,
           all: [o, c, a, r1, acc1, rc1, f2, r2, acc2, rc2, f3, r3, acc3, rc3, end] };
}

test("kind 3412 is read from the relay and is a trade stage labelled reply", () => {
  assert.equal(FOLLOW_UP, 3412);
  assert.ok(MAXPLAYER_TAGGED_KINDS.includes(FOLLOW_UP), "requested under the #t maxplayer filter it carries");
  assert.equal(TRADE_STAGES[FOLLOW_UP], "reply");
});

test("a public follow-up parses to its job, seller, turn, deadline and message", () => {
  const { o, r1, f2 } = threeTurns();
  const e = parseEvent(f2)!;
  assert.equal(e.stage, "reply");
  assert.equal(e.offerId, o.id, "keyed on the root offer, not the result it replies to");
  assert.equal(e.buyer, BUYER);
  assert.equal(e.targetSeller, SELLER);
  assert.equal(e.status, "turn");
  assert.equal(e.turn, 2);
  assert.equal(e.replyTo, r1.id);
  assert.equal(e.deadline, T0 + 20 + 900);
  assert.equal(e.message, "add 2 more fruits");
  assert.equal(e.privateMessage, false);
});

test("a private follow-up shows no message, even if an i tag rides along", () => {
  const { o, r1 } = threeTurns();
  const f = turn(o, r1, 2, T0 + 20, [["content-id", hex("c")], ["content-commitment", hex("d")], ["i", "leaked?"]]);
  const e = parseEvent(f)!;
  assert.equal(e.privateMessage, true);
  assert.equal(e.message, null);
});

test("a malformed turn number is no turn", () => {
  const { o, r1 } = threeTurns();
  for (const bad of ["1", "0", "2x", "-3", "1e3", ""]) {
    const f = ev(FOLLOW_UP, BUYER, T0, [root(o), reply(r1), ["status", "turn"], ["turn", bad], ["p", SELLER]]);
    assert.equal(parseEvent(f)!.turn, null, `turn ${JSON.stringify(bad)}`);
  }
});

test("feed: a public reply names buyer, seller, turn and the escaped message", () => {
  const { all, f2, o, r1 } = threeTurns();
  assert.equal(lineFor(all, f2),
    '<span class="person">sage</span> replied to <span class="person">chef-flay</span> · turn 2 · add 2 more fruits');
  const hostile = turn(o, r1, 2, T0 + 21, [["i", '<img src=x onerror="alert(1)">']]);
  const line = lineFor([...all, hostile], hostile);
  assert.doesNotMatch(line, /<img/, "the buyer's message is escaped");
  assert.match(line, /&lt;img src=x onerror=&quot;alert\(1\)&quot;&gt;/);
});

test("feed: a private reply says so instead of a message", () => {
  const { all, o, r1 } = threeTurns();
  const f = turn(o, r1, 2, T0 + 20, [["content-id", hex("c")]]);
  const line = lineFor([...all, f], f);
  assert.match(line, /replied to <span class="person">chef-flay<\/span> · turn 2 · <span [^>]*>private reply<\/span>$/);
});

test("feed: a close reads as ending the session", () => {
  const { all, end } = threeTurns();
  assert.equal(lineFor(all, end),
    '<span class="person">sage</span> ended the session with <span class="person">chef-flay</span>');
});

test("feed: result, accept and receipt lines carry their turn on a multi-turn job", () => {
  const t = threeTurns();
  const cases: [RawEvent, number][] = [
    [t.r1, 1], [t.acc1, 1], [t.rc1, 1],
    [t.r2, 2], [t.acc2, 2], [t.rc2, 2],
    [t.r3, 3], [t.acc3, 3], [t.rc3, 3],
  ];
  for (const [e, n] of cases) {
    assert.match(lineFor(t.all, e), new RegExp(` · turn ${n}(?: · |$)`), `${parseEvent(e)!.stage} of turn ${n}`);
  }
  // Relay order is not event order: the numbering cannot depend on it.
  const shuffled = [...t.all].reverse();
  assert.match(lineFor(shuffled, t.rc2), / · turn 2 · /);
});

test("feed: a one-turn job's lines carry no turn", () => {
  const o = offer(T0);
  const c = claim(o, T0 + 1);
  const r = result(o, T0 + 10);
  const all = [o, c, award(o, c, T0 + 2), r, accept(o, c, T0 + 11), receipt(o, r, T0 + 12)];
  for (const e of all.slice(3)) assert.doesNotMatch(lineFor(all, e), /turn/);
});

test("receipts: each turn's receipt counts once — summed across turns, never doubled", () => {
  const t = threeTurns();
  assert.equal(buildTrades(t.all)[0]!.receiptAmount, 300, "three turns of 100");
  const dup = { ...t.rc2, id: id() };
  assert.equal(buildTrades([...t.all, dup])[0]!.receiptAmount, 300, "a re-published receipt for the same turn adds nothing");
  assert.equal(buildTrades(t.all)[0]!.turns, 3);
});

test("lamps: a reply puts the job back to work until that turn is answered", () => {
  const t = threeTurns();
  const upToTurn2 = [t.o, t.c, t.a, t.r1, t.acc1, t.rc1, t.f2];
  const now = T0 + 25;
  let active = activeTradeJobs(upToTurn2, now);
  assert.equal(active.byBuyer.get(BUYER)?.length, 1, "the buyer is active again after replying");
  assert.equal(active.bySeller.get(SELLER)?.length, 1, "and the seller is working again");
  const jobs = inProgressJobs(upToTurn2, now);
  assert.equal(jobs.length, 1, "the reopened turn is in progress");
  assert.equal(jobs[0]!.startedAt, t.f2.created_at, "from the reply, not the original award");
  assert.equal(jobs[0]!.deadline, T0 + 20 + 900, "against the turn's own deadline");

  const answered = [...upToTurn2, t.r2];
  active = activeTradeJobs(answered, T0 + 35);
  assert.equal(active.byBuyer.get(BUYER)?.length ?? 0, 0, "the turn's result ends it again");
  assert.equal(active.bySeller.get(SELLER)?.length ?? 0, 0);
  assert.equal(inProgressJobs(answered, T0 + 35).length, 0);
  assert.equal(openFollowUp(buildTrades(answered)[0]!), null);
});

test("lamps: a close never reopens, and neither does a turn past its deadline", () => {
  const t = threeTurns();
  const active = activeTradeJobs(t.all, T0 + 65);
  assert.equal(active.byBuyer.get(BUYER)?.length ?? 0, 0, "a closed session is not working");
  assert.equal(inProgressJobs(t.all, T0 + 65).length, 0);

  const stale = [t.o, t.c, t.a, t.r1, t.acc1, t.rc1, t.f2];
  const late = activeTradeJobs(stale, T0 + 20 + 901);
  assert.equal(late.bySeller.get(SELLER)?.length ?? 0, 0, "an unanswered turn past its deadline stops sweeping");
});

test("lamps: a reopened FREE job ends on the turn's result too", () => {
  const o = ev(OFFER, BUYER, T0, [["i", "free notes"], ["amount", "0", "sat"], ["param", "payment", "none"],
    ["param", "deadline", String(T0 + 3600)]]);
  const c = claim(o, T0 + 1);
  const r1 = result(o, T0 + 10);
  const f2 = turn(o, r1, 2, T0 + 20);
  const base = [o, c, award(o, c, T0 + 2), r1, accept(o, c, T0 + 11), f2];
  assert.equal(activeTradeJobs(base, T0 + 25).bySeller.get(SELLER)?.length, 1, "working again after the reply");
  const r2 = result(o, T0 + 30, f2);
  assert.equal(activeTradeJobs([...base, r2], T0 + 35).bySeller.get(SELLER)?.length ?? 0, 0, "done again on its result");
});
