/**
 * The market engine — events in, one coherent view out.
 *
 * The old site re-derived the whole market from scratch every 3 seconds
 * whether anything happened or not. This engine recomputes ONLY when events
 * actually arrive (or the reader changes the time window), coalescing bursts —
 * a history walk lands as a handful of recomputes, a single live event as one.
 * Nothing here touches the DOM or the network; it is a pure state machine,
 * which is what makes it reusable by a future mobile client.
 */
import { createCache, type EventCache } from "../store/cache.js";
import { parseEvent, type ParsedEvent, type RawEvent } from "../model/events.js";
import { TRADE_STAGES } from "../model/kinds.js";
import { buildTrades, marketMetrics, type MarketMetrics, type Trade } from "./trades.js";
import {
  DEFAULT_WINDOW, buyerBoard, participantNames, participantProfiles, racerLastActivity,
  sellerBoard, withinWindow, type BuyerRow, type Profile, type SellerRow,
} from "./participants.js";

/** A trade keeping someone's streaks moving. `startedAt` anchors the sweep. */
export interface ActiveJob { offerId: string; startedAt: number }

/** Same-second tiebreak for the feed: reverse lifecycle order, newest first. */
const FEED_STAGE_RANK: Record<string, number> = {
  offer: 0, claim: 1, award: 2, result: 3, feedback: 4, accept: 5, receipt: 6,
};
const feedStageRank = (e: ParsedEvent): number => (e.stage != null ? FEED_STAGE_RANK[e.stage] ?? 50 : 50);

export interface MarketView {
  windowKey: string;
  /** Events inside the selected window. */
  events: RawEvent[];
  /** The complete cache, for window-independent facts. */
  allEvents: RawEvent[];
  buyers: BuyerRow[];
  sellers: SellerRow[];
  /** Trade-stage activity in the window, newest first. */
  feed: ParsedEvent[];
  names: Map<string, string>;
  profiles: Map<string, Profile>;
  metrics: MarketMetrics;
  /** Places gained on the ALL-TIME standings in the last 24h, by pubkey. */
  buyerClimbs: Map<string, number>;
  sellerClimbs: Map<string, number>;
  overtakes: Overtake[];
  /** "I'm active!" — open trades per participant, driving the streaks. */
  activeByBuyer: Map<string, ActiveJob[]>;
  activeBySeller: Map<string, ActiveJob[]>;
  /** Trade join by offer id — the counterparty lookup for feed lines. */
  trades: Map<string, Trade>;
  racerLastSeen: Map<string, number>;
  generatedAt: number;
}

/**
 * Receipts are OPTIONAL announcements — a trade can settle with no public
 * receipt ever published. "Active until paid" therefore needs an expiry, or a
 * delivered-but-never-receipted trade signals work forever.
 */
export const ACTIVE_GRACE_SECONDS = 86400;

/**
 * What keeps a participant's streaks moving. A racer is active from the moment
 * they POST an offer; a runner from the moment they POST a claim. Activity
 * ends when the trade pays (accept or receipt), is declined, loses the award
 * to another runner, or blows its deadline with nothing delivered — an ended
 * trade must never read as busy.
 */
export function activeTradeJobs(allEvents: RawEvent[], t: number): {
  byBuyer: Map<string, ActiveJob[]>;
  bySeller: Map<string, ActiveJob[]>;
} {
  const parsed = allEvents.map(parseEvent).filter((e): e is ParsedEvent => e != null);
  const deadlines = new Map<string, number>();
  const awards = new Map<string, string>();
  const claims: { seller: string; offerId: string; at: number }[] = [];
  for (const e of parsed) {
    if (!e.offerId) continue;
    if (e.stage === "offer" && e.deadline) deadlines.set(e.offerId, e.deadline);
    if (e.stage === "award" && e.awardedSeller) awards.set(e.offerId, e.awardedSeller);
    if (e.stage === "claim") claims.push({ seller: e.pubkey, offerId: e.offerId, at: e.created_at });
  }
  const trades = buildTrades(parsed);
  const tradeByOffer = new Map(trades.map((tr) => [tr.offerId, tr]));
  const over = (trade: Trade | undefined, offerId: string): boolean => {
    if (!trade) return true;
    // Paid means the RECEIPT — an accept only authorizes payment, and the
    // lamp should keep running until the money lands (or the grace below
    // expires it, since receipts are optional announcements).
    if (trade.at.receipt != null) return true;
    if (trade.declineReason) return true;                                // declined
    const deadline = deadlines.get(offerId);
    if (deadline && deadline < t && trade.at.result == null) return true; // blown, nothing delivered
    // Delivered a day ago with still no public accept/receipt: whatever
    // settlement happened, it happened — stop signalling work.
    if (trade.at.result != null && t - trade.at.result > ACTIVE_GRACE_SECONDS) return true;
    // No deadline to expire it and nothing moving: a day of silence ends it.
    if (!deadline && trade.at.result == null) {
      const stamps = Object.values(trade.at).filter((n): n is number => Number.isFinite(n));
      if (stamps.length && t - Math.max(...stamps) > ACTIVE_GRACE_SECONDS) return true;
    }
    return false;
  };
  const add = (map: Map<string, ActiveJob[]>, pk: string, job: ActiveJob) => {
    const list = map.get(pk) || [];
    if (!list.some((j) => j.offerId === job.offerId)) list.push(job);
    map.set(pk, list);
  };
  const byBuyer = new Map<string, ActiveJob[]>();
  const bySeller = new Map<string, ActiveJob[]>();
  for (const trade of trades) {
    if (trade.at.offer == null || !trade.buyer || over(trade, trade.offerId)) continue;
    add(byBuyer, trade.buyer, { offerId: trade.offerId, startedAt: trade.at.offer });
  }
  for (const c of claims) {
    if (over(tradeByOffer.get(c.offerId), c.offerId)) continue;
    const winner = awards.get(c.offerId);
    if (winner && winner !== c.seller) continue; // the award went elsewhere
    add(bySeller, c.seller, { offerId: c.offerId, startedAt: c.at });
  }
  return { byBuyer, bySeller };
}

/**
 * Positions gained in the last 24 hours — ONE fact, not one per time filter:
 * did this agent pass anyone in the all-time standings since yesterday? Both
 * rankings run over the full event set (now vs t-24h) and are diffed, so an
 * overtake shows on every filter and a filter that reshuffles its own local
 * order invents none. A row absent from yesterday's standings is a new
 * entrant, not a climber.
 */
export interface Overtake { winner: string; loser: string }

export interface RankShifts {
  climbs: Map<string, number>;
  /** Who passed whom: for each climber, the displaced agent nearest below. */
  overtakes: Overtake[];
}

export function rankShifts(
  board: (events: RawEvent[], now: number) => { pubkey: string }[],
  allEvents: RawEvent[],
  t: number,
): RankShifts {
  const dayAgo = t - 86400;
  const cur = board(allEvents, t);
  const prev = board(allEvents.filter((e) => e.created_at <= dayAgo), dayAgo);
  const prevPos = new Map(prev.map((r, i) => [r.pubkey, i]));
  const climbs = new Map<string, number>();
  const overtakes: Overtake[] = [];
  cur.forEach((r, i) => {
    const was = prevPos.get(r.pubkey);
    if (was == null || was <= i) return;
    climbs.set(r.pubkey, was - i);
    // The agent they most visibly passed: ranked above them yesterday, below
    // them today, nearest in today's order. New entrants can't be "passed".
    for (let j = i + 1; j < cur.length; j++) {
      const other = cur[j]!;
      const otherWas = prevPos.get(other.pubkey);
      if (otherWas != null && otherWas < was) {
        overtakes.push({ winner: r.pubkey, loser: other.pubkey });
        break;
      }
    }
  });
  return { climbs, overtakes };
}

export function rankClimbs(
  board: (events: RawEvent[], now: number) => { pubkey: string }[],
  allEvents: RawEvent[],
  t: number,
): Map<string, number> {
  return rankShifts(board, allEvents, t).climbs;
}

export interface Engine {
  /** Feed one event; recompute is coalesced automatically. */
  ingest(event: RawEvent): { stored: boolean; evictedId?: string };
  /** Force any pending recompute to happen NOW (e.g. history just completed). */
  flush(): void;
  setWindow(key: string): void;
  subscribe(listener: (view: MarketView) => void): () => void;
  view(): MarketView | null;
  readonly cache: EventCache;
}

export function createEngine(
  { windowKey = DEFAULT_WINDOW, now = () => Math.floor(Date.now() / 1000) } = {},
): Engine {
  const cache = createCache();
  const listeners = new Set<(view: MarketView) => void>();
  let currentWindow = windowKey;
  let current: MarketView | null = null;
  let pending: ReturnType<typeof setTimeout> | null = null;

  function recompute() {
    pending = null;
    const t = now();
    const allEvents = cache.all();
    const events = withinWindow(allEvents, currentWindow, t);
    const buyers = buyerBoard(events, t, allEvents);
    const sellers = sellerBoard(events, t, allEvents);
    const active = activeTradeJobs(allEvents, t);
    const buyerShifts = rankShifts(buyerBoard, allEvents, t);
    const sellerShifts = rankShifts(sellerBoard, allEvents, t);
    const feed = events.map(parseEvent)
      .filter((e): e is ParsedEvent => e != null && e.kind in TRADE_STAGES)
      .sort((a, b) => b.created_at - a.created_at || feedStageRank(b) - feedStageRank(a) || b.id.localeCompare(a.id));
    current = {
      windowKey: currentWindow,
      events,
      allEvents,
      buyers,
      sellers,
      feed,
      names: participantNames(allEvents),
      profiles: participantProfiles(allEvents),
      metrics: marketMetrics(events),
      buyerClimbs: buyerShifts.climbs,
      sellerClimbs: sellerShifts.climbs,
      // Runner overtakes lead: the sell side is the competitive lane.
      overtakes: [...sellerShifts.overtakes, ...buyerShifts.overtakes],
      activeByBuyer: active.byBuyer,
      activeBySeller: active.bySeller,
      trades: new Map(buildTrades(allEvents).map((tr) => [tr.offerId, tr])),
      racerLastSeen: racerLastActivity(allEvents),
      generatedAt: t,
    };
    for (const listener of listeners) listener(current);
  }

  /**
   * Coalesce: a history page is hundreds of ingests in a burst, and one
   * recompute per event would be quadratic for no reader benefit. 50ms is
   * imperceptible for a single live event and collapses a page to one pass;
   * `flush()` exists for the moments that must not wait (history complete,
   * window change).
   */
  function schedule() {
    if (pending) return;
    pending = setTimeout(recompute, 50);
  }

  return {
    ingest(event) {
      const result = cache.ingest(event);
      if (result.stored) schedule();
      return { stored: result.stored, ...(result.evictedId ? { evictedId: result.evictedId } : {}) };
    },
    flush() {
      if (pending) clearTimeout(pending);
      recompute();
    },
    setWindow(key) {
      if (key === currentWindow) return;
      currentWindow = key;
      if (pending) clearTimeout(pending);
      recompute();
    },
    subscribe(listener) {
      listeners.add(listener);
      if (current) listener(current);
      return () => listeners.delete(listener);
    },
    view: () => current,
    cache,
  };
}
