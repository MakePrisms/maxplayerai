/**
 * A missed terminal event must not leave a lamp flashing.
 *
 * Bob, 2026-09-04: "sometimes events get missed on specific instances and so
 * events like Receipt doesn't show up and the lamps keep flashing even when
 * the job is already done ... my phone will show the right status, my
 * computer won't." Same relay, same job; one browser lost the event and its
 * forward mark fenced it from ever asking again.
 *
 * Driven end to end — relay source → engine → cache — against a fake relay
 * that HONOURS the filters it is sent (`since`, `until`, `kinds`, `#t`,
 * `limit`), so the source's own REQ floor decides what it gets back. That is
 * the point: at the base sha these tests go red on behaviour, not on wiring.
 *
 * The two modules under test are imported dynamically with a fallback for the
 * same reason: this file must LOAD at the base sha, where neither the overlap
 * constant nor the recovery module exists, so the evidence is an assertion
 * failure rather than a module-not-found.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { activeTradeJobs, createEngine } from "../src/market/engine.js";
import { inProgressJobs } from "../src/market/participants.js";
import { ACCEPT, AWARD, CLAIM, HEARTBEAT, MAXPLAYER_TAG, OFFER, RECEIPT, RESULT } from "../src/model/kinds.js";
import { createRelaySource } from "../src/source/relay.js";
import type { RawEvent } from "../src/model/events.js";

const config = await import("../src/config.js") as { POLL_OVERLAP_SECONDS?: number; RECOVERY_MAX_SECONDS?: number };
const OVERLAP = config.POLL_OVERLAP_SECONDS ?? 0;
const RECOVERY_MAX = config.RECOVERY_MAX_SECONDS ?? Infinity;
const { recoveryFloor } = await import("../src/source/recovery.js")
  .catch((): { recoveryFloor: (events: RawEvent[], now: number, max?: number) => number | null } =>
    ({ recoveryFloor: () => null }));

/* ---------------- fixtures ---------------- */

const hex = (seed: string): string => seed.repeat(64).slice(0, 64);
const BUYER = hex("a");
const SELLER = hex("b");
const BUYER2 = hex("c");
const T0 = 1_700_000_000;
const NOW = T0 + 6000;

let idCounter = 0;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");
const tagged = (tags: string[][]): string[][] => [["t", MAXPLAYER_TAG], ...tags];

function ev(kind: number, pubkey: string, created_at: number, tags: string[][] = [], content = ""): RawEvent {
  return { id: id(), kind, pubkey, created_at, tags, content };
}
const offer = (buyer: string, created_at: number, deadline: number): RawEvent =>
  ev(OFFER, buyer, created_at, tagged([["i", "do the thing"], ["amount", "10"], ["param", "deadline", String(deadline)]]));
const claim = (offerId: string, seller: string, created_at: number): RawEvent =>
  ev(CLAIM, seller, created_at, tagged([["e", offerId, "", "root"]]));
const award = (offerId: string, seller: string, created_at: number): RawEvent =>
  ev(AWARD, BUYER, created_at, tagged([["e", offerId, "", "root"], ["p", seller]]));
const result = (offerId: string, seller: string, created_at: number): RawEvent =>
  ev(RESULT, seller, created_at, tagged([["e", offerId, "", "root"]]));
const receipt = (offerId: string, created_at: number): RawEvent =>
  ev(RECEIPT, BUYER, created_at, tagged([["e", offerId, "", "root"], ["amount", "10"]]));
const accept = (offerId: string, created_at: number): RawEvent =>
  ev(ACCEPT, BUYER, created_at, tagged([["e", offerId, "", "root"]]));
const heartbeat = (seller: string, created_at: number): RawEvent =>
  ev(HEARTBEAT, seller, created_at, tagged([["d", "seat"]]), "{}");

/* ---------------- a relay that honours its filters ---------------- */

type Filter = Record<string, unknown>;

function matches(e: RawEvent, f: Filter): boolean {
  const kinds = f.kinds as number[] | undefined;
  if (kinds && !kinds.includes(e.kind)) return false;
  if (typeof f.since === "number" && e.created_at < f.since) return false;
  if (typeof f.until === "number" && e.created_at > f.until) return false;
  const t = f["#t"] as string[] | undefined;
  if (t && !(e.tags ?? []).some((tag) => tag[0] === "t" && t.includes(tag[1] ?? ""))) return false;
  return true;
}

/** Stored events, answered per REQ exactly as a NIP-01 relay would: newest first, capped by `limit`, then EOSE. */
class FakeRelay {
  events: RawEvent[] = [];
  sockets: RelaySocket[] = [];
  open(): RelaySocket {
    const s = new RelaySocket(this);
    this.sockets.push(s);
    return s;
  }
}

class RelaySocket {
  readyState = 1;
  onopen: (() => void) | null = null;
  onmessage: ((msg: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  /** Every REQ this socket was sent, in order. */
  reqs: { sub: string; filters: Filter[] }[] = [];
  private queue: string[] = [];
  constructor(private relay: FakeRelay) {}

  send(text: string): void {
    const frame = JSON.parse(text) as unknown[];
    if (frame[0] !== "REQ") return;
    const sub = String(frame[1]);
    const filters = frame.slice(2) as Filter[];
    this.reqs.push({ sub, filters });
    const hits = new Map<string, RawEvent>();
    for (const f of filters) for (const e of this.relay.events) if (matches(e, f)) hits.set(e.id, e);
    const limit = Math.min(...filters.map((f) => (typeof f.limit === "number" ? f.limit : Infinity)));
    const page = [...hits.values()].sort((a, b) => b.created_at - a.created_at).slice(0, limit);
    for (const e of page) this.queue.push(JSON.stringify(["EVENT", sub, e]));
    this.queue.push(JSON.stringify(["EOSE", sub]));
  }
  close(): void {}

  /** Deliver everything queued (the source may REQ again while draining). Returns EVENT frames delivered. */
  pump(): number {
    let delivered = 0;
    while (this.queue.length) {
      const data = this.queue.shift()!;
      if (data.startsWith("[\"EVENT\"")) delivered += 1;
      this.onmessage?.({ data });
    }
    return delivered;
  }
}

/* ---------------- boot, the way main.ts does it ---------------- */

/** IndexedDB stand-in → engine → relay source, with the store marked complete. */
function boot(relay: FakeRelay, cached: RawEvent[]) {
  const engine = createEngine({ windowKey: "all", now: () => NOW });
  for (const e of cached) engine.ingest(e);
  engine.flush();
  /** Events the cache actually took from the relay — a duplicate is not one. */
  let stored = 0;
  const timers: (() => void)[] = [];
  const source = createRelaySource(
    {
      url: "wss://test.invalid",
      transport: "poll",
      sinceHint: engine.cache.newest,
      storeComplete: true,
      recoveryFloor: recoveryFloor(engine.cache.all(), NOW),
      openSocket: () => relay.open() as unknown as WebSocket,
      now: () => NOW,
      setTimer: ((fn: () => void) => { timers.push(fn); return timers.length as unknown as ReturnType<typeof setTimeout>; }),
      clearTimer: () => {},
    },
    { onEvent: (e) => { if (engine.ingest(e).stored) stored += 1; }, onStatus: () => {}, onSynced: () => {} },
  );
  source.start();
  const sock = relay.sockets[relay.sockets.length - 1]!;
  sock.onopen!();
  sock.pump();
  engine.flush();
  return {
    engine,
    source,
    sock,
    /** One poll tick: fire the timer, drain the answer, settle the engine. */
    tick: () => { timers.shift()?.(); const n = sock.pump(); engine.flush(); return n; },
    stored: () => stored,
  };
}

const lampOn = (events: RawEvent[], pubkey: string): boolean => {
  const a = activeTradeJobs(events, NOW);
  return Boolean(a.byBuyer.get(pubkey)?.length || a.bySeller.get(pubkey)?.length);
};

/* ---------------- D1: prevention ---------------- */

test("a terminal event stamped in the mark's own second, delivered late, still ends the job", () => {
  // The same-second race. The mark rises to T from one event; the next ask
  // starts at T+1; a RECEIPT also stamped T that the relay had not returned
  // yet is now permanently below the floor. `since` is inclusive, so the ask
  // must reach at least T itself.
  const relay = new FakeRelay();
  const o = offer(BUYER, T0, T0 + 50_000);
  const c = claim(o.id, SELLER, T0 + 1);
  const a = award(o.id, SELLER, T0 + 2);
  const r = result(o.id, SELLER, T0 + 3);
  relay.events.push(o, c, a, r);
  const b = boot(relay, [o, c, a, r]);
  assert.ok(lampOn(b.engine.cache.all(), SELLER), "delivered, unpaid: the runner's lamp is on");

  // Tick 1: an unrelated offer stamped T raises the mark to T.
  const T = T0 + 100;
  relay.events.push(offer(BUYER2, T, T + 50_000));
  b.tick();
  assert.equal(b.engine.cache.size, 5, "tick 1 delivered the offer that raised the mark");

  // Then the receipt, ALSO stamped T, reaches the relay. Tick 2 must still see it.
  const paid = receipt(o.id, T);
  relay.events.push(paid);
  b.tick();
  const again = b.sock.reqs[b.sock.reqs.length - 1]!.filters[0]!.since as number;
  assert.ok(again <= T, `tick 2 asked since ${again}, above the mark ${T} — the receipt stamped ${T} can never be returned`);
  assert.ok(b.engine.cache.has(paid.id), "the late receipt was delivered");
  assert.equal(lampOn(b.engine.cache.all(), SELLER), false, "and the runner's lamp is out");
  assert.equal(lampOn(b.engine.cache.all(), BUYER), false, "and the racer's");
});

test("every forward ask trails the mark by exactly the overlap; the mark stays monotonic", () => {
  const relay = new FakeRelay();
  const o = offer(BUYER, T0, T0 + 50_000);
  relay.events.push(o);
  const b = boot(relay, [o]);
  // Nothing open (an offer alone is a racer's lamp, but no award) — so the
  // forward walk started at the hint, less the overlap.
  const walk = b.sock.reqs[0]!.filters[0]!;
  assert.equal(walk.since, Math.min(T0, recoveryFloor([o], NOW) ?? T0) - OVERLAP);

  relay.events.push(offer(BUYER2, T0 + 900, T0 + 50_000));
  b.tick();
  relay.events.push(offer(BUYER2, T0 + 850, T0 + 50_000)); // slow clock: stamped 50s below the mark
  const n = b.tick();
  const since = b.sock.reqs[b.sock.reqs.length - 1]!.filters[0]!.since as number;
  assert.equal(since, T0 + 900 - OVERLAP, "the tick after an event trails the NEW mark by the overlap");
  assert.ok(n >= 1, "the skewed event inside the overlap was delivered");
  assert.equal(b.engine.cache.size, 3);
});

/* ---------------- D2: recovery ---------------- */

test("a store seeded PAST a missed receipt converges to job-ended on the next boot, without clearing IndexedDB", () => {
  // THE STICKY CASE — Bob's computer. The cache holds the job's offer, claim,
  // award and result, plus later events that raised its newest stamp far
  // above the receipt it never received. History is marked complete, so the
  // base source resumes from `newest + 1` and the receipt is never asked for
  // again: every reload rebuilds the same lamp from the same IndexedDB. The
  // phone, with a fresh store, walks history and shows the job done.
  const relay = new FakeRelay();
  const o = offer(BUYER, T0, T0 + 50_000);
  const c = claim(o.id, SELLER, T0 + 1);
  const a = award(o.id, SELLER, T0 + 2);
  const r = result(o.id, SELLER, T0 + 3);
  const paid = receipt(o.id, T0 + 100);                   // on the relay, never in this store
  const beat = heartbeat(SELLER, T0 + 4000);
  const later = offer(BUYER2, T0 + 5000, T0 + 50_000);     // what raised the mark
  relay.events.push(o, c, a, r, paid, beat, later);
  const cached = [o, c, a, r, beat, later];

  const b = boot(relay, cached);
  assert.ok(b.engine.cache.newest! > paid.created_at, "precondition: the store's mark is above the missed receipt");
  assert.equal(b.engine.cache.has(paid.id), true,
    `the boot walk did not reach the receipt (asked since ${String(b.sock.reqs[0]!.filters[0]!.since)}, receipt at ${paid.created_at})`);
  assert.equal(lampOn(b.engine.cache.all(), SELLER), false, "the runner's lamp is out");
  assert.equal(lampOn(b.engine.cache.all(), BUYER), false, "the racer's lamp is out");
  for (const e of cached) assert.ok(b.engine.cache.has(e.id), "nothing the store held was thrown away to get here");

  // BOUNDED: the reach-back is the boot's walk, not the tick. The very next
  // poll asks from the mark, not from the job's start.
  b.tick();
  const poll = b.sock.reqs[b.sock.reqs.length - 1]!.filters[0]!.since as number;
  assert.equal(poll, T0 + 5000 - OVERLAP, "the poll floor is the mark less the overlap, not the recovery floor");
});

test("a missed ACCEPT heals the same way, and an in-progress row (no result at all) heals too", () => {
  const relay = new FakeRelay();
  const o = offer(BUYER, T0, T0 + 50_000);
  const c = claim(o.id, SELLER, T0 + 1);
  const a = award(o.id, SELLER, T0 + 2);
  const ok = accept(o.id, T0 + 200);
  const later = offer(BUYER2, T0 + 5000, T0 + 50_000);
  relay.events.push(o, c, a, ok, later);
  const cached = [o, c, a, later];
  assert.equal(inProgressJobs(cached, NOW).length, 1, "precondition: the store shows the job awarded and undelivered");

  const b = boot(relay, cached);
  assert.equal(inProgressJobs(b.engine.cache.all(), NOW).length, 0, "the in-progress row is gone");
  assert.equal(lampOn(b.engine.cache.all(), SELLER), false);
});

test("recoveryFloor: null with nothing open, the oldest open job otherwise, never further back than the cap", () => {
  const o = offer(BUYER, T0, T0 + 50_000);
  const c = claim(o.id, SELLER, T0 + 1);
  const a = award(o.id, SELLER, T0 + 2);
  const r = result(o.id, SELLER, T0 + 3);
  assert.equal(recoveryFloor([o, c, a, r, receipt(o.id, T0 + 4)], NOW), null, "a paid job needs no recovery");
  assert.equal(recoveryFloor([o, c, a, r], NOW), T0, "an unpaid delivery reaches back to the offer");
  assert.equal(recoveryFloor([], NOW), null);

  // An award nobody ever delivers against stays open forever; the floor must not.
  const ancient = NOW - 30 * 86400;
  const o2 = offer(BUYER, ancient, NOW + 50_000);
  const stale = [o2, claim(o2.id, SELLER, ancient + 1), award(o2.id, SELLER, ancient + 2)];
  assert.equal(recoveryFloor(stale, NOW), NOW - RECOVERY_MAX, "capped at RECOVERY_MAX_SECONDS");
  assert.equal(recoveryFloor(stale, NOW, 600), NOW - 600, "and the cap is a parameter");
});

/* ---------------- re-delivery is free ---------------- */

test("re-delivery under the overlap changes nothing: no duplicate rows, no slot churn, identical view", () => {
  const relay = new FakeRelay();
  const o = offer(BUYER, T0, T0 + 50_000);
  const c = claim(o.id, SELLER, T0 + 1);
  const a = award(o.id, SELLER, T0 + 2);
  const r = result(o.id, SELLER, T0 + 3);
  const paid = receipt(o.id, T0 + 100);
  const oldBeat = heartbeat(SELLER, T0 + 4900);            // inside the overlap: re-delivered every tick
  const newBeat = heartbeat(SELLER, T0 + 4990);            // supersedes oldBeat in the cache
  const later = offer(BUYER2, T0 + 5000, T0 + 50_000);
  relay.events.push(o, c, a, r, paid, oldBeat, newBeat, later);
  const b = boot(relay, [o, c, a, r, paid, oldBeat, newBeat, later]);

  const settled = b.stored();
  const ids = [...b.engine.cache.all().map((e) => e.id)].sort();
  const slot = b.engine.cache.slot(HEARTBEAT, SELLER, "seat")?.id;
  assert.equal(slot, newBeat.id, "precondition: the newest heartbeat holds the slot");
  const snapshot = (): string => {
    const v = b.engine.view()!;
    return JSON.stringify({ buyers: v.buyers, sellers: v.sellers, feed: v.feed, metrics: v.metrics });
  };
  const before = snapshot();

  let redelivered = 0;
  for (let i = 0; i < 5; i++) redelivered += b.tick();
  assert.ok(redelivered >= 5, `the overlap re-delivered events (${redelivered}) — otherwise this test proves nothing`);

  assert.deepEqual([...b.engine.cache.all().map((e) => e.id)].sort(), ids, "no duplicate rows");
  assert.equal(b.engine.cache.size, ids.length);
  assert.equal(b.engine.cache.slot(HEARTBEAT, SELLER, "seat")?.id, newBeat.id, "the superseded heartbeat did not retake its slot");
  // The engine only schedules a recompute for a stored event (market.test.ts
  // pins that), so "nothing stored" is "nothing repainted".
  assert.equal(b.stored(), settled, "the cache took none of the re-delivered events");
  assert.equal(snapshot(), before, "render input is byte-identical");
});
