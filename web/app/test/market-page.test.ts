/**
 * The Market page's data layer (src/site/market-data.ts): WHEN the page is told.
 *
 * bob, 2026-11-20: the page must load in under three seconds, show placeholders until what it shows is whole, and
 * never count up or change afterwards; and an agent that has a name must always show it. Before this layer a first
 * visit read the whole history (six seconds of requests) and the board counted up while it did.
 *
 * Driven end to end — the real relay source, engine, cache and IndexedDB store — against a fake relay that honours
 * the filters it is sent and answers ONE request per `step()`, so a test can stand between two pages of history.
 */
import "fake-indexeddb/auto";
import { IDBFactory } from "fake-indexeddb";
import assert from "node:assert/strict";
import { test } from "node:test";
import { ACCEPT, AWARD, CLAIM, HEARTBEAT, MAXPLAYER_TAG, OFFER, PROFILE, RECEIPT, RESULT } from "../src/model/kinds.js";
import { createEventDb } from "../src/store/db.js";
import { decodeNpub } from "../src/trade/nip19.js";
import { startMarket, type MarketHandle, type PlainView } from "../src/site/market-data.js";
import type { RawEvent } from "../src/model/events.js";

/* ---------------- a market: 240 jobs this week, 150 older ---------------- */

const hex = (seed: string): string => seed.repeat(64).slice(0, 64);
const BUYER = hex("a"), BUYER2 = hex("b");
const SELLERS = [hex("c"), hex("d"), hex("e")] as const;
const NAMES: Record<string, string> = { [BUYER]: "north-desk", [BUYER2]: "growth-team", [SELLERS[0]]: "swift-code", [SELLERS[1]]: "pixel-frontend", [SELLERS[2]]: "plain-review" };
const SECRET = "translate the unannounced plan";
const DAY = 86400;
const NOW = Math.floor(Date.now() / 1000);

let idCounter = 0;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");
const tagged = (tags: string[][]): string[][] => [["t", MAXPLAYER_TAG], ...tags];
const ev = (kind: number, pubkey: string, created_at: number, tags: string[][] = [], content = ""): RawEvent =>
  ({ id: id(), kind, pubkey, created_at, tags, content });

/** One finished job: offer, claim, award, result, accept, receipt, a few seconds apart. */
function job(buyer: string, seller: string, at: number): RawEvent[] {
  const offer = ev(OFFER, buyer, at, tagged([["i", SECRET], ["amount", "100"], ["param", "deadline", String(at + 3600)]]));
  const root = [["e", offer.id, "", "root"]];
  return [
    offer,
    ev(CLAIM, seller, at + 1, tagged(root)),
    ev(AWARD, buyer, at + 2, tagged([...root, ["p", seller]])),
    ev(RESULT, seller, at + 3, tagged(root)),
    ev(ACCEPT, buyer, at + 4, tagged(root)),
    ev(RECEIPT, buyer, at + 5, tagged([...root, ["amount", "100"]])),
  ];
}

const WEEK_JOBS = 240, OLD_JOBS = 150;     // the week (6 events a job) is more than one 1000-event page
function market(): RawEvent[] {
  const events: RawEvent[] = [];
  for (let i = 0; i < WEEK_JOBS; i++) events.push(...job(i % 4 ? BUYER : BUYER2, SELLERS[i % 3]!, NOW - 600 - i * Math.floor((6 * DAY) / WEEK_JOBS)));
  for (let i = 0; i < OLD_JOBS; i++) events.push(...job(i % 3 ? BUYER : BUYER2, SELLERS[i % 2]!, NOW - 8 * DAY - i * 6 * 3600));
  for (const s of SELLERS) events.push(ev(HEARTBEAT, s, NOW - 30, tagged([["d", "seat"], ["rate", "100"], ["accepting", "y"], ["admits_targeted", "open"]]), "{}"));
  for (const [pk, name] of Object.entries(NAMES)) events.push(ev(PROFILE, pk, NOW - 40 * DAY, [], JSON.stringify({ name, about: `${name} does the work` })));
  return events;
}

/* ---------------- a relay that honours its filters, one request at a time ---------------- */

type Filter = Record<string, unknown>;
function matches(e: RawEvent, f: Filter): boolean {
  const kinds = f.kinds as number[] | undefined;
  if (kinds && !kinds.includes(e.kind)) return false;
  if (typeof f.since === "number" && e.created_at < f.since) return false;
  if (typeof f.until === "number" && e.created_at > f.until) return false;
  const t = f["#t"] as string[] | undefined;
  if (t && !(e.tags ?? []).some((tag) => tag[0] === "t" && t.includes(tag[1] ?? ""))) return false;
  const ids = f.ids as string[] | undefined;
  if (ids && !ids.includes(e.id)) return false;
  const refs = f["#e"] as string[] | undefined;
  if (refs && !(e.tags ?? []).some((tag) => tag[0] === "e" && refs.includes(tag[1] ?? ""))) return false;
  return true;
}

class RelaySocket {
  readyState = 1;
  onopen: (() => void) | null = null;
  onmessage: ((msg: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  reqs: Filter[][] = [];
  private queue: string[] = [];
  constructor(private events: RawEvent[]) {}
  send(text: string): void {
    const frame = JSON.parse(text) as unknown[];
    if (frame[0] !== "REQ") return;
    const sub = String(frame[1]), filters = frame.slice(2) as Filter[];
    this.reqs.push(filters);
    const hits = new Map<string, RawEvent>();
    for (const f of filters) for (const e of this.events) if (matches(e, f)) hits.set(e.id, e);
    const limit = Math.min(...filters.map((f) => (typeof f.limit === "number" ? f.limit : Infinity)));
    for (const e of [...hits.values()].sort((a, b) => b.created_at - a.created_at).slice(0, limit)) this.queue.push(JSON.stringify(["EVENT", sub, e]));
    this.queue.push(JSON.stringify(["EOSE", sub]));
  }
  close(): void { this.readyState = 3; }
  /** Deliver the answer to ONE request: its events, then its EOSE. The reader may ask again; that waits. */
  step(): boolean {
    while (this.queue.length) {
      const data = this.queue.shift()!;
      this.onmessage?.({ data });
      if (data.startsWith('["EOSE"')) return true;
    }
    return false;
  }
  drain(): void { while (this.step()) { /* until the reader stops asking */ } }
}

class FakeRelay {
  sockets: RelaySocket[] = [];
  constructor(public events: RawEvent[]) {}
  open = (): WebSocket => {
    const s = new RelaySocket(this.events);
    this.sockets.push(s);
    return s as unknown as WebSocket;
  };
  /** The socket that asked for profiles only, and the reader's own. Both exist once startMarket has returned. */
  get names(): RelaySocket { return this.sockets[0]!; }
  get source(): RelaySocket { return this.sockets[1]!; }
}

const memory = (init: Record<string, string> = {}) => {
  const m = new Map(Object.entries(init));
  return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => { m.set(k, v); }, has: (k: string) => m.has(k) };
};
const tick = (ms = 0) => new Promise((r) => setTimeout(r, ms));
/** What a reader can see of a view: everything but the moment it was computed. */
const seen = (v: PlainView) => JSON.stringify({ ...v, generatedAt: 0 });

async function open(relay: FakeRelay, storage = memory(), markMs = 5, waitMs = 60_000): Promise<{ h: MarketHandle; views: PlainView[]; states: string[] }> {
  const views: PlainView[] = [], states: string[] = [];
  const h = await startMarket({
    feedLimit: 50,
    onView: (v) => views.push(v),
    onStatus: (s) => states.push(s),
    seams: { openSocket: relay.open, storage, spot: false, waitMs, markMs },
  });
  relay.names.onopen?.();
  relay.source.onopen?.();
  return { h, views, states };
}

test("a first visit: nothing until the week is whole, then all of it at once, and it does not change", async () => {
  globalThis.indexedDB = new IDBFactory();
  const relay = new FakeRelay(market());
  const { h, views, states } = await open(relay);
  try {
    relay.names.step();
    assert.equal(views.length, 0, "names alone show nothing");

    // The reader asks for PAGE_SIZE (1000) events at a time, newest first. The week is 1440 events and three heartbeats.
    relay.source.step();
    await tick(120);         // longer than the engine's own 50 ms batching: a recompute per request would have run by now
    assert.equal(h.computes(), 0, "the engine does not compute while history is being read");
    assert.equal(views.length, 0, "half a week must not reach the page: its numbers would count up");
    assert.equal(h.ready(), "none");
    assert.equal(h.setWindow("24h"), false, "nothing is whole yet, not even a day");
    h.setWindow("week");

    relay.source.step();
    assert.equal(views.length, 1, "the page that crosses the week's edge makes the week whole");
    assert.equal(h.computes(), 1);
    const first = views[0]!;
    assert.equal(h.ready(), "recent");
    assert.equal(first.windowKey, "week");
    assert.equal(first.stats.posted, WEEK_JOBS);
    assert.equal(first.stats.delivered, WEEK_JOBS);
    assert.equal(first.stats.buyers, 2);
    assert.deepEqual(first.experts.map((e) => e.name).sort(), ["pixel-frontend", "plain-review", "swift-code"], "every expert by name, in the first view");
    assert.ok(first.experts.every((e) => e.named && e.online), "names and lamps are in the first view");
    assert.ok(first.buyers.every((b) => b.named));
    assert.equal(first.feed.length, 50);

    // What needs the whole history is refused, not half-answered.
    assert.equal(h.setWindow("all"), false, "All time is not whole yet");
    assert.equal(h.expert(SELLERS[0]), null, "a profile counts all of an expert's history: not yet");
    assert.equal(views.length, 1, "asking for All time early shows nothing new");
    assert.equal(h.setWindow("week"), true);
    assert.equal(seen(h.view()!), seen(first), "back on Week: the same view");

    const before = h.computes();
    relay.source.drain();
    await tick(120);
    assert.equal(h.ready(), "all");
    assert.ok(states.includes("live"));
    assert.equal(h.computes(), before + 1, "the rest of history costs ONE more compute, not one per request");
    assert.equal(views.length, 2, "and one more view");
    assert.equal(seen(views[1]!), seen(first), "the week's view is identical once the older history is in");

    assert.equal(h.setWindow("all"), true);
    const all = views.at(-1)!;
    assert.equal(all.windowKey, "all");
    assert.equal(all.stats.delivered, WEEK_JOBS + OLD_JOBS);

    const profile = h.expert(SELLERS[0]);
    assert.ok(profile, "a profile once everything is in");
    assert.equal(profile.name, "swift-code");
    assert.equal(profile.delivered, WEEK_JOBS / 3 + OLD_JOBS / 2, "all of its deliveries, not the week's");
    assert.equal(profile.admitsTargeted, "open");
    assert.equal(profile.free, false);
    assert.equal(Buffer.from(decodeNpub(profile.npub)!).toString("hex"), SELLERS[0], "the npub is this expert's key");
    assert.deepEqual(h.expert(profile.npub), profile, "a profile opens by npub as well as by hex");
    assert.ok(profile.recent.length > 0 && profile.recent.every((f) => (["claim", "result", "feedback"].includes(f.stage) ? f.actor.key : f.other?.key) === SELLERS[0]), "its recent jobs are the ones it did as an expert");
    assert.equal(h.expert(BUYER), null, "a key that only buys has no expert profile");
    // Bob (2026-11-21): buyers have a profile too.
    const buyer = h.buyer(BUYER);
    assert.ok(buyer, "a buyer's profile once everything is in");
    assert.equal(Buffer.from(decodeNpub(buyer.npub)!).toString("hex"), BUYER);
    assert.ok(buyer.posted > 0, "it counts the jobs it posted");
    assert.deepEqual(h.buyer(buyer.npub), buyer, "a buyer's profile opens by npub as well as by hex");
    assert.ok(buyer.recent.length > 0 && buyer.recent.every((f) => (["claim", "result", "feedback"].includes(f.stage) ? f.other?.key : f.actor.key) === BUYER), "its recent jobs are the ones it did as a buyer");
    assert.equal(h.buyer(SELLERS[2]!), null, "a key that only sells has no buyer profile");
    assert.equal(h.expert("npub1nonsense"), null);
  } finally { h.stop(); }
});

test("the task text never reaches the page", async () => {
  globalThis.indexedDB = new IDBFactory();
  const relay = new FakeRelay(market());
  const { h, views } = await open(relay);
  try {
    relay.names.step();
    relay.source.drain();
    h.setWindow("all");
    assert.ok(views.length >= 2);
    for (const v of views) assert.ok(!JSON.stringify(v).includes(SECRET), "a view carries the event, never what the buyer wrote");
    assert.ok(!JSON.stringify(h.expert(SELLERS[1])).includes(SECRET));
  } finally { h.stop(); }
});

test("nothing is shown before the names are in, whichever comes first", async () => {
  globalThis.indexedDB = new IDBFactory();
  const relay = new FakeRelay(market());
  const { h, views } = await open(relay);
  try {
    relay.source.step();
    relay.source.step();
    assert.equal(views.length, 0, "the week is in but the names are not: a key would show where a name exists");
    relay.source.drain();
    assert.equal(views.length, 0, "even with everything else in");
    relay.names.step();
    assert.equal(views.length, 1);
    assert.ok(views[0]!.experts.every((e) => e.named) && views[0]!.buyers.every((b) => b.named));
  } finally { h.stop(); }
});

/** A browser store the way an earlier visit left it: `events` written, and marked complete by the store itself. */
async function leaveStore(events: RawEvent[]): Promise<void> {
  globalThis.indexedDB = new IDBFactory();
  const db = await createEventDb();
  db.save(events);
  await tick(20);
  db.markHistoryComplete();
  await tick(20);
}

test("a store that says complete but lacks a profile and has no mark of ours is read again in full", async () => {
  // bob's Safari, 2026-11-20: the activity list showed 5b298ab6 where every other browser showed maxie-agent. The
  // store had been marked complete before its last events, the profiles, were written.
  const events = market();
  const lost = events.find((e) => e.kind === PROFILE && e.pubkey === SELLERS[0])!;
  await leaveStore(events.filter((e) => e !== lost));
  const relay = new FakeRelay(events);
  const storage = memory();
  const { h, views } = await open(relay, storage);
  try {
    relay.names.step();
    relay.source.drain();
    assert.equal(relay.source.reqs[0]![0]!.since, undefined, "an unmarked store is not trusted: the read starts from the newest event, not from where the store ends");
    assert.ok(views.length >= 1);
    assert.equal(views.at(-1)!.experts.find((e) => e.key === SELLERS[0])?.name, "swift-code");
    await tick(60);
    assert.ok(storage.has("maxplayer-market-store"), "once the read has ended and been written, the store carries our mark");
    const stored = await (await createEventDb()).loadAll();
    assert.ok(stored.some((e) => e.id === lost.id), "and the profile it lacked");
  } finally { h.stop(); }
});

test("a trusted store still gets its names from the relay: one whole view, by name", async () => {
  const events = market();
  const lost = events.find((e) => e.kind === PROFILE && e.pubkey === BUYER)!;
  await leaveStore(events.filter((e) => e !== lost));
  const relay = new FakeRelay(events);
  const { h, views } = await open(relay, memory({ "maxplayer-market-store": "2" }));
  try {
    relay.source.drain();
    assert.equal(typeof relay.source.reqs[0]![0]!.since, "number", "a marked store only catches up");
    assert.equal(views.length, 0, "the catch-up is in, the names are not: nothing yet");
    relay.names.step();
    assert.equal(views.length, 1, "a return visit is ONE view");
    assert.equal(h.ready(), "all");
    assert.equal(views[0]!.buyers.find((b) => b.key === BUYER)?.name, "north-desk", "the name the store had lost");
    assert.equal(views[0]!.stats.delivered, WEEK_JOBS);
  } finally { h.stop(); }
});

test("the store is marked whole only after what was read has been written", async () => {
  globalThis.indexedDB = new IDBFactory();
  const events = market();
  const relay = new FakeRelay(events);
  const storage = memory();
  // markMs is long enough that opening a second handle on the store (slow on a loaded test machine) cannot outlast it:
  // at 80 ms the full suite sometimes saw the mark already written when it looked "at the moment the read ends".
  const { h } = await open(relay, storage, 600);
  try {
    relay.names.step();
    relay.source.drain();
    assert.equal(h.ready(), "all");
    assert.ok(!storage.has("maxplayer-market-store"), "not at the moment the read ends: its last events are still queued");
    assert.equal(await (await createEventDb()).historyComplete(), false);
    for (let waited = 0; !storage.has("maxplayer-market-store") && waited < 3000; waited += 50) await tick(50);
    assert.ok(storage.has("maxplayer-market-store"));
    const db = await createEventDb();
    assert.equal(await db.historyComplete(), true);
    assert.equal((await db.loadAll()).length, events.length, "every event the relay holds is in the store by then");
  } finally { h.stop(); }
});

/* ---------------- review fixes (PR #1361 round 1) ---------------- */

test("a job that began before the week and moved in it: the week is shown with the whole job, and does not change", async () => {
  globalThis.indexedDB = new IDBFactory();
  const events = market();
  // Offered and awarded nine days ago, delivered and paid an hour ago. Its result names no buyer: the other side of
  // "swift-code delivered" comes from the offer, which the week's own pages do not hold.
  const at = NOW - 9 * DAY;
  const offer = ev(OFFER, BUYER2, at, tagged([["i", SECRET], ["amount", "100"], ["param", "deadline", String(NOW + DAY)]]));
  const root = [["e", offer.id, "", "root"]];
  const result = ev(RESULT, SELLERS[0], NOW - 3600, tagged(root));
  events.push(offer, ev(CLAIM, SELLERS[0], at + 1, tagged(root)), ev(AWARD, BUYER2, at + 2, tagged([...root, ["p", SELLERS[0]]])), result);
  const relay = new FakeRelay(events);
  const { h, views } = await open(relay);
  try {
    relay.names.step();
    relay.source.step();
    relay.source.step();
    assert.equal(views.length, 0, "the week's pages are in, the job that began before it is not: nothing yet");
    const jobs = relay.sockets[2]!;
    assert.ok(jobs, "the reader asks for that job on a socket of its own");
    jobs.onopen?.();
    jobs.drain();
    assert.equal(views.length, 1);
    const row = views[0]!.feed.find((f) => f.id === result.id);
    assert.equal(row?.other?.key, BUYER2, "the delivery names its buyer in the first view");
    relay.source.drain();
    await tick(120);
    assert.equal(h.ready(), "all");
    assert.equal(seen(views.at(-1)!), seen(views[0]!), "and the week is identical once all of history is in");
  } finally { h.stop(); }
});

test("a return visit on a slow relay shows its store as stale, not as the read being over", async () => {
  const events = market();
  await leaveStore(events);
  const relay = new FakeRelay(events);
  const { h, views } = await open(relay, memory({ "maxplayer-market-store": "2" }), 5, 40);
  try {
    relay.names.step();
    await tick(80);           // the catch-up has not answered within waitMs
    assert.equal(views.length, 1, "the store is shown rather than nothing");
    assert.equal(h.ready(), "stale", "but it does not say it is whole and current");
    const before = h.computes();
    relay.source.step();
    await tick(120);
    assert.equal(h.computes(), before, "events of the catch-up do not recompute one by one");
    relay.source.drain();
    await tick(20);
    assert.equal(h.ready(), "all", "the catch-up ends it");
  } finally { h.stop(); }
});

test("names: a profile in the boundary second of a full page is not skipped, and the requests do not pile up", async () => {
  globalThis.indexedDB = new IDBFactory();
  const events = market().filter((e) => !(e.kind === PROFILE && e.pubkey === SELLERS[2]));
  // 500 newer profiles of other keys, the last of them in the same second as plain-review's.
  const edge = NOW - 50 * DAY;
  for (let i = 0; i < 499; i++) events.push(ev(PROFILE, hex((1000 + i).toString(16)), NOW - 41 * DAY + i, [], JSON.stringify({ name: `n${i}` })));
  events.push(ev(PROFILE, hex("f0"), edge, [], JSON.stringify({ name: "edge-mate" })));
  events.push(ev(PROFILE, SELLERS[2], edge, [], JSON.stringify({ name: "plain-review" })));
  const relay = new FakeRelay(events);
  const sent: string[] = [];
  const { h, views } = await open(relay);
  const names = relay.names, send = names.send.bind(names);
  names.send = (text: string) => { sent.push(text); send(text); };
  try {
    names.drain();
    relay.source.step();
    relay.source.step();
    assert.equal(views.length, 1);
    assert.equal(views[0]!.experts.find((e) => e.key === SELLERS[2])?.name, "plain-review", "named in the first view");
    assert.ok(sent.some((t) => t.startsWith('["CLOSE","n1"')), "the first page is closed before the next is asked for");
  } finally { h.stop(); }
});
