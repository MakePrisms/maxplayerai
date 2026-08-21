/**
 * Profile aggregation: the "should I hire / should I claim" metrics for one pubkey.
 *
 * Pure (no DOM / relay). Builds on jobs.js grouping so the job-linking rules live in one
 * place. Two views: SELLER (reputation for a buyer deciding whom to hire) and BUYER
 * (reputation for a seller deciding whose offer to claim — the side nobody usually shows).
 */
import { groupEvents, jobFromGroup, JOB_STATUS } from "./jobs.js";
import { SELLER_HEARTBEAT_D } from "./kinds.js";
import { harnessFamilyFromId } from "./parse.js";

/** Liveness thresholds (seconds) from last-seen to now. */
export const LIVE_WINDOW_S = 30 * 60;
export const RECENT_WINDOW_S = 24 * 60 * 60;

/**
 * Resolve the current heartbeat per author: newest kind-30340 by (author, d). Addressable
 * events supersede, so we key by author+d and keep the max created_at — NEVER by event id.
 * @param {any[]} events normalized events
 * @returns {Map<string, any>} author pubkey → newest heartbeat event for that author
 */
export function resolveHeartbeats(events) {
  /** @type {Map<string, any>} */
  const byKey = new Map(); // `${author}\0${d}` → newest event
  for (const ev of events) {
    if (!ev || ev.role !== "heartbeat") continue;
    const key = `${ev.pubkey}\0${ev.heartbeat?.d || ""}`;
    const prev = byKey.get(key);
    if (!prev || prev.created_at < ev.created_at) byKey.set(key, ev);
  }
  /** @type {Map<string, any>} */
  const byAuthor = new Map();
  for (const ev of byKey.values()) {
    const prev = byAuthor.get(ev.pubkey);
    if (!prev || prev.created_at < ev.created_at) byAuthor.set(ev.pubkey, ev);
  }
  return byAuthor;
}

/**
 * How recent a seat's announcement must be for that seat to be counted as existing.
 *
 * Kind-30340 is replaceable, so a relay hands back the last thing a seat said and nothing marks it
 * as over. Accumulating what the relay serves therefore builds a directory of seats that are gone.
 * Measured on relay.maxplayer.ai 2026-08-21: 27 announcements resolved to 9 seats inside this
 * window and 18 outside it, the excluded ones a median of ~11 days old and the oldest 16.6 days.
 *
 * 300s rather than `LIVE_WINDOW_S` below: the two do not disagree on that measurement, because the
 * age distribution was bimodal with ZERO announcements between 300s and 1h, so the cut lands in an
 * empty region and both windows selected the same 9 seats. The tighter one is the charter's and is
 * the one stated in the UI. Two freshness constants in one app is how a page later contradicts
 * itself, so this is named here rather than left implicit — see #857's follow-up.
 */
export const SEAT_FRESH_WINDOW_S = 300;

/**
 * The live seat directory: seats that exist right now, with the fossils counted rather than dropped
 * silently.
 *
 * Resolved at the seat ADDRESS — (pubkey, kind, d) with `d = SELLER_HEARTBEAT_D` — not by pubkey
 * alone. `resolveHeartbeats` above deliberately collapses to newest-per-author across every `d`,
 * which is right for "is this pubkey alive" and wrong here: the pre-rename `mobee-seller` address is
 * still live on the relay, so an author-wide collapse can answer with the retired address's event.
 * Measured 2026-08-21, 4 pubkeys published under both values and the retired one won 0 of 4 — so
 * that collapse is benign by coincidence of timestamps, not by construction.
 *
 * Returns the fossil count alongside the seats. A directory that silently drops rows cannot be
 * told from a relay that served none, so the denominator is part of the answer, not a debug aid.
 *
 * @param {any[]} events normalized events
 * @param {number} now unix-seconds
 */
export function resolveSeatDirectory(events, now = nowSeconds()) {
  /** @type {Map<string, any>} */
  const byPubkey = new Map();
  for (const ev of events) {
    if (!ev || ev.role !== "heartbeat") continue;
    if (ev.heartbeat?.d !== SELLER_HEARTBEAT_D) continue;
    const prev = byPubkey.get(ev.pubkey);
    if (!prev || prev.created_at < ev.created_at) byPubkey.set(ev.pubkey, ev);
  }

  const seats = [];
  let fossilsExcluded = 0;
  for (const ev of byPubkey.values()) {
    if (now - ev.created_at > SEAT_FRESH_WINDOW_S) {
      fossilsExcluded += 1;
      continue;
    }
    seats.push(ev);
  }
  seats.sort((a, b) => b.created_at - a.created_at);

  return {
    seats,
    fossilsExcluded,
    addressesSeen: byPubkey.size,
    freshWindowS: SEAT_FRESH_WINDOW_S,
    resolvedAt: now,
  };
}

/**
 * Verdicts for a seat's advertised harness roster, paired against what that seat has actually
 * delivered. Advertising is discovery metadata; the falsifier is the `harness` tag on the seat's
 * OWN kind-3403 results, which is a real falsifier because dispatch enforces the family
 * exact-or-nothing — a job runs on the harness it asked for or it does not run.
 *
 * ⚠ ABSENCE OF A RECEIPT IS NOT EVIDENCE AGAINST A CLAIM, and getting this wrong is how the panel
 * would end up calling honest seats liars. Because dispatch is exact-or-nothing, a seat that
 * advertises `codex` and has only claude receipts has not been contradicted — it means nobody asked
 * for codex. So a per-entry verdict is only ever `agreed` or `unverified`. There is no per-entry
 * `disagreed`.
 *
 * What DOES contradict a roster is a delivery OUTSIDE it — a receipt naming a harness the seat does
 * not advertise. That is a seat-level finding, `contradictedBy`, and it requires a NON-EMPTY roster:
 * a seat advertising nothing has made no claim, so nothing it delivers can contradict it. Measured
 * on the relay 2026-08-21, every seat that looked like an off-menu delivery advertised an empty
 * roster — 0 real contradictions across the relay, 7 seats with no roster at all.
 *
 * FOUR outcomes, deliberately, because the two ways of reaching "no conclusion" are different facts:
 *   agreed        the comparison ran and matched
 *   unverified    the comparison ran and found nothing — no job asked for that harness
 *   incomparable  the comparison could not run: the label names no family we can read, so the
 *                 preset-label and adapter-identity namespaces cannot be bridged for it
 *   (no receipts) `hasDeliveries` false — no evidence of any kind
 * Collapsing `incomparable` into a disagreement is how a well-behaved seat ends up wearing the
 * lying-seat costume, which defeats the one rule this panel exists to honour.
 *
 * ⚠ ONLY `agreed` HAS A POSITIVE CONTROL ON LIVE DATA. Measured 2026-08-21: 8 of 9 live seats agreed
 * by family and 1 by exact id, 0 contradictions, 0 incomparable. The unit tests are the only thing
 * proving the other branches fire at all; the live reading cannot show it.
 *
 * @param {any[]} events normalized events
 * @param {string} pubkey the seat
 * @param {string[]} advertised the seat's `agents` roster, verbatim
 */
export function verifyAdvertisedHarnesses(events, pubkey, advertised = []) {
  /** @type {Map<string, number>} delivered harness id → receipt count */
  const delivered = new Map();
  for (const ev of events) {
    if (!ev || ev.role !== "result" || ev.pubkey !== pubkey) continue;
    const id = ev.result?.usage?.harness_id;
    if (id) delivered.set(id, (delivered.get(id) || 0) + 1);
  }
  const ids = [...delivered.keys()];

  const matches = (label, id) => {
    // Exact first: an out-of-enum preset name IS the identity on both sides, so `deepseek-v4-flash`
    // advertised against `deepseek-v4-flash` delivered agrees on the string. A family comparison
    // alone would map both to null and read that as no information.
    if (id === label) return "id";
    const lf = harnessFamilyFromId(label);
    return lf && harnessFamilyFromId(id) === lf ? "family" : null;
  };

  const claims = advertised.map((label) => {
    for (const id of ids) {
      const how = matches(label, id);
      if (how) {
        return { advertised: label, verdict: "agreed", on: how, deliveredId: id, receipts: delivered.get(id) };
      }
    }
    // No match. WHY there is no match decides the verdict, and collapsing the two would let a
    // well-behaved seat wear the lying-seat costume. If the label names no family we can read, the
    // namespaces cannot be bridged at all and we have no basis for any conclusion. If it does name
    // one, the comparison genuinely ran and found nothing — which under exact-or-nothing dispatch
    // means no job asked for that harness, not that the seat cannot serve it.
    const comparable = harnessFamilyFromId(label) != null;
    return {
      advertised: label,
      verdict: comparable ? "unverified" : "incomparable",
      on: null,
      deliveredId: null,
      receipts: 0,
    };
  });

  // A delivery contradicts a STATED roster only when it is comparable to that roster and matches
  // nothing in it. Comparability is the same question change 1 settled: an id whose family we cannot
  // read carries no family, so `sh` — the argv0 basename fallback — could BE the advertised harness
  // launched through a shell. Flagging it as off-menu would assert knowledge we do not have. Same
  // for a delivered out-of-enum preset name, which is indistinguishable from that basename case.
  //
  // Measured 2026-08-21: asking the looser question "is this delivery declared?" produced 7 hits on
  // the relay and every one was a seat with an EMPTY roster — no claim, so nothing to contradict.
  // Real contradictions: 0.
  const contradictions = [];
  const incomparableDeliveries = [];
  if (advertised.length) {
    for (const id of ids) {
      if (advertised.some((label) => matches(label, id))) continue;
      const row = { deliveredId: id, receipts: delivered.get(id) };
      const idFamily = harnessFamilyFromId(id);
      const anyComparableLabel = advertised.some((label) => harnessFamilyFromId(label) != null);
      if (idFamily != null && anyComparableLabel) contradictions.push(row);
      else incomparableDeliveries.push(row);
    }
  }

  return {
    claims,
    contradictedBy: contradictions,
    incomparableDeliveries,
    deliveredIds: ids,
    hasDeliveries: ids.length > 0,
    advertisesNothing: advertised.length === 0,
  };
}

/**
 * Liveness state for a pubkey. Prefers a real heartbeat; falls back to the pubkey's last
 * marketplace activity when no heartbeat exists (so the signal is useful before sellers
 * emit kind-30340). Returns { state, lastSeen, source } — never throws, never fabricates.
 */
export function resolveLiveness(pubkey, heartbeats, lastActivityTs, now) {
  const hb = heartbeats.get(pubkey);
  let lastSeen = null;
  let source = "none";
  if (hb) {
    lastSeen = hb.created_at;
    source = "heartbeat";
  } else if (lastActivityTs != null) {
    lastSeen = lastActivityTs;
    source = "activity";
  }
  let state = "offline";
  if (lastSeen != null) {
    const age = now - lastSeen;
    if (age <= LIVE_WINDOW_S) state = "live";
    else if (age <= RECENT_WINDOW_S) state = "recent";
    else state = "stale";
  }
  return { state, lastSeen, source, heartbeatMessage: hb?.heartbeat?.message || null };
}

/**
 * Seller reputation for `pubkey`.
 * @param {any[]} events normalized events
 * @param {string} pubkey seller
 * @param {Map<string, any>} profiles
 * @param {number} now unix-seconds
 */
export function sellerMetrics(events, pubkey, profiles = new Map(), now = nowSeconds()) {
  const groups = groupEvents(events);
  const heartbeats = resolveHeartbeats(events);

  const engaged = new Set();
  const delivered = new Set();
  const refused = new Set();
  let satsEarned = 0;
  const deliveryTimes = [];
  let lastActivity = null;
  const myJobs = [];

  for (const g of groups.values()) {
    const claimsP = g.claims.filter((e) => e.pubkey === pubkey);
    const resultsP = g.results.filter((e) => e.pubkey === pubkey);
    const errorsP = g.feedbacks.filter((e) => e.pubkey === pubkey && e.feedback?.isError);
    if (!claimsP.length && !resultsP.length && !errorsP.length) continue;

    engaged.add(g.id);
    if (resultsP.length) delivered.add(g.id);
    if (errorsP.length) refused.add(g.id);
    for (const e of [...claimsP, ...resultsP, ...errorsP]) {
      if (lastActivity == null || e.created_at > lastActivity) lastActivity = e.created_at;
    }
    // Earnings: receipts on jobs this seller actually delivered.
    if (resultsP.length) {
      for (const r of g.receipts) satsEarned += r.receipt?.amount_sats || 0;
    }
    // Delivery time: this seller's first claim → first delivery.
    if (claimsP.length && resultsP.length) {
      const c = Math.min(...claimsP.map((e) => e.created_at));
      const d = Math.min(...resultsP.map((e) => e.created_at));
      if (d >= c) deliveryTimes.push(d - c);
    }
    myJobs.push(jobFromGroup(g, profiles, now));
  }

  myJobs.sort((a, b) => b.last_activity - a.last_activity);
  return {
    pubkey,
    role: "seller",
    profile: profiles.get(pubkey) || null,
    jobsCompleted: delivered.size,
    jobsEngaged: engaged.size,
    satsEarned,
    refusals: refused.size,
    refusalRate: engaged.size ? refused.size / engaged.size : null,
    meanDeliverySec: mean(deliveryTimes),
    deliverySamples: deliveryTimes.length,
    liveness: resolveLiveness(pubkey, heartbeats, lastActivity, now),
    relationships: relationshipPairs(groups, pubkey, "seller", profiles),
    recentJobs: myJobs.slice(0, 20),
  };
}

/**
 * Buyer reputation for `pubkey` — the claim-side view: how promptly they pay, how often
 * their jobs fail or expire unpaid.
 */
export function buyerMetrics(events, pubkey, profiles = new Map(), now = nowSeconds()) {
  const groups = groupEvents(events);

  let jobsPosted = 0;
  let satsPaid = 0;
  let refused = 0;
  let expiredUnpaid = 0;
  const payLatencies = [];
  let lastActivity = null;
  const myJobs = [];

  for (const g of groups.values()) {
    if (!g.offer || g.offer.pubkey !== pubkey) continue;
    jobsPosted += 1;
    const job = jobFromGroup(g, profiles, now);
    myJobs.push(job);

    if (g.offer.created_at != null && (lastActivity == null || g.offer.created_at > lastActivity)) {
      lastActivity = g.offer.created_at;
    }
    for (const r of g.receipts) satsPaid += r.receipt?.amount_sats || 0;
    if (job.status === JOB_STATUS.REFUSED) refused += 1;
    if (job.status === JOB_STATUS.EXPIRED) expiredUnpaid += 1;

    // Pay promptness: buyer's own award → the settlement receipt.
    const awards = g.awards.filter((e) => e.pubkey === pubkey);
    if (awards.length && g.receipts.length) {
      const acc = Math.min(...awards.map((e) => e.created_at));
      const rec = Math.min(...g.receipts.map((e) => e.created_at));
      if (rec >= acc) payLatencies.push(rec - acc);
    }
  }

  myJobs.sort((a, b) => b.last_activity - a.last_activity);
  return {
    pubkey,
    role: "buyer",
    profile: profiles.get(pubkey) || null,
    jobsPosted,
    satsPaid,
    refusals: refused,
    refusalRate: jobsPosted ? refused / jobsPosted : null,
    expiredUnpaid,
    meanPayLatencySec: mean(payLatencies),
    paySamples: payLatencies.length,
    lastActivity,
    relationships: relationshipPairs(groups, pubkey, "buyer", profiles),
    recentJobs: myJobs.slice(0, 20),
  };
}

/**
 * Repeat counterparties: the other party on 2+ shared jobs, newest activity first. From a
 * seller's profile these are buyers (link to buyer profiles); from a buyer's, sellers.
 * @param {Map<string, any>} groups
 * @param {string} pubkey the profile subject
 * @param {"seller"|"buyer"} role subject's role
 * @param {Map<string, any>} profiles
 */
export function relationshipPairs(groups, pubkey, role, profiles = new Map()) {
  /** @type {Map<string, {pubkey:string, trades:number, last:number}>} */
  const counts = new Map();
  const bump = (pk, at) => {
    if (!pk || pk === pubkey) return;
    let c = counts.get(pk);
    if (!c) { c = { pubkey: pk, trades: 0, last: 0 }; counts.set(pk, c); }
    c.trades += 1;
    if (at > c.last) c.last = at;
  };

  for (const g of groups.values()) {
    const at = g.offer?.created_at || 0;
    if (role === "seller") {
      // subject is a seller on this job → counterparty is the buyer (the offer author)
      const isSeller =
        g.claims.some((e) => e.pubkey === pubkey) || g.results.some((e) => e.pubkey === pubkey);
      if (isSeller && g.offer) bump(g.offer.pubkey, at);
    } else {
      // subject is the buyer → counterparties are the distinct sellers on the job
      if (!g.offer || g.offer.pubkey !== pubkey) continue;
      const sellers = new Set();
      for (const e of [...g.claims, ...g.results]) sellers.add(e.pubkey);
      for (const s of sellers) bump(s, at);
    }
  }

  return [...counts.values()]
    .filter((c) => c.trades >= 2)
    .sort((a, b) => b.trades - a.trades || b.last - a.last)
    .map((c) => ({
      pubkey: c.pubkey,
      trades: c.trades,
      profile: profiles.get(c.pubkey) || null,
      otherRole: role === "seller" ? "buyer" : "seller",
    }));
}

function mean(values) {
  if (!values.length) return null;
  return Math.round(values.reduce((a, b) => a + b, 0) / values.length);
}

function nowSeconds() {
  return Math.floor(Date.now() / 1000);
}
