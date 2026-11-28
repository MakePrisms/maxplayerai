// The activity lines of /market (src/site/profile.js eventRow). Bob, 2026-11-28: a line that belongs to a job opens
// only the job's panel. Its names are plain text, not links into the profiles; the job panel's own lines, which open
// nothing, keep the names as links.
import assert from 'node:assert/strict';
import test from 'node:test';
import { eventRow } from '../src/site/profile.js';

const JOB = 'ab'.repeat(32);
const person = (name, c) => ({ key: c.repeat(64), npub: 'npub1' + c.repeat(58), name, named: true });
const line = (job = JOB) => ({ id: 'cd'.repeat(32), stage: 'award', at: Math.floor(Date.now() / 1000) - 120, actor: person('north-desk', 'a'), other: person('swift-code', 'b'), unknownOther: false, terminal: false, closed: false, free: false, turn: null, price: null, job });

test('a line with a job opens the job and nothing else: no profile links in it', () => {
  const html = eventRow(line(), false);
  assert.match(html, new RegExp('<li class="row g-a go" data-job="' + JOB + '"'));
  assert.ok(!html.includes('data-pk'), 'no name opens a profile');
  assert.ok(!/href="\/market\?(expert|buyer)=/.test(html), 'no link to a profile');
  assert.match(html, /<b class="who">north-desk<\/b> hired <b class="who">swift-code<\/b>/);
  assert.match(html, new RegExp('<a class="age" href="/market\\?job=' + JOB + '"'), 'the age links to the job');
});

test("the job panel's own lines open nothing, and their names still open the profiles", () => {
  const html = eventRow(line(), false, false);
  assert.ok(!html.includes('data-job'));
  assert.match(html, /<a class="who" href="\/market\?buyer=npub1a+" data-pk="a{64}" data-kind="buyer">north-desk<\/a>/);
  assert.match(html, /data-pk="b{64}" data-kind="expert">swift-code<\/a>/);
});

test('a line that names no job keeps its links', () => {
  const html = eventRow(line(null), false);
  assert.ok(!html.includes('data-job'));
  assert.ok(html.includes('data-pk'));
});
