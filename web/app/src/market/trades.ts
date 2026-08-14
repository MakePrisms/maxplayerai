/**
 * Trade join and market metrics.
 *
 * Events arrive out of order and one trade is spread across several of them,
 * so everything here keys on the root offer id and takes the EARLIEST
 * timestamp per stage — a re-published or duplicated event must not move a
 * trade's clock.
 *
 * On counting settlements: a receipt is an optional announcement. Payment
 * itself travels as encrypted gift-wrap, so a trade can settle with no receipt
 * ever published. Every settlement figure here is therefore a FLOOR — named to
 * say so, because a name is the only thing that survives being copied into a
 * view.
 */
import { parseEvent, type ParsedEvent, type RawEvent } from "../model/events.js";
import type { Stage } from "../model/kinds.js";

export interface Trade {
  offerId: string;
  buyer: string | null;
  seller: string | null;
  offerAmount: number | null;
  receiptAmount: number | null;
  declineReason: string | null;
  selfTrade: boolean;
  at: Partial<Record<Stage, number>>;
}

/**
 * Per-array memo: within one engine recompute the SAME event array is joined
 * by several consumers (boards, metrics, active jobs, counterparties). The
 * array is rebuilt each recompute, so keying on its identity gives exactly
 * one join per pass and zero staleness.
 */
const TRADES_CACHE = new WeakMap<object, Trade[]>();

/** Join parsed events into one record per trade, keyed by root offer id. */
export function buildTrades(events: (RawEvent | ParsedEvent)[]): Trade[] {
  const hit = TRADES_CACHE.get(events);
  if (hit) return hit;
  const trades = buildTradesUncached(events);
  TRADES_CACHE.set(events, trades);
  return trades;
}

function buildTradesUncached(events: (RawEvent | ParsedEvent)[]): Trade[] {
  const trades = new Map<string, Trade>();
  const ensure = (offerId: string): Trade => {
    let t = trades.get(offerId);
    if (!t) {
      t = { offerId, buyer: null, seller: null, offerAmount: null, receiptAmount: null,
            declineReason: null, selfTrade: false, at: {} };
      trades.set(offerId, t);
    }
    return t;
  };
  const earliest = (trade: Trade, stage: Stage, ts: number) => {
    const seen = trade.at[stage];
    trade.at[stage] = seen == null ? ts : Math.min(seen, ts);
  };

  for (const raw of events) {
    const e = raw && (raw as ParsedEvent).stage !== undefined ? (raw as ParsedEvent) : parseEvent(raw as RawEvent);
    if (!e || !e.stage || !e.offerId) continue;
    const t = ensure(e.offerId);
    earliest(t, e.stage, e.created_at);
    if (e.buyer) t.buyer = t.buyer || e.buyer;
    if (e.seller) t.seller = t.seller || e.seller;
    if (e.stage === "offer" && e.amount != null) t.offerAmount = e.amount;
    if (e.stage === "offer" && e.selfTrade) t.selfTrade = true;
    if (e.stage === "receipt" && e.amount != null) t.receiptAmount = e.amount;
    // Only a TERMINAL feedback (claim_released / refusal / error, §7.2) is a
    // decline. `reason` alone can't gate this: feedbackReason() always returns
    // text ("unspecified" at worst), so keying on it made every routine
    // `progress` note read as a decline — ending working lamps and counting
    // as a release.
    if (e.stage === "feedback" && e.terminal && e.reason) t.declineReason = t.declineReason || e.reason;
  }
  return [...trades.values()];
}

export interface MarketFunnel {
  posted: number;
  claimed: number;
  awarded: number;
  delivered: number;
  receipted: number;
}

export interface MarketMetrics {
  funnel: MarketFunnel;
  rootedElsewhere: number;
  receiptsOnRecord: number;
  satsInReceipts: number;
  buyers: number;
  sellers: number;
  firstEventAt: number | null;
  lastEventAt: number | null;
  daysActive: number;
  tradesTracked: number;
  selfTrades: number;
}

/**
 * Market metrics.
 *
 * The funnel counts only trades whose OFFER we actually saw — a trade first
 * observed at a later stage has unknowable early stages, and silently treating
 * it as "posted" would inflate the top of the funnel. Those are reported
 * separately as `rootedElsewhere` rather than dropped without trace.
 */
export function marketMetrics(events: RawEvent[]): MarketMetrics {
  const all = buildTrades(events);
  // Self-commissioned trades are real work but not market demand. They are
  // removed from every trade-derived figure and reported separately —
  // excluding them silently would be its own dishonesty.
  const selfTrades = all.filter((t) => t.selfTrade).length;
  const trades = all.filter((t) => !t.selfTrade);
  const withOffer = trades.filter((t) => t.at.offer != null);
  const reached = (stage: Stage) => withOffer.filter((t) => t.at[stage] != null).length;

  const receipted = trades.filter((t) => t.at.receipt != null);
  const buyers = new Set(trades.map((t) => t.buyer).filter(Boolean));
  const sellers = new Set(trades.map((t) => t.seller).filter(Boolean));

  let first: number | null = null;
  let last: number | null = null;
  for (const e of events) {
    const ts = e && e.created_at;
    if (typeof ts !== "number") continue;
    if (first === null || ts < first) first = ts;
    if (last === null || ts > last) last = ts;
  }

  return {
    funnel: {
      posted: withOffer.length,
      claimed: reached("claim"),
      awarded: reached("award"),
      delivered: reached("result"),
      receipted: reached("receipt"),
    },
    /** Trades seen only from a later stage — their offer predates what we hold. */
    rootedElsewhere: trades.length - withOffer.length,
    /** FLOOR: co-signed receipts published. Settlements without one are invisible here. */
    receiptsOnRecord: receipted.length,
    /** FLOOR: sats across those receipts only. */
    satsInReceipts: receipted.reduce((sum, t) => sum + (t.receiptAmount || 0), 0),
    buyers: buyers.size,
    sellers: sellers.size,
    firstEventAt: first,
    lastEventAt: last,
    /** Whole UTC days spanned, inclusive. */
    daysActive: first == null || last == null ? 0 : Math.floor(last / 86400) - Math.floor(first / 86400) + 1,
    tradesTracked: trades.length,
    /** Excluded from every figure above. Real work; not demand. */
    selfTrades,
  };
}
