/**
 * Per-participant track records, and the time window they are measured over.
 *
 * A window filters the EVENTS, not the finished trades: a trade is a real
 * thing that happened at several moments, so "the last 24 hours" means the
 * activity inside that period, and a trade straddling the boundary contributes
 * only the stages that fall inside it.
 */
import { buildTrades } from "./trades.js";
import { parseEvent, type ParsedEvent, type RawEvent, type AdvertisementTag } from "../model/events.js";
import { ACCEPT, AWARD, CLAIM, FEEDBACK, HEARTBEAT, OFFER, PROFILE, RECEIPT, RESULT } from "../model/kinds.js";

/**
 * The two states an awarded, undelivered job can be in. Derived presentation
 * state, not relay truth: the relay has no event that says "this job stalled"
 * (#682). Until it does, this is the site's own reading of an absence.
 */
export const JOB_WORKING = "working";
export const JOB_OVERDUE = "overdue";
export type JobState = typeof JOB_WORKING | typeof JOB_OVERDUE;

/**
 * How long after an offer's own deadline a job may still be called working.
 * Exists for clock skew and propagation, not for patience.
 */
export const STALLED_GRACE_SECONDS = 300;

export interface Window { key: string; label: string; seconds: number | null }

/** Selectable periods, longest label first so the UI can render them in order. */
export const WINDOWS: readonly Window[] = Object.freeze([
  { key: "24h", label: "24 hours", seconds: 86400 },
  { key: "week", label: "Week", seconds: 604800 },
  { key: "all", label: "All time", seconds: null },
]);

export const DEFAULT_WINDOW = "week";

/** A seller with no heartbeat this recently is not claimed to be online. */
export const LIVE_WITHIN_SECONDS = 300;

export function windowSeconds(key: string): number | null {
  const w = WINDOWS.find((x) => x.key === key);
  return w ? w.seconds : null;
}

/** Events inside the window. A null window means everything. */
export function withinWindow<T extends { created_at: number }>(events: T[], windowKey: string, now: number): T[] {
  const span = windowSeconds(windowKey);
  if (span == null) return events;
  const floor = now - span;
  return events.filter((e) => e && e.created_at >= floor);
}

const median = (xs: number[]): number | null => {
  if (!xs.length) return null;
  const a = [...xs].sort((p, q) => p - q);
  const mid = Math.floor(a.length / 2);
  return a.length % 2 ? a[mid]! : Math.round((a[mid - 1]! + a[mid]!) / 2);
};

export interface Profile {
  name: string | null;
  about: string | null;
  metadata: Record<string, unknown>;
  at: number;
}

/**
 * Display names published as kind-0 metadata, keyed by pubkey.
 *
 * A kind-0 ENRICHES a participant; it never creates one. What earns a row
 * stays evidence of doing the thing: a claim, delivery or receipt for a
 * seller (plus the heartbeat that says a seat is open), an offer or award
 * for a buyer. Measured when kind-0 first arrived: 13 of 24 seller rows were
 * profile-owners with no market activity at all.
 */
function profileMetadata(parsed: ParsedEvent[]): Map<string, Profile> {
  const profiles = new Map<string, Profile>();
  for (const p of parsed) {
    if (p.kind !== PROFILE) continue;
    const prev = profiles.get(p.pubkey);
    // Newest wins. The cache already resolves kind-0 to one per author, but
    // these boards are pure over whatever array they are handed.
    if (!prev || p.created_at >= prev.at) {
      profiles.set(p.pubkey, { name: p.name ?? null, about: p.about ?? null, metadata: p.profile || {}, at: p.created_at });
    }
  }
  return profiles;
}

/** Public profile names. Hex remains the fallback, never the label beside a known name. */
export function participantNames(events: RawEvent[]): Map<string, string> {
  const names = new Map<string, string>();
  for (const [pubkey, p] of participantProfiles(events)) if (p.name) names.set(pubkey, p.name);
  return names;
}

/** Latest complete public profile per participant, independent of the window. */
export function participantProfiles(events: RawEvent[]): Map<string, Profile> {
  return profileMetadata(events.map(parseEvent).filter((e): e is ParsedEvent => e != null));
}

/**
 * Latest buyer-side job action per racer, independent of the board window.
 * A racer is active when it AUTHORS a job lifecycle action, not merely because
 * its profile exists or a runner acts on its job.
 */
export function racerLastActivity(events: RawEvent[]): Map<string, number> {
  const buyerStages = new Set(["offer", "award", "accept", "receipt"]);
  const latest = new Map<string, number>();
  for (const e of events.map(parseEvent)) {
    if (!e || !e.stage || !buyerStages.has(e.stage)) continue;
    latest.set(e.pubkey, Math.max(latest.get(e.pubkey) || 0, e.created_at));
  }
  return latest;
}

interface NamedRow {
  pubkey: string;
  name: string | null;
  about: string | null;
  profile: Record<string, unknown> | null;
  profileAt: number;
}

/**
 * Name the rows that exist. Applied AFTER every row-creating pass, so it
 * cannot depend on whether a profile arrived before or after the claim or
 * advert that earned the row — relay order is not ours to choose.
 */
function applyProfiles(rows: Map<string, NamedRow>, parsed: ParsedEvent[]) {
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

export interface Job {
  offerId: string;
  awardId: string;
  claimId: string | null;
  buyer: string;
  seller: string;
  startedAt: number;
  deadline: number | null;
  state: JobState;
}

/**
 * Awarded jobs that have not been delivered, joined to the exact winning
 * claim. A trade may have several claimants; only the seller named by AWARD is
 * working. Input order is irrelevant because relay pages do not promise it.
 *
 * `state` is `working` while the offer's deadline (plus grace) is ahead of
 * `now`, `overdue` once behind. Delivery is still the only thing that ENDS a
 * job — an overdue job keeps its award; overdue only means "stop showing this
 * as current execution."
 *
 * `now` is required, deliberately: before #681 this function had no clock, so
 * an award whose seller published nothing stayed "working" forever. A
 * defaulted clock would let any caller silently reproduce that bug.
 */
export function inProgressJobs(events: RawEvent[], now: number, graceSeconds = STALLED_GRACE_SECONDS): Job[] {
  if (!Number.isFinite(now)) {
    throw new TypeError("inProgressJobs(events, now): now is required — see #681");
  }
  const parsed = events.map(parseEvent).filter((e): e is ParsedEvent => e != null);
  const claims = new Map(parsed
    .filter((e) => e.kind === CLAIM && e.seller)
    .map((e) => [e.id, e.seller as string]));

  interface PendingJob {
    offerId: string;
    buyer: string | null;
    award: ParsedEvent | null;
    deadline: number | null;
    terminals: ParsedEvent[];
  }
  const jobs = new Map<string, PendingJob>();
  const get = (offerId: string): PendingJob => {
    let job = jobs.get(offerId);
    if (!job) {
      job = { offerId, buyer: null, award: null, deadline: null, terminals: [] };
      jobs.set(offerId, job);
    }
    return job;
  };

  for (const e of parsed) {
    if (!e.offerId) continue;
    const job = get(e.offerId);
    if (e.buyer) job.buyer ||= e.buyer;
    if (e.kind === OFFER) {
      // The deadline is the offer's own — the only public statement of when
      // the work was due. We may never see the offer at all.
      if (e.deadline) job.deadline = e.deadline;
    } else if (e.kind === AWARD) {
      if (!job.award || e.created_at < job.award.created_at ||
          (e.created_at === job.award.created_at && e.id < job.award.id)) job.award = e;
    } else if ([RESULT, ACCEPT, RECEIPT, FEEDBACK].includes(e.kind)) {
      job.terminals.push(e);
    }
  }

  const active: Job[] = [];
  for (const job of jobs.values()) {
    if (!job.award) continue;
    const award = job.award;
    const seller = award.awardedSeller || (award.claimId ? claims.get(award.claimId) : null) || null;
    const buyer = award.buyer || job.buyer;
    if (!buyer || !seller) continue;
    // Feedback ends the attempt only when its protocol class says so: a
    // `status=progress` note — explicitly non-terminal per §7.2, and REQUIRED
    // from a working seller — must not clear the lamp before any result.
    const finished = job.terminals.some((e) =>
      e.kind === ACCEPT || e.kind === RECEIPT ||
      (e.kind === RESULT && e.seller === seller) ||
      (e.kind === FEEDBACK && e.seller === seller && e.created_at >= award.created_at && e.terminal));
    if (finished) continue;
    // No deadline means we never saw the offer, not that the job is on time.
    // An absence is not evidence of lateness — it stays working and visible.
    const overdue = job.deadline != null && now > job.deadline + graceSeconds;
    active.push({
      offerId: job.offerId,
      awardId: award.id,
      claimId: award.claimId ?? null,
      buyer,
      seller,
      startedAt: award.created_at,
      deadline: job.deadline,
      state: overdue ? JOB_OVERDUE : JOB_WORKING,
    });
  }
  return active.sort((a, b) => b.startedAt - a.startedAt || a.offerId.localeCompare(b.offerId));
}

export interface BuyerRow extends NamedRow {
  posted: number;
  awarded: number;
  receipted: number;
  satsPaid: number;
  prices: number[];
  lastSeen: number;
  unpaidDeliveries: number;
  inProgressJobs: Job[];
  workingJobs: Job[];
  medianPrice: number | null;
}

/**
 * Buyers, ranked by sats paid.
 *
 * `satsPaid` counts published receipts only — the same floor that applies
 * everywhere, since a buyer can settle without announcing it.
 */
export function buyerBoard(events: RawEvent[], now: number, activeEvents: RawEvent[] = events): BuyerRow[] {
  const trades = buildTrades(events);
  const parsed = events.map(parseEvent).filter((e): e is ParsedEvent => e != null);
  const rows = new Map<string, BuyerRow>();
  const get = (pk: string): BuyerRow => {
    let r = rows.get(pk);
    if (!r) {
      r = { pubkey: pk, posted: 0, awarded: 0, receipted: 0, satsPaid: 0,
            prices: [], lastSeen: 0, unpaidDeliveries: 0, inProgressJobs: [], workingJobs: [],
            // Self-published, from kind-0. A claim, like the seller's.
            name: null, about: null, profile: null, profileAt: 0, medianPrice: null };
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
      // Delivered with no receipt published. Not proof of non-payment —
      // surfaced as an open question, never as a debt.
      r.unpaidDeliveries += 1;
    }
    if (t.offerAmount != null) r.prices.push(t.offerAmount);
  }

  // `inProgressJobs` is every awarded-undelivered job, overdue included — the
  // detail view keeps showing the award. `workingJobs` is the subset that is
  // still current execution. Split here, once.
  for (const job of inProgressJobs(activeEvents, now)) {
    const row = get(job.buyer);
    row.inProgressJobs.push(job);
    if (job.state === JOB_WORKING) row.workingJobs.push(job);
  }

  // Posting or awarding work earns the row; the profile only names it.
  applyProfiles(rows, parsed);

  return [...rows.values()]
    .map((r) => ({ ...r, medianPrice: median(r.prices) }))
    // The pubkey tiebreak mirrors the seller board: without it, tied racers
    // sort in event-arrival order, which differs between today's board and the
    // board-as-of-yesterday — a phantom "climb" from a shuffle, not a move.
    .sort((a, b) => b.satsPaid - a.satsPaid || b.posted - a.posted ||
      a.pubkey.localeCompare(b.pubkey));
}

export interface SellerRow extends NamedRow {
  claimed: number;
  delivered: number;
  receipted: number;
  satsEarned: number;
  released: number;
  deliverTimes: number[];
  lastSeen: number;
  online: boolean;
  inProgressJobs: Job[];
  workingJobs: Job[];
  harnessCounts: Record<string, number>;
  askSats: number | null;
  accepting: string | null;
  queueDepth: number | null;
  acceptedMints: string[];
  advertisedAgents: string[];
  advertisementTags: AdvertisementTag[];
  advertisementContent: Record<string, unknown> | null;
  advertisedAt: number;
  medianDeliverSeconds: number | null;
  completionRate: number | null;
  harness: string | null;
  harnesses: { name: string; deliveries: number }[];
}

/**
 * Sellers, ranked by track record: work delivered, then how much of what they
 * took they finished, then sats. Leads with the sturdiest number — a delivery
 * is a protocol step the buyer needs in order to pay; a receipt is optional.
 *
 * Being online does not lift a seller up the board: it is a claim about this
 * minute, not evidence. `online` comes from a heartbeat inside
 * LIVE_WITHIN_SECONDS; `lastSeen` is the last time they did anything at all.
 * "Traded recently" is not "available now".
 */
export function sellerBoard(events: RawEvent[], now: number, activeEvents: RawEvent[] = events): SellerRow[] {
  const trades = buildTrades(events);
  const parsed = events.map(parseEvent).filter((e): e is ParsedEvent => e != null);

  const rows = new Map<string, SellerRow>();
  const get = (pk: string): SellerRow => {
    let r = rows.get(pk);
    if (!r) {
      r = { pubkey: pk, claimed: 0, delivered: 0, receipted: 0, satsEarned: 0,
            released: 0, deliverTimes: [], lastSeen: 0, online: false, inProgressJobs: [], workingJobs: [],
            // Which runtime actually did the work, counted per delivery — a
            // seller may move between harnesses, so a tally, not a label.
            harnessCounts: {},
            // Self-advertised, from the newest protocol-v1 heartbeat. Claims,
            // not measurements. Unknown tags stay intact for the detail view.
            name: null, about: null, profile: null, profileAt: 0,
            askSats: null, accepting: null, queueDepth: null,
            acceptedMints: [], advertisedAgents: [], advertisementTags: [],
            advertisementContent: null, advertisedAt: 0,
            medianDeliverSeconds: null, completionRate: null, harness: null, harnesses: [] };
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
        r.askSats = p.rateSats ?? null;
        r.accepting = p.accepting ?? null;
        r.queueDepth = p.queueDepth ?? null;
        r.acceptedMints = p.acceptedMints ?? [];
        r.advertisedAgents = p.agents ?? [];
        r.advertisementTags = p.advertisementTags ?? [];
        r.advertisementContent = p.advertisementContent ?? null;
      }
    } else if (p.kind === RESULT && p.harness) {
      const r = get(p.pubkey);
      r.harnessCounts[p.harness] = (r.harnessCounts[p.harness] || 0) + 1;
    }
  }

  for (const job of inProgressJobs(activeEvents, now)) {
    const row = get(job.seller);
    row.inProgressJobs.push(job);
    if (job.state === JOB_WORKING) row.workingJobs.push(job);
  }

  // kind-0 is the single publisher of the seat name (§6.1 / #275). Named last,
  // once every row that earned a place on this board exists.
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
        harness: ranked.length ? ranked[0]![0] : null,
        harnesses: ranked.map(([name, n]) => ({ name, deliveries: n })),
      };
    })
    // A seller with no claims has no rate to compare — it sorts below any
    // measured rate rather than tying with a zero.
    .sort((a, b) =>
      b.delivered - a.delivered ||
      (b.completionRate ?? -1) - (a.completionRate ?? -1) ||
      b.satsEarned - a.satsEarned ||
      b.lastSeen - a.lastSeen ||
      // Last resort, so the board does not reshuffle as new events arrive.
      a.pubkey.localeCompare(b.pubkey));
}

export interface ParticipantDetail {
  pubkey: string;
  buyer: BuyerRow | null;
  seller: SellerRow | null;
  trades: ReturnType<typeof buildTrades>;
  activity: ParsedEvent[];
}

/** Everything known about one participant, for a detail view. */
export function participantDetail(events: RawEvent[], pubkey: string, now: number, activeEvents: RawEvent[] = events): ParticipantDetail {
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
 * a job they bought, sold, or were directly offered. Deliberately broader than
 * trades: profile and heartbeat activity remains visible, and a receipt is
 * retained even when its author is neither displayed role.
 */
export function participantActivity(events: RawEvent[], pubkey: string): ParsedEvent[] {
  const parsed = events.map(parseEvent).filter((e): e is ParsedEvent => e != null);
  const roots = new Set<string>();
  for (const e of parsed) {
    if (e.pubkey === pubkey || e.buyer === pubkey || e.seller === pubkey ||
        e.awardedSeller === pubkey || e.acceptedSeller === pubkey ||
        e.receiptSeller === pubkey || e.targetSeller === pubkey) {
      if (e.offerId) roots.add(e.offerId);
    }
  }
  return parsed
    .filter((e) => e.pubkey === pubkey || (e.offerId && roots.has(e.offerId)))
    // Newest first; same-second ties fall back to REVERSE lifecycle order so a
    // receipt reads above the offer it settles, never below it.
    .sort((a, b) => b.created_at - a.created_at || stageRank(b) - stageRank(a) || b.id.localeCompare(a.id));
}

/**
 * Lifecycle rank for breaking timestamp ties. Agents fire fast: an offer and
 * its claim routinely land in the SAME second, and an id tiebreak then orders
 * them at random — a claim rendered before the offer it answers. Stage order
 * is the truth the timestamps are too coarse to carry.
 */
const STAGE_RANK: Record<string, number> = {
  offer: 0, claim: 1, award: 2, result: 3, feedback: 4, accept: 5, receipt: 6,
};
const stageRank = (e: ParsedEvent): number => (e.stage != null ? STAGE_RANK[e.stage] ?? 50 : 50);

/** One complete public job history, oldest stage first. */
export function relatedActivity(events: RawEvent[], offerId: string | null | undefined): ParsedEvent[] {
  if (!offerId) return [];
  return events.map(parseEvent)
    .filter((e): e is ParsedEvent => e != null && e.offerId === offerId)
    .sort((a, b) => a.created_at - b.created_at || stageRank(a) - stageRank(b) || a.id.localeCompare(b.id));
}
