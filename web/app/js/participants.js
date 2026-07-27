/**
 * Per-participant track records, and the time window they are measured over.
 *
 * A window filters the EVENTS, not the finished trades: a trade is a real thing
 * that happened at several moments, so "the last 24 hours" means the activity
 * inside that period, and a trade straddling the boundary contributes only the
 * stages that fall inside it. Filtering finished trades by a single timestamp
 * would silently pick one stage's clock to stand for the whole trade.
 */
import { buildTrades } from "./trades.js";
import { parseEvent } from "./model.js";
import { HANDLER, HEARTBEAT } from "./kinds.js";

/** Selectable periods, longest label first so the UI can render them in order. */
export const WINDOWS = Object.freeze([
  { key: "24h", label: "24 hours", seconds: 86400 },
  { key: "week", label: "Week", seconds: 604800 },
  { key: "all", label: "All time", seconds: null },
]);

export const DEFAULT_WINDOW = "week";

/** A seller with no heartbeat this recently is not claimed to be online. */
export const LIVE_WITHIN_SECONDS = 300;

export function windowSeconds(key) {
  const w = WINDOWS.find((x) => x.key === key);
  return w ? w.seconds : null;
}

/** Events inside the window. A null window means everything. */
export function withinWindow(events, windowKey, now) {
  const span = windowSeconds(windowKey);
  if (span == null) return events;
  const floor = now - span;
  return events.filter((e) => e && e.created_at >= floor);
}

const median = (xs) => {
  if (!xs.length) return null;
  const a = [...xs].sort((p, q) => p - q);
  const mid = Math.floor(a.length / 2);
  return a.length % 2 ? a[mid] : Math.round((a[mid - 1] + a[mid]) / 2);
};

/**
 * Buyers, ranked by sats paid.
 *
 * `satsPaid` counts published receipts only — the same floor that applies
 * everywhere, since a buyer can settle without announcing it.
 */
export function buyerBoard(events, now) {
  const trades = buildTrades(events);
  const rows = new Map();
  const get = (pk) => {
    let r = rows.get(pk);
    if (!r) {
      r = { pubkey: pk, posted: 0, awarded: 0, receipted: 0, satsPaid: 0,
            prices: [], lastSeen: 0, unpaidDeliveries: 0 };
      rows.set(pk, r);
    }
    return r;
  };

  for (const t of trades) {
    if (!t.buyer) continue;
    const r = get(t.buyer);
    if (t.at.offer != null) { r.posted += 1; r.lastSeen = Math.max(r.lastSeen, t.at.offer); }
    if (t.at.award != null) { r.awarded += 1; r.lastSeen = Math.max(r.lastSeen, t.at.award); }
    if (t.at.receipt != null) {
      r.receipted += 1;
      r.satsPaid += t.receiptAmount || 0;
      r.lastSeen = Math.max(r.lastSeen, t.at.receipt);
    } else if (t.at.result != null) {
      // Delivered to this buyer with no receipt published. Not proof of
      // non-payment — settlement can happen without one — so it is surfaced
      // as an open question, never as a debt.
      r.unpaidDeliveries += 1;
    }
    if (t.offerAmount != null) r.prices.push(t.offerAmount);
  }

  return [...rows.values()]
    .map((r) => ({ ...r, medianPrice: median(r.prices) }))
    .sort((a, b) => b.satsPaid - a.satsPaid || b.posted - a.posted);
}

/**
 * Sellers, ranked by sats earned.
 *
 * `online` comes from a heartbeat inside LIVE_WITHIN_SECONDS — a claim about
 * right now. `lastSeen` is the last time they did anything at all. The two are
 * kept apart because "traded recently" is not "available now", and conflating
 * them would advertise sellers that cannot take work.
 */
export function sellerBoard(events, now) {
  const trades = buildTrades(events);
  const parsed = events.map(parseEvent).filter(Boolean);

  const rows = new Map();
  const get = (pk) => {
    let r = rows.get(pk);
    if (!r) {
      r = { pubkey: pk, claimed: 0, delivered: 0, receipted: 0, satsEarned: 0,
            released: 0, deliverTimes: [], lastSeen: 0, online: false, capabilities: [] };
      rows.set(pk, r);
    }
    return r;
  };

  for (const t of trades) {
    if (!t.seller) continue;
    const r = get(t.seller);
    if (t.at.claim != null) { r.claimed += 1; r.lastSeen = Math.max(r.lastSeen, t.at.claim); }
    if (t.at.result != null) {
      r.delivered += 1;
      r.lastSeen = Math.max(r.lastSeen, t.at.result);
      if (t.at.claim != null) r.deliverTimes.push(t.at.result - t.at.claim);
    }
    if (t.at.receipt != null) { r.receipted += 1; r.satsEarned += t.receiptAmount || 0; }
    // A claim that produced feedback but never a delivery is a released claim —
    // the single most common failure on this market.
    if (t.declineReason && t.at.result == null) r.released += 1;
  }

  for (const p of parsed) {
    if (p.kind === HEARTBEAT) {
      const r = get(p.pubkey);
      r.lastSeen = Math.max(r.lastSeen, p.created_at);
      if (now - p.created_at <= LIVE_WITHIN_SECONDS) r.online = true;
    } else if (p.kind === HANDLER) {
      const r = get(p.pubkey);
      const name = p.handler?.name || p.handler?.display_name || p.d;
      if (name && !r.capabilities.includes(name)) r.capabilities.push(name);
    }
  }

  return [...rows.values()]
    .map((r) => ({
      ...r,
      medianDeliverSeconds: median(r.deliverTimes),
      // Of the claims this seller took, how many turned into a delivery.
      completionRate: r.claimed > 0 ? r.delivered / r.claimed : null,
    }))
    .sort((a, b) => Number(b.online) - Number(a.online) || b.satsEarned - a.satsEarned);
}

/** Everything known about one participant, for a detail view. */
export function participantDetail(events, pubkey, now) {
  const buyer = buyerBoard(events, now).find((r) => r.pubkey === pubkey) || null;
  const seller = sellerBoard(events, now).find((r) => r.pubkey === pubkey) || null;
  const trades = buildTrades(events)
    .filter((t) => t.buyer === pubkey || t.seller === pubkey)
    .sort((a, b) => (b.at.offer ?? b.at.claim ?? 0) - (a.at.offer ?? a.at.claim ?? 0));
  return { pubkey, buyer, seller, trades };
}
