import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { eventId, verify, VERIFY_CACHE_LIMIT, verifyCacheSize, clearVerifyCache } from '../src/challenge/shared/events.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';

const key = '01'.padStart(64, '0'), other = '02'.padStart(64, '0');
const pub = k => Buffer.from(schnorr.getPublicKey(k)).toString('hex');
const sign = (e, k = key) => { e.pubkey ??= pub(k); e.id = eventId(e); e.sig = Buffer.from(schnorr.sign(e.id, k)).toString('hex'); return e; };
const challenge = (content = 'goal') => sign({ kind: 30341, created_at: 100, content,
  tags: [['t', 'maxplayer'], ['v', '1'], ['d', 'test'], ['title', 'T'], ['starts', '1'], ['ends', '2'], ['budget', '1', 'sat'], ['status', 'in_progress']] });
// Counts real schnorr checks without changing their result.
function spy(t) { clearVerifyCache(); t.after(clearVerifyCache); return t.mock.method(schnorr, 'verify'); }

test('a repeated event costs one schnorr verify across re-reads and the relay+store double check', t => {
  const calls = spy(t), e = challenge(), store = new EventStore();
  for (let read = 0; read < 20; read++) {
    // relay.mjs verifies each frame, then EventStore.add verifies it again.
    assert.equal(verify(structuredClone(e)), true);
    store.add(structuredClone(e));
  }
  assert.equal(store.events.size, 1);
  assert.equal(calls.mock.callCount(), 1, 'one schnorr verify per distinct (id,pubkey,sig)');
});

test('a cached id+sig never accepts different content, tags, kind, time or pubkey', t => {
  spy(t);
  const e = challenge();
  assert.equal(verify(e), true);
  assert.equal(new EventStore().add(e), true);
  for (const change of [{ content: 'tampered' }, { tags: [...e.tags, ['p', pub(other)]] }, { tags: e.tags.slice(1) },
    { kind: 30342 }, { created_at: 101 }, { pubkey: pub(other) }]) {
    const forged = { ...e, ...change };
    assert.equal(verify(forged), false, `cache hit must not accept ${Object.keys(change)[0]}`);
    assert.equal(new EventStore().add(forged), false);
  }
  // The genuine event is still accepted from the cache.
  assert.equal(verify(e), true);
});

test('a forged signature is rejected every time, including for a cached id and pubkey', t => {
  const calls = spy(t), e = challenge();
  assert.equal(verify(e), true);
  const before = calls.mock.callCount();
  // Valid signature from another key, a stolen signature from another event, and junk.
  const stolen = challenge('other').sig, junk = 'ab'.repeat(64), otherKey = Buffer.from(schnorr.sign(e.id, other)).toString('hex');
  for (const sig of [stolen, junk, otherKey]) for (let i = 0; i < 3; i++) {
    assert.equal(verify({ ...e, sig }), false);
    assert.equal(new EventStore().add({ ...e, sig }), false);
  }
  assert.equal(calls.mock.callCount() - before, 18, 'failed verifications are never cached');
  // A forged sig never poisons the cache for the genuine event either.
  assert.equal(verify(e), true);
});

// Distinct events with real ids, built once (schnorr is stubbed in the tests that use them).
let bulk;
function events(n) {
  const pubkey = pub(key), sig = 'cd'.repeat(64);
  bulk ??= [];
  for (let i = bulk.length; i < n; i++) { const e = { pubkey, kind: 1, created_at: i, tags: [], content: String(i), sig }; e.id = eventId(e); bulk.push(e); }
  return bulk.slice(0, n);
}
function stubbed(t, random) {
  clearVerifyCache(); t.after(clearVerifyCache);
  if (random) t.mock.method(Math, 'random', random);
  return t.mock.method(schnorr, 'verify', () => true);
}
const pass = (calls, list) => { const before = calls.mock.callCount(); for (const e of list) assert.equal(verify(e), true); return calls.mock.callCount() - before; };

test(`cache stays within VERIFY_CACHE_LIMIT (${VERIFY_CACHE_LIMIT}) under a flood of unique events, replacing a random slot`, t => {
  // Pin the random choice to the middle slot so the evicted entry is known.
  const calls = stubbed(t, () => 0.5), list = events(VERIFY_CACHE_LIMIT + 500), mid = VERIFY_CACHE_LIMIT / 2;
  for (const e of list) { assert.equal(verify(e), true); assert.ok(verifyCacheSize() <= VERIFY_CACHE_LIMIT); }
  assert.equal(verifyCacheSize(), VERIFY_CACHE_LIMIT);
  const count = calls.mock.callCount();
  assert.equal(count, VERIFY_CACHE_LIMIT + 500);
  // The oldest entry survives (no FIFO/LRU eviction), as does every unchosen slot.
  for (const i of [0, 1, mid - 1, mid + 1, VERIFY_CACHE_LIMIT - 1, VERIFY_CACHE_LIMIT + 499]) assert.equal(verify(list[i]), true);
  assert.equal(calls.mock.callCount(), count, 'only the randomly chosen slot was replaced');
  assert.equal(verify(list[mid]), true);
  assert.equal(calls.mock.callCount(), count + 1, 'the chosen slot\'s entry was evicted');
  assert.equal(verifyCacheSize(), VERIFY_CACHE_LIMIT);
});

// Review r1 F1: with LRU, a stable re-scan of cap+1 events missed every entry on every pass.
for (const [name, random] of [['first slot', () => 0], ['last slot', () => 0.999999]]) {
  test(`a second pass over VERIFY_CACHE_LIMIT + 1 events costs exactly 2 schnorr checks, not cap+1 (random choice pinned to the ${name})`, t => {
    const calls = stubbed(t, random), list = events(VERIFY_CACHE_LIMIT + 1);
    assert.equal(pass(calls, list), VERIFY_CACHE_LIMIT + 1, 'first pass verifies everything');
    // One entry was displaced by the last insert: re-verifying it displaces one more, which the
    // pass then re-verifies too when it is still ahead (both pinned choices hit that case).
    assert.equal(pass(calls, list), 2, 'second pass of cap+1');
    assert.equal(pass(calls, list), 2, 'third pass of cap+1');
    assert.equal(verifyCacheSize(), VERIFY_CACHE_LIMIT);
  });
}

test('repeated passes over VERIFY_CACHE_LIMIT + 1 events stay near a mean of 2 schnorr checks per pass with real randomness', t => {
  const calls = stubbed(t), list = events(VERIFY_CACHE_LIMIT + 1);
  pass(calls, list);
  let total = 0;
  for (let i = 0; i < 3; i++) total += pass(calls, list);
  // Mean 2 per pass, so ~6 expected; LRU would be 3 * (cap + 1). The ceiling fails a
  // regression to dozens of misses per pass while staying far above the observed tail.
  assert.ok(total <= 30, `3 re-passes cost ${total} schnorr checks`);
});
