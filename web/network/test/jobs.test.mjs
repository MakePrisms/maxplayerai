import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { parseEvent } from "../js/parse.js";
import { aggregateJobs, computePulse, JOB_STATUS } from "../js/jobs.js";
import { AWARD, OFFER } from "../js/kinds.js";

/* Recorded from a live relay (wss://relay.example) — real event chains
 * covering each lifecycle stage. See test/fixtures/events.json. */
const RAW = JSON.parse(
  readFileSync(new URL("./fixtures/events.json", import.meta.url), "utf8"),
);
const NORMALIZED = RAW.map(parseEvent).filter(Boolean);

/** Split normalized events into market events + a pubkey→profile map, as the store does. */
function split(normalized) {
  const profiles = new Map();
  const market = [];
  for (const ev of normalized) {
    if (ev.role === "profile") {
      const prev = profiles.get(ev.pubkey);
      if (!prev || prev.created_at <= ev.created_at) {
        profiles.set(ev.pubkey, { ...ev.profile, created_at: ev.created_at });
      }
    } else {
      market.push(ev);
    }
  }
  return { market, profiles };
}

const { market, profiles } = split(NORMALIZED);

function jobStartingWith(jobs, prefix) {
  const j = jobs.find((x) => x.id.startsWith(prefix));
  assert.ok(j, `expected a job for offer ${prefix}`);
  return j;
}

/** parseEvent a hand-built raw event (for the two stages absent from the recording). */
function synth(raw) {
  const n = parseEvent(raw);
  assert.ok(n, "synthetic event must parse");
  return n;
}

test("aggregates real chains to the correct status per lifecycle stage", () => {
  // now sits after 244faf's creation but before its deadline, so a claim-only job reads
  // as CLAIMED (not yet expired); the paid / delivered / refused jobs are terminal.
  const now = 1784576000;
  const jobs = aggregateJobs(market, profiles, now);

  assert.equal(jobStartingWith(jobs, "9e0e2122").status, JOB_STATUS.PAID);
  assert.equal(jobStartingWith(jobs, "7077f318").status, JOB_STATUS.DELIVERED);
  assert.equal(jobStartingWith(jobs, "2951de66").status, JOB_STATUS.REFUSED);
  assert.equal(jobStartingWith(jobs, "244faf68").status, JOB_STATUS.CLAIMED);
  // 1858f3db's deadline is already in the past with nothing delivered → EXPIRED.
  assert.equal(jobStartingWith(jobs, "1858f3db").status, JOB_STATUS.EXPIRED);
});

test("a claim-only job flips to EXPIRED once its deadline passes", () => {
  const afterDeadline = 1784600000; // > 244faf deadline 1784579215
  const jobs = aggregateJobs(market, profiles, afterDeadline);
  assert.equal(jobStartingWith(jobs, "244faf68").status, JOB_STATUS.EXPIRED);
});

test("OPEN and WORKING stages derive correctly", () => {
  const future = 1784999999;
  const openOffer = synth({
    id: "0".repeat(64),
    pubkey: "a".repeat(64),
    kind: OFFER,
    created_at: 1784576000,
    tags: [["i", "an open task"], ["amount", "7", "sat"], ["param", "deadline", String(future)], ["t", "mobee"], ["v", "2"]],
    content: "",
  });
  const workOffer = synth({
    id: "b".repeat(64),
    pubkey: "c".repeat(64),
    kind: OFFER,
    created_at: 1784576000,
    tags: [["i", "a working task"], ["amount", "9", "sat"], ["param", "deadline", String(future)], ["t", "mobee"], ["v", "2"]],
    content: "",
  });
  const award = synth({
    id: "d".repeat(64),
    pubkey: "c".repeat(64), // buyer awards the claim
    kind: AWARD,
    created_at: 1784576100,
    tags: [["status", "accepted"], ["e", "b".repeat(64), "", "root"], ["e", "aa".repeat(32)], ["p", "e".repeat(64)], ["t", "mobee"], ["v", "2"]],
    content: "",
  });
  const jobs = aggregateJobs([openOffer, workOffer, award], new Map(), 1784576200);
  assert.equal(jobStartingWith(jobs, "0000").status, JOB_STATUS.OPEN);
  assert.equal(jobStartingWith(jobs, "bbbb").status, JOB_STATUS.WORKING);
});

test("buyer is the offer author; sellers are the claim/result authors", () => {
  const jobs = aggregateJobs(market, profiles, 1784576000);
  const paid = jobStartingWith(jobs, "9e0e2122");
  assert.ok(paid.buyer && paid.buyer.pubkey, "buyer resolved from offer author");
  assert.ok(paid.sellers.length >= 1, "at least one seller on a paid job");
  // buyer and seller are distinct pubkeys
  for (const s of paid.sellers) {
    assert.notEqual(s.pubkey, paid.buyer.pubkey, "seller is not the buyer");
  }
});

test("timeline is chronological, plain-English, and party-attributed", () => {
  const jobs = aggregateJobs(market, profiles, 1784576000);
  const paid = jobStartingWith(jobs, "9e0e2122");
  const tl = paid.timeline;
  assert.ok(tl.length >= 2, "paid job has multiple timeline entries");
  // chronological
  for (let i = 1; i < tl.length; i += 1) {
    assert.ok(tl[i].at >= tl[i - 1].at, "timeline sorted ascending");
  }
  // every entry is attributed to buyer or seller and carries readable text
  for (const e of tl) {
    assert.ok(e.actor === "buyer" || e.actor === "seller", "entry attributed");
    assert.ok(typeof e.text === "string" && e.text.length > 0, "entry has text");
    assert.ok(!/[{}]/.test(e.text), "no raw JSON leaks into timeline text");
  }
  // a paid job's last entry is the payment, expressed in sats
  const paidEntry = tl.find((e) => /paid/.test(e.text));
  assert.ok(paidEntry, "payment shows in the timeline");
  assert.match(paidEntry.text, /\d+ sats/);
});

test("pulse: sats settled today sums receipts on the current UTC day", () => {
  // the recording holds exactly one receipt: 15 sats at 1784552841. Set `now` to that day.
  const now = 1784552841 + 3600;
  const jobs = aggregateJobs(market, profiles, now);
  const pulse = computePulse(market, jobs, now);
  assert.equal(pulse.satsSettledToday, 15);
});

test("pulse: a receipt from a previous day does not count as settled today", () => {
  const now = 1784595265; // a later UTC day than the receipt
  const jobs = aggregateJobs(market, profiles, now);
  const pulse = computePulse(market, jobs, now);
  assert.equal(pulse.satsSettledToday, 0);
});

test("pulse: open offers count matches OPEN-status jobs", () => {
  const future = 1784999999;
  const openOffer = synth({
    id: "f".repeat(64),
    pubkey: "a".repeat(64),
    kind: OFFER,
    created_at: 1784576000,
    tags: [["i", "task"], ["amount", "3", "sat"], ["param", "deadline", String(future)], ["t", "mobee"], ["v", "2"]],
    content: "",
  });
  const events = [...market, openOffer];
  const now = 1784576100;
  const jobs = aggregateJobs(events, profiles, now);
  const pulse = computePulse(events, jobs, now);
  const openCount = jobs.filter((j) => j.status === JOB_STATUS.OPEN).length;
  assert.equal(pulse.openOffers, openCount);
  assert.equal(pulse.openOffers, 1, "exactly the one future-deadline open offer");
});

test("pulse: active sellers counts distinct claim/result/handler authors in the last 24h", () => {
  // Independently derived from the recording: 6 distinct sellers acted within 24h of `now`.
  const now = 1784595265;
  const jobs = aggregateJobs(market, profiles, now);
  const pulse = computePulse(market, jobs, now);
  assert.equal(pulse.activeSellers, 6);
});
