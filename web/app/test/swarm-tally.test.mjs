// The buyer-signed tally in the swarm job's `update` (docs/specs/swarm.md §4, §7.5): never shown as
// raw text; a private swarm's rows show each listed member's buyer-reported tasks and minutes; a
// public swarm keeps the relay-computed minutes. Driven through the real projection (SwarmStore +
// swarmSnapshot), the page's parser and its row renderer.
import assert from 'node:assert/strict';
import test from 'node:test';
import { schnorr } from '@noble/curves/secp256k1';
import { eventId } from '../src/challenge/shared/events.mjs';
import { SWARM, SWARM_JOB, SWARM_MEMBERSHIP, SWARM_DECISION, OFFER, CLAIM, AWARD } from '../src/model/kinds.ts';
// Namespace imports: on a base without parseUpdate the tests fail at assertions, not at load.
import * as model from '../src/swarm/shared/model.mjs';
import { swarmAddress } from '../src/swarm/shared/wire.mjs';
import * as live from '../src/swarm/live.ts';
const { SwarmStore, swarmSnapshot } = model, { parseSnapshot, renderSellers, sellerLine } = live;
const parseUpdate = v => { assert.equal(typeof model.parseUpdate, 'function', 'model.parseUpdate exists'); return model.parseUpdate(v); };
const TALLY_MAX = model.TALLY_MAX ?? 65536;
import { demoSnapshot } from '../src/swarm/demo.ts';

const key = n => n.toString(16).padStart(64, '0');
const pub = k => Buffer.from(schnorr.getPublicKey(k)).toString('hex');
const BUYER = key(1), OTHER = key(2), S1 = key(3), S2 = key(4), APPLICANT = key(5), STRANGER = key(6);
const buyer = pub(BUYER);
const NS = [['t', 'maxplayer'], ['v', '1']];
function sign(k, kind, created_at, tags, content = '') {
  const e = { pubkey: pub(k), kind, created_at, tags, content };
  e.id = eventId(e); e.sig = Buffer.from(schnorr.sign(e.id, k)).toString('hex'); return e;
}
const D = 1_800_000_000 - (1_800_000_000 % 86400), NOW = D + 12 * 3600;
const a = swarmAddress(buyer, 'math');
const swarmEvent = tasks => sign(BUYER, SWARM, D - 1000, [...NS, ['d', 'math'], ['name', 'Math'], ['status', 'open'], ...(tasks ? [['tasks', tasks]] : [])], 'Hard problems');
// The shown swarm job, republished (same d) with `update` at `at`.
const job = (update, at = NOW - 60, k = BUYER) => sign(k, SWARM_JOB, at, [...NS, ['d', 'today'], ['a', a], ['title', 'Today'], ['starts', String(D + 10 * 3600)],
  ['ends', String(D + 14 * 3600)], ['status', 'in_progress'], ['budget', '5000', 'sat'], ...(update === null ? [] : [['update', update]])], 'Goal');
const member = (s, status = 'joined') => sign(s, SWARM_MEMBERSHIP, D - 900, [...NS, ['d', `${buyer}:math`], ['a', a], ['p', buyer], ['since', String(D - 900)], ['minutes', '60'], ['status', status]]);
const accept = s => sign(BUYER, SWARM_DECISION, D - 800, [...NS, ['d', pub(s)], ['p', pub(s)], ['decision', 'accepted']]);
const base = tasks => [swarmEvent(tasks), member(S1), member(S2), member(APPLICANT), accept(S1), accept(S2)];
// A public task for S1: awarded at 10:01, deadline 12:00, still running at NOW (so 119 relay minutes).
function publicTask() {
  const offer = sign(BUYER, OFFER, D + 10 * 3600 + 60, [...NS, ['i', `[swarm:${buyer}:today]\nDo it`], ['p', pub(S1)], ['param', 'deadline', String(D + 12 * 3600 + 600)], ['amount', '100', 'sat']]);
  const claim = sign(S1, CLAIM, offer.created_at + 1, [...NS, ['e', offer.id, '', 'root'], ['p', buyer], ['status', 'processing']]);
  const award = sign(BUYER, AWARD, offer.created_at + 2, [...NS, ['e', offer.id, '', 'root'], ['e', claim.id], ['p', buyer], ['p', pub(S1)], ['status', 'accepted']]);
  return [offer, claim, award];
}
const tally = members => JSON.stringify({ members });
const entry = (done, minutes) => ({ tasks_done: done, minutes_today: minutes });
const TALLY = tally({ [pub(S1)]: entry(3, 42.5), [pub(S2)]: entry(1, 7), [pub(APPLICANT)]: entry(9, 99), [pub(STRANGER)]: entry(9, 99) });
function project(events, now = NOW) {
  const store = new SwarmStore(); for (const e of events) store.add(e);
  return parseSnapshot(swarmSnapshot(store.events.values(), { buyer, swarm: 'math' }, now));
}
const by = (s, k) => s.sellers.find(x => x.pubkey === pub(k));

// Minimal DOM double: remote text only reaches textContent.
class El { textContent = ''; className = ''; children = []; attrs = {}; constructor(tag) { this.tag = tag; }
  setAttribute(k, v) { this.attrs[k] = v; } append(...c) { this.children.push(...c); } replaceChildren(...c) { this.children = c; } }
function render(s, now = NOW) {
  const ids = {};
  const doc = { getElementById: id => (ids[id] ??= new El(id)), createElement: t => new El(t), createDocumentFragment: () => new El('#f') };
  renderSellers(doc, s, now);
  const rows = ids['swarm-sellers'].children[0]?.children ?? [];
  // Bob (2026-11-21): no npub line under a member; its name links to its profile (data-pk is its key).
  return { summary: ids['seller-summary'].textContent, rows: rows.map(r => r.children.map(c => c.textContent)), keys: rows.map(r => r.children[0]?.attrs['data-pk']) };
}
const allText = r => [r.summary, ...r.rows.flat()].join('\n');

test('parseUpdate: a status line stays text; a tally parses; JSON is never text', () => {
  assert.deepEqual(parseUpdate('3 results recorded; 1 jobs running'), { text: '3 results recorded; 1 jobs running' });
  assert.deepEqual(parseUpdate('[note] solved part 2'), { text: '[note] solved part 2' }, 'bracketed text that is not JSON stays text');
  assert.equal(parseUpdate(''), null);
  assert.equal(parseUpdate('   '), null);
  assert.equal(parseUpdate(undefined), null);
  assert.equal(parseUpdate(null), null);
  const t = parseUpdate(TALLY).tally;
  assert.deepEqual(t.get(pub(S1)), { tasksDone: 3, minutesToday: 42.5 });
  assert.equal(t.size, 4, 'parseUpdate keeps every well-formed entry; membership is checked by the snapshot');
  // Malformed, unknown shape or oversized JSON: nothing to show, never the raw text.
  for (const bad of ['{"members":', '{', '{"members":[]}', '{"members":null}', '{"foo":1}', '{}', '[1,2]', '["members"]', '{"members":{}} trailing',
    tally({ x: entry(1, 1) }).padEnd(TALLY_MAX + 1, ' ')]) {
    const r = parseUpdate(bad);
    assert.ok(r === null || (r.tally && r.text === undefined), `hidden: ${bad.slice(0, 40)}`);
    assert.ok(!r?.text, `never text: ${bad.slice(0, 40)}`);
  }
  assert.equal(parseUpdate(`{"members":{"${pub(S1)}":{"tasks_done":1,"minutes_today":1}}}`.padEnd(TALLY_MAX + 1, ' ')), null, 'oversized tally ignored');
  assert.equal(parseUpdate('{"members":{}}').tally.size, 0, 'an empty tally is a tally');
});

test('parseUpdate: bad entries are dropped one by one, the rest kept', () => {
  const good = pub(S1);
  const t = parseUpdate(tally({
    [good]: entry(2, 10),
    [good.toUpperCase().slice(0, 64).replace(/^./, 'A')]: entry(1, 1), // not lowercase hex
    abc: entry(1, 1),
    [pub(S2)]: entry(-1, 1),
    [pub(APPLICANT)]: entry(1.5, 1),
    [pub(STRANGER)]: entry(1, 1441),
    [key(7)]: entry(1, -0.5),
    [key(8)]: entry('1', 1),
    [key(9)]: entry(1, '5'),
    [key(10)]: entry(1, null),
    [key(11)]: [1, 2],
    [key(12)]: entry(2 ** 53, 1),
  })).tally;
  assert.deepEqual([...t.entries()], [[good, { tasksDone: 2, minutesToday: 10 }]]);
});

test('public swarm: tally is not shown as text and relay minutes stay the source of truth', () => {
  const s = project([...base(), job(TALLY), ...publicTask()]);
  assert.equal(s.swarm.tasks, 'public');
  assert.deepEqual(s.updates, [], 'the raw tally is not an update line');
  assert.equal(Math.round(by(s, S1).minutesUsed), 119, 'relay-computed minutes');
  assert.deepEqual(by(s, S1).reported, { tasksDone: 3, minutesToday: 42.5 }, 'carried in the snapshot');
  const r = render(s);
  const relayLine = `${Math.floor(by(s, S1).minutesUsed)} / 60 min`;
  assert.notEqual(relayLine, '42 / 60 min');
  assert.ok(r.rows.find(x => x[1] === relayLine), 'S1 shows the relay minutes, not the tally\'s 42');
  assert.ok(r.rows.find(x => x[1] === '0 / 60 min'), 'S2 shows relay 0, not the tally\'s 7');
  assert.ok(!allText(r).includes('reported by the buyer'), 'no tally line on a public swarm');
  assert.ok(!allText(r).includes('{'), 'no JSON on the page');
  assert.match(r.summary, /working now/);
  // An old runner's plain status line is still shown.
  assert.deepEqual(project([...base(), job('2 results recorded; 1 jobs running')]).updates, ['2 results recorded; 1 jobs running']);
  assert.deepEqual(project([...base(), job('{"members":')]).updates, [], 'malformed JSON: no update line');
});

test('private swarm: rows show buyer-reported minutes for listed members only, with no line under the name', () => {
  const s = project([...base('private'), job(TALLY)]);
  assert.equal(s.swarm.tasks, 'private');
  assert.deepEqual(s.updates, []);
  assert.deepEqual(s.jobs, [], 'no public tasks');
  assert.deepEqual(by(s, S1).reported, { tasksDone: 3, minutesToday: 42.5 });
  assert.deepEqual(by(s, S2).reported, { tasksDone: 1, minutesToday: 7 });
  assert.deepEqual(s.applicants.map(x => x.pubkey), [pub(APPLICANT)]);
  assert.ok(!('reported' in s.applicants[0]), 'an applicant in the tally gets nothing');
  assert.equal(s.sellers.some(x => x.pubkey === pub(STRANGER)), false, 'a non-member in the tally is not listed');
  const r = render(s);
  const row = k => r.rows[r.keys.indexOf(pub(k))];
  assert.equal(row(S1)[1], '42 / 60 min');
  assert.equal(row(S1).length, 3, 'name, minutes, state: no task-count line under the name');
  assert.equal(row(S2)[1], '7 / 60 min');
  assert.equal(row(S2).length, 3);
  assert.ok(!/reported by the buyer|in this job|No report/.test(allText(r)), 'no reported-tasks text anywhere');
  assert.equal(r.rows.length, 3, 'two members + one applicant');
  assert.ok(!allText(r).includes('99'), 'applicant/stranger tally values never shown');
  assert.equal(r.summary, '2 agents · 1 waiting for approval', 'no "working now" count without public tasks');
  // Minutes tick only with a new tally, not with the page clock.
  assert.equal(sellerLine(s, by(s, S1), NOW + 600), '42 / 60 min');
});

test('private swarm: no tally yet, a stale tally, and a member with no entry', () => {
  const none = project([...base('private'), job(null)]);
  assert.equal(by(none, S1).reported, null);
  assert.equal(sellerLine(none, by(none, S1), NOW), '– / 60 min');
  assert.ok(render(none).rows.every(x => x.length === 3), 'no no-report-yet line under the name');
  const partial = project([...base('private'), job(tally({ [pub(S1)]: entry(1, 5) }))]);
  assert.equal(sellerLine(partial, by(partial, S2), NOW), '– / 60 min');
  // A tally published yesterday (UTC): its task count is the swarm job's, its minutes are not today's.
  const old = project([...base('private'), job(TALLY, D - 60)]);
  assert.deepEqual(by(old, S1).reported, { tasksDone: 3, minutesToday: null });
  assert.equal(sellerLine(old, by(old, S1), NOW), '– / 60 min');
  assert.equal(by(old, S1).reported.tasksDone, 3);
  // A plain text update on a private swarm is a status line; rows wait for a tally.
  const text = project([...base('private'), job('Starting up')]);
  assert.deepEqual(text.updates, ['Starting up']);
  assert.equal(by(text, S1).reported, null);
});

test('private swarm: only the buyer-signed swarm job is read', () => {
  // Another key signs a swarm job naming this swarm with a bigger tally: refused (a buyer ≠ author).
  const forged = job(tally({ [pub(S1)]: entry(50, 60) }), NOW, OTHER);
  const s = project([...base('private'), job(tally({ [pub(S1)]: entry(1, 5) }), NOW - 300), forged]);
  assert.deepEqual(by(s, S1).reported, { tasksDone: 1, minutesToday: 5 });
  // A tampered copy of the buyer's event (bad signature) is dropped by the store.
  const real = job(tally({ [pub(S1)]: entry(2, 6) }), NOW - 10);
  const tampered = { ...real, tags: real.tags.map(t => t[0] === 'update' ? ['update', tally({ [pub(S1)]: entry(40, 60) })] : t) };
  const t = project([...base('private'), job(tally({ [pub(S1)]: entry(1, 5) }), NOW - 300), tampered]);
  assert.deepEqual(by(t, S1).reported, { tasksDone: 1, minutesToday: 5 });
  // The newest buyer-signed version wins.
  assert.deepEqual(by(project([...base('private'), job(tally({ [pub(S1)]: entry(1, 5) }), NOW - 300), real]), S1).reported, { tasksDone: 2, minutesToday: 6 });
});

test('private swarm: a leaver with reported work is listed as Left; without, not listed', () => {
  const events = [swarmEvent('private'), member(S1), member(S2, 'left'), accept(S1), accept(S2)];
  const worked = project([...events, job(tally({ [pub(S1)]: entry(0, 0), [pub(S2)]: entry(1, 12) }))]);
  assert.deepEqual(render(worked).rows.map(x => x[2]).sort(), ['Left', 'Ready']);
  const idle = project([...events, job(tally({ [pub(S1)]: entry(0, 0), [pub(S2)]: entry(0, 0) }))]);
  assert.deepEqual(render(idle).rows.map(x => x[2]), ['Ready']);
});

test('the snapshot parser refuses malformed reported figures', () => {
  const s = project([...base('private'), job(TALLY)]);
  for (const reported of [{ tasksDone: -1, minutesToday: 1 }, { tasksDone: 1.5, minutesToday: 1 }, { tasksDone: 1, minutesToday: -1 }, { tasksDone: 1, minutesToday: '1' }, 'x'])
    assert.throws(() => parseSnapshot({ ...s, sellers: [{ ...s.sellers[0], reported }] }), /Invalid seller/);
  assert.ok(parseSnapshot({ ...s, sellers: [{ ...s.sellers[0], reported: { tasksDone: 0, minutesToday: null } }] }));
});

test('demo: ?demo=private renders the tally rows through the same parser, without a task-count line', () => {
  const s = parseSnapshot(demoSnapshot(1_792_100_000, { privateTasks: true }));
  assert.equal(s.swarm.tasks, 'private');
  const r = render(s, 1_792_100_000);
  assert.ok(r.rows.length > 0 && r.rows.every(x => x.length === 3), 'name, minutes, state only');
  assert.ok(!/reported by the buyer|No report/.test(allText(r)));
  assert.equal(parseSnapshot(demoSnapshot(1_792_100_000)).swarm.tasks, 'public', 'plain ?demo unchanged');
});

// Bob (2026-11-21): an agent waiting for approval shows its profile name, like an accepted member, not its npub.
test('an applicant waiting for approval is listed by its profile name', () => {
  const profile = sign(APPLICANT, 0, D - 950, [], JSON.stringify({ name: 'io-seller' }));
  const s = project([...base('public'), profile]);
  assert.equal(s.applicants.find(x => x.pubkey === pub(APPLICANT))?.name, 'io-seller');
  const r = render(s);
  assert.ok(r.rows.some(x => x[0] === 'io-seller' && x[2] === 'Waiting for approval'));
});
