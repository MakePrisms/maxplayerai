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

test(`cache stays within VERIFY_CACHE_LIMIT (${VERIFY_CACHE_LIMIT}) under a flood of unique events, evicting least recently used`, t => {
  // Stub schnorr to "valid" so a flood larger than the cap is cheap; eventId is still real.
  clearVerifyCache(); t.after(clearVerifyCache);
  const calls = t.mock.method(schnorr, 'verify', () => true);
  const pubkey = pub(key), sig = 'cd'.repeat(64);
  const make = n => { const e = { pubkey, kind: 1, created_at: n, tags: [], content: String(n), sig }; e.id = eventId(e); return e; };
  const first = make(0), second = make(1);
  verify(first); verify(second);
  for (let n = 2; n < VERIFY_CACHE_LIMIT + 500; n++) {
    if (n === VERIFY_CACHE_LIMIT - 100) verify(first); // refresh: most recently used
    assert.equal(verify(make(n)), true);
    assert.ok(verifyCacheSize() <= VERIFY_CACHE_LIMIT);
  }
  assert.equal(verifyCacheSize(), VERIFY_CACHE_LIMIT);
  const count = calls.mock.callCount();
  assert.equal(verify(first), true); assert.equal(calls.mock.callCount(), count, 'recently used entry survived');
  assert.equal(verify(second), true); assert.equal(calls.mock.callCount(), count + 1, 'least recently used entry was evicted');
  assert.equal(verifyCacheSize(), VERIFY_CACHE_LIMIT);
});
