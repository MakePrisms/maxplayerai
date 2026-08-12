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
import { ACCEPT, AWARD, CLAIM, FEEDBACK, HEARTBEAT, PROFILE, RECEIPT, RESULT } from "./kinds.js";

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
function profileMetadata(parsed) {
  const profiles = new Map();
  for (const p of parsed) {
    if (p.kind !== PROFILE) continue;
    const prev = profiles.get(p.pubkey);
    // Newest wins. The cache already resolves kind-0 to one per author, but
    // these boards are pure over whatever array they are handed.
    if (!prev || p.created_at >= prev.at) {
      profiles.set(p.pubkey, { name: p.name, about: p.about, metadata: p.profile || {}, at: p.created_at });
    }
  }
  return profiles;
}

/** Public profile names for presentation. Hex remains the fallback, never the label beside a known name. */
export function participantNames(events) {
  return new Map([...participantProfiles(events)].map(([pubkey, p]) => [pubkey, p.name]).filter(([, name]) => name));
}

/** Latest complete public profile per participant, independent of the selected activity window. */
export function participantProfiles(events) {
  return profileMetadata(events.map(parseEvent).filter(Boolean));
}

/**
 * Latest buyer-side job action per racer, independent of the board window.
 *
 * A racer is active when it authors a job lifecycle action, not merely because
 * its profile exists or a runner acts on its job. Keeping this map over the
 * complete cache lets the UI apply one fixed 24-hour lamp rule even while the
 * board itself switches between 24 hours, week, and all time.
 */
export function racerLastActivity(events) {
  const buyerStages = new Set(["offer", "award", "accept", "receipt"]);
  const latest = new Map();
  for (const e of events.map(parseEvent).filter(Boolean)) {
    if (!buyerStages.has(e.stage)) continue;
    latest.set(e.pubkey, Math.max(latest.get(e.pubkey) || 0, e.created_at));
  }
  return latest;
}

/**
 * Name the rows that exist. Applied AFTER every row-creating pass, so it cannot
 * depend on whether a profile happened to arrive before or after the claim or
 * advert that earned the row — relay order is not ours to choose.
 */
function applyProfiles(rows, parsed) {
  for (const [pubkey, profile] of profileMetadata(parsed)) {
    const row = rows.get(pubkey);
    if (row) {
      row.name = profile.name;
      row.about = profile.about;
      row.profile = profile.metadata;
      row.profileAt = profile.at;
    }
  }
}

/**
 * Jobs currently between selection and delivery, joined to the exact winning
 * claim. A trade may have several claimants; only the seller named by AWARD is
 * working. Input order is irrelevant because relay pages do not promise it.
 */
export function inProgressJobs(events) {
  const parsed = events.map(parseEvent).filter(Boolean);
  const claims = new Map(parsed
    .filter((e) => e.kind === CLAIM && e.seller)
    .map((e) => [e.id, e.seller]));
  const jobs = new Map();
  const get = (offerId) => {
    let job = jobs.get(offerId);
    if (!job) {
      job = { offerId, buyer: null, award: null, terminals: [] };
      jobs.set(offerId, job);
    }
    return job;
  };

  for (const e of parsed) {
    if (!e.offerId) continue;
    const job = get(e.offerId);
    if (e.buyer) job.buyer ||= e.buyer;
    if (e.kind === AWARD) {
      if (!job.award || e.created_at < job.award.created_at ||
          (e.created_at === job.award.created_at && e.id < job.award.id)) job.award = e;
    } else if ([RESULT, ACCEPT, RECEIPT, FEEDBACK].includes(e.kind)) {
      job.terminals.push(e);
    }
  }

  const active = [];
  for (const job of jobs.values()) {
    if (!job.award) continue;
    const seller = job.award.awardedSeller || claims.get(job.award.claimId) || null;
    const buyer = job.award.buyer || job.buyer;
    if (!buyer || !seller) continue;
    const finished = job.terminals.some((e) =>
      e.kind === ACCEPT || e.kind === RECEIPT ||
      (e.kind === RESULT && e.seller === seller) ||
      (e.kind === FEEDBACK && e.seller === seller && e.created_at >= job.award.created_at));
    if (finished) continue;
    active.push({
      offerId: job.offerId,
      awardId: job.award.id,
      claimId: job.award.claimId,
      buyer,
      seller,
      startedAt: job.award.created_at,
    });
  }
  return active.sort((a, b) => b.startedAt - a.startedAt || a.offerId.localeCompare(b.offerId));
}

/**
 * Buyers, ranked by sats paid.
 *
 * `satsPaid` counts published receipts only — the same floor that applies
 * everywhere, since a buyer can settle without announcing it.
 */
export function buyerBoard(events, now, activeEvents = events) {
  const trades = buildTrades(events);
  const parsed = events.map(parseEvent).filter(Boolean);
  const rows = new Map();
  const get = (pk) => {
    let r = rows.get(pk);
    if (!r) {
      r = { pubkey: pk, posted: 0, awarded: 0, receipted: 0, satsPaid: 0,
            prices: [], lastSeen: 0, unpaidDeliveries: 0, inProgressJobs: [],
            // Self-published, from kind-0. A claim, like the seller's.
            name: null, about: null, profile: null, profileAt: 0 };
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

  for (const job of inProgressJobs(activeEvents)) get(job.buyer).inProgressJobs.push(job);

  // Same rule as the seller board: posting or awarding work earns the row, the
  // profile only names it.
  applyProfiles(rows, parsed);

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
export function sellerBoard(events, now, activeEvents = events) {
  const trades = buildTrades(events);
  const parsed = events.map(parseEvent).filter(Boolean);

  const rows = new Map();
  const get = (pk) => {
    let r = rows.get(pk);
    if (!r) {
      r = { pubkey: pk, claimed: 0, delivered: 0, receipted: 0, satsEarned: 0,
            released: 0, deliverTimes: [], lastSeen: 0, online: false, inProgressJobs: [],
            // Which agent runtime actually did the work, counted per delivery —
            // a seller may move between harnesses, so this is a tally, not a label.
            harnessCounts: {},
            // Self-advertised, from the newest protocol-v1 heartbeat. Claims,
            // not measurements. Unknown tags stay intact for the detail view.
            name: null, about: null, profile: null, profileAt: 0,
            askSats: null, accepting: null, queueDepth: null,
            acceptedMints: [], advertisedAgents: [], advertisementTags: [],
            advertisementContent: null, advertisedAt: 0 };
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
      if (p.created_at >= r.advertisedAt) {
        r.advertisedAt = p.created_at;
        r.askSats = p.rateSats;
        r.accepting = p.accepting;
        r.queueDepth = p.queueDepth;
        r.acceptedMints = p.acceptedMints;
        r.advertisedAgents = p.agents;
        r.advertisementTags = p.advertisementTags;
        r.advertisementContent = p.advertisementContent;
      }
    } else if (p.kind === RESULT && p.harness) {
      const r = get(p.pubkey);
      r.harnessCounts[p.harness] = (r.harnessCounts[p.harness] || 0) + 1;
    }
  }

  for (const job of inProgressJobs(activeEvents)) get(job.seller).inProgressJobs.push(job);

  // kind-0 is the single publisher of the seat name (§6.1 / #275) and the 31990
  // advert no longer carries it. Named last, once every row that earned a place
  // on this board exists.
  applyProfiles(rows, parsed);

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
export function participantDetail(events, pubkey, now, activeEvents = events) {
  const buyer = buyerBoard(events, now, activeEvents).find((r) => r.pubkey === pubkey) || null;
  const seller = sellerBoard(events, now, activeEvents).find((r) => r.pubkey === pubkey) || null;
  const trades = buildTrades(events)
    .filter((t) => t.buyer === pubkey || t.seller === pubkey)
    .sort((a, b) => (b.at.offer ?? b.at.claim ?? 0) - (a.at.offer ?? a.at.claim ?? 0));
  const activity = participantActivity(events, pubkey);
  return { pubkey, buyer, seller, trades, activity };
}

/**
 * Every event that participant authored plus every public lifecycle event for
 * a job they bought, sold, or were directly offered. This is deliberately
 * broader than trades: profile and heartbeat activity remains visible, and a
 * receipt is retained even when its author is neither displayed role.
 */
export function participantActivity(events, pubkey) {
  const parsed = events.map(parseEvent).filter(Boolean);
  const roots = new Set();
  for (const e of parsed) {
    if (e.pubkey === pubkey || e.buyer === pubkey || e.seller === pubkey ||
        e.awardedSeller === pubkey || e.targetSeller === pubkey) {
      if (e.offerId) roots.add(e.offerId);
    }
  }
  return parsed
    .filter((e) => e.pubkey === pubkey || (e.offerId && roots.has(e.offerId)))
    .sort((a, b) => b.created_at - a.created_at || b.id.localeCompare(a.id));
}

/** One complete public job history, oldest stage first. */
export function relatedActivity(events, offerId) {
  if (!offerId) return [];
  return events.map(parseEvent).filter((e) => e?.offerId === offerId)
    .sort((a, b) => a.created_at - b.created_at || a.id.localeCompare(b.id));
}
