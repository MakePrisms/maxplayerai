// Swarm events (docs/specs/swarm.md §4): validators and readers, a line-for-line port of
// crates/maxplayer-core/src/swarm.rs (parse_*_draft, latest_per_coordinate, Decisions,
// eligible_sellers). Same rule, same order, so the board and the CLI/daemon agree on every event;
// src/swarm/shared/vectors.json pins it (test/swarm-vectors.test.mjs).
//
// Inputs must already have passed signature and id verification (SwarmStore.add). Each parser
// returns the parsed object, or null for an event the Rust parser refuses.
import { SWARM, SWARM_JOB, SWARM_MEMBERSHIP, SWARM_DECISION } from '../../model/kinds.ts';
import { newer } from '../../challenge/shared/events.mjs';

export const MINUTES_MAX = 480, NAME_MAX = 128;
const NOTE_MAX = 4096, TAGS_MAX = 16384;
const JOB_STATUSES = ['scheduled', 'in_progress', 'completed', 'cancelled'];

export const validSlug = s => typeof s === 'string' && /^[a-z0-9-]{1,64}$/.test(s);
export const validHex = s => typeof s === 'string' && /^[0-9a-f]{64}$/.test(s);
const named = (e, k) => e.tags.filter(t => t[0] === k);
// Rust `tag`: exactly one tag named k (any arity), else undefined.
const one = (e, k) => { const t = named(e, k); return t.length === 1 ? t[0] : undefined; };
// Rust `value`: exactly one tag k, arity 2, its value.
export const value = (e, k) => { const t = one(e, k); return t && t.length === 2 ? t[1] : undefined; };
// Rust `optional`: absent ⇒ null, present ⇒ value (undefined when malformed).
const optional = (e, k) => named(e, k).length ? value(e, k) : null;
// Rust `number`: ASCII digits only. JS keeps safe integers (unix times and sats far below 2^53).
const number = s => typeof s === 'string' && /^\d+$/.test(s) && Number.isSafeInteger(+s) ? +s : null;
const base = (e, kind) => e.kind === kind && value(e, 't') === 'maxplayer' && value(e, 'v') === '1' && validHex(e.pubkey);

/** `30344:<buyer>:<swarm d>`, the `a` value naming a swarm. */
export const swarmAddress = (buyer, swarm) => `${SWARM}:${buyer}:${swarm}`;
/** { buyer, swarm } from a swarm `a` value, or null. */
export function parseSwarmAddress(a) {
  if (typeof a !== 'string') return null;
  const i = a.indexOf(':'), j = i < 0 ? -1 : a.indexOf(':', i + 1);
  if (j < 0) return null;
  const kind = a.slice(0, i), buyer = a.slice(i + 1, j), swarm = a.slice(j + 1);
  return kind === String(SWARM) && validHex(buyer) && validSlug(swarm) ? { buyer, swarm } : null;
}

export function parseSwarm(e) {
  if (!base(e, SWARM)) return null;
  const id = value(e, 'd'), name = value(e, 'name'), status = value(e, 'status'), tag = optional(e, 'tasks');
  const tasks = tag === null ? 'public' : tag; // malformed (undefined) is refused below
  if (!validSlug(id) || typeof name !== 'string' || !name.trim() || [...name].length > NAME_MAX) return null;
  if (!['open', 'closed'].includes(status) || !['public', 'private'].includes(tasks)) return null;
  return { buyer: e.pubkey, id, name, purpose: e.content, status, tasks };
}

export function parseSwarmJob(e) {
  if (!base(e, SWARM_JOB)) return null;
  const id = value(e, 'd'), a = parseSwarmAddress(value(e, 'a'));
  if (!validSlug(id) || !a || a.buyer !== e.pubkey) return null;
  const starts = number(value(e, 'starts')), ends = number(value(e, 'ends'));
  if (starts === null || ends === null || !(ends > starts)) return null;
  const b = one(e, 'budget');
  if (!b || b.length !== 3 || b[2] !== 'sat') return null;
  const budgetSats = number(b[1]), status = value(e, 'status'), title = value(e, 'title');
  if (budgetSats === null || !JOB_STATUSES.includes(status) || typeof title !== 'string' || !title.trim()) return null;
  const update = optional(e, 'update'), result = optional(e, 'result');
  if (update === undefined || result === undefined) return null;
  return { buyer: e.pubkey, swarm: a.swarm, id, title, goal: e.content, starts, ends, budgetSats, status, update, result };
}

// UTF-16 lengths, as the Rust side counts them (encode_utf16) and as JSON.stringify writes them.
const oversized = e => e.content.length > NOTE_MAX || JSON.stringify(e.tags).length > TAGS_MAX;
export function parseMembership(e) {
  if (!base(e, SWARM_MEMBERSHIP)) return null;
  const a = parseSwarmAddress(value(e, 'a'));
  if (!a || value(e, 'p') !== a.buyer || value(e, 'd') !== `${a.buyer}:${a.swarm}`) return null;
  const since = number(value(e, 'since')), minutes = number(value(e, 'minutes')), status = value(e, 'status');
  if (since === null || minutes === null || minutes < 1 || minutes > MINUTES_MAX || !['joined', 'left'].includes(status)) return null;
  return { buyer: a.buyer, swarm: a.swarm, seller: e.pubkey, since, minutes, status, note: oversized(e) ? '' : e.content };
}

export function parseDecision(e) {
  if (!base(e, SWARM_DECISION)) return null;
  const seller = value(e, 'p');
  if (!validHex(seller)) return null;
  const aTag = optional(e, 'a');
  if (aTag === undefined) return null;
  let swarm = null;
  if (aTag !== null) {
    const a = parseSwarmAddress(aTag);
    if (!a || a.buyer !== e.pubkey || value(e, 'd') !== `${seller}:${a.swarm}`) return null;
    swarm = a.swarm;
  } else if (value(e, 'd') !== seller) return null;
  const decision = value(e, 'decision'), membership = optional(e, 'e');
  if (!['accepted', 'denied'].includes(decision) || membership === undefined || (membership !== null && !validHex(membership))) return null;
  return { buyer: e.pubkey, seller, swarm, decision, membershipEventId: membership, reason: e.content };
}

/** The eligibility rule (§4): one of the two latest decisions accepted, neither denied. */
export const decisionAllows = (buyerWide, perSwarm) =>
  [buyerWide, perSwarm].includes('accepted') && ![buyerWide, perSwarm].includes('denied');

// Newest per (author, kind, d), created_at tie to the lower id, over events that parse: an invalid
// newer event never hides a valid older one. Returns [event, parsed] pairs.
function latest(events, parse) {
  const map = new Map();
  for (const e of events) {
    const p = parse(e);
    if (!p) continue;
    const key = `${e.pubkey}:${e.kind}:${value(e, 'd')}`, old = map.get(key);
    if (!old || newer(e, old[0])) map.set(key, [e, p]);
  }
  return [...map.values()];
}
export const latestSwarms = events => latest(events, parseSwarm);
export const latestSwarmJobs = events => latest(events, parseSwarmJob);
export const latestMemberships = events => latest(events, parseMembership);

/** One buyer's latest decisions, buyer-wide and per swarm; only that buyer's own events count. */
export class Decisions {
  constructor(buyer, events) {
    this.buyerWide = new Map(); this.perSwarm = new Map();
    for (const [, d] of latest([...events].filter(e => e.pubkey === buyer), parseDecision)) {
      if (d.swarm === null) this.buyerWide.set(d.seller, d.decision);
      else this.perSwarm.set(`${d.seller}:${d.swarm}`, d.decision);
    }
  }
  wide(seller) { return this.buyerWide.get(seller) ?? null; }
  forSwarm(seller, swarm) { return this.perSwarm.get(`${seller}:${swarm}`) ?? null; }
  allows(seller, swarm) { return decisionAllows(this.wide(seller), this.forSwarm(seller, swarm)); }
  denies(seller, swarm) { return [this.wide(seller), this.forSwarm(seller, swarm)].includes('denied'); }
}

/** Sellers a swarm job may give tasks to (Rust eligible_sellers), sorted by pubkey. */
export function eligibleSellers(job, events) {
  const all = [...events], decisions = new Decisions(job.buyer, all);
  return latestMemberships(all)
    .filter(([e, m]) => m.buyer === job.buyer && m.swarm === job.swarm && m.status === 'joined' &&
      m.since <= job.starts && e.created_at <= job.starts && decisions.allows(m.seller, m.swarm))
    .map(([, m]) => m.seller).sort();
}
