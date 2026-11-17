// /swarm board (docs/specs/swarm.md §12 PR 3): our relay only, every buyer's swarms, one swarm per
// /swarm/<naddr>; the trade window covers today and the shown swarm job.
import assert from 'node:assert/strict';
import test from 'node:test';
import { schnorr } from '@noble/curves/secp256k1';
import { eventId } from '../src/challenge/shared/events.mjs';
import { SWARM, SWARM_JOB, SWARM_MEMBERSHIP, SWARM_DECISION, OFFER, CLAIM, AWARD, FOLLOW_UP, PROFILE } from '../src/model/kinds.ts';
import { SwarmStore, swarmIndex, swarmSnapshot, dayStart } from '../src/swarm/shared/model.mjs';
import { SwarmReader, TRADE_WINDOW_MARGIN_S } from '../src/swarm/shared/relay.mjs';
import { swarmAddress } from '../src/swarm/shared/wire.mjs';
import { naddrEncode } from '../src/swarm/shared/naddr.mjs';
import { route, swarmPath, jobLine, renderIndex, firstLine } from '../src/swarm/index.ts';
import { parseSnapshot } from '../src/swarm/live.ts';

const key = n => n.toString(16).padStart(64, '0');
const pub = k => Buffer.from(schnorr.getPublicKey(k)).toString('hex');
const A = key(1), B = key(2), S1 = key(3), S2 = key(4);
const NS = [['t', 'maxplayer'], ['v', '1']];
function sign(k, kind, created_at, tags, content = '') {
  const e = { pubkey: pub(k), kind, created_at, tags, content };
  e.id = eventId(e); e.sig = Buffer.from(schnorr.sign(e.id, k)).toString('hex'); return e;
}
const D = 1_800_000_000 - (1_800_000_000 % 86400), NOW = D + 12 * 3600;
const swarm = (k, d, name, at, status = 'open') => sign(k, SWARM, at, [...NS, ['d', d], ['name', name], ['status', status]], `# ${name}\nPurpose of ${d}`);
const job = (k, sw, d, starts, ends, status = 'in_progress') => sign(k, SWARM_JOB, starts - 60, [...NS, ['d', d], ['a', swarmAddress(pub(k), sw)], ['title', `Job ${d}`], ['starts', String(starts)], ['ends', String(ends)], ['status', status], ['budget', '5000', 'sat']], 'Goal');
const member = (s, k, sw, at) => sign(s, SWARM_MEMBERSHIP, at, [...NS, ['d', `${pub(k)}:${sw}`], ['a', swarmAddress(pub(k), sw)], ['p', pub(k)], ['since', String(at)], ['minutes', '30'], ['status', 'joined']]);
const accept = (k, s, at) => sign(k, SWARM_DECISION, at, [...NS, ['d', pub(s)], ['p', pub(s)], ['decision', 'accepted']]);

const events = [
  swarm(A, 'math', 'Math', D - 100), swarm(B, 'art', 'Art', D - 50), swarm(B, 'old', 'Old', D - 200, 'closed'),
  job(A, 'math', 'today', D + 10 * 3600, D + 14 * 3600), job(B, 'art', 'later', D + 20 * 3600, D + 22 * 3600, 'scheduled'),
  member(S1, A, 'math', D - 90), member(S2, A, 'math', D - 90), accept(A, S1, D - 80),
  member(S1, B, 'art', D - 40), // undecided in B's swarms
  sign(B, PROFILE, D - 10, [], JSON.stringify({ name: 'Bea' })),
];

test('index: every buyer\'s open swarms, newest first, with members, job and buyer name', () => {
  const store = new SwarmStore(); for (const e of events) store.add(e);
  const rows = swarmIndex(store.events.values(), NOW);
  assert.deepEqual(rows.map(r => [r.name, r.members, r.running?.id ?? null, r.next?.id ?? null, r.buyerName]), [
    ['Art', 0, null, 'later', 'Bea'],
    ['Math', 1, 'today', null, ''],
  ]);
  assert.deepEqual(swarmIndex(events, NOW, { all: true }).map(r => r.name), ['Art', 'Math', 'Old'], '--all adds closed swarms');
  const when = s => `t${s}`;
  assert.equal(jobLine(rows[1], when), 'Running now: Job today');
  assert.equal(jobLine(rows[0], when), `Next job t${D + 20 * 3600}: Job later`);
  assert.equal(jobLine({ ...rows[0], next: null }, when), 'No job scheduled');
});

test('index rendering: remote text only as text; links are built from keys and slugs', () => {
  class El { textContent = ''; children = []; attrs = {}; className = ''; href = ''; constructor(tag) { this.tag = tag; }
    setAttribute(k, v) { this.attrs[k] = v; } querySelector() { return null; } append(...c) { this.children.push(...c); } replaceChildren(...c) { this.children = c; } }
  const ids = {};
  const doc = { getElementById: id => (ids[id] ??= new El(id)), createElement: t => new El(t), createTextNode: t => Object.assign(new El('#text'), { textContent: t }), createDocumentFragment: () => new El('#f') };
  const rows = swarmIndex(events, NOW).map(r => ({ ...r, name: '<img src=x onerror=alert(1)>' }));
  renderIndex(doc, rows, 'live', s => `t${s}`, 'wss://relay.maxplayer.ai');
  const items = ids['swarm-list'].children[0].children;
  assert.equal(items.length, 2);
  assert.equal(items[0].children[0].textContent, '<img src=x onerror=alert(1)>');
  assert.equal(items[0].children[0].href, swarmPath(pub(B), 'art', 'wss://relay.maxplayer.ai'));
  assert.equal(ids['swarm-count'].textContent, '2 open swarms');
  renderIndex(doc, [], 'live', s => `t${s}`, 'wss://relay.maxplayer.ai');
  assert.equal(ids['swarm-count'].textContent, 'No open swarms yet. Check back soon.');
});

test('routes: /swarm is the index; /swarm/<naddr of a swarm> one swarm; anything else is not found', () => {
  assert.deepEqual(route('/swarm'), { view: 'index' });
  assert.deepEqual(route('/swarm/'), { view: 'index' });
  const naddr = naddrEncode({ kind: SWARM, pubkey: pub(A), identifier: 'math', relays: ['wss://relay.maxplayer.ai'] });
  assert.deepEqual(route(`/swarm/${naddr}`), { view: 'swarm', buyer: pub(A), swarm: 'math', naddr });
  assert.equal(swarmPath(pub(A), 'math', 'wss://relay.maxplayer.ai'), `/swarm/${naddr}`, 'the board links the CLI address');
  assert.equal(route(`/swarm/${naddrEncode({ kind: SWARM_JOB, pubkey: pub(A), identifier: 'today' })}`).view, 'missing', 'a swarm job address is not a swarm');
  assert.equal(route(`/swarm/${naddrEncode({ kind: SWARM, pubkey: pub(A), identifier: 'Bad Slug' })}`).view, 'missing');
  assert.equal(route('/swarm/npub1xyz').view, 'missing');
});

test('one swarm: members with today\'s minutes, applicants, jobs, and a snapshot the page accepts', () => {
  const now = D + 11 * 3600;
  const offer = sign(A, OFFER, D + 10 * 3600 + 60, [...NS, ['i', `[swarm:${pub(A)}:today]\nDo it`], ['p', pub(S1)], ['param', 'deadline', String(D + 12 * 3600)], ['amount', '100', 'sat']]);
  const claim = sign(S1, CLAIM, offer.created_at + 5, [...NS, ['e', offer.id, '', 'root'], ['p', pub(A)], ['status', 'processing']]);
  const award = sign(A, AWARD, offer.created_at + 10, [...NS, ['e', offer.id, '', 'root'], ['e', claim.id], ['p', pub(A)], ['p', pub(S1)], ['status', 'accepted']]);
  const s = swarmSnapshot([...events, offer, claim, award], { buyer: pub(A), swarm: 'math' }, now);
  assert.equal(s.swarm.name, 'Math');
  assert.equal(s.swarmJob.id, 'today');
  assert.deepEqual(s.sellers.map(x => [x.pubkey, x.state, Math.round(x.minutesUsed)]), [[pub(S1), 'working', 59]]);
  assert.deepEqual(s.applicants.map(x => x.pubkey), [pub(S2)], 'joined, no decision: waiting for approval');
  assert.deepEqual(s.swarmJobs.map(j => [j.id, j.tasks]), [['today', 1]]);
  assert.equal(s.budget.committedSats, 100);
  assert.equal(parseSnapshot(s), s);
  assert.throws(() => parseSnapshot({ ...s, swarm: { ...s.swarm, buyerNpub: 'npub1wrong' } }));
  assert.deepEqual(swarmSnapshot(events, { buyer: pub(A), swarm: 'nope' }, now).swarm, null);
});

class Socket extends EventTarget {
  static all = []; sent = []; readyState = 1;
  constructor(url) { super(); this.url = url; Socket.all.push(this); }
  send(d) { this.sent.push(JSON.parse(d)); }
  message(m) { this.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(m) })); }
  close() { if (this.readyState === 3) return; this.readyState = 3; this.dispatchEvent(new Event('close')); }
}
const until = async (ok, ms = 2000) => { const end = Date.now() + ms; while (!ok()) { if (Date.now() > end) throw Error('timed out'); await new Promise(r => setTimeout(r, 5)); } };
const reqs = ws => ws.sent.filter(m => m[0] === 'REQ');

test('reader, index: one connection to our relay, every buyer\'s swarm kinds, no author filter', async t => {
  Socket.all = [];
  const reader = new SwarmReader({ url: 'ws://ours', store: new SwarmStore(), WebSocketImpl: Socket, queueMs: 1, pollMs: 600000 });
  reader.start(); t.after(() => reader.stop());
  assert.equal(Socket.all.length, 1); assert.equal(Socket.all[0].url, 'ws://ours');
  Socket.all[0].dispatchEvent(new Event('open'));
  await until(() => reqs(Socket.all[0]).length >= 4);
  const filters = reqs(Socket.all[0]).map(m => m[2]);
  assert.deepEqual([...new Set(filters.map(f => f.kinds[0]))].sort(), [SWARM_JOB, SWARM_MEMBERSHIP, SWARM_DECISION, SWARM].sort(), 'history page and live REQ per kind');
  assert.ok(filters.every(f => !f.authors && !f['#p']), 'no buyer filter (§5.2)');
});

test('reader, one swarm: #a-scoped swarm reads, then a trade window from the earlier of today and the shown job', async t => {
  Socket.all = [];
  const buyer = pub(A), store = new SwarmStore();
  const shown = job(A, 'math', 'started-yesterday', D - 3 * 3600, D + 3 * 3600);
  const reader = new SwarmReader({ url: 'ws://ours', store, swarm: { buyer, swarm: 'math' }, now: () => NOW, WebSocketImpl: Socket, queueMs: 1, pollMs: 600000 });
  reader.start(); t.after(() => reader.stop());
  const [sw, tr] = Socket.all;
  assert.ok(sw && tr && sw.url === 'ws://ours' && tr.url === 'ws://ours');
  for (const ws of [sw, tr]) ws.dispatchEvent(new Event('open'));
  await until(() => reqs(sw).length >= 4);
  const f = Object.fromEntries(reqs(sw).map(m => [m[2].kinds[0], m[2]]));
  const a = swarmAddress(buyer, 'math');
  assert.deepEqual(f[SWARM_JOB]['#a'], [a]); assert.deepEqual(f[SWARM_JOB].authors, [buyer]);
  assert.deepEqual(f[SWARM_MEMBERSHIP]['#a'], [a]); assert.equal(f[SWARM_MEMBERSHIP].authors, undefined);
  assert.deepEqual(f[SWARM_DECISION].authors, [buyer]);
  await new Promise(r => setTimeout(r, 20));
  assert.equal(reqs(tr).length, 0, 'no trade page before the window is chosen');
  const jobReq = reqs(sw).find(m => m[2].kinds[0] === SWARM_JOB)[1];
  sw.message(['EVENT', jobReq, shown]);
  for (const m of reqs(sw)) sw.message(['EOSE', m[1]]);
  assert.equal(reader.trade.windowSince, Math.min(dayStart(NOW), D - 3 * 3600) - TRADE_WINDOW_MARGIN_S);
  await until(() => reqs(tr).some(m => m[2].kinds[0] === OFFER));
  const offers = reqs(tr).find(m => m[2].kinds[0] === OFFER)[2];
  assert.deepEqual(offers.authors, [buyer]); assert.equal(offers.since, reader.trade.windowSince);
  await until(() => reqs(tr).some(m => m[2].kinds[0] === FOLLOW_UP)); // session turns are read
});

test('purpose line: the first line of prose, not a repeated "# Name" heading', () => {
  assert.equal(firstLine('# Math\nHard open problems.'), 'Hard open problems.');
  assert.equal(firstLine('# Only a heading'), 'Only a heading');
  assert.equal(firstLine('\n\nPlain'), 'Plain');
  assert.equal(firstLine('x'.repeat(300)).length, 280);
});
