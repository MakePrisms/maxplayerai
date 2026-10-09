/**
 * The trade book: raw listing and status events in, a validated view out. Pure; no DOM.
 *
 * A lot reaches the page only through validate.ts:
 *   - a listing that fails parse_lot's structural rules is DROPPED (counted);
 *   - a listing whose status chain fails lifecycle() is QUARANTINED: counted
 *     and flagged, never shown as open, sold or cancelled;
 *   - a listing with no status yet is PENDING until every relay still
 *     answering has delivered its full history for it — "no available
 *     revision" is a quarantine only once the read is complete, never while
 *     loading;
 *   - otherwise the chain's status, and "expired" once now >= expires_at.
 *
 * Status events are kept only when they are signed by the lot's maker and
 * e-tag it, as the CLI's discover() queries them; anything else is ignored
 * rather than allowed to quarantine somebody else's listing.
 */
import { TRADE_LOT, TRADE_STATUS } from "../model/kinds.js";
import { MAX_STATUS_EVENTS, checkLot, lifecycle, type Lot, type NostrEvent, type Status } from "./validate.js";

export type LotState = Status | "expired" | "quarantined" | "pending";

export interface LotRow {
  id: string;
  lot: Lot;
  state: LotState;
  /** Why it is quarantined, or the chain's last status before it expired. */
  detail?: string;
  /** created_at of the newest status in the chain, or of the listing itself. */
  updated_at: number;
  /** Price of one sat of the give mint in sats of the want mint. */
  price: number;
  /** Status event ids in chain order (valid chains only). */
  chain: string[];
}

export interface BookView {
  open: LotRow[];
  /** Sold, cancelled and expired, newest first. */
  closed: LotRow[];
  quarantined: LotRow[];
  pending: number;
  /** Signed listings that fail the CLI's listing rules. */
  rejected: number;
  stats: {
    open: number;
    openGiveSats: number;
    sold24h: number;
    soldSats24h: number;
    makers: number;
    mints: number;
  };
}

/** Per-lot cap on held status events: one more than lifecycle()'s bound, so an overflow still trips it. */
const STATUS_CAP = MAX_STATUS_EVENTS + 1;

export function createBook() {
  /** Valid listings by id (structure checked once; expiry is a function of time). */
  const lots = new Map<string, { event: NostrEvent; lot: Lot }>();
  /** Status events by lot id, then event id. Unverified until lifecycle() runs. */
  const statuses = new Map<string, Map<string, NostrEvent>>();
  /** Statuses that arrived before their listing. */
  const orphans = new Map<string, Map<string, NostrEvent>>();
  /** Event ids already judged as listings (good or bad), so repeats cost nothing. */
  const judged = new Set<string>();
  let rejected = 0;
  /** Relays that delivered each lot's full status history at least once. */
  const historyFrom = new Map<string, Set<string>>();

  function addStatus(map: Map<string, Map<string, NostrEvent>>, lotId: string, e: NostrEvent): boolean {
    let m = map.get(lotId);
    if (!m) map.set(lotId, (m = new Map()));
    if (m.has(e.id) || m.size >= STATUS_CAP) return false;
    m.set(e.id, e);
    return true;
  }

  /** Returns true when the event changed the book. */
  function ingest(raw: unknown): boolean {
    const e = raw as NostrEvent;
    if (!e || typeof e !== "object" || typeof e.id !== "string") return false;
    if (e.kind === TRADE_LOT) {
      if (judged.has(e.id)) return false;
      judged.add(e.id);
      const r = checkLot(e);
      if (!r.ok) { rejected++; return true; }
      lots.set(e.id, { event: e, lot: r.lot });
      const waiting = orphans.get(e.id);
      if (waiting) {
        orphans.delete(e.id);
        for (const s of waiting.values()) if (s.pubkey === e.pubkey) addStatus(statuses, e.id, s);
      }
      return true;
    }
    if (e.kind === TRADE_STATUS && Array.isArray(e.tags)) {
      // Route by the e tag(s); lifecycle() decides whether the binding is valid.
      const targets = e.tags.filter((t) => Array.isArray(t) && t[0] === "e" && typeof t[1] === "string").map((t) => t[1] as string);
      let changed = false;
      for (const lotId of new Set(targets)) {
        const owner = lots.get(lotId);
        if (owner) {
          if (e.pubkey !== owner.event.pubkey) continue;
          changed = addStatus(statuses, lotId, e) || changed;
        } else if (orphans.size < 2048) {
          addStatus(orphans, lotId, e);
        }
      }
      return changed;
    }
    return false;
  }

  function markHistoryRead(lotIds: string[], relay: string): void {
    for (const id of lotIds) {
      let s = historyFrom.get(id);
      if (!s) historyFrom.set(id, (s = new Set()));
      s.add(relay);
    }
  }

  function known(): { id: string; maker: string }[] {
    return [...lots.values()].map(({ event }) => ({ id: event.id, maker: event.pubkey }));
  }

  /** Relays that have delivered this lot's full status history. */
  function readBy(id: string): ReadonlySet<string> {
    return historyFrom.get(id) ?? new Set();
  }

  /**
   * `answering`: relays currently reachable. A lot without any status is
   * judged only once each of them has read its history in full (or, with
   * none listed, once any relay has).
   */
  function view(now: number, answering: readonly string[] = []): BookView {
    const open: LotRow[] = [];
    const closed: LotRow[] = [];
    const quarantined: LotRow[] = [];
    let pending = 0;
    for (const [id, { event, lot }] of lots) {
      const chain = [...(statuses.get(id)?.values() ?? [])];
      const price = lot.want.net / lot.give.net;
      const base = { id, lot, price, updated_at: event.created_at, chain: [] as string[] };
      // The page shows only listings the CLI would still treat as current or
      // recent; a listing stamped in the future is not shown yet.
      if (event.created_at > now + 60) continue;
      if (chain.length === 0) {
        const reads = readBy(id);
        if (reads.size > 0 && answering.every((r) => reads.has(r))) quarantined.push({ ...base, state: "quarantined", detail: "initial available revision missing" });
        else pending++;
        continue;
      }
      const v = lifecycle(event, chain);
      if (!v.ok) {
        quarantined.push({ ...base, state: "quarantined", detail: v.error });
        continue;
      }
      const last = v.events[v.events.length - 1] as NostrEvent;
      const updated_at = Math.max(event.created_at, last.created_at);
      const chainIds = v.events.map((e) => e.id);
      if (v.status === "available") {
        if (now >= lot.expires_at) closed.push({ ...base, chain: chainIds, state: "expired", updated_at: lot.expires_at });
        else open.push({ ...base, chain: chainIds, state: "available", updated_at });
      } else {
        closed.push({ ...base, chain: chainIds, state: v.status, updated_at });
      }
    }
    // Cheapest first: lowest want-per-give is the best deal for a taker.
    open.sort((a, b) => a.price - b.price || a.lot.expires_at - b.lot.expires_at || a.id.localeCompare(b.id));
    closed.sort((a, b) => b.updated_at - a.updated_at || a.id.localeCompare(b.id));
    quarantined.sort((a, b) => b.updated_at - a.updated_at);
    const day = now - 86400;
    const sold = closed.filter((r) => r.state === "sold" && r.updated_at >= day);
    const mints = new Set<string>();
    for (const r of open) { mints.add(r.lot.give.mint_url); mints.add(r.lot.want.mint_url); }
    return {
      open,
      closed,
      quarantined,
      pending,
      rejected,
      stats: {
        open: open.length,
        openGiveSats: open.reduce((s, r) => s + r.lot.give.net, 0),
        sold24h: sold.length,
        soldSats24h: sold.reduce((s, r) => s + r.lot.give.net, 0),
        makers: new Set(open.map((r) => r.lot.maker)).size,
        mints: mints.size,
      },
    };
  }

  return { ingest, markHistoryRead, readBy, known, view, get size() { return lots.size; } };
}

export type Book = ReturnType<typeof createBook>;
