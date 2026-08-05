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
import { HANDLER, HEARTBEAT, PROFILE, RESULT } from "./kinds.js";

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
 * Display names published as kind-0 metadata, keyed by pubkey.
 *
 * A kind-0 ENRICHES a participant; it never creates one. Publishing profile
 * metadata makes an account, not a market participant — kind-0 is a Nostr
 * standard that anyone on the relay publishes, so a row minted from one puts
 * strangers in a column. Measured when kind-0 first began arriving: 13 of 24
 * seller rows were profile-owners with no claim, no delivery, no advert and no
 * heartbeat, including a pubkey whose only activity was BUYING.
 *
 * What earns a row stays evidence of doing the thing: a claim, a delivery or a
 * receipt for a seller, plus the advert and heartbeat that say a seat is open
 * for work; an offer or an award for a buyer.
 */
function profileNames(parsed) {
  const names = new Map();
  for (const p of parsed) {
    if (p.kind !== PROFILE || !p.name) continue;
    const prev = names.get(p.pubkey);
    // Newest wins. The cache already resolves kind-0 to one per author, but
    // these boards are pure over whatever array they are handed.
    if (!prev || p.created_at >= prev.at) names.set(p.pubkey, { name: p.name, at: p.created_at });
  }
  return names;
}

/**
 * Name the rows that exist. Applied AFTER every row-creating pass, so it cannot
 * depend on whether a profile happened to arrive before or after the claim or
 * advert that earned the row — relay order is not ours to choose.
 */
function applyProfileNames(rows, parsed) {
  for (const [pubkey, { name }] of profileNames(parsed)) {
    const row = rows.get(pubkey);
    if (row) row.name = name;
  }
}

/**
 * Buyers, ranked by sats paid.
 *
 * `satsPaid` counts published receipts only — the same floor that applies
 * everywhere, since a buyer can settle without announcing it.
 */
export function buyerBoard(events, now) {
  const trades = buildTrades(events);
  const parsed = events.map(parseEvent).filter(Boolean);
  const rows = new Map();
  const get = (pk) => {
    let r = rows.get(pk);
    if (!r) {
      r = { pubkey: pk, posted: 0, awarded: 0, receipted: 0, satsPaid: 0,
            prices: [], lastSeen: 0, unpaidDeliveries: 0,
            // Self-published, from kind-0. A claim, like the seller's.
            name: null };
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

  // Same rule as the seller board: posting or awarding work earns the row, the
  // profile only names it.
  applyProfileNames(rows, parsed);

  return [...rows.values()]
    .map((r) => ({ ...r, medianPrice: median(r.prices) }))
    .sort((a, b) => b.satsPaid - a.satsPaid || b.posted - a.posted);
}

/**
 * Sellers, ranked by track record: work delivered, then how much of what they
 * took they finished, then sats. That order matches the columns as they read
 * left to right, and it leads with the sturdiest number — a delivery is a
 * protocol step the buyer needs in order to pay, whereas a receipt is optional,
 * so sats are a floor and deliveries are not.
 *
 * Being online does not lift a seller up the board. It is a claim about this
 * minute, not evidence of anything, and ranking on it puts a seller that has
 * never finished a job above one with a history. It is still shown per row.
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
            released: 0, deliverTimes: [], lastSeen: 0, online: false, capabilities: [],
            // Which agent runtime actually did the work, counted per delivery —
            // a seller may move between harnesses, so this is a tally, not a label.
            harnessCounts: {},
            // Self-advertised, from the newest advert. Claims, not measurements.
            name: null, about: null, askSats: null, openPool: false, mint: null, advertisedAt: 0 };
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
      // The advert is what a seller says about itself: its asking rate and
      // whether it will take work nobody offered it directly. Kept distinct
      // from measured behaviour — a claim, not a track record. The seat NAME is
      // no longer read here: kind-0 metadata is its single publisher (§6.1 /
      // #275), resolved in the PROFILE arm below.
      const r = get(p.pubkey);
      if (p.name && !r.capabilities.includes(p.name)) r.capabilities.push(p.name);
      if (p.created_at >= r.advertisedAt) {
        r.advertisedAt = p.created_at;
        r.about = p.about || r.about;
        r.askSats = p.askSats != null ? p.askSats : r.askSats;
        r.openPool = p.openPool;
        r.mint = p.mint || r.mint;
      }
    } else if (p.kind === RESULT && p.harness) {
      const r = get(p.pubkey);
      r.harnessCounts[p.harness] = (r.harnessCounts[p.harness] || 0) + 1;
    }
  }

  // kind-0 is the single publisher of the seat name (§6.1 / #275) and the 31990
  // advert no longer carries it. Named last, once every row that earned a place
  // on this board exists.
  applyProfileNames(rows, parsed);

  return [...rows.values()]
    .map((r) => {
      const ranked = Object.entries(r.harnessCounts).sort((a, b) => b[1] - a[1]);
      return {
        ...r,
        medianDeliverSeconds: median(r.deliverTimes),
        // Of the claims this seller took, how many turned into a delivery.
        completionRate: r.claimed > 0 ? r.delivered / r.claimed : null,
        /** Most-used harness, or null if this seller has never delivered. */
        harness: ranked.length ? ranked[0][0] : null,
        harnesses: ranked.map(([name, n]) => ({ name, deliveries: n })),
      };
    })
    // A seller with no claims has no rate to compare, so it sorts below any
    // measured rate rather than tying with a zero.
    .sort((a, b) =>
      b.delivered - a.delivered ||
      (b.completionRate ?? -1) - (a.completionRate ?? -1) ||
      b.satsEarned - a.satsEarned ||
      b.lastSeen - a.lastSeen ||
      // Last resort, so the board does not reshuffle as new events arrive.
      a.pubkey.localeCompare(b.pubkey));
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
