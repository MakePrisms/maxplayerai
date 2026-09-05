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
  /**
   * The runner that actually won and was paid — resolved from the buyer-signed
   * award/accept/receipt, NOT from whoever claimed first. Null when no seller
   * is yet known, or when `sellerConflict` is set.
   */
  seller: string | null;
  /**
   * The buyer-authenticated records name more than one seller (e.g. an award
   * for X but an accept/receipt for Y). The winner cannot be trusted, so
   * `seller` is null and the UI must say so rather than pick one.
   */
  sellerConflict: boolean;
  offerAmount: number | null;
  receiptAmount: number | null;
  /** Terminal feedback published by `seller` — the winning attempt ending. A
   *  losing claimant's refusal is its own business and belongs in `releasedBy`,
   *  not here; with no identified winner there is no attempt to end, so this
   *  stays null rather than guessing at whichever refusal paged first. */
  declineReason: string | null;
  /** Every runner that claimed this trade, earliest first. */
  claimants: { seller: string; at: number }[];
  /** Runners whose own terminal feedback ended their own claim. */
  releasedBy: string[];
  /** Runners that published a result for this trade. */
  deliveredBy: string[];
  selfTrade: boolean;
  /**
   * The offer settles with no payment (`settlesWithoutPayment`). Its ACCEPT is
   * the last event there will ever be: no receipt is owed, so "delivered with
   * no receipt" is completion here, not an open question.
   */
  free: boolean;
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

/**
 * The seller signals gathered for one trade, in trust order. `authenticated`
 * holds every seller named by a buyer-signed record (award/accept/receipt): one
 * distinct value is the winner, two or more is a conflict. `delivered` and
 * `claimed` are seller-authored fallbacks used only when no buyer-signed record
 * exists — and each is the EARLIEST such event, so the result is independent of
 * relay arrival order.
 */
interface SellerSignals {
  authenticated: Set<string>;
  delivered: string | null;
  deliveredAt: number;
  claimed: string | null;
  claimedAt: number;
}

/**
 * Resolve the winning seller from the gathered signals.
 *
 * Buyer-signed records decide it: they are the only statements a claimant
 * cannot forge for itself. A losing late claim carries no award, accept or
 * receipt, so it can never be named here. When those records disagree the
 * winner is genuinely unknown — reported as a conflict, never guessed. Only in
 * the total absence of a buyer-signed record do we fall back to the runner that
 * delivered, then to the earliest claimant.
 */
function resolveSeller(sig: SellerSignals): { seller: string | null; conflict: boolean } {
  if (sig.authenticated.size > 1) return { seller: null, conflict: true };
  if (sig.authenticated.size === 1) return { seller: [...sig.authenticated][0]!, conflict: false };
  return { seller: sig.delivered ?? sig.claimed ?? null, conflict: false };
}

function buildTradesUncached(events: (RawEvent | ParsedEvent)[]): Trade[] {
  const trades = new Map<string, Trade>();
  const signals = new Map<string, SellerSignals>();
  const ensure = (offerId: string): Trade => {
    let t = trades.get(offerId);
    if (!t) {
      t = { offerId, buyer: null, seller: null, sellerConflict: false, offerAmount: null,
            receiptAmount: null, declineReason: null,
            claimants: [], releasedBy: [], deliveredBy: [], selfTrade: false, free: false, at: {} };
      trades.set(offerId, t);
      signals.set(offerId, { authenticated: new Set(), delivered: null, deliveredAt: Infinity,
                             claimed: null, claimedAt: Infinity });
    }
    return t;
  };
  // Charging a refusal cannot happen in the ingest pass: the buyer-signed record
  // that names the winner may arrive AFTER the feedback it has to be compared
  // against. So collect every terminal refusal, then resolve.
  const terminals = new Map<string, { seller: string; at: number; reason: string }[]>();
  const earliest = (trade: Trade, stage: Stage, ts: number) => {
    const seen = trade.at[stage];
    trade.at[stage] = seen == null ? ts : Math.min(seen, ts);
  };

  for (const raw of events) {
    const e = raw && (raw as ParsedEvent).stage !== undefined ? (raw as ParsedEvent) : parseEvent(raw as RawEvent);
    if (!e || !e.stage || !e.offerId) continue;
    const t = ensure(e.offerId);
    const sig = signals.get(e.offerId)!;
    earliest(t, e.stage, e.created_at);
    if (e.buyer) t.buyer = t.buyer || e.buyer;
    // Buyer-signed seller bindings — the authoritative winner. See resolveSeller.
    const authed = e.awardedSeller || e.acceptedSeller || e.receiptSeller;
    if (authed) sig.authenticated.add(authed);
    // Seller-authored fallbacks, earliest-wins so arrival order can't decide it.
    if (e.stage === "result" && e.seller && e.created_at < sig.deliveredAt) {
      sig.delivered = e.seller; sig.deliveredAt = e.created_at;
    }
    if (e.stage === "claim" && e.seller && e.created_at < sig.claimedAt) {
      sig.claimed = e.seller; sig.claimedAt = e.created_at;
    }
    // The full rosters the runner board counts off: one row per runner that
    // claimed, and delivery checked per runner rather than trade-wide.
    if (e.stage === "claim" && e.seller && !t.claimants.some((c) => c.seller === e.seller)) {
      t.claimants.push({ seller: e.seller, at: e.created_at });
    }
    if (e.stage === "result" && e.seller && !t.deliveredBy.includes(e.seller)) t.deliveredBy.push(e.seller);
    if (e.stage === "offer" && e.amount != null) t.offerAmount = e.amount;
    if (e.stage === "offer" && e.selfTrade) t.selfTrade = true;
    if (e.stage === "offer" && e.free) t.free = true;
    // A floor, like the stamp above is an earliest: several receipts can name
    // one trade, they arrive in relay order, and a plain assignment would let
    // whichever paged last decide the figure.
    if (e.stage === "receipt" && e.amount != null) {
      t.receiptAmount = Math.max(t.receiptAmount ?? 0, e.amount);
    }
    // Only a TERMINAL feedback (claim_released / refusal / error, §7.2) is a
    // decline. `reason` alone can't gate this: feedbackReason() always returns
    // text ("unspecified" at worst), so keying on it made every routine
    // `progress` note read as a decline — ending working lamps and counting
    // as a release.
    if (e.stage === "feedback" && e.terminal && e.reason && e.seller) {
      const list = terminals.get(e.offerId) ?? [];
      list.push({ seller: e.seller, at: e.created_at, reason: e.reason });
      terminals.set(e.offerId, list);
    }
  }

  for (const t of trades.values()) {
    const { seller, conflict } = resolveSeller(signals.get(t.offerId)!);
    t.seller = seller;
    t.sellerConflict = conflict;

    t.claimants.sort((a, b) => a.at - b.at || a.seller.localeCompare(b.seller));
    const seen = (terminals.get(t.offerId) ?? []).slice().sort((a, b) => a.at - b.at || a.seller.localeCompare(b.seller));
    for (const f of seen) {
      if (!t.releasedBy.includes(f.seller)) t.releasedBy.push(f.seller);
    }
    // The trade declines when the WINNER's attempt ends, so only the winner's
    // own refusal counts — a losing claimant refusing is that claimant's record
    // (`releasedBy`) and nothing more. With no winner identified, or with the
    // buyer-signed records in conflict, there is no attempt to call ended:
    // null, on the same principle that leaves `seller` null rather than guess.
    t.declineReason = t.seller ? seen.find((f) => f.seller === t.seller)?.reason ?? null : null;
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
