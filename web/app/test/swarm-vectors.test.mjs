// The shared swarm vectors (docs/specs/swarm.md §7.4): signed events in, eligibility and minutes
// per swarm out. The daemon's Rust port runs the same file (PR 4).
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { verify } from '../src/challenge/shared/events.mjs';
import { SwarmStore, minutesToday, taskFacts, swarmSnapshot } from '../src/swarm/shared/model.mjs';
import { eligibleSellers, latestSwarmJobs } from '../src/swarm/shared/wire.mjs';

const vectors = JSON.parse(readFileSync(new URL('../src/swarm/shared/vectors.json', import.meta.url), 'utf8'));
const split = key => { const i = key.indexOf(':'); return [key.slice(0, i), key.slice(i + 1)]; };

for (const c of vectors.cases) {
  test(`swarm vectors: ${c.name}`, () => {
    assert.ok(c.events.every(verify), 'every vector event is signed');
    // Through the store, as the board reads them; the store must not change any answer.
    const store = new SwarmStore();
    for (const e of c.events) store.add(e);
    for (const events of [c.events, [...store.events.values()]]) {
      const jobs = latestSwarmJobs(events).map(([, j]) => j);
      for (const [key, want] of Object.entries(c.expect.eligible ?? {})) {
        const [buyer, d] = split(key), job = jobs.find(j => j.buyer === buyer && j.id === d);
        assert.ok(job, `job ${key}`);
        assert.deepEqual(eligibleSellers(job, events), want, `eligible in ${key}`);
      }
      for (const [key, want] of Object.entries(c.expect.minutesToday ?? {})) {
        const [buyer, d] = split(key);
        const got = Object.fromEntries(minutesToday(taskFacts(events, buyer, jobs), d, c.now));
        assert.deepEqual(got, want, `minutes in ${key}`);
        // The page shows the same numbers for members.
        const page = swarmSnapshot(events, { buyer, swarm: d }, c.now);
        for (const [seller, minutes] of Object.entries(want)) assert.equal(page.sellers.find(s => s.pubkey === seller)?.minutesUsed, minutes, `page minutes for ${seller} in ${key}`);
      }
    }
  });
}

test('swarm vectors: mutations of the rules change the answers', () => {
  // Guards against a vacuous file: each case must have expectations that a wrong rule would miss.
  const [elig, mins] = vectors.cases;
  assert.ok(Object.values(elig.expect.eligible).every(list => list.length >= 2));
  assert.ok(Object.values(mins.expect.minutesToday).some(m => Object.keys(m).length >= 3));
  // A day later nothing from that day counts.
  const jobs = latestSwarmJobs(mins.events).map(([, j]) => j);
  const buyer = vectors.keys.buyer;
  assert.equal(minutesToday(taskFacts(mins.events, buyer, jobs), 'math', mins.now + 86400).size, 0);
});
