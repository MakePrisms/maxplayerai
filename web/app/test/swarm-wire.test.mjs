// Swarm wire (docs/specs/swarm.md §4): the JS validators refuse what swarm.rs refuses, and naddr
// links are the CLI's addresses.
import assert from 'node:assert/strict';
import test from 'node:test';
import { SWARM, SWARM_JOB, SWARM_MEMBERSHIP, SWARM_DECISION } from '../src/model/kinds.ts';
import { naddrDecode, naddrEncode } from '../src/swarm/shared/naddr.mjs';
import { Decisions, decisionAllows, parseDecision, parseMembership, parseSwarm, parseSwarmAddress, parseSwarmJob, swarmAddress } from '../src/swarm/shared/wire.mjs';

const BUYER = 'ab'.repeat(32), SELLER = 'cd'.repeat(32), OTHER = 'ef'.repeat(32);
const NS = [['t', 'maxplayer'], ['v', '1']];
const ev = (pubkey, kind, tags, content = '', created_at = 100, id = '00'.repeat(32)) => ({ id, pubkey, kind, tags, content, created_at, sig: '' });
const swarm = (tags = []) => ev(BUYER, SWARM, [...NS, ['d', 'math'], ['name', 'Math'], ['status', 'open'], ...tags], 'Hard problems');
const job = (over = {}) => ev(BUYER, SWARM_JOB, [...NS, ['d', 'today'], ['a', over.a ?? swarmAddress(BUYER, 'math')], ['title', 'T'], ['starts', '10'], ['ends', over.ends ?? '20'], ['status', over.status ?? 'scheduled'], over.budget ?? ['budget', '100', 'sat']]);
const membership = (over = {}) => ev(SELLER, SWARM_MEMBERSHIP, [...NS, ['d', over.d ?? `${BUYER}:math`], ['a', swarmAddress(BUYER, 'math')], ['p', over.p ?? BUYER], ['since', '5'], ['minutes', over.minutes ?? '60'], ['status', 'joined']], over.note ?? 'hi');
const decision = (tags, author = BUYER) => ev(author, SWARM_DECISION, [...NS, ...tags]);

test('CLI naddr: decodes and re-encodes byte for byte', () => {
  // Printed by `maxplayer swarm show 79be…/math-club` (nostr 0.44, relay wss://relay.maxplayer.ai).
  const cli = 'naddr1qqyk6ct5dqkkxmr4vgpzq7d7vel0nh9m4326qc54e6rskpczn07dktww9rv4nu5ptvt0s9ucqvzqqqrk3qq3samnwvaz7tmjv4kxz7fwd4shsurvv9uk2u3wv95sud32f9';
  const d = naddrDecode(cli);
  assert.deepEqual(d, { kind: SWARM, pubkey: '79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798', identifier: 'math-club', relays: ['wss://relay.maxplayer.ai'] });
  assert.equal(naddrEncode(d), cli);
  assert.equal(naddrDecode(cli.toUpperCase()), null, 'mixed or upper case refused');
  assert.equal(naddrDecode(cli.slice(0, -1) + (cli.endsWith('q') ? 'p' : 'q')), null, 'bad checksum');
  assert.equal(naddrDecode(`npub1${cli.slice(6)}`), null);
  assert.equal(naddrDecode('naddr1'), null);
  assert.deepEqual(naddrDecode(naddrEncode({ kind: SWARM_JOB, pubkey: BUYER, identifier: 'day-1' })), { kind: SWARM_JOB, pubkey: BUYER, identifier: 'day-1', relays: [] });
});

test('swarm: name, status and tasks as swarm.rs', () => {
  assert.deepEqual(parseSwarm(swarm()), { buyer: BUYER, id: 'math', name: 'Math', purpose: 'Hard problems', status: 'open', tasks: 'public' });
  assert.equal(parseSwarm(swarm([['tasks', 'private']])).tasks, 'private');
  assert.equal(parseSwarm(swarm([['tasks', 'secret']])), null);
  assert.equal(parseSwarm(swarm([['tasks', 'private', 'x']])), null, 'malformed optional tag is refused, not defaulted');
  assert.equal(parseSwarm(swarm([['name', 'Again']])), null, 'duplicate tag');
  assert.equal(parseSwarm({ ...swarm(), tags: swarm().tags.map(t => t[0] === 'name' ? ['name', 'x'.repeat(129)] : t) }), null);
  assert.ok(parseSwarm({ ...swarm(), tags: swarm().tags.map(t => t[0] === 'name' ? ['name', '😀'.repeat(128)] : t) }), 'name counts characters, not UTF-16 units');
  assert.equal(parseSwarm({ ...swarm(), tags: swarm().tags.map(t => t[0] === 'v' ? ['v', '2'] : t) }), null);
});

test('swarm job: a names a swarm of the same buyer; times, budget, status', () => {
  assert.equal(parseSwarmJob(job()).swarm, 'math');
  assert.equal(parseSwarmJob(job({ a: swarmAddress(OTHER, 'math') })), null);
  assert.equal(parseSwarmJob(job({ a: `${SWARM_JOB}:${BUYER}:math` })), null);
  assert.equal(parseSwarmJob(job({ ends: '10' })), null);
  assert.equal(parseSwarmJob(job({ budget: ['budget', '100'] })), null);
  assert.equal(parseSwarmJob(job({ status: 'archived' })), null);
  assert.equal(parseSwarmAddress(`${SWARM}:${BUYER}:a:b`), null);
});

test('membership: a, p and d agree; minutes 1..480; oversized note dropped', () => {
  assert.deepEqual(parseMembership(membership()), { buyer: BUYER, swarm: 'math', seller: SELLER, since: 5, minutes: 60, status: 'joined', note: 'hi' });
  assert.equal(parseMembership(membership({ d: BUYER })), null, 'the pre-swarm d (buyer only) is refused');
  assert.equal(parseMembership(membership({ p: OTHER })), null);
  for (const minutes of ['0', '481', '-1', '1.5']) assert.equal(parseMembership(membership({ minutes })), null);
  assert.equal(parseMembership(membership({ note: 'x'.repeat(4097) })).note, '');
});

test('decision: buyer-wide or per swarm, each with its d', () => {
  assert.equal(parseDecision(decision([['d', SELLER], ['p', SELLER], ['decision', 'accepted']])).swarm, null);
  assert.equal(parseDecision(decision([['d', `${SELLER}:math`], ['a', swarmAddress(BUYER, 'math')], ['p', SELLER], ['decision', 'denied']])).swarm, 'math');
  assert.equal(parseDecision(decision([['d', `${SELLER}:math`], ['p', SELLER], ['decision', 'denied']])), null, 'per-swarm d needs a');
  assert.equal(parseDecision(decision([['d', SELLER], ['a', swarmAddress(BUYER, 'math')], ['p', SELLER], ['decision', 'denied']])), null);
  assert.equal(parseDecision(decision([['d', `${SELLER}:math`], ['a', swarmAddress(OTHER, 'math')], ['p', SELLER], ['decision', 'accepted']])), null, "another buyer's swarm");
  assert.equal(parseDecision(decision([['d', SELLER], ['p', SELLER], ['decision', 'accepted'], ['e', 'nope']])), null);
});

test('decisions: latest per d, eligibility rule', () => {
  assert.equal(decisionAllows('accepted', null), true);
  assert.equal(decisionAllows(null, 'accepted'), true);
  assert.equal(decisionAllows('accepted', 'denied'), false);
  assert.equal(decisionAllows('denied', 'accepted'), false);
  assert.equal(decisionAllows(null, null), false);
  const d = new Decisions(BUYER, [
    { ...decision([['d', SELLER], ['p', SELLER], ['decision', 'denied']]), created_at: 1, id: '01'.repeat(32) },
    { ...decision([['d', SELLER], ['p', SELLER], ['decision', 'accepted']]), created_at: 2, id: '02'.repeat(32) },
    { ...decision([['d', SELLER], ['p', SELLER], ['decision', 'denied']], OTHER), created_at: 3, id: '03'.repeat(32) },
  ]);
  assert.equal(d.allows(SELLER, 'math'), true, "the latest of this buyer's own decisions; another buyer's never counts");
});
