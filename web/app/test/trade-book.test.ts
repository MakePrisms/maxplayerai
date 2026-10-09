/**
 * The /trades book and relay reader: validated state, quarantine, expiry,
 * relay paging and the read-only contract — driven without a network.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { schnorr } from "@noble/curves/secp256k1";
import { sha256 } from "@noble/hashes/sha256";
import { bytesToHex } from "@noble/hashes/utils";
import { TRADE_LOT, TRADE_STATUS } from "../src/model/kinds.js";
import { completedStats, createBook } from "../src/trade/book.js";
import { lifecycle, parseLot, type NostrEvent } from "../src/trade/validate.js";
import { allowedRelay, createTradeReader, drain, lotFilter, statusFilters, STATUS_CHUNK } from "../src/trade/relays.js";
import { dockSide, mintLabel, rate, timeLeft } from "../src/trade/format.js";

/** The fixture generator's test keys: bytes 0x11… (maker) and 0x22… (other). */
const MAKER = "11".repeat(32);
const OTHER = "22".repeat(32);
const T0 = 1_791_500_000;
const A = "https://mint.minibits.cash/Bitcoin";
const B = "https://testnut.cashu.space";

function sign(secret: string, kind: number, created_at: number, tags: string[][], content: string): NostrEvent {
  const pubkey = bytesToHex(schnorr.getPublicKey(secret));
  const id = bytesToHex(sha256(new TextEncoder().encode(JSON.stringify([0, pubkey, created_at, kind, tags, content]))));
  return { id, pubkey, created_at, kind, tags, content, sig: bytesToHex(schnorr.sign(id, secret)) };
}

function lot(secret = MAKER, created = T0, give = 64, want = 48, mints: [string, string] = [A, B]): NostrEvent {
  const pubkey = bytesToHex(schnorr.getPublicKey(secret));
  const content = JSON.stringify({
    trade_v: 1,
    give: { mint_url: mints[0], unit: "sat", net: give },
    want: { mint_url: mints[1], unit: "sat", net: want },
    maker_trade_pubkey: pubkey,
    expires_at: created + 86400,
    deadline_policy: { long_seconds: 3600, short_seconds: 900, min_gap_seconds: 2700 },
    fee_policy: "sender-funds-net-v1",
  });
  return sign(secret, TRADE_LOT, created, [
    ["t", "maxplayer"], ["v", "1"], ["g", mints[0]], ["w", mints[1]], ["u", "sat"], ["x", "sat"], ["expiration", String(created + 86400)],
  ], content);
}

function status(l: NostrEvent, seq: number, prev: string, s: string, secret = MAKER, at = l.created_at + seq): NostrEvent {
  return sign(secret, TRADE_STATUS, at, [["t", "maxplayer"], ["v", "1"], ["e", l.id]],
    JSON.stringify({ trade_v: 1, lot_id: l.id, seq, prev, status: s }));
}

const NOW = T0 + 600;

test("the TS signer produces listings the validator accepts (sanity for the cases below)", () => {
  const l = lot();
  assert.equal(parseLot(l, NOW).ok, true);
  assert.equal(lifecycle(l, [status(l, 1, l.id, "available")]).ok, true);
});

test("lifecycle: 256 statuses pass, 257 exceed the bound", () => {
  const l = lot();
  const chain: NostrEvent[] = [];
  let prev = l.id;
  for (let seq = 1; seq <= 257; seq++) {
    const e = status(l, seq, prev, "available");
    chain.push(e);
    prev = e.id;
  }
  const ok = lifecycle(l, chain.slice(0, 256));
  assert.ok(ok.ok && ok.status === "available");
  assert.equal(lifecycle(l, chain).ok, false);
});

test("book: open, sold, cancelled and expired land in the right lists", () => {
  const book = createBook();
  const open = lot(MAKER, T0, 64, 48);
  const sold = lot(MAKER, T0 + 1, 100, 90);
  const cancelled = lot(MAKER, T0 + 2, 10, 20);
  const old = lot(MAKER, T0 - 86400 + 100, 5, 5);
  for (const l of [open, sold, cancelled, old]) book.ingest(l);
  const s1 = (l: NostrEvent) => status(l, 1, l.id, "available");
  const ss = s1(sold), sc = s1(cancelled);
  for (const e of [s1(open), ss, status(sold, 2, ss.id, "sold"), sc, status(cancelled, 2, sc.id, "cancelled"), s1(old)]) book.ingest(e);
  const v = book.view(NOW);
  assert.deepEqual(v.open.map((r) => r.id), [open.id]);
  assert.deepEqual(new Map(v.closed.map((r) => [r.id, r.state])), new Map([[sold.id, "sold"], [cancelled.id, "cancelled"], [old.id, "expired"]]));
  assert.equal(v.quarantined.length, 0);
  assert.equal(v.stats.open, 1);
  assert.equal(v.stats.openGiveSats, 64);
  assert.equal(v.stats.sold24h, 1);
  assert.equal(v.stats.soldSats24h, 100);
  assert.equal(v.stats.mints, 2);
  assert.equal(v.open[0]!.price, 48 / 64);
  assert.equal(v.open[0]!.chain.length, 1);
});

test("book: a forked or gapped chain is quarantined, never shown as open or sold", () => {
  const book = createBook();
  const fork = lot(MAKER, T0, 1, 2);
  const gap = lot(MAKER, T0 + 1, 3, 4);
  book.ingest(fork); book.ingest(gap);
  const f1 = status(fork, 1, fork.id, "available");
  book.ingest(f1);
  book.ingest(status(fork, 2, f1.id, "sold"));
  book.ingest(status(fork, 2, f1.id, "cancelled", MAKER, T0 + 50));
  const g1 = status(gap, 1, gap.id, "available");
  book.ingest(g1);
  book.ingest(status(gap, 3, g1.id, "sold"));
  const v = book.view(NOW);
  assert.equal(v.open.length + v.closed.length, 0);
  assert.deepEqual(new Map(v.quarantined.map((r) => [r.id, r.detail])), new Map([
    [fork.id, "forked status history"],
    [gap.id, "incomplete status history"],
  ]));
});

test("book: a stranger's status cannot quarantine or close someone else's listing", () => {
  const book = createBook();
  const l = lot();
  const s1 = status(l, 1, l.id, "available");
  book.ingest(l); book.ingest(s1);
  book.ingest(status(l, 2, s1.id, "sold", OTHER));
  book.ingest(status(l, 2, s1.id, "cancelled", OTHER));
  const v = book.view(NOW);
  assert.deepEqual(v.open.map((r) => r.id), [l.id]);
  assert.equal(v.quarantined.length, 0);
});

test("book: invalid listings are dropped and counted; duplicates cost nothing", () => {
  const book = createBook();
  const bad = { ...lot(), content: lot().content + " x" };
  assert.equal(book.ingest(bad), true);
  assert.equal(book.ingest(bad), false);
  assert.equal(book.ingest({ kind: TRADE_LOT }), false);
  assert.equal(book.ingest(null), false);
  const v = book.view(NOW);
  assert.equal(v.rejected, 1);
  assert.equal(v.open.length + v.closed.length + v.quarantined.length, 0);
});

test("book: a status that arrives before its listing is kept and joined", () => {
  const book = createBook();
  const l = lot();
  book.ingest(status(l, 1, l.id, "available"));
  book.ingest(l);
  assert.deepEqual(book.view(NOW).open.map((r) => r.id), [l.id]);
});

test("book: no status is pending while loading, quarantined once every answering relay has read it", () => {
  const book = createBook();
  const l = lot();
  book.ingest(l);
  assert.equal(book.view(NOW).pending, 1);
  book.markHistoryRead([l.id], "wss://a");
  assert.equal(book.view(NOW, ["wss://a", "wss://b"]).pending, 1, "relay b has not answered for it yet");
  book.markHistoryRead([l.id], "wss://b");
  const v = book.view(NOW, ["wss://a", "wss://b"]);
  assert.equal(v.pending, 0);
  assert.equal(v.quarantined[0]?.detail, "initial available revision missing");
});

test("book: open lots sort cheapest first; listings from the future wait", () => {
  const book = createBook();
  const dear = lot(MAKER, T0, 10, 20);
  const cheap = lot(OTHER, T0, 10, 5);
  // parse_lot tolerates 60s of clock skew; beyond that a listing waits.
  const future = lot(MAKER, NOW + 90, 10, 1);
  for (const [l, key] of [[dear, MAKER], [cheap, OTHER], [future, MAKER]] as const) { book.ingest(l); book.ingest(status(l, 1, l.id, "available", key)); }
  assert.deepEqual(book.view(NOW).open.map((r) => r.id), [cheap.id, dear.id]);
  assert.deepEqual(book.view(NOW + 30).open.map((r) => r.id), [future.id, cheap.id, dear.id]);
  assert.equal(book.view(NOW).stats.makers, 2);
});

test("book: nostr:// mints are shown alongside https mints", () => {
  const book = createBook();
  const nostrMint = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
  const l = lot(MAKER, T0, 21, 20, [nostrMint, B]);
  book.ingest(l); book.ingest(status(l, 1, l.id, "available"));
  assert.equal(book.view(NOW).open[0]?.lot.give.mint_url, nostrMint);
});

/* ---------------- relay reader ---------------- */

test("filters: listings by tag and window, statuses by lot AND maker, never negotiation", () => {
  assert.deepEqual(lotFilter(100), { kinds: [TRADE_LOT], "#t": ["maxplayer"], since: 100, limit: 500 });
  const lots = Array.from({ length: STATUS_CHUNK + 1 }, (_, i) => ({ id: String(i).padStart(64, "0"), maker: i % 2 ? "b" : "a" }));
  const fs = statusFilters(lots);
  assert.equal(fs.length, 2);
  assert.deepEqual(fs[0]!.kinds, [TRADE_STATUS]);
  assert.equal((fs[0]!["#e"] as string[]).length, STATUS_CHUNK);
  assert.deepEqual(fs[0]!.authors, ["a", "b"]);
  assert.deepEqual(fs[1]!.authors, ["a"]);
});

test("relay URLs: wss only, and never the production relay", () => {
  for (const ok of ["wss://nos.lol", "wss://relay.primal.net", "ws://127.0.0.1:7777"]) assert.equal(allowedRelay(ok), true, ok);
  for (const no of ["wss://relay.maxplayer.ai", "wss://RELAY.maxplayer.ai/", "ws://nos.lol", "https://nos.lol", "nope"]) assert.equal(allowedRelay(no), false, no);
});

test("drain pages below a relay's own cap until a page brings nothing new", async () => {
  const events = Array.from({ length: 7 }, (_, i) => ({ id: `e${i}`, created_at: 100 - Math.floor(i / 2) }) as NostrEvent);
  const asks: Record<string, unknown>[] = [];
  // The relay caps every page at 3, newest first, `until` inclusive.
  const req = async (f: Record<string, unknown>) => {
    asks.push(f);
    const until = (f.until as number | undefined) ?? Infinity;
    return events.filter((e) => e.created_at <= until).slice(0, 3);
  };
  const got: string[] = [];
  const { complete } = await drain(req, { kinds: [TRADE_LOT] }, (e) => got.push(e.id));
  assert.equal(complete, true);
  assert.deepEqual(got.sort(), events.map((e) => e.id).sort());
  assert.equal(asks[0]!.until, undefined);
});

class FakeSocket {
  readyState = 0;
  sent: unknown[][] = [];
  onopen: (() => void) | null = null;
  onmessage: ((m: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  constructor(public url: string, private store: NostrEvent[]) {
    queueMicrotask(() => { this.readyState = 1; this.onopen?.(); });
  }
  send(text: string) {
    const f = JSON.parse(text) as unknown[];
    this.sent.push(f);
    if (f[0] !== "REQ") return;
    const sub = f[1];
    const filter = f[2] as { kinds: number[]; "#e"?: string[]; authors?: string[]; until?: number };
    const hits = this.store.filter((e) => filter.kinds.includes(e.kind)
      && (!filter["#e"] || e.tags.some((t) => t[0] === "e" && filter["#e"]!.includes(t[1]!)))
      && (!filter.authors || filter.authors.includes(e.pubkey))
      && (filter.until == null || e.created_at <= filter.until));
    queueMicrotask(() => {
      for (const e of hits) this.onmessage?.({ data: JSON.stringify(["EVENT", sub, e]) });
      this.onmessage?.({ data: JSON.stringify(["EOSE", sub]) });
    });
  }
  close() { this.readyState = 3; }
}

test("reader: unions relays into a validated book and only ever sends REQ and CLOSE", async () => {
  const l1 = lot(MAKER, T0, 64, 48);
  const l2 = lot(OTHER, T0 + 5, 10, 12);
  const s1 = status(l1, 1, l1.id, "available");
  const s2 = status(l2, 1, l2.id, "available", OTHER);
  const sold = status(l2, 2, s2.id, "sold", OTHER);
  // Relay a has l1 and only the first half of l2's chain; relay b has the rest.
  const stores: Record<string, NostrEvent[]> = { "wss://a.example": [l1, s1, l2, s2], "wss://b.example": [l2, sold] };
  const sockets: FakeSocket[] = [];
  const book = createBook();
  const rounds = new Set<string>();
  let resolveDone!: () => void;
  const done = new Promise<void>((r) => { resolveDone = r; });
  const reader = createTradeReader(
    {
      relays: Object.keys(stores),
      openSocket: (u) => { const s = new FakeSocket(u, stores[u]!); sockets.push(s); return s as unknown as WebSocket; },
      now: () => NOW,
      setTimer: () => 0,
      clearTimer: () => {},
      knownLots: () => book.known(),
    },
    {
      onEvent: (e) => { book.ingest(e); },
      onStatusesRead: (ids, relay) => book.markHistoryRead(ids, relay),
      onRelayState: () => {},
      onRound: (r) => { rounds.add(r); if (rounds.size === 2) resolveDone(); },
    },
  );
  reader.start();
  await done;
  reader.stop();
  const v = book.view(NOW, Object.keys(stores));
  assert.deepEqual(v.open.map((r) => r.id), [l1.id]);
  assert.deepEqual(v.closed.map((r) => [r.id, r.state]), [[l2.id, "sold"]]);
  const types = new Set(sockets.flatMap((s) => s.sent.map((f) => f[0])));
  assert.deepEqual([...types].sort(), ["CLOSE", "REQ"]);
  const kinds = new Set(sockets.flatMap((s) => s.sent.filter((f) => f[0] === "REQ").flatMap((f) => (f[2] as { kinds: number[] }).kinds)));
  assert.deepEqual([...kinds].sort(), [TRADE_LOT, TRADE_STATUS].sort());
});

test("reader: a refused relay URL fails without a socket", () => {
  const states: string[] = [];
  let opened = 0;
  const reader = createTradeReader(
    { relays: ["wss://relay.maxplayer.ai"], openSocket: () => { opened++; return {} as WebSocket; }, knownLots: () => [] },
    { onEvent() {}, onStatusesRead() {}, onRelayState: (_u, s) => states.push(s), onRound() {} },
  );
  reader.start();
  assert.equal(opened, 0);
  assert.deepEqual(states, ["failed"]);
});

test("format: mints, prices and time left", () => {
  assert.equal(mintLabel("https://mint.minibits.cash/Bitcoin"), "mint.minibits.cash/Bitcoin");
  assert.equal(mintLabel("http://127.0.0.1:3338"), "http://127.0.0.1:3338");
  assert.equal(mintLabel("nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d"), "nostr:npub10xlx…ge6d");
  assert.equal(rate(48 / 64), "0.75");
  assert.equal(rate(1 / 3), "0.3333");
  assert.equal(rate(1e6), "1000000");
  assert.equal(rate(1e-6), "0.000001");
  assert.equal(rate(0), "—");
  assert.equal(timeLeft(0), "—");
  assert.equal(timeLeft(45), "45s");
  assert.equal(timeLeft(12 * 60 + 5), "12m");
  assert.equal(timeLeft(23 * 3600 + 5 * 60), "23h 05m");
});

test("completed trades: sold lots only, windowed by the sold status's time", () => {
  const book = createBook();
  const sell = (l: NostrEvent, soldAt: number, key = MAKER) => {
    const s1 = status(l, 1, l.id, "available", key, l.created_at + 1);
    book.ingest(l); book.ingest(s1); book.ingest(status(l, 2, s1.id, "sold", key, soldAt));
  };
  const t = T0 + 10 * 86400;
  const recent = lot(MAKER, t - 3600, 100, 90);
  const lastWeek = lot(OTHER, t - 3 * 86400, 50, 60, [B, A]);
  const old = lot(MAKER, t - 9 * 86400, 10, 9);
  sell(recent, t - 3600 + 120);
  sell(lastWeek, t - 3 * 86400 + 600, OTHER);
  sell(old, t - 9 * 86400 + 60);
  const c1 = lot(MAKER, t - 7200, 5, 5);
  const c1s = status(c1, 1, c1.id, "available");
  book.ingest(c1); book.ingest(c1s); book.ingest(status(c1, 2, c1s.id, "cancelled", MAKER, t - 7000));
  const v = book.view(t);
  const day = completedStats(v, t, 86400);
  assert.deepEqual(day.rows.map((r) => r.id), [recent.id]);
  assert.equal(day.giveSats, 100);
  assert.equal(day.wantSats, 90);
  assert.equal(day.medianFill, 120 - 1 + 1);
  assert.equal(day.cancelled, 1);
  const week = completedStats(v, t, 7 * 86400);
  assert.deepEqual(week.rows.map((r) => r.id), [recent.id, lastWeek.id]);
  assert.equal(week.sellers, 2);
  assert.equal(week.pairs, 2);
  assert.equal(week.medianFill, (120 + 600) / 2);
  const all = completedStats(v, t, null);
  assert.equal(all.trades, 3);
  assert.equal(all.giveSats, 160);
  assert.equal(all.wantSats, 159);
  assert.equal(completedStats(book.view(t), t, 60).medianFill, null);
});

test("completed trades never count a quarantined or stranger-closed lot", () => {
  const book = createBook();
  const l = lot();
  const s1 = status(l, 1, l.id, "available");
  book.ingest(l); book.ingest(s1);
  book.ingest(status(l, 2, s1.id, "sold", OTHER));
  const fork = lot(MAKER, T0 + 1, 7, 8);
  const f1 = status(fork, 1, fork.id, "available");
  book.ingest(fork); book.ingest(f1);
  book.ingest(status(fork, 2, f1.id, "sold"));
  book.ingest(status(fork, 2, f1.id, "cancelled", MAKER, T0 + 99));
  assert.equal(completedStats(book.view(NOW), NOW, null).trades, 0);
});

test("the lot popup docks to the clicked side, never the middle", () => {
  assert.equal(dockSide("lots", 1300, 1440), "left");
  assert.equal(dockSide("recent", 10, 1440), "right");
  assert.equal(dockSide("done", 200, 1440), "left");
  assert.equal(dockSide("done", 720, 1440), "right");
  assert.equal(dockSide("done", null, 1440), "left");
});
