/**
 * Trade join and market metrics.
 *
 * Events arrive out of order and one trade is spread across several of them, so
 * everything here keys on the root offer id and takes the EARLIEST timestamp per
 * stage — a re-published or duplicated event must not move a trade's clock.
 *
 * On counting settlements: a receipt is an optional announcement. Payment itself
 * travels as encrypted gift-wrap, so a trade can settle with no receipt ever
 * published. Every settlement figure here is therefore a FLOOR — named to say
 * so, because a name is the only thing that survives being copied into a view.
 */
import { parseEvent } from "./model.js";

/** Join parsed events into one record per trade, keyed by root offer id. */
export function buildTrades(events) {
  const trades = new Map();
  const ensure = (offerId) => {
    let t = trades.get(offerId);
    if (!t) {
      t = { offerId, buyer: null, seller: null, offerAmount: null, receiptAmount: null,
            declineReason: null, at: {} };
      trades.set(offerId, t);
    }
    return t;
  };
  const earliest = (trade, stage, ts) => {
    const seen = trade.at[stage];
    trade.at[stage] = seen == null ? ts : Math.min(seen, ts);
  };

  for (const raw of events) {
    const e = raw && raw.stage !== undefined ? raw : parseEvent(raw);
    if (!e || !e.stage || !e.offerId) continue;
    const t = ensure(e.offerId);
    earliest(t, e.stage, e.created_at);
    if (e.buyer) t.buyer = t.buyer || e.buyer;
    if (e.seller) t.seller = t.seller || e.seller;
    if (e.stage === "offer" && e.amount != null) t.offerAmount = e.amount;
    if (e.stage === "receipt" && e.amount != null) t.receiptAmount = e.amount;
    if (e.stage === "feedback" && e.reason) t.declineReason = t.declineReason || e.reason;
  }
  return [...trades.values()];
}

/**
 * Market metrics.
 *
 * The funnel counts only trades whose OFFER we actually saw — a trade first
 * observed at a later stage has unknowable early stages, and silently treating
 * it as "posted" would inflate the top of the funnel. Those are reported
 * separately as `rootedElsewhere` rather than dropped without trace.
 */
export function marketMetrics(events) {
  const trades = buildTrades(events);
  const withOffer = trades.filter((t) => t.at.offer != null);
  const reached = (stage) => withOffer.filter((t) => t.at[stage] != null).length;

  const receipted = trades.filter((t) => t.at.receipt != null);
  const buyers = new Set(trades.map((t) => t.buyer).filter(Boolean));
  const sellers = new Set(trades.map((t) => t.seller).filter(Boolean));

  let first = null;
  let last = null;
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
    daysActive: first == null ? 0 : Math.floor(last / 86400) - Math.floor(first / 86400) + 1,
    tradesTracked: trades.length,
  };
}

/** Conversion rate of each funnel stage against the one before it, 0–1. */
export function conversionRates({ posted, claimed, awarded, delivered, receipted }) {
  const rate = (n, of) => (of > 0 ? n / of : 0);
  return {
    claimed: rate(claimed, posted),
    awarded: rate(awarded, claimed),
    delivered: rate(delivered, awarded || claimed),
    receipted: rate(receipted, delivered),
  };
}
