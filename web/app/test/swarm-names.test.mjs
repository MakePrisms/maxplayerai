// Review of #1361, r2 #1: applicants never push the buyer's or a member's name out of the profile cap.
import assert from 'node:assert/strict';
import test from 'node:test';
import { nameKeys } from '../src/swarm/shared/relay.mjs';

const k = (i) => i.toString(16).padStart(64, '0');
test('names: buyer and members first; applicants only fill the room left', () => {
  const buyer = k(999), members = [k(500), k(501)], applicants = Array.from({ length: 300 }, (_, i) => k(i));
  const keys = nameKeys(buyer, members, applicants, 256);
  assert.equal(keys.length, 256);
  assert.deepEqual(keys.slice(0, 3), [buyer, ...members]);
  assert.deepEqual(nameKeys(buyer, members, [members[0], k(7)], 256), [buyer, ...members, k(7)], 'no duplicates');
  assert.deepEqual(nameKeys(buyer, members, applicants, 2), [buyer, ...members], 'members over the cap stay; no applicant');
});
