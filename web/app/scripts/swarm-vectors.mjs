// Writes src/swarm/shared/vectors.json: signed events in, eligibility and minutes per swarm out
// (docs/specs/swarm.md §7.4). The board's suite runs them (test/swarm-vectors.test.mjs); the
// daemon's Rust port (PR 4) runs the same file. Deterministic: fixed keys, zero aux randomness.
// Run: node --import tsx scripts/swarm-vectors.mjs
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { schnorr } from '@noble/curves/secp256k1';
import { bytesToHex } from '@noble/hashes/utils';
import { OFFER, CLAIM, RESULT, AWARD, ACCEPT, FOLLOW_UP, SWARM, SWARM_JOB, SWARM_MEMBERSHIP, SWARM_DECISION } from '../src/model/kinds.ts';
import { eventId } from '../src/challenge/shared/events.mjs';

const key = n => n.toString(16).padStart(64, '0');
const KEYS = { buyer: key(1), other_buyer: key(2), s1: key(11), s2: key(12), s3: key(13), s4: key(14), s5: key(15), s6: key(16), s7: key(17), s8: key(18) };
const PUB = Object.fromEntries(Object.entries(KEYS).map(([n, k]) => [n, bytesToHex(schnorr.getPublicKey(k))]));
const NS = [['t', 'maxplayer'], ['v', '1']];
function sign(who, kind, created_at, tags, content = '') {
  const e = { pubkey: PUB[who], created_at, kind, tags, content };
  e.id = eventId(e); e.sig = bytesToHex(schnorr.sign(e.id, KEYS[who], new Uint8Array(32)));
  return e;
}
const addr = (buyer, d) => `${SWARM}:${PUB[buyer]}:${d}`;
const swarm = (buyer, d, at) => sign(buyer, SWARM, at, [...NS, ['d', d], ['name', d], ['status', 'open'], ['tasks', 'public']], `The ${d} swarm`);
const job = (buyer, sw, d, starts, ends, at = starts - 60) => sign(buyer, SWARM_JOB, at, [...NS, ['d', d], ['a', addr(buyer, sw)], ['title', d], ['starts', String(starts)], ['ends', String(ends)], ['status', 'in_progress'], ['budget', '100000', 'sat']], `Goal of ${d}`);
const member = (seller, buyer, sw, at, { minutes = 60, status = 'joined', since = at } = {}) =>
  sign(seller, SWARM_MEMBERSHIP, at, [...NS, ['d', `${PUB[buyer]}:${sw}`], ['a', addr(buyer, sw)], ['p', PUB[buyer]], ['since', String(since)], ['minutes', String(minutes)], ['status', status]]);
const decide = (buyer, seller, decision, at, sw = null) => sign(buyer, SWARM_DECISION, at,
  [...NS, ['d', sw ? `${PUB[seller]}:${sw}` : PUB[seller]], ...(sw ? [['a', addr(buyer, sw)]] : []), ['p', PUB[seller]], ['decision', decision]]);

// One task: offer → claim → award (at `award`) → optional result and accept. Returns its events and ids.
function task(seller, jobD, { offerAt, award, deadline, result = null, accept = null, prefix = `[swarm:${PUB.buyer}:${jobD}]\n` }) {
  const offer = sign('buyer', OFFER, offerAt, [...NS, ['i', `${prefix}Work on ${jobD}`], ['p', PUB[seller]], ['param', 'deadline', String(deadline)], ['amount', '100', 'sat']]);
  const claim = sign(seller, CLAIM, (award ?? offerAt + 35) - 5, [...NS, ['e', offer.id, '', 'root'], ['p', PUB.buyer], ['status', 'processing']]);
  const out = [offer, claim];
  if (award !== null) out.push(sign('buyer', AWARD, award, [...NS, ['e', offer.id, '', 'root'], ['e', claim.id], ['p', PUB.buyer], ['p', PUB[seller]], ['status', 'accepted']]));
  let res = null;
  if (result !== null) { res = sign(seller, RESULT, result, [...NS, ['e', offer.id, '', 'root'], ['p', PUB.buyer]], 'answer'); out.push(res); }
  if (accept !== null) out.push(sign('buyer', ACCEPT, accept, [...NS, ['e', offer.id, '', 'root'], ['e', claim.id], ['p', PUB.buyer], ['p', PUB[seller]], ['status', 'accepted']]));
  return { events: out, offer, claim, result: res };
}
// Session turn n on a task: FOLLOW-UP → turn RESULT → ACCEPT bound to that result.
function turn(t, seller, n, { at, deadline, result = null, accept = null }) {
  const f = sign('buyer', FOLLOW_UP, at, [...NS, ['e', t.offer.id, '', 'root'], ['e', t.result.id, '', 'reply'], ['status', 'turn'], ['turn', String(n)], ['param', 'deadline', String(deadline)], ['i', 'Next step'], ['p', PUB[seller]]]);
  const out = [f];
  if (result !== null) {
    const r = sign(seller, RESULT, result, [...NS, ['e', t.offer.id, '', 'root'], ['e', f.id, '', 'reply'], ['p', PUB.buyer]], 'turn answer');
    out.push(r);
    if (accept !== null) out.push(sign('buyer', ACCEPT, accept, [...NS, ['e', t.offer.id, '', 'root'], ['e', t.claim.id], ['e', r.id, '', 'reply'], ['p', PUB.buyer], ['p', PUB[seller]], ['status', 'accepted']]));
  }
  return out;
}

const DAY = 86400, D = 1_800_000_000 - (1_800_000_000 % DAY); // a UTC midnight
const h = n => D + n * 3600;

// Eligibility: buyer-wide and per-swarm decisions (§4), membership timing and status.
const T = h(12);
const eligibility = [
  swarm('buyer', 'math', h(0)), swarm('buyer', 'physics', h(0)),
  job('buyer', 'math', 'math-1', T, T + 3600), job('buyer', 'physics', 'physics-1', T, T + 3600),
  ...['s1', 's2', 's3', 's4', 's5', 's7', 's8'].flatMap(s => [member(s, 'buyer', 'math', h(1)), member(s, 'buyer', 'physics', h(1))]),
  member('s6', 'buyer', 'math', T + 60), // joined after the job started: next job only
  // Published after the start with an earlier `since`: still next job only (created_at counts too).
  member('s6', 'buyer', 'physics', T + 60, { since: h(1) }),
  member('s7', 'buyer', 'math', h(2), { status: 'left' }), member('s7', 'buyer', 'physics', h(2), { status: 'left' }),
  // A newer membership that does not parse (minutes 999) never hides the valid older one.
  sign('s1', SWARM_MEMBERSHIP, h(3), [...NS, ['d', `${PUB.buyer}:math`], ['a', addr('buyer', 'math')], ['p', PUB.buyer], ['since', String(h(3))], ['minutes', '999'], ['status', 'joined']]),
  decide('buyer', 's1', 'accepted', h(4)),
  decide('buyer', 's2', 'accepted', h(4)), decide('buyer', 's2', 'denied', h(4), 'math'),
  decide('buyer', 's3', 'accepted', h(4), 'math'),
  decide('buyer', 's4', 'denied', h(4)), decide('buyer', 's4', 'accepted', h(4), 'math'),
  decide('buyer', 's6', 'accepted', h(4)), decide('buyer', 's7', 'accepted', h(4)),
  decide('other_buyer', 's8', 'accepted', h(4)), // another buyer's decision admits no one here
  // A later accept replaces an earlier deny (latest decision per d wins).
  decide('buyer', 's5', 'denied', h(4)), decide('buyer', 's5', 'accepted', h(5)),
];

// Minutes per swarm per UTC day: shared across the swarm's jobs, by award (or follow-up) day.
const now = h(12);
const j1 = job('buyer', 'math', 'math-a', h(8), h(20)), j2 = job('buyer', 'math', 'math-b', h(10), h(22));
const j0 = job('buyer', 'math', 'math-night', h(-4), h(6)), p1 = job('buyer', 'physics', 'physics-a', h(8), h(20));
const s1b = task('s1', 'math-b', { offerAt: h(11) - 60, award: h(11), deadline: h(13), result: h(11) + 600, accept: h(11) + 1200 });
const minutes = [
  swarm('buyer', 'math', h(-24)), swarm('buyer', 'physics', h(-24)), j0, j1, j2, p1,
  ...['s1', 's2', 's3', 's4'].flatMap(s => [member(s, 'buyer', 'math', h(-20)), decide('buyer', s, 'accepted', h(-20))]),
  member('s1', 'buyer', 'physics', h(-20)),
  ...task('s1', 'math-a', { offerAt: h(9) - 60, award: h(9), deadline: h(11), result: h(9) + 900, accept: h(9) + 1800 }).events, // 30
  ...s1b.events, // 20
  ...turn(s1b, 's1', 2, { at: h(11) + 2400, deadline: h(12) + 2400, result: h(11) + 3000, accept: h(11) + 3300 }), // 15
  ...task('s1', 'physics-a', { offerAt: h(9) - 60, award: h(9), deadline: h(11), result: h(9) + 1800, accept: h(10) }).events, // physics 60
  ...task('s2', 'math-night', { offerAt: h(-1) - 60, award: h(-1), deadline: h(1), result: h(0), accept: h(0) + 1800 }).events, // yesterday
  ...task('s2', 'math-a', { offerAt: h(11) - 60, award: h(11), deadline: h(13) }).events, // running: 60 at noon
  ...task('s3', 'math-a', { offerAt: h(10) - 60, award: h(10), deadline: h(12) + 1800, result: h(10) - 60 }).events, // result dated before the award: the whole window, 150
  ...task('s3', 'math-a', { offerAt: h(10) - 30, award: null, deadline: h(11) }).events.slice(0, 2), // never awarded
  ...task('s4', 'math-a', { offerAt: h(9), award: h(9) + 60, deadline: h(10), accept: h(9) + 1260, prefix: `[challenge:${PUB.buyer}:math-a]\n` }).events, // legacy prefix
  ...task('s4', 'math-a', { offerAt: h(7), award: h(9), deadline: h(10), accept: h(9) + 1200 }).events, // offer before the job's window
];

const out = {
  description: 'Swarm eligibility and minutes (docs/specs/swarm.md §4, §7.4). Events are signed; eligible lists, per swarm job "<buyer>:<job d>", the sellers eligible_sellers returns (sorted hex); minutesToday lists, per swarm "<buyer>:<swarm d>", the minutes each seller with tasks used in the UTC day holding `now`. Generated by web/app/scripts/swarm-vectors.mjs.',
  keys: PUB,
  cases: [
    { name: 'decisions and membership timing', now: T + 600, events: eligibility,
      expect: { eligible: { [`${PUB.buyer}:math-1`]: [PUB.s1, PUB.s3, PUB.s5].sort(), [`${PUB.buyer}:physics-1`]: [PUB.s1, PUB.s2, PUB.s5].sort() } } },
    { name: 'minutes per swarm per UTC day', now, events: minutes,
      expect: { minutesToday: { [`${PUB.buyer}:math`]: { [PUB.s1]: 65, [PUB.s2]: 60, [PUB.s3]: 150 }, [`${PUB.buyer}:physics`]: { [PUB.s1]: 60 } } } },
  ],
};
const target = join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'swarm', 'shared', 'vectors.json');
writeFileSync(target, JSON.stringify(out, null, 1) + '\n');
console.log(`wrote ${target}`);
