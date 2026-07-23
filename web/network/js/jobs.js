/**
 * Job aggregation: collapse a marketplace event chain into ONE job per offer.
 *
 * A "job" is the unit the feed renders — an offer and everything that happened to it
 * (claims → accepts → result → payment, or a refusal / an expiry). This module is pure
 * (no DOM, no relay, no kind literals — it works on the ROLE that parse.js assigns) so
 * the whole lifecycle is unit-testable from recorded fixture events.
 */

/** Lifecycle stages, colored as a status stripe in the feed. */
export const JOB_STATUS = Object.freeze({
  OPEN: "open",
  CLAIMED: "claimed",
  WORKING: "working",
  DELIVERED: "delivered",
  PAID: "paid",
  REFUSED: "refused",
  EXPIRED: "expired",
});

/** Human one-liner per status (feed shows this, never the raw enum). */
export const STATUS_LABELS = Object.freeze({
  open: "Open — waiting for a seller",
  claimed: "Claimed — a seller took it",
  working: "Working — buyer accepted the claim",
  delivered: "Delivered — result posted, not yet paid",
  paid: "Paid — settled",
  refused: "Refused — the job failed",
  expired: "Expired — deadline passed, nothing delivered",
});

/** A seller counts as "active" if it acted within this window of `now` (seconds). */
export const ACTIVE_SELLER_WINDOW_S = 24 * 60 * 60;

/**
 * Aggregate normalized market events into job cards, newest-activity first.
 *
 * @param {any[]} events normalized events (parse.js output); profiles are ignored here
 * @param {Map<string, any>} [profiles] pubkey → profile record
 * @param {number} [now] unix-seconds; drives expiry. Defaults to wall clock.
 * @returns {any[]} job cards
 */
export function aggregateJobs(events, profiles = new Map(), now = nowSeconds()) {
  const groups = groupEvents(events);
  const jobs = [];
  for (const g of groups.values()) {
    jobs.push(jobFromGroup(g, profiles, now));
  }
  // Newest activity first; stable tiebreak by id so renders don't jitter.
  jobs.sort((a, b) => b.last_activity - a.last_activity || (a.id < b.id ? -1 : 1));
  return jobs;
}

/**
 * Group marketplace events into per-job buckets, keyed by offer id. Exposed so profile
 * metrics can read the raw sub-events (with authors + timestamps) without re-deriving
 * the linking rules. Each bucket: { id, offer, claims, awards, results, receipts, feedbacks }.
 * @param {any[]} events normalized events
 * @returns {Map<string, any>}
 */
export function groupEvents(events) {
  /** @type {Map<string, any>} */
  const groups = new Map();
  const group = (jobId) => {
    let g = groups.get(jobId);
    if (!g) {
      g = { id: jobId, offer: null, claims: [], awards: [], results: [], receipts: [], feedbacks: [] };
      groups.set(jobId, g);
    }
    return g;
  };

  for (const ev of events) {
    if (!ev) continue;
    switch (ev.role) {
      case "offer":
        group(ev.id).offer = ev;
        break;
      case "claim": {
        const jobId = ev.claim?.offerId;
        if (jobId) group(jobId).claims.push(ev);
        break;
      }
      case "award": {
        const jobId = ev.award?.offerId;
        if (jobId) group(jobId).awards.push(ev);
        break;
      }
      case "feedback": {
        const jobId = ev.feedback?.offerId;
        if (jobId) group(jobId).feedbacks.push(ev);
        break;
      }
      case "result": {
        const jobId = ev.result?.offerId;
        if (jobId) group(jobId).results.push(ev);
        break;
      }
      case "receipt": {
        const jobId = ev.receipt?.offerId;
        if (jobId) group(jobId).receipts.push(ev);
        break;
      }
      default:
        break;
    }
  }
  return groups;
}

/** Build one job card from a group. Exposed for the profile "recent jobs" lists. */
export function jobFromGroup(g, profiles = new Map(), now = nowSeconds()) {
  const offer = g.offer;
  const deadline = offer?.offer?.deadline ?? null;
  const status = deriveStatus(g, deadline, now);

  const sellerKeys = new Set();
  for (const c of g.claims) sellerKeys.add(c.pubkey);
  for (const r of g.results) sellerKeys.add(r.pubkey);
  // A targeted offer names its intended seller even before a claim lands.
  if (offer?.offer?.seller) sellerKeys.add(offer.offer.seller);

  const paidSats = g.receipts.reduce(
    (sum, r) => sum + (r.receipt?.amount_sats || 0),
    0,
  );
  const amountSats =
    offer?.offer?.amount_sats ??
    g.results[0]?.result?.amount_sats ??
    g.receipts[0]?.receipt?.amount_sats ??
    null;

  const all = [
    offer,
    ...g.claims,
    ...g.awards,
    ...g.results,
    ...g.receipts,
    ...g.feedbacks,
  ].filter(Boolean);
  const createdAt = all.length ? Math.min(...all.map((e) => e.created_at)) : now;
  const lastActivity = all.length ? Math.max(...all.map((e) => e.created_at)) : now;

  return {
    id: g.id,
    status,
    has_offer: Boolean(offer),
    buyer: offer ? party(offer.pubkey, profiles) : null,
    sellers: [...sellerKeys].map((pk) => party(pk, profiles)),
    task: offer?.offer?.task ?? null,
    job_class: offer?.offer?.job_class ?? null,
    amount_sats: amountSats,
    paid_sats: paidSats || null,
    deadline,
    created_at: createdAt,
    last_activity: lastActivity,
    timeline: buildTimeline(g),
  };
}

/**
 * Status waterfall — highest-precedence terminal outcome wins. Expiry is checked
 * before working/claimed so an overdue job with nothing delivered reads as expired.
 */
function deriveStatus(g, deadline, now) {
  if (g.receipts.length) return JOB_STATUS.PAID;
  if (g.feedbacks.some((f) => f.feedback?.isError)) return JOB_STATUS.REFUSED;
  if (g.results.length) return JOB_STATUS.DELIVERED;
  if (deadline != null && deadline < now) return JOB_STATUS.EXPIRED;
  if (g.awards.length) return JOB_STATUS.WORKING;
  if (g.claims.length) return JOB_STATUS.CLAIMED;
  return JOB_STATUS.OPEN;
}

/** Chronological plain-English event list — no raw JSON, no nostr jargon. */
function buildTimeline(g) {
  const entries = [];
  if (g.offer) {
    const amt = g.offer.offer?.amount_sats;
    entries.push({
      at: g.offer.created_at,
      actor: "buyer",
      pubkey: g.offer.pubkey,
      text: amt != null ? `posted a job for ${amt} sats` : "posted a job",
    });
  }
  for (const c of g.claims) {
    entries.push({ at: c.created_at, actor: "seller", pubkey: c.pubkey, text: "claimed the job" });
  }
  for (const a of g.awards) {
    entries.push({ at: a.created_at, actor: "buyer", pubkey: a.pubkey, text: "accepted the claim" });
  }
  for (const r of g.results) {
    entries.push({ at: r.created_at, actor: "seller", pubkey: r.pubkey, text: "delivered the result" });
  }
  for (const f of g.feedbacks) {
    const text = f.feedback?.isError ? "reported the job failed" : "posted a progress update";
    entries.push({ at: f.created_at, actor: "seller", pubkey: f.pubkey, text });
  }
  for (const r of g.receipts) {
    const amt = r.receipt?.amount_sats;
    entries.push({
      at: r.created_at,
      actor: "buyer",
      pubkey: r.pubkey,
      text: amt != null ? `paid ${amt} sats` : "paid the seller",
    });
  }
  entries.sort((a, b) => a.at - b.at);
  return entries;
}

/**
 * Pulse-strip numbers over the same normalized events + derived jobs.
 * @param {any[]} events normalized market events
 * @param {any[]} jobs output of aggregateJobs (for the open-offer count)
 * @param {number} [now] unix-seconds
 */
export function computePulse(events, jobs, now = nowSeconds()) {
  const dayStart = startOfUtcDay(now);
  const activeWindow = now - ACTIVE_SELLER_WINDOW_S;
  let satsSettledToday = 0;
  const activeSellers = new Set();

  for (const ev of events) {
    if (!ev) continue;
    if (ev.role === "receipt" && ev.created_at >= dayStart) {
      satsSettledToday += ev.receipt?.amount_sats || 0;
    }
    if (ev.created_at >= activeWindow) {
      if (ev.role === "result" || ev.role === "handler" || ev.role === "claim") {
        activeSellers.add(ev.pubkey);
      }
    }
  }

  const openOffers = jobs.filter((j) => j.status === JOB_STATUS.OPEN).length;
  return { satsSettledToday, openOffers, activeSellers: activeSellers.size };
}

function party(pubkey, profiles) {
  return { pubkey, profile: profiles.get(pubkey) || null };
}

function startOfUtcDay(nowSec) {
  const d = new Date(nowSec * 1000);
  return Math.floor(Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate()) / 1000);
}

function nowSeconds() {
  return Math.floor(Date.now() / 1000);
}
