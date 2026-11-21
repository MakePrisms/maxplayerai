// The /swarm board's projection of our relay's events: every buyer's swarms (index) and one swarm
// with its swarm jobs, members, and minutes per member per day (docs/specs/swarm.md §4, §7.4).
//
// Minutes are per swarm per UTC day: a member's minutes used today in a swarm are the sum over that
// swarm's tasks to the member whose start falls in today's UTC day, across all the swarm's jobs.
// A task is an awarded offer of a swarm job (start = AWARD) or a later session turn on it (start =
// FOLLOW-UP). Each runs until the buyer's ACCEPT of it, else until now, capped at its deadline;
// seller-signed times never close the meter, and a result dated at or before the start counts the
// whole window. The daemon (PR 4) ports the same rules; src/swarm/shared/vectors.json pins both.
import { OFFER, CLAIM, RESULT, AWARD, ACCEPT, FOLLOW_UP, PROFILE, SWARM, SWARM_JOB, SWARM_MEMBERSHIP, SWARM_DECISION } from '../../model/kinds.ts';
import { verify, tag, one, hex, root, claimRef, mentions, newer, npubEncode, tradeNamespace } from '../../challenge/shared/events.mjs';
import { Decisions, latestMemberships, latestSwarmJobs, latestSwarms, parseDecision, parseMembership, parseSwarm, parseSwarmJob, swarmAddress, validHex, value } from './wire.mjs';

export const DAY_S = 86400;
export const dayStart = now => now - (((now % DAY_S) + DAY_S) % DAY_S);
const SWARM_PARSERS = { [SWARM]: parseSwarm, [SWARM_JOB]: parseSwarmJob, [SWARM_MEMBERSHIP]: parseMembership, [SWARM_DECISION]: parseDecision };
const ascending = (a, b) => a.created_at - b.created_at || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
const earliest = list => list.sort(ascending)[0];

/** `[swarm:<buyer>:<swarm job d>]\n`, the public task prefix (§4; the legacy `[challenge:…]` is not read). */
export const swarmTaskPrefix = (buyer, job) => `[swarm:${buyer}:${job}]\n`;
const TASK_PREFIX = /^\[swarm:([0-9a-f]{64}):([a-z0-9-]{1,64})\]\n/;

// Verified events the board holds. Swarm events are kept only when they parse, one per
// (author, kind, d) (newest wins, an invalid newer event never evicts a valid one); profiles one per
// author; trade events all.
export class SwarmStore {
  events = new Map(); latest = new Map();
  add(e) {
    if (this.events.has(e?.id) || !verify(e)) return false;
    const parse = SWARM_PARSERS[e.kind];
    let key = null;
    if (parse) { if (!parse(e)) return false; key = `${e.pubkey}:${e.kind}:${value(e, 'd')}`; }
    else if (e.kind === PROFILE) key = `${e.pubkey}:${PROFILE}`;
    if (key !== null) {
      const old = this.latest.get(key);
      if (old && !newer(e, old)) return false;
      if (old) this.events.delete(old.id);
      this.latest.set(key, e);
    }
    this.events.set(e.id, e);
    return true;
  }
}

function profileNames(events) {
  const names = new Map();
  for (const e of events) {
    if (e.kind !== PROFILE) continue;
    try {
      const p = JSON.parse(e.content);
      const name = typeof p?.display_name === 'string' && p.display_name.trim() ? p.display_name : typeof p?.name === 'string' ? p.name : '';
      if (name.trim()) names.set(e.pubkey, name.trim().slice(0, 64));
    } catch {}
  }
  return names;
}

const deadlineOf = e => {
  const d = e.tags.filter(t => t[0] === 'param' && t[1] === 'deadline');
  return d.length === 1 && typeof d[0][2] === 'string' && /^\d+$/.test(d[0][2]) && Number.isSafeInteger(+d[0][2]) ? +d[0][2] : null;
};
const replyTo = e => { const r = e.tags.filter(t => t[0] === 'e' && t[3] === 'reply'); return r.length === 1 && hex(r[0][1]) ? r[0][1] : null; };
const hasReply = e => e.tags.some(t => t[0] === 'e' && t[3] === 'reply');

/**
 * Every task of `jobs` (one buyer's parsed swarm jobs): one fact per awarded offer (turn 1) and per
 * session turn on it. Time-independent; taskAt() applies `now`.
 */
export function taskFacts(events, buyer, jobs) {
  const window = new Map(jobs.filter(j => j.buyer === buyer).map(j => [j.id, j]));
  const byRoot = new Map(), offers = [];
  for (const e of events) {
    if (e.kind === OFFER) { if (e.pubkey === buyer && tradeNamespace(e)) offers.push(e); continue; }
    if (![CLAIM, AWARD, RESULT, ACCEPT, FOLLOW_UP].includes(e.kind)) continue;
    const r = root(e);
    if (r) { if (!byRoot.has(r)) byRoot.set(r, []); byRoot.get(r).push(e); }
  }
  const facts = [];
  for (const offer of offers) {
    if (!one(offer, 'i') || !one(offer, 'p') || !hex(tag(offer, 'p'))) continue;
    const m = TASK_PREFIX.exec(tag(offer, 'i'));
    const job = m && m[1] === buyer ? window.get(m[2]) : undefined;
    if (!job || offer.created_at < job.starts || offer.created_at > job.ends) continue;
    const deadline = deadlineOf(offer);
    if (deadline === null) continue;
    const seller = tag(offer, 'p'), related = byRoot.get(offer.id) ?? [];
    const claims = new Set(related.filter(e => e.kind === CLAIM && e.pubkey === seller && tradeNamespace(e) && mentions(e, buyer) && tag(e, 'status') === 'processing').map(e => e.id));
    const award = earliest(related.filter(e => e.kind === AWARD && e.pubkey === buyer && tradeNamespace(e) && tag(e, 'status') === 'accepted' && mentions(e, buyer) && mentions(e, seller) && claims.has(claimRef(e))));
    if (!award) continue;
    const results = related.filter(e => e.kind === RESULT && e.pubkey === seller && tradeNamespace(e));
    const accepts = related.filter(e => e.kind === ACCEPT && e.pubkey === buyer && tradeNamespace(e) && claimRef(e) === claimRef(award) && mentions(e, buyer) && mentions(e, seller) && tag(e, 'status') === 'accepted');
    const noPayment = offer.tags.some(t => t[0] === 'param' && t[1] === 'payment' && t[2] === 'none');
    const amountTag = offer.tags.find(t => t[0] === 'amount');
    const amountSats = noPayment ? 0 : one(offer, 'amount') && amountTag?.[2] === 'sat' && /^\d+$/.test(amountTag[1]) && Number.isSafeInteger(+amountTag[1]) ? +amountTag[1] : null;
    const task = tag(offer, 'i').slice(m[0].length);
    // Turn 1: its result carries no `reply`; its ACCEPT names no reply, or names a turn-1 result.
    const firstResults = new Set(results.filter(r => !hasReply(r)).map(r => r.id));
    const result = earliest(results.filter(r => firstResults.has(r.id)));
    const accept = earliest(accepts.filter(a => a.created_at >= award.created_at && (!hasReply(a) || firstResults.has(replyTo(a)))));
    const common = { offer: offer.id, job: job.id, swarm: job.swarm, seller, task, amountSats };
    facts.push({ ...common, id: offer.id, turn: 1, start: award.created_at, deadline, acceptedAt: accept?.created_at ?? null,
      backdated: !!result && result.created_at <= award.created_at, result: result ? { id: result.id, content: result.content } : null });
    // Later turns: buyer FOLLOW-UP (status turn) to this seller, ended by the ACCEPT of its result.
    for (const f of related) {
      if (f.kind !== FOLLOW_UP || f.pubkey !== buyer || !f.tags.some(t => t[0] === 't' && t[1] === 'maxplayer') || tag(f, 'status') !== 'turn' || !one(f, 'p') || tag(f, 'p') !== seller || f.created_at < award.created_at) continue;
      const turn = one(f, 'turn') && /^\d+$/.test(tag(f, 'turn')) ? +tag(f, 'turn') : null, turnDeadline = deadlineOf(f);
      if (turn === null || turn < 2 || turnDeadline === null) continue;
      const answers = results.filter(r => replyTo(r) === f.id), ids = new Set(answers.map(r => r.id));
      const answer = earliest(answers), done = earliest(accepts.filter(a => a.created_at >= f.created_at && ids.has(replyTo(a))));
      facts.push({ ...common, id: f.id, turn, start: f.created_at, deadline: turnDeadline, acceptedAt: done?.created_at ?? null,
        backdated: !!answer && answer.created_at <= f.created_at, result: answer ? { id: answer.id, content: answer.content } : null });
    }
  }
  return facts;
}

/** A task at `now`: running, end and minutes used. */
export function taskAt(f, now) {
  const accepted = f.acceptedAt !== null, running = !accepted && now < f.deadline;
  const end = accepted ? f.acceptedAt : f.backdated ? f.deadline : now;
  const usedSeconds = Math.max(0, Math.min(end - f.start, f.deadline - f.start));
  return { ...f, end: running ? null : end, running, accepted, usedMinutes: usedSeconds / 60 };
}

/** Minutes each seller used in `swarm` (buyer, d) in the UTC day holding `now`, from task facts. */
export function minutesToday(facts, swarm, now) {
  const from = dayStart(now), out = new Map();
  for (const f of facts) if (f.swarm === swarm && f.start >= from && f.start < from + DAY_S) out.set(f.seller, (out.get(f.seller) ?? 0) + taskAt(f, now).usedMinutes);
  return out;
}

/** Index: every buyer's swarms (open only unless `all`), newest swarm event first. */
export function swarmIndex(events, now, { all = false } = {}) {
  const list = [...events], names = profileNames(list);
  const jobs = latestSwarmJobs(list).map(([, j]) => j), members = latestMemberships(list).map(([, m]) => m);
  const decisions = new Map();
  const decisionsOf = buyer => { if (!decisions.has(buyer)) decisions.set(buyer, new Decisions(buyer, list)); return decisions.get(buyer); };
  return latestSwarms(list).filter(([, s]) => all || s.status === 'open')
    .sort(([a], [b]) => b.created_at - a.created_at || (a.id < b.id ? -1 : 1))
    .map(([e, s]) => {
      const all = jobs.filter(j => j.buyer === s.buyer && j.swarm === s.id);
      const own = all.filter(j => ['scheduled', 'in_progress'].includes(j.status));
      const running = own.filter(j => j.status === 'in_progress' && j.starts <= now && now < j.ends).sort((a, b) => b.ends - a.ends)[0] ?? null;
      const next = own.filter(j => j.starts > now).sort((a, b) => a.starts - b.starts)[0] ?? null;
      const d = decisionsOf(s.buyer);
      return { ...s, address: swarmAddress(s.buyer, s.id), buyerNpub: npubEncode(s.buyer), buyerName: names.get(s.buyer) ?? '', createdAt: e.created_at,
        members: members.filter(m => m.buyer === s.buyer && m.swarm === s.id && m.status === 'joined' && d.allows(m.seller, m.swarm)).length,
        openJobs: own.length, jobsDone: all.filter(j => j.status === 'completed').length, running: running && { id: running.id, title: running.title, starts: running.starts, ends: running.ends },
        next: next && { id: next.id, title: next.title, starts: next.starts, ends: next.ends } };
    });
}

/** The swarm job a swarm page shows: the newest (by event time) not cancelled, else the newest. */
export const shownJob = jobPairs => shownJobPair(jobPairs)?.[1] ?? null;
function shownJobPair(jobPairs) {
  const sorted = [...jobPairs].sort(([a], [b]) => b.created_at - a.created_at || (a.id < b.id ? -1 : 1));
  return sorted.find(([, j]) => j.status !== 'cancelled') ?? sorted[0] ?? null;
}

// The swarm job's `update` (docs/specs/swarm.md §4, §7.5). The daemon publishes a buyer-signed JSON
// tally there every 180 s: {"members":{"<seller hex>":{"tasks_done":n,"minutes_today":m}}}, where
// tasks_done counts the member's collected tasks in this swarm job and minutes_today the minutes it
// used today (UTC) in the swarm. The old runner wrote a plain status line instead.
export const TALLY_MAX = 65536; // UTF-16 units; ~600 members' entries
const MINUTES_PER_DAY = 1440;
const plainObject = v => v !== null && typeof v === 'object' && !Array.isArray(v);
/**
 * Reads an `update` value. Returns { text } for a plain status line, { tally } (Map seller →
 * { tasksDone, minutesToday }) for a tally, or null when there is nothing to show: empty, JSON that
 * is malformed, oversized or not a tally. JSON is never returned as text. A tally entry that is not
 * a hex key with a non-negative integer task count and 0–1440 minutes is dropped on its own.
 */
export function parseUpdate(update) {
  if (typeof update !== 'string' || !update.trim()) return null;
  const t = update.trim();
  // `{…}` is a tally attempt: shown only when it is one, never as text. A whole-line JSON array is an
  // unknown shape (hidden too); anything else, `[note] …` included, is a plain status line.
  if (t[0] === '[') { try { JSON.parse(t); return null; } catch { return { text: update }; } }
  if (t[0] !== '{') return { text: update };
  if (update.length > TALLY_MAX) return null;
  let v;
  try { v = JSON.parse(t); } catch { return null; }
  if (!plainObject(v) || !Object.hasOwn(v, 'members') || !plainObject(v.members)) return null;
  const tally = new Map();
  for (const [seller, m] of Object.entries(v.members)) {
    if (!validHex(seller) || !plainObject(m)) continue;
    const done = m.tasks_done, minutes = m.minutes_today;
    if (!Number.isSafeInteger(done) || done < 0) continue;
    if (typeof minutes !== 'number' || !Number.isFinite(minutes) || minutes < 0 || minutes > MINUTES_PER_DAY) continue;
    tally.set(seller, { tasksDone: done, minutesToday: minutes });
  }
  return { tally };
}

const BUDGET_NOTE = 'Estimates from awarded offer prices; ACCEPT authorises payment, not proof of settlement. Private payments and fees are not visible.';
/**
 * One swarm's page: the swarm, its shown job (hero, goal, budget), its members with today's minutes
 * in this swarm, today's tasks (jobs, which drive the minutes clock) and its swarm jobs.
 */
export function swarmSnapshot(events, { buyer, swarm: slug }, now) {
  const list = [...events], names = profileNames(list);
  const swarmPair = latestSwarms(list).find(([, s]) => s.buyer === buyer && s.id === slug);
  const empty = { asOf: now, swarm: null, swarmJob: null, swarmJobs: [], sellers: [], jobs: [], updates: [], applicants: [], budget: null };
  if (!swarmPair) return empty;
  const s = swarmPair[1];
  const jobPairs = latestSwarmJobs(list).filter(([, j]) => j.buyer === buyer && j.swarm === slug);
  const jobs = jobPairs.map(([, j]) => j), shown = shownJobPair(jobPairs), c = shown?.[1] ?? null;
  const facts = taskFacts(list, buyer, jobs);
  const from = dayStart(now);
  // The tally comes only from the shown swarm job: an event this buyer signed for this swarm
  // (latestSwarmJobs keys on the author; parseSwarmJob requires the `a` buyer to be the author).
  // Its minutes are today's only when it was published today (UTC); an older tally keeps its task
  // count (per swarm job) and reports no minutes.
  const update = c ? parseUpdate(c.update) : null;
  const tally = update?.tally ?? null, tallyToday = !!shown && shown[0].created_at >= from && shown[0].created_at < from + DAY_S;
  const reportedFor = seller => {
    const r = tally?.get(seller);
    return r ? { tasksDone: r.tasksDone, minutesToday: tallyToday ? r.minutesToday : null } : null;
  };
  const today = facts.filter(f => f.start >= from && f.start < from + DAY_S).map(f => taskAt(f, now));
  const decisions = new Decisions(buyer, list);
  const memberships = latestMemberships(list).filter(([, m]) => m.buyer === buyer && m.swarm === slug);
  const sellers = [], applicants = [];
  for (const [e, m] of memberships) {
    const allowed = decisions.allows(m.seller, slug), denied = decisions.denies(m.seller, slug);
    if (denied) continue; // denied members are never listed publicly
    if (!allowed) {
      if (m.status === 'joined') applicants.push({ pubkey: m.seller, npub: npubEncode(m.seller), minutesOffered: m.minutes, since: m.since, joinedAt: e.created_at, note: m.note });
      continue;
    }
    const own = today.filter(t => t.seller === m.seller), running = own.filter(t => t.running);
    const used = own.reduce((n, t) => n + t.usedMinutes, 0);
    const eligible = !!c && m.status === 'joined' && m.since <= c.starts && e.created_at <= c.starts;
    const state = m.status === 'left' ? 'left' : running.length ? (running.every(t => t.result) ? 'awaiting acceptance' : 'working') : eligible && own.length ? 'done' : 'accepted';
    sellers.push({ pubkey: m.seller, npub: npubEncode(m.seller), name: names.get(m.seller) ?? '', since: m.since, membershipStatus: m.status,
      minutesOffered: m.minutes, minutesUsed: used, minutesRemaining: m.minutes - used, state, eligible, decision: 'accepted',
      currentJobStart: running.length ? Math.min(...running.map(t => t.start)) : null, note: m.note,
      // Buyer-reported (tally) figures, listed members only. The page shows them for private swarms.
      reported: reportedFor(m.seller) });
  }
  sellers.sort((a, b) => a.npub.localeCompare(b.npub));
  applicants.sort((a, b) => b.joinedAt - a.joinedAt || a.pubkey.localeCompare(b.pubkey));
  const shownTasks = c ? facts.filter(f => f.job === c.id).map(f => taskAt(f, now)) : [];
  return {
    asOf: now,
    swarm: { ...s, address: swarmAddress(buyer, slug), buyerNpub: npubEncode(buyer), buyerName: names.get(buyer) ?? '' },
    swarmJob: c && { id: c.id, buyer, buyerNpub: npubEncode(buyer), title: c.title, goal: c.goal, starts: c.starts, ends: c.ends, status: c.status, result: c.result ?? '' },
    swarmJobs: jobPairs.map(([e, j]) => ({ id: j.id, title: j.title, starts: j.starts, ends: j.ends, status: j.status, budgetSats: j.budgetSats, createdAt: e.created_at, tasks: facts.filter(f => f.job === j.id && f.turn === 1).length }))
      .sort((a, b) => b.starts - a.starts || b.createdAt - a.createdAt),
    sellers, applicants,
    jobs: today.map(t => ({ id: t.id, seller: t.seller, start: t.start, deadline: t.deadline, end: t.end, running: t.running, usedMinutes: t.usedMinutes, job: t.job, turn: t.turn })),
    updates: update?.text ? [update.text] : [],
    budget: c && { totalSats: c.budgetSats, committedSats: shownTasks.filter(t => !t.accepted).reduce((n, t) => n + (t.amountSats ?? 0), 0),
      paidSats: shownTasks.filter(t => t.accepted).reduce((n, t) => n + (t.amountSats ?? 0), 0), estimated: true,
      unknownAmounts: shownTasks.filter(t => t.amountSats === null).length, note: BUDGET_NOTE },
  };
}
