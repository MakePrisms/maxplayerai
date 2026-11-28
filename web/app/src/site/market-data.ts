/**
 * Market data for the Market page (/market, src/site/market.js).
 *
 * This is NOT a second implementation of the market. It drives the same reader the old board used (the relay
 * source, the event store, the engine and the spot quote) and hands the page plain data: numbers and strings, no
 * HTML. The page draws them its own way.
 *
 * What this file decides, and ui/board.ts does not:
 *   - the activity lines carry the event only (who posted, claimed, hired, delivered, paid, and the price). The
 *     buyer's task text and follow-up messages are NOT passed on (bob, 2026-11-20: the first thing a visitor sees
 *     must not be someone's raw task text);
 *   - sellers are called experts on the page; the words are chosen in market.js, not here;
 *   - an expert's profile (`expert()`) is counted over ALL the events the reader holds, not the board's period, and
 *     carries the npub (the old sheet showed the hex key). It passes on what bob chose as the key attributes: the
 *     description, online or working, the rate, jobs delivered, who the seat admits, and the agent and model;
 *   - WHEN the engine computes and WHEN the page is told (see startMarket).
 */
import { RELAY_URL, TRANSPORT } from "../config.js";
import { createEngine, type MarketView } from "../market/engine.js";
import { openFollowUp } from "../market/trades.js";
import { DEFAULT_WINDOW, WINDOWS, participantDetail } from "../market/participants.js";
import { parseEvent, type ParsedEvent, type RawEvent } from "../model/events.js";
import { createRelaySource } from "../source/relay.js";
import { recoveryFloor } from "../source/recovery.js";
import { createEventDb, type EventDb } from "../store/db.js";
import { now, short } from "../ui/format.js";
import { onSpotChange, spotUsd, startSpot, usd } from "../ui/spot.js";
import { MAXPLAYER_TAGGED_KINDS, OFFER as OFFER_KIND, PROFILE, UNTAGGED_KINDS } from "../model/kinds.js";
import { decodeNpub, npubEncode } from "../trade/nip19.js";

const RACER_ACTIVE_SECONDS = 86400;      // as board.ts: a buyer is "active" for a day after its last action

export interface Who { key: string; npub: string; name: string; named: boolean }
export interface ExpertRow extends Who {
  rank: number; online: boolean; working: number;
  delivered: number; ask: string | null; earned: string; agents: string[];
}
export interface BuyerRow extends Who {
  rank: number; active: boolean; working: number;
  posted: number; paid: number; spent: string;
}
export interface FeedRow {
  id: string; stage: string; at: number;
  actor: Who; other: Who | null; unknownOther: boolean;
  /** offer: sent to one named expert. reply: the buyer closed the session. feedback: it ended the work. */
  direct: boolean; closed: boolean; terminal: boolean;
  free: boolean; priv: boolean; self: boolean;
  turn: number | null; price: string | null;
  /** The job (root offer id) this event belongs to; null when it names none. Opens the job's panel. */
  job: string | null;
}
export interface PlainView {
  windowKey: string;
  windows: { key: string; label: string }[];
  stats: { expertsOnline: number; expertsSeen: number; delivered: number; posted: number; receipts: number; volume: string; buyers: number };
  experts: ExpertRow[];
  buyers: BuyerRow[];
  feed: FeedRow[];
  feedTotal: number;
  generatedAt: number;
}
export interface ExpertProfile extends Who {
  about: string | null;
  online: boolean; working: number;
  /** The last time this expert did anything on the market (0 = never seen). */
  lastSeen: number;
  /** The advertised minimum for a job, as the board shows it; null when the seat states none. */
  rate: string | null; free: boolean;
  /** Jobs delivered over everything the reader holds (all time). */
  delivered: number;
  /** `admits_targeted` of the newest heartbeat: open, named (a list of buyers exists) or closed. null = not stated. */
  admitsTargeted: string | null;
  /** `admits_pool` of the newest heartbeat: open or closed. null = not stated. */
  admitsPool: string | null;
  /** Harness families, or the advertised agents where the seat states no family. */
  agents: string[];
  models: { family: string; model: string }[];
  recent: FeedRow[];
  /** Its jobs in progress now (root offer ids), newest first: the profile links to each. */
  live: string[];
}
/** A buyer's profile (Bob, 2026-11-21: buyers are clickable like experts). Counted over all history, like an expert's. */
export interface BuyerProfile extends Who {
  about: string | null;
  /** Active: did anything on the market in the last 24 hours. working: its jobs in progress now. */
  active: boolean; working: number;
  lastSeen: number;
  /** Jobs posted, jobs with a published receipt, and what those receipts add up to (all time). */
  posted: number; paid: number; spent: string;
  recent: FeedRow[];
  /** Its jobs in progress now (root offer ids), newest first. */
  live: string[];
}
/**
 * One job, for its panel (Bob, 2026-11-28: an activity line opens the job, with a summary and all its activity).
 * Like the activity lines, it carries the facts only: who, the price, the status and the events. The buyer's task
 * text and messages are NOT passed on (bob, 2026-11-20).
 */
export interface JobDetail {
  id: string;
  buyer: Who | null;
  /** The expert the buyer hired (or, with no buyer-signed record, the one that delivered or claimed first). */
  expert: Who | null;
  /** The buyer's own records name more than one expert: who did the work cannot be told. */
  expertUnknown: boolean;
  /**
   * open: posted, nobody has claimed it. claimed: claimed, nobody hired yet. working: hired, not delivered.
   * round: a session job sent back for another round. delivered: delivered, not accepted or paid (as far as the
   * market can see). accepted, paid: done. stopped: the expert ended its work. closed: the buyer closed the session.
   * ended: no delivery and it is over (its deadline passed, or a day went by with nothing).
   */
  status: "open" | "claimed" | "working" | "round" | "delivered" | "accepted" | "paid" | "stopped" | "closed" | "ended";
  /** In progress now, by the same rule as the lamps (engine activeTradeJobs). */
  live: boolean;
  postedAt: number;
  deadline: number | null;
  /** The newest event of the job. */
  lastAt: number;
  price: string | null; paid: string | null;
  free: boolean; priv: boolean;
  /** Sent to one named expert rather than posted to all. */
  direct: boolean;
  rounds: number;
  claimants: number;
  /** Every event of the job, oldest first. */
  events: FeedRow[];
}
export interface MarketHandle {
  /**
   * Ask for a period. true: the view for it is `view()` now (the page was told if it changed). false: that period
   * needs data that is not in yet (All time needs all of history); the page will be told when it is, and should show
   * placeholders until then.
   */
  setWindow(key: string): boolean;
  /** The last view the page was given. Its `windowKey` says which period it is for. */
  view(): PlainView | null;
  /**
   * One expert by npub or 64-hex key. null: all of history is not in yet (`ready()` is not "all"), a bad key, or a key
   * that has never sold on this market.
   */
  expert(id: string, recent?: number): ExpertProfile | null;
  /** One buyer by npub or 64-hex key; null as for `expert()`, or a key that has never posted a job here. */
  buyer(id: string, recent?: number): BuyerProfile | null;
  /**
   * One job by its offer id (64 hex). null: the reader does not hold it yet (an older job before the whole history
   * is in; ready() says when that is), or no such job.
   */
  job(id: string): JobDetail | null;
  /**
   * "none": nothing whole yet. "recent": 24 hours and Week are whole. "all": everything is.
   * "stale": the relay is slow or cannot be reached, and a return visit shows what the browser holds (whole, but as
   * of its last visit); the catch-up still replaces it when it arrives.
   * "partial": the read is over but stopped at the source's page backstop, so older history is missing.
   */
  ready(): "none" | "recent" | "stale" | "partial" | "all";
  /**
   * How many times the engine has computed the market, shown or not. A first visit costs two (the week, then
   * everything), whatever the number of requests the history takes; it used to cost one per request.
   */
  computes(): number;
  /** Stop reading and drop every timer. The page never calls it (it lives as long as the tab); tests do. */
  stop(): void;
}
export interface StartOptions {
  onView(view: PlainView): void;
  /** connecting, syncing, live, reconnecting, failed. "live" comes when all of history is in. */
  onStatus?(state: string): void;
  feedLimit?: number;
  /** Seams for tests (test/market-page.test.ts). The page passes none. */
  seams?: {
    openSocket?: (url: string) => WebSocket;
    db?: EventDb;
    /** Where our own "this store is whole" mark is kept. Default: localStorage. null: nowhere. */
    storage?: Pick<Storage, "getItem" | "setItem"> | null;
    /** false: do not ask for the dollar rate. */
    spot?: boolean;
    /** How long the first view waits for the dollar rate and the names, and a return visit for its catch-up. Default 2500 ms. */
    waitMs?: number;
    /** How long after the read ends the store is marked whole. Default 1750 ms. */
    markMs?: number;
  };
}

const npubs = new Map<string, string>();
function npub(pubkey: string): string {
  let v = npubs.get(pubkey);
  if (v == null) { try { v = npubEncode(pubkey); } catch { v = ""; } npubs.set(pubkey, v); }
  return v;
}
/** npub or 64-hex -> hex; null for anything else. */
function toHex(id: string): string | null {
  const s = String(id || "").trim().toLowerCase();
  if (/^[0-9a-f]{64}$/.test(s)) return s;
  if (!s.startsWith("npub1")) return null;
  const bytes = decodeNpub(s);
  return bytes ? Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("") : null;
}

function who(view: MarketView, pubkey: string): Who {
  const name = view.names.get(pubkey) || null;
  return { key: pubkey, npub: npub(pubkey), name: name ?? short(pubkey), named: name != null };
}

/** The other side of an event, as board.ts resolves it. */
function other(view: MarketView, e: ParsedEvent, want: "buyer" | "seller"): { who: Who | null; unknown: boolean } {
  const t = e.offerId ? view.trades.get(e.offerId) : null;
  if (want === "buyer") {
    const pk = e.buyer || t?.buyer;
    return { who: pk && pk !== e.pubkey ? who(view, pk) : null, unknown: false };
  }
  const pk = e.awardedSeller || e.targetSeller || t?.seller;
  if (pk && pk !== e.pubkey) return { who: who(view, pk), unknown: false };
  return { who: null, unknown: !!t?.sellerConflict };
}

function turnOf(view: MarketView, e: ParsedEvent): number | null {
  const t = e.offerId ? view.trades.get(e.offerId) : null;
  if (!t || t.turns < 2) return null;
  return t.turnOf.get(e.id) ?? null;
}

function feedRow(view: MarketView, e: ParsedEvent): FeedRow {
  const stage = e.stage ?? "";
  const side = stage === "claim" || stage === "result" ? "buyer" : "seller";
  const o = stage === "feedback" ? { who: null, unknown: false }
    : stage === "offer" ? { who: e.targetSeller && e.targetSeller !== e.pubkey ? who(view, e.targetSeller) : null, unknown: false }
    : other(view, e, side);
  return {
    id: e.id, stage, at: e.created_at,
    actor: who(view, e.pubkey), other: o.who, unknownOther: o.unknown,
    direct: stage === "offer" && !!e.targetSeller,
    closed: stage === "reply" && e.status === "close",
    terminal: stage === "feedback" && !!e.terminal,
    free: stage === "offer" && !!e.free,
    priv: stage === "offer" && e.executionVisibility === "private",
    self: stage === "offer" && !!e.selfTrade,
    turn: stage === "reply" ? (e.turn ?? null) : turnOf(view, e),
    price: (stage === "offer" && !e.free && e.amount != null) || (stage === "receipt" && e.amount != null) ? usd(e.amount) : null,
    job: e.offerId ?? null,
  };
}

function plain(view: MarketView, feedLimit: number): PlainView {
  const t = now();
  const m = view.metrics;
  return {
    windowKey: view.windowKey,
    windows: WINDOWS.map((w) => ({ key: w.key, label: w.label })),
    stats: {
      expertsOnline: view.sellers.filter((r) => r.online).length,
      expertsSeen: view.sellers.length,
      delivered: m.funnel.delivered,
      posted: m.funnel.posted,
      receipts: m.receiptsOnRecord,
      volume: usd(m.satsInReceipts),
      buyers: m.buyers,
    },
    experts: view.sellers.map((r, i) => ({
      ...who(view, r.pubkey), rank: i + 1,
      online: r.online, working: (view.activeBySeller.get(r.pubkey) || []).length,
      delivered: r.delivered, ask: r.askSats == null ? null : usd(r.askSats), earned: usd(r.satsEarned),
      agents: r.harnessFamilies ?? [],
    })),
    buyers: view.buyers.map((r, i) => {
      const lastAt = view.racerLastSeen.get(r.pubkey) || 0;
      return {
        ...who(view, r.pubkey), rank: i + 1,
        active: lastAt > 0 && t - lastAt <= RACER_ACTIVE_SECONDS, working: (view.activeByBuyer.get(r.pubkey) || []).length,
        posted: r.posted, paid: r.receipted, spent: usd(r.satsPaid),
      };
    }),
    feed: view.feed.slice(0, feedLimit).map((e) => feedRow(view, e)),
    feedTotal: view.feed.length,
    generatedAt: view.generatedAt,
  };
}

const TRADE_STAGE = new Set(["offer", "claim", "award", "result", "accept", "receipt", "feedback", "reply"]);
const SELLER_ACTS = new Set(["claim", "result", "feedback"]);
function expertProfile(view: MarketView, id: string, recent: number): ExpertProfile | null {
  const pubkey = toHex(id);
  if (!pubkey) return null;
  const d = participantDetail(view.allEvents, pubkey, now(), view.allEvents);
  const s = d.seller;
  if (!s) return null;
  const tag = (name: string) => s.advertisementTags?.find((t) => t.name === name)?.values[0] ?? null;
  return {
    ...who(view, pubkey),
    about: view.profiles.get(pubkey)?.about ?? s.about ?? null,
    online: s.online, working: (view.activeBySeller.get(pubkey) || []).length, lastSeen: s.lastSeen || 0,
    rate: s.askSats == null ? null : usd(s.askSats), free: s.askSats === 0,
    delivered: s.delivered,
    admitsTargeted: tag("admits_targeted"), admitsPool: tag("admits_pool"),
    agents: s.harnessFamilies?.length ? s.harnessFamilies : (s.advertisedAgents ?? []),
    models: s.harnessModels ?? [],
    // Its work as an expert only. The same key may also buy (grokker hires chef-flay): those jobs are not this
    // profile's. The expert acts in claim, result and feedback; in every other stage it is the other side.
    recent: d.activity.filter((e) => e.stage != null && TRADE_STAGE.has(e.stage)).slice(0, 400)
      .map((e) => feedRow(view, e))
      .filter((f) => (SELLER_ACTS.has(f.stage) ? f.actor.key === pubkey : f.other?.key === pubkey))
      .slice(0, recent),
    live: liveJobs(view.activeBySeller.get(pubkey)),
  };
}

function buyerProfile(view: MarketView, id: string, recent: number): BuyerProfile | null {
  const pubkey = toHex(id);
  if (!pubkey) return null;
  const d = participantDetail(view.allEvents, pubkey, now(), view.allEvents);
  const b = d.buyer;
  if (!b) return null;
  const lastAt = Math.max(view.racerLastSeen.get(pubkey) || 0, b.lastSeen || 0);
  return {
    ...who(view, pubkey),
    about: view.profiles.get(pubkey)?.about ?? null,
    active: lastAt > 0 && now() - lastAt <= RACER_ACTIVE_SECONDS, working: (view.activeByBuyer.get(pubkey) || []).length,
    lastSeen: lastAt,
    posted: b.posted, paid: b.receipted, spent: usd(b.satsPaid),
    // Its work as a buyer only: it acts in every stage but claim, result and feedback, where it is the other side.
    recent: d.activity.filter((e) => e.stage != null && TRADE_STAGE.has(e.stage)).slice(0, 400)
      .map((e) => feedRow(view, e))
      .filter((f) => (SELLER_ACTS.has(f.stage) ? f.other?.key : f.actor.key) === pubkey)
      .slice(0, recent),
    live: liveJobs(view.activeByBuyer.get(pubkey)),
  };
}

const liveJobs = (jobs: { offerId: string; startedAt: number }[] | undefined): string[] =>
  [...(jobs || [])].sort((a, b) => b.startedAt - a.startedAt).map((j) => j.offerId);

/** Same-second order inside one job: the lifecycle's own (an offer before its claim). */
const JOB_STAGE_RANK: Record<string, number> = { offer: 0, claim: 1, award: 2, result: 3, feedback: 4, accept: 5, receipt: 6, reply: 7 };
let jobMemo: { view: MarketView; id: string; job: JobDetail | null } | null = null;
function jobDetail(view: MarketView, id: string): JobDetail | null {
  const key = String(id || "").trim().toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(key)) return null;
  if (jobMemo && jobMemo.view === view && jobMemo.id === key) return jobMemo.job;     // drawn again on every redraw
  const t = view.trades.get(key);
  let job: JobDetail | null = null;
  const events = t ? view.allEvents.map(parseEvent)
    .filter((e): e is ParsedEvent => e != null && e.stage != null && TRADE_STAGE.has(e.stage) && (e.offerId === key || e.id === key))
    .sort((a, b) => a.created_at - b.created_at || (JOB_STAGE_RANK[a.stage!] ?? 50) - (JOB_STAGE_RANK[b.stage!] ?? 50)) : [];
  const offer = events.find((e) => e.stage === "offer" && e.id === key);
  if (t && offer) {
    const live = !!t.buyer && (view.activeByBuyer.get(t.buyer) || []).some((j) => j.offerId === key);
    const closed = t.lastFollowUp?.status === "close";
    const status: JobDetail["status"] = live
      ? (openFollowUp(t) ? "round" : t.at.award != null ? "working" : t.claimants.length ? "claimed" : "open")
      : t.at.receipt != null ? "paid" : t.at.accept != null ? "accepted" : closed ? "closed"
      : t.declineReason ? "stopped" : t.at.result != null ? "delivered" : "ended";
    job = {
      id: key,
      buyer: t.buyer ? who(view, t.buyer) : null,
      expert: t.seller ? who(view, t.seller) : null,
      expertUnknown: t.sellerConflict,
      status, live,
      postedAt: offer.created_at,
      deadline: offer.deadline ?? null,
      lastAt: events[events.length - 1]!.created_at,
      price: !t.free && t.offerAmount != null ? usd(t.offerAmount) : null,
      paid: t.receiptAmount != null ? usd(t.receiptAmount) : null,
      free: t.free, priv: offer.executionVisibility === "private",
      direct: !!offer.targetSeller,
      rounds: t.turns,
      claimants: t.claimants.length,
      events: events.map((e) => feedRow(view, e)),
    };
  }
  jobMemo = { view, id: key, job };
  return job;
}

/**
 * Names for the first screen: every profile (kind 0) the relay holds, on a socket of our own.
 *
 * The production source reads profiles LAST, after the whole of the market's history (about six seconds on a first
 * visit), and a board without them would show keys and then change them to names. This asks for them at once: one
 * small answer (232 events, 0.3 MB on 2026-11-20). The source still reads them again at the end; the cache drops
 * what it holds. Paged like the source pages, one cursor, in case the relay ever holds more than one answer's worth.
 */
const NAMES_PAGE = 500, NAMES_MAX_PAGES = 20;
function fetchNames(openSocket: (url: string) => WebSocket, waitMs: number, onEvent: (e: RawEvent) => void, done: (ok: boolean) => void): () => void {
  let ws: WebSocket, finished = false, oldest: number | null = null, n = 0, fresh = 0, page = 0;
  const seen = new Set<string>();
  let timer: ReturnType<typeof setTimeout> | null = null;
  // End the request, then close with the normal code: a bare close() made Safari's engine log "the network
  // connection was lost" in the console on every first visit.
  const close = () => {
    try {
      ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
      if (ws.readyState === 1 && page > 0) ws.send(JSON.stringify(["CLOSE", `n${page}`]));
      ws.close(1000);
    } catch { /* already gone */ }
  };
  const finish = (ok: boolean) => {
    if (finished) return;
    finished = true;
    if (timer) clearTimeout(timer);
    close();
    done(ok);
  };
  const ask = () => {
    if (page > 0) ws.send(JSON.stringify(["CLOSE", `n${page}`]));     // one subscription at a time
    n = 0; fresh = 0; page += 1;
    const filter: Record<string, unknown> = { kinds: [...UNTAGGED_KINDS], limit: NAMES_PAGE };
    // `until` is inclusive and is kept at the oldest second seen: `oldest - 1` skipped every profile stamped in the
    // boundary second that did not fit the page before. What is asked for twice is dropped by id.
    if (oldest != null) filter.until = oldest;
    ws.send(JSON.stringify(["REQ", `n${page}`, filter]));
  };
  try { ws = openSocket(RELAY_URL); } catch { done(false); return () => {}; }
  ws.onopen = ask;
  ws.onmessage = (msg: MessageEvent) => {
    let frame: unknown;
    try { frame = JSON.parse(msg.data as string); } catch { return; }
    if (!Array.isArray(frame) || frame[1] !== `n${page}`) return;
    if (frame[0] === "EVENT" && frame[2]) {
      const e = frame[2] as RawEvent;
      n += 1;
      if (oldest == null || e.created_at < oldest) oldest = e.created_at;
      if (seen.has(e.id)) return;
      seen.add(e.id);
      fresh += 1;
      onEvent(e);
    } else if (frame[0] === "EOSE") {
      if (n < NAMES_PAGE) finish(true);              // the relay has nothing older: every profile is in
      else if (fresh === 0) finish(false);           // a whole page in one second: paging cannot get past it
      else if (page >= NAMES_MAX_PAGES) finish(false); // stopped at the cap: NOT every profile, so not "in full"
      else ask();
    } else if (frame[0] === "CLOSED") finish(false);
  };
  ws.onerror = ws.onclose = () => finish(false);
  timer = setTimeout(() => finish(false), waitMs);
  return () => { if (!finished) { finished = true; if (timer) clearTimeout(timer); close(); } };
}

/**
 * Every event of the given jobs, on a socket of our own: the jobs that moved this week but began before it.
 *
 * The week's numbers come from the week's events, but a few things on it are read from the job as a whole: who the
 * other side of a delivery was, which round of a session it is, and whether someone is working on it. A job offered
 * eight days ago and delivered today had none of those until the full history was in, so the week changed when it
 * arrived ("X delivered" became "X delivered to Y"). Measured 2026-11-21: 1 such job among the 273 the week touches.
 * One request per 25 jobs (the job's offer by id, and every market event that names it).
 */
function fetchJobs(openSocket: (url: string) => WebSocket, ids: string[], waitMs: number, onEvent: (e: RawEvent) => void, done: (ok: boolean) => void): () => void {
  const chunks: string[][] = [];
  for (let i = 0; i < ids.length; i += 25) chunks.push(ids.slice(i, i + 25));
  let ws: WebSocket, finished = false, page = 0;
  let timer: ReturnType<typeof setTimeout> | null = null;
  const close = () => {
    try {
      ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
      if (ws.readyState === 1 && page > 0) ws.send(JSON.stringify(["CLOSE", `j${page}`]));
      ws.close(1000);
    } catch { /* already gone */ }
  };
  const finish = (ok: boolean) => {
    if (finished) return;
    finished = true;
    if (timer) clearTimeout(timer);
    close();
    done(ok);
  };
  const ask = () => {
    if (page >= chunks.length) return finish(true);
    if (page > 0) ws.send(JSON.stringify(["CLOSE", `j${page}`]));
    const chunk = chunks[page]!;
    page += 1;
    ws.send(JSON.stringify(["REQ", `j${page}`, { ids: chunk }, { kinds: [...MAXPLAYER_TAGGED_KINDS], "#e": chunk }]));
  };
  try { ws = openSocket(RELAY_URL); } catch { done(false); return () => {}; }
  ws.onopen = ask;
  ws.onmessage = (msg: MessageEvent) => {
    let frame: unknown;
    try { frame = JSON.parse(msg.data as string); } catch { return; }
    if (!Array.isArray(frame) || frame[1] !== `j${page}`) return;
    if (frame[0] === "EVENT" && frame[2]) onEvent(frame[2] as RawEvent);
    else if (frame[0] === "EOSE") ask();
    else if (frame[0] === "CLOSED") finish(false);
  };
  ws.onerror = ws.onclose = () => finish(false);
  timer = setTimeout(() => finish(false), waitMs);
  return () => { if (!finished) { finished = true; if (timer) clearTimeout(timer); close(); } };
}

/** Our mark on a browser store this code has written and found complete (see startMarket). */
const STORE_MARK = "maxplayer-market-store", STORE_MARK_VALUE = "2";

/** The longest period the board offers that has an end: a week. "All time" has none. */
const RECENT_SECONDS = Math.max(...WINDOWS.map((w) => w.seconds ?? 0));
const isOpenEnded = (key: string) => (WINDOWS.find((w) => w.key === key)?.seconds ?? null) == null;

/**
 * Start reading the market.
 *
 * The reader is the one main.ts drives, unchanged: the same source, engine, store and spot quote. What differs is
 * WHEN the engine works and WHEN the page is told, because a first visit used to take 6 to 10 seconds and the
 * board counted up while it did (Bob, 2026-11-20: load in under 3 s, placeholders until it is complete, no jumps).
 *
 * Measured on 2026-11-20 against the production relay: it holds 18,143 market events, 23 MB,
 * and gives at most 1,000 per request; the source asked for 500 at a time, newest first, 37 requests one after the
 * other: 5.9 s of network alone. (Since 2026-11-21 it asks for 1,000, the relay's cap: about 20 requests.) And the engine recomputed the whole market after every request (50 ms after the
 * first event of each): 47 recomputes, about 7 s of the main thread in WebKit.
 *
 * So:
 *  1. No recompute while history is read. Events go into the engine's cache (`engine.cache.ingest`, which stores
 *     and does not schedule one) and the engine computes once when there is something complete to show.
 *  2. The first screen needs a week, not the whole past. The source reads newest first, so the moment it hands
 *     over an event older than the board's longest period (a week), every event of the week is in: the first four
 *     requests, about 1.2 s. With the names (fetchNames) and the dollar rate, that is a COMPLETE week or 24 hours:
 *     the board's numbers for a period come from that period's events only, so they do not change when the older
 *     history arrives. The page gets its first view then, and not before.
 *  3. The rest of history is read behind it, still without recomputing, and the engine computes once at the end.
 *     "All time" and an expert's profile need all of it: until then `setWindow("all")` answers false and `expert()`
 *     null, and the page shows placeholders for those.
 *  4. A return visit has the past in the browser's store and only catches up: it is shown when the catch-up is in
 *     (or after 2.5 s, or at once if the relay cannot be reached), so it does not show old numbers and then new.
 *  5. Names are never taken on trust from the store: every visit asks the relay for all profiles and waits for them
 *     (up to 2.5 s) before anything is shown.
 */
export async function startMarket(opts: StartOptions): Promise<MarketHandle> {
  const feedLimit = opts.feedLimit ?? 60;
  const seams = opts.seams ?? {};
  const waitMs = seams.waitMs ?? 2500, markMs = seams.markMs ?? 1750;
  const openSocket = seams.openSocket ?? ((url: string) => new WebSocket(url));
  let storage: Pick<Storage, "getItem" | "setItem"> | null = null;
  if (seams.storage !== undefined) storage = seams.storage;
  else { try { storage = localStorage; } catch { /* no storage: every visit is a first visit */ } }
  const t0 = Date.now();
  if (seams.spot !== false) startSpot();
  const engine = createEngine({ windowKey: DEFAULT_WINDOW, shifts: false });
  const db = seams.db ?? await createEventDb();
  const timers: ReturnType<typeof setTimeout>[] = [];
  const later = (fn: () => void, ms: number) => { timers.push(setTimeout(fn, ms)); };
  const every = (fn: () => void, ms: number) => { timers.push(setInterval(fn, ms)); };

  let full = false;            // the read is over: the views are whole for every period, and live events recompute
  let partial = false;         // ... but it stopped at the source's page backstop, so older history is missing
  let stale = false;           // the relay is slow or unreachable: a return visit shows its trusted store meanwhile
  let recent = false;          // every event of the last week is in, with names and the rate: 24 hours and Week are whole
  let pastWeek = false;        // a whole answer of the source has reached back past the week
  let sawOld = false;          // ... an event older than the week has arrived; it counts once its answer is whole
  let jobs = false;            // the jobs the week touches but which began before it are in (see fetchJobs)
  let stopJobs: () => void = () => {};
  let names = false;           // the names request has answered in full: every profile the relay holds is in
  let namesDone = false;       // ... or it has failed or run out of time, and is not waited for any longer
  let wanted = DEFAULT_WINDOW;  // the period the page asked for last
  let engineWindow = DEFAULT_WINDOW;
  let last: PlainView | null = null;
  let raw: MarketView | null = null;

  const persistQueue: RawEvent[] = [];
  const ingest = (event: RawEvent, persist: boolean) => {
    // Until everything is in, store without scheduling a recompute (see 1 above).
    const result = full ? engine.ingest(event) : engine.cache.ingest(event);
    if (result.stored && persist) persistQueue.push(event);
    if (result.evictedId) db.evict([result.evictedId]);
    if (persistQueue.length >= 1000) db.save(persistQueue.splice(0));
  };
  every(() => { if (persistQueue.length) db.save(persistQueue.splice(0)); }, 2000);

  let computes = 0;
  engine.subscribe((view) => {
    computes += 1;
    raw = view;
    // Nothing reaches the page that is not whole for its period.
    if (!((full || stale) ? namesDone : recent && !isOpenEnded(view.windowKey))) return;
    last = plain(view, feedLimit);
    opts.onView(last);
  });

  const cached = await db.loadAll();
  for (const event of cached) ingest(event, false);
  // The browser's store is trusted only if THIS code marked it. main.ts marks a store complete before its last
  // events are written, and the last events of a read are the profiles: a reader who reloaded at the wrong moment
  // kept a store that said "complete" without an agent's name, for good (bob's Safari showed 5b298ab6 for
  // maxie-agent). The store is shared with the old board (same database), so a store without our mark is read
  // again in full, which also puts back whatever it lacks.
  let trusted = false;
  try { trusted = storage?.getItem(STORE_MARK) === STORE_MARK_VALUE; } catch { /* storage refused: a first visit */ }
  const storeComplete = cached.length > 0 && trusted && await db.historyComplete();
  const newestCached = engine.cache.newest;

  /** Compute once and tell the page: the period it asked for if that is whole now, else the one the engine is on. */
  const show = () => {
    const ok = full || stale || !isOpenEnded(wanted);
    if (ok && wanted !== engineWindow) { engineWindow = wanted; engine.setWindow(wanted); } else engine.flush();
  };
  /** The week is whole: show it. */
  const tryRecent = () => {
    if (full || recent || !pastWeek || !jobs || !names) return;
    if (seams.spot !== false && spotUsd() == null && Date.now() - t0 < waitMs) return;   // the dollar rate: wait for it, but not for long
    recent = true;
    show();
  };
  /** Everything is whole. */
  const becomeFull = () => {
    if (full) return;
    full = true;
    show();
  };
  onSpotChange(() => { if (full || recent || stale) engine.flush(); else tryRecent(); });
  later(tryRecent, waitMs);
  // online lamps go stale honestly between events; not while the tab is hidden, and at once when it is back (#9)
  const doc = typeof document === "undefined" ? null : document;     // tests run without a page
  const lampsDue = () => { if ((full || recent || stale) && !doc?.hidden) engine.flush(); };
  every(lampsDue, 60000);
  const onVisible = () => { if (!doc?.hidden) lampsDue(); };
  doc?.addEventListener("visibilitychange", onVisible);

  // The names are asked for on EVERY visit and nothing is shown before they are in (bob: always load the name). The
  // store is not relied on for them: it is one small request (232 profiles, 0.3 MB), and a name missing from the
  // store would otherwise show as a key.
  const stopNames = fetchNames(openSocket, waitMs, (event) => ingest(event, true), (ok) => {
    names = ok; namesDone = true;
    if (full || stale) show(); else tryRecent();
  });
  // The relay cannot be reached, or is slow: a return visit shows what the browser holds rather than nothing. That
  // is NOT the read being over: it is reported as "stale", events keep going into the cache without a recompute, and
  // the catch-up replaces it in one view when it ends.
  const showStore = () => { if (storeComplete && !full && !stale) { stale = true; show(); } };
  if (storeComplete) later(showStore, waitMs);

  const source = createRelaySource(
    {
      url: RELAY_URL,
      transport: TRANSPORT,
      sinceHint: newestCached,
      storeComplete,
      recoveryFloor: recoveryFloor(engine.cache.all(), now()),
      openSocket,
    },
    {
      onEvent: (event) => {
        ingest(event, true);
        // Newest first: an event from before the week means the week is all here. (Profiles are not market events
        // and come in their own, later, request.)
        if (!sawOld && event.kind !== PROFILE && event.created_at < now() - RECENT_SECONDS - 300) sawOld = true;
      },
      // Newest first ACROSS requests, not within one: the week is all here once a WHOLE answer has gone past it.
      onPage: () => {
        if (pastWeek || !sawOld) return;
        pastWeek = true;
        const floor = now() - RECENT_SECONDS - 300;
        const held = new Map<string, number>();
        const wanted = new Set<string>();
        for (const e of engine.cache.all()) if (e.kind === OFFER_KIND) held.set(e.id, e.created_at);
        for (const e of engine.cache.all()) {
          if (e.created_at < floor || e.kind === OFFER_KIND) continue;
          const offerId = parseEvent(e)?.offerId;
          if (!offerId) continue;
          const at = held.get(offerId);
          if (at == null || at < floor) wanted.add(offerId);       // began before the week: read it whole
        }
        const done = () => { jobs = true; tryRecent(); };
        if (!wanted.size) return done();
        stopJobs = fetchJobs(openSocket, [...wanted], waitMs, (event) => ingest(event, true), done);
      },
      onStatus: ({ state }) => {
        opts.onStatus?.(state === "history" ? "syncing" : state);
        if (state === "failed" || state === "reconnecting") showStore();
      },
      onSynced: ({ complete }) => {
        // The store may call itself complete only once what was read is IN it. The mark is its own small write and
        // the events are queued ones: marked at once (as main.ts does), a reader who left in the next second or two
        // kept a store that said "complete" without its last events, and every later visit trusted it. (Found
        // 2026-11-20: a return visit counted 2,680 jobs delivered, the first visit 2,761.) So: write what is
        // queued, and mark a moment later. Leaving before that costs one more full read, never a hole.
        // (The write waits a quarter of a second so that it does not land in the same moment as the one recompute.)
        // The marker is written in the same transaction as the last queued events, and our own mark only once that
        // transaction has committed with no earlier write of this visit having failed (a store with a hole must not
        // say it is whole: quota, a private window).
        if (complete) {
          later(() => {
            if (persistQueue.length) db.save(persistQueue.splice(0));
            void db.markHistoryComplete().then((ok) => {
              if (ok) { try { storage?.setItem(STORE_MARK, STORE_MARK_VALUE); } catch { /* storage refused */ } }
            });
          }, markMs);
        } else {
          later(() => { if (persistQueue.length) db.save(persistQueue.splice(0)); }, Math.min(250, markMs));
        }
        partial = !complete;
        stale = false;
        becomeFull();
      },
    },
  );
  source.start();

  return {
    setWindow: (key) => {
      wanted = key;
      if (!(full || stale || (recent && !isOpenEnded(key)))) return false;   // not whole yet: the page is told when it is
      if (key !== engineWindow) { engineWindow = key; engine.setWindow(key); }   // computes and tells the page at once
      return true;
    },
    view: () => last,
    expert: (id, n = 6) => ((full || stale) && namesDone && raw ? expertProfile(raw, id, n) : null),
    buyer: (id, n = 6) => ((full || stale) && namesDone && raw ? buyerProfile(raw, id, n) : null),
    // A job of the week is whole as soon as the week is (fetchJobs read the older ones it touches); an older one
    // answers once everything is in.
    job: (id) => ((full || stale || recent) && namesDone && raw ? jobDetail(raw, id) : null),
    ready: () => (full && namesDone ? (partial ? "partial" : "all") : stale && namesDone ? "stale" : recent ? "recent" : "none"),
    computes: () => computes,
    stop: () => {
      for (const t of timers) clearTimeout(t);      // clears intervals too
      doc?.removeEventListener("visibilitychange", onVisible);
      stopNames();
      stopJobs();
      source.stop();
    },
  };
}
