/**
 * Read-only reader for the token-trade market: listings (TRADE_LOT) and their
 * hash-chained statuses (TRADE_STATUS), from the public relays the trade CLI publishes to.
 *
 * Mirrors `Market::discover` in crates/maxplayer-trade/src/market.rs:
 *   - every configured relay is queried and the results are UNIONED by id;
 *   - listings: `{kinds:[TRADE_LOT], #t:[maxplayer], since}`;
 *   - statuses: `{kinds:[TRADE_STATUS], #e:[lot], authors:[maker]}` — by lot AND
 *     author, so a stranger's status can never quarantine somebody's listing;
 *   - a relay that never sends EOSE is an error, not an empty market.
 *
 * Never signs, never holds a key, never sends EVENT or AUTH, and never asks
 * for trade negotiation (private, NIP-44 encrypted). The production relay is
 * refused outright, as the CLI refuses it.
 *
 * The socket and clock are injectable so the whole loop runs in tests.
 */
import { MAXPLAYER_TAG, TRADE_LOT, TRADE_STATUS } from "../model/kinds.js";
import type { NostrEvent } from "./validate.js";

/** market.rs DEFAULT_RELAYS. */
export const DEFAULT_TRADE_RELAYS: readonly string[] = Object.freeze([
  "wss://nos.lol",
  "wss://relay.primal.net",
  "wss://offchain.pub",
]);

/** Listings live 24h; reading 48h back also shows the recently expired, sold and cancelled. */
export const LOOKBACK_SECONDS = 2 * 86400;
/** Each refresh re-reads this far below the last one, for late or slow-clocked events. */
export const OVERLAP_SECONDS = 120;
export const REFRESH_MS = 30_000;
/** A REQ with no EOSE in this long is a failure (market.rs uses 10s). */
export const QUERY_TIMEOUT_MS = 12_000;
/** Lots per status REQ; each filter pairs these ids with their makers. */
export const STATUS_CHUNK = 40;
export const PAGE_LIMIT = 500;
/** Backstop on paging one filter. Not an expected limit. */
export const MAX_PAGES = 20;
const BACKOFF_MS = [2000, 5000, 15000, 30000];

type Filter = Record<string, unknown>;

export type RelayState = "connecting" | "syncing" | "live" | "failed";

export interface TradeReaderCallbacks {
  /** One raw event, untrusted and possibly a duplicate. */
  onEvent(event: NostrEvent, relay: string): void;
  /** These lots have had their full status history read from `relay`. */
  onStatusesRead(lotIds: string[], relay: string): void;
  onRelayState(relay: string, state: RelayState, detail?: string): void;
  /** One relay finished a full refresh; render now. */
  onRound(relay: string): void;
}

export interface TradeReaderOptions {
  relays?: readonly string[];
  openSocket?: (url: string) => WebSocket;
  now?: () => number;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (id: unknown) => void;
  /** The valid lots known so far, whose status chains must be read. */
  knownLots: () => { id: string; maker: string }[];
}

export function allowedRelay(url: string): boolean {
  try {
    const u = new URL(url);
    if (u.hostname === "relay.maxplayer.ai") return false;
    return u.protocol === "wss:" || (u.protocol === "ws:" && (u.hostname === "127.0.0.1" || u.hostname === "localhost"));
  } catch {
    return false;
  }
}

export function lotFilter(since: number, until?: number): Filter {
  const f: Filter = { kinds: [TRADE_LOT], "#t": [MAXPLAYER_TAG], since, limit: PAGE_LIMIT };
  if (until != null) f.until = until;
  return f;
}

export function statusFilters(lots: { id: string; maker: string }[], since?: number): Filter[] {
  const out: Filter[] = [];
  for (let i = 0; i < lots.length; i += STATUS_CHUNK) {
    const chunk = lots.slice(i, i + STATUS_CHUNK);
    const f: Filter = {
      kinds: [TRADE_STATUS],
      "#e": chunk.map((l) => l.id),
      authors: [...new Set(chunk.map((l) => l.maker))],
      limit: PAGE_LIMIT,
    };
    if (since != null) f.since = since;
    out.push(f);
  }
  return out;
}

class Closed extends Error {}

/** One relay connection: request(filter) resolves with the events up to EOSE. */
class Conn {
  private ws: WebSocket;
  private pending = new Map<string, { events: NostrEvent[]; done: (e: NostrEvent[]) => void; fail: (err: Error) => void; timer: unknown }>();
  private counter = 0;
  readonly opened: Promise<void>;
  dead = false;

  constructor(
    readonly url: string,
    open: (url: string) => WebSocket,
    private setTimer: (fn: () => void, ms: number) => unknown,
    private clearTimer: (id: unknown) => void,
    private onDead: (why: string) => void,
  ) {
    this.ws = open(url);
    this.opened = new Promise((resolve, reject) => {
      const t = setTimer(() => reject(new Closed("connect timeout")), QUERY_TIMEOUT_MS);
      this.ws.onopen = () => { clearTimer(t); resolve(); };
      this.ws.onerror = () => { clearTimer(t); reject(new Closed("connection error")); this.kill("connection error"); };
    });
    this.opened.catch(() => {});
    this.ws.onclose = () => this.kill("connection closed");
    this.ws.onmessage = (msg: MessageEvent) => this.frame(msg.data);
  }

  private frame(data: unknown): void {
    let f: unknown;
    try { f = JSON.parse(String(data)); } catch { return; }
    if (!Array.isArray(f)) return;
    const p = this.pending.get(String(f[1]));
    if (!p) return; // AUTH, NOTICE, OK and strays: we answer none of them.
    if (f[0] === "EVENT") {
      if (p.events.length < 4096 && f[2] && typeof f[2] === "object") p.events.push(f[2] as NostrEvent);
    } else if (f[0] === "EOSE") {
      this.finish(String(f[1]));
      this.send(["CLOSE", f[1]]);
      p.done(p.events);
    } else if (f[0] === "CLOSED") {
      this.finish(String(f[1]));
      p.fail(new Error(`relay closed the read: ${String(f[2] ?? "")}`));
    }
  }

  private finish(sub: string): void {
    const p = this.pending.get(sub);
    if (p) this.clearTimer(p.timer);
    this.pending.delete(sub);
  }

  private send(frame: unknown[]): void {
    if (this.ws.readyState === 1) this.ws.send(JSON.stringify(frame));
  }

  request(filter: Filter): Promise<NostrEvent[]> {
    if (this.dead) return Promise.reject(new Closed("connection closed"));
    const sub = `t${++this.counter}`;
    return new Promise((done, fail) => {
      const timer = this.setTimer(() => {
        this.finish(sub);
        this.send(["CLOSE", sub]);
        fail(new Error("no EOSE (timeout)"));
      }, QUERY_TIMEOUT_MS);
      this.pending.set(sub, { events: [], done, fail, timer });
      this.send(["REQ", sub, filter]);
    });
  }

  kill(why: string): void {
    if (this.dead) return;
    this.dead = true;
    for (const [sub, p] of this.pending) { this.finish(sub); p.fail(new Closed(why)); }
    this.ws.onopen = this.ws.onmessage = this.ws.onerror = this.ws.onclose = null;
    try { this.ws.close(); } catch { /* already gone */ }
    this.onDead(why);
  }
}

/**
 * Page one filter to exhaustion. A relay caps each REQ at its own limit, which
 * may be below ours, so a short page proves nothing; paging stops only when a
 * page brings no event we had not already seen. `until` stays inclusive of the
 * oldest second (events sharing it would otherwise be skipped); ids dedupe.
 */
export async function drain(request: (f: Filter) => Promise<NostrEvent[]>, filter: Filter, onEvent: (e: NostrEvent) => void): Promise<{ complete: boolean }> {
  const seen = new Set<string>();
  let until: number | undefined;
  for (let page = 0; page < MAX_PAGES; page++) {
    const f = until == null ? filter : { ...filter, until };
    const events = await request(f);
    let fresh = 0;
    let oldest: number | undefined;
    for (const e of events) {
      if (typeof e?.id !== "string" || seen.has(e.id)) continue;
      seen.add(e.id);
      fresh++;
      onEvent(e);
      if (Number.isSafeInteger(e.created_at) && (oldest == null || e.created_at < oldest)) oldest = e.created_at;
    }
    if (fresh === 0 || oldest == null) return { complete: true };
    until = oldest;
  }
  return { complete: false };
}

export interface TradeReader { start(): void; stop(): void }

export function createTradeReader(opts: TradeReaderOptions, cb: TradeReaderCallbacks): TradeReader {
  const {
    relays = DEFAULT_TRADE_RELAYS,
    openSocket = (u) => new WebSocket(u),
    now = () => Math.floor(Date.now() / 1000),
    setTimer = (fn, ms) => setTimeout(fn, ms),
    clearTimer = (id) => clearTimeout(id as ReturnType<typeof setTimeout>),
  } = opts;
  let stopped = false;
  const loops: (() => void)[] = [];

  function runRelay(url: string): void {
    if (!allowedRelay(url)) {
      cb.onRelayState(url, "failed", "refused relay URL");
      return;
    }
    let conn: Conn | null = null;
    let timer: unknown = null;
    let attempt = 0;
    /** Lots whose full status history this relay has delivered. */
    const fullyRead = new Set<string>();
    let lastRound: number | null = null;

    const schedule = (ms: number) => { if (!stopped) timer = setTimer(() => void round(), ms); };
    loops.push(() => { if (timer != null) clearTimer(timer); conn?.kill("stopped"); });

    async function round(): Promise<void> {
      if (stopped) return;
      const started = now();
      try {
        if (!conn || conn.dead) {
          cb.onRelayState(url, "connecting");
          conn = new Conn(url, openSocket, setTimer, clearTimer, () => {});
          await conn.opened;
        }
        if (lastRound == null) cb.onRelayState(url, "syncing");
        const c = conn;
        const req = (f: Filter) => c.request(f);
        const emit = (e: NostrEvent) => cb.onEvent(e, url);
        const floor = started - LOOKBACK_SECONDS;
        const lotSince = lastRound == null ? floor : Math.max(floor, lastRound - OVERLAP_SECONDS);
        await drain(req, lotFilter(lotSince), emit);

        const known = opts.knownLots();
        const fresh = known.filter((l) => !fullyRead.has(l.id));
        for (const f of statusFilters(fresh)) {
          const { complete } = await drain(req, f, emit);
          if (complete) {
            const ids = (f["#e"] as string[]);
            for (const id of ids) fullyRead.add(id);
            cb.onStatusesRead(ids, url);
          }
        }
        // Lots already read in full only need what arrived since. Terminal
        // chains cannot change validly, but a later fork must still surface.
        const old = known.filter((l) => fullyRead.has(l.id) && !fresh.includes(l));
        if (lastRound != null && old.length) {
          for (const f of statusFilters(old, lastRound - OVERLAP_SECONDS)) await drain(req, f, emit);
        }
        lastRound = started;
        attempt = 0;
        cb.onRelayState(url, "live");
        cb.onRound(url);
        schedule(REFRESH_MS);
      } catch (err) {
        conn?.kill("failed");
        conn = null;
        cb.onRelayState(url, "failed", err instanceof Error ? err.message : String(err));
        cb.onRound(url);
        schedule(BACKOFF_MS[Math.min(attempt++, BACKOFF_MS.length - 1)] as number);
      }
    }
    void round();
  }

  return {
    start() { for (const url of relays) runRelay(url); },
    stop() { stopped = true; for (const stop of loops) stop(); },
  };
}
