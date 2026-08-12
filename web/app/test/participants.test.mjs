import assert from "node:assert/strict";
import { test } from "node:test";

import {
  DEFAULT_WINDOW, LIVE_WITHIN_SECONDS, WINDOWS,
  JOB_OVERDUE, JOB_WORKING, STALLED_GRACE_SECONDS,
  buyerBoard, inProgressJobs, participantActivity, participantDetail, participantNames, racerLastActivity, relatedActivity,
  sellerBoard, withinWindow, windowSeconds,
} from "../js/participants.js";
import { ACCEPT, AWARD, CLAIM, FEEDBACK, HEARTBEAT, OFFER, PROFILE, RECEIPT, RESULT } from "../js/kinds.js";


/** Fixtures use readable labels; the wire uses 32 bytes of hex. Map one to the other. */
const _ids = new Map();
const H = (label) => {
  if (!_ids.has(label)) _ids.set(label, (_ids.size + 1).toString(16).padStart(64, "0"));
  return _ids.get(label);
};

const pk = (c) => c.repeat(64);
const NOW = 1_800_000_000;
const ev = (kind, { id, pubkey = pk("a"), at = NOW, tags = [], content = "" }) =>
  ({ id: H(id), kind, pubkey, created_at: at, tags, content });
const root = (offerId) => ["e", H(offerId), "", "root"];

function trade(offerId, { buyer = pk("b"), seller = pk("c"), sats = 10, t0 = NOW - 3600, receipt = true } = {}) {
  const out = [
    ev(OFFER, { id: offerId, pubkey: buyer, at: t0, tags: [["amount", String(sats), "sat"]] }),
    ev(CLAIM, { id: offerId + "c", pubkey: seller, at: t0 + 60, tags: [root(offerId)] }),
    ev(AWARD, { id: offerId + "a", pubkey: buyer, at: t0 + 70, tags: [root(offerId)] }),
    ev(RESULT, { id: offerId + "r", pubkey: seller, at: t0 + 120, tags: [root(offerId)] }),
  ];
  if (receipt) out.push(ev(RECEIPT, { id: offerId + "p", at: t0 + 130, tags: [root(offerId), ["amount", String(sats), "sat"]] }));
  return out;
}

test("the default window is a week and every window is selectable", () => {
  assert.equal(DEFAULT_WINDOW, "week");
  assert.deepEqual(WINDOWS.map((w) => w.key), ["24h", "week", "all"]);
  assert.equal(windowSeconds("24h"), 86400);
  assert.equal(windowSeconds("all"), null, "all time is unbounded");
});

test("a window filters events, and all-time filters nothing", () => {
  const events = [ev(OFFER, { id: "recent", at: NOW - 3600 }), ev(OFFER, { id: "old", at: NOW - 86400 * 10 })];
  assert.deepEqual(withinWindow(events, "24h", NOW).map((e) => e.id), [H("recent")]);
  assert.deepEqual(withinWindow(events, "week", NOW).map((e) => e.id), [H("recent")]);
  assert.equal(withinWindow(events, "all", NOW).length, 2);
});

test("racer activity is buyer-authored and independent of the selected board window", () => {
  const recent = pk("1"), old = pk("2"), runner = pk("3");
  const events = [
    ev(OFFER, { id: "recent-offer", pubkey: recent, at: NOW - 3600 }),
    ev(ACCEPT, { id: "recent-accept", pubkey: recent, at: NOW - 120, tags: [root("recent-offer")] }),
    ev(OFFER, { id: "old-offer", pubkey: old, at: NOW - 86400 * 2 }),
    ev(RESULT, { id: "runner-result", pubkey: runner, at: NOW - 10, tags: [root("recent-offer")] }),
  ];

  const latest = racerLastActivity(events);
  assert.equal(latest.get(recent), NOW - 120, "the latest buyer-side action wins");
  assert.equal(latest.get(old), NOW - 86400 * 2, "older activity remains available for a gray lamp");
  assert.equal(latest.has(runner), false, "runner-authored delivery is not racer activity");
});

test("buyers rank by sats paid and carry their posting history", () => {
  const big = pk("1"), small = pk("2");
  const board = buyerBoard([
    ...trade("o1", { buyer: big, sats: 50 }),
    ...trade("o2", { buyer: big, sats: 30 }),
    ...trade("o3", { buyer: small, sats: 5 }),
  ], NOW);

  assert.equal(board.length, 2);
  assert.equal(board[0].pubkey, big, "highest spend first");
  assert.equal(board[0].satsPaid, 80);
  assert.equal(board[0].posted, 2);
  assert.equal(board[0].receipted, 2);
  assert.equal(board[0].medianPrice, 40);
  assert.equal(board[1].satsPaid, 5);
});

test("a delivery with no receipt is an open question, not a debt", () => {
  const [row] = buyerBoard(trade("o1", { receipt: false }), NOW);
  assert.equal(row.receipted, 0);
  assert.equal(row.satsPaid, 0);
  assert.equal(row.unpaidDeliveries, 1, "surfaced, because settlement can happen unannounced");
});

test("sellers carry a completion rate and a median delivery time", () => {
  const s = pk("9");
  const board = sellerBoard([
    ...trade("o1", { seller: s, sats: 10, t0: NOW - 7200 }),
    ...trade("o2", { seller: s, sats: 20, t0: NOW - 3600 }),
  ], NOW);

  const [row] = board;
  assert.equal(row.pubkey, s);
  assert.equal(row.claimed, 2);
  assert.equal(row.delivered, 2);
  assert.equal(row.satsEarned, 30);
  assert.equal(row.completionRate, 1);
  assert.equal(row.medianDeliverSeconds, 60, "claim at +60, result at +120");
});

test("a claim that produced feedback but no delivery counts as released", () => {
  const s = pk("9");
  const events = [
    ...trade("o1", { seller: s }).slice(0, 2),
    ev(FEEDBACK, { id: "f1", pubkey: s, at: NOW - 3000, tags: [root("o1")], content: "claim_released: withdrew" }),
  ];
  const [row] = sellerBoard(events, NOW);
  assert.equal(row.released, 1);
  assert.equal(row.delivered, 0);
  assert.equal(row.completionRate, 0);
});

test("online means a recent heartbeat, not merely recent trading", () => {
  const fresh = pk("1"), stale = pk("2");
  const events = [
    ...trade("o1", { seller: fresh, t0: NOW - 3600 }),
    ...trade("o2", { seller: stale, t0: NOW - 3600 }),
    ev(HEARTBEAT, { id: "hb1", pubkey: fresh, at: NOW - 10, tags: [["d", "seat"]] }),
    ev(HEARTBEAT, { id: "hb2", pubkey: stale, at: NOW - LIVE_WITHIN_SECONDS - 60, tags: [["d", "seat"]] }),
  ];
  const board = sellerBoard(events, NOW);
  const byKey = Object.fromEntries(board.map((r) => [r.pubkey, r]));

  assert.equal(byKey[fresh].online, true);
  assert.equal(byKey[stale].online, false, "a stale heartbeat is not availability");
  assert.ok(byKey[stale].lastSeen > 0, "but they are still known to exist");
});

test("sellers rank by track record, and being online does not lift them", () => {
  const veteran = pk("1"), steady = pk("2"), flaky = pk("3"), newcomer = pk("4");
  const events = [
    // Two finished jobs, paid, but not around right now.
    ...trade("o-vet-1", { seller: veteran, sats: 10 }),
    ...trade("o-vet-2", { seller: veteran, sats: 10 }),
    // One finished job each, unpaid, so sats cannot break the tie — the
    // difference is that flaky also walked away from a claim.
    ...trade("o-steady", { seller: steady, receipt: false }),
    ...trade("o-flaky", { seller: flaky, receipt: false }),
    ...trade("o-flaky-open", { seller: flaky }).slice(0, 2),
    // Live this minute, has never finished anything.
    ...trade("o-new", { seller: newcomer }).slice(0, 2),
    ev(HEARTBEAT, { id: "hb-new", pubkey: newcomer, at: NOW - 10, tags: [["d", "seat"]] }),
  ];

  const board = sellerBoard(events, NOW);
  assert.deepEqual(board.map((r) => r.pubkey), [veteran, steady, flaky, newcomer]);
  assert.equal(board[0].delivered, 2, "most delivered leads");
  assert.equal(board[1].completionRate, 1, "equal deliveries break on completion rate");
  assert.equal(board[2].completionRate, 0.5);
  assert.equal(board[3].online, true, "online, and still last — it is not a ranking signal");
});

test("the current heartbeat advertisement attaches every field to its seller", () => {
  const s = pk("9");
  const events = [
    ...trade("o1", { seller: s }),
    ev(HEARTBEAT, { id: "h1", pubkey: s, at: NOW - 100, tags: [
      ["d", "maxplayer-seller"], ["rate", "77"], ["accepting", "y"], ["queue_depth", "1"],
      ["accepted_mints", "https://mint"], ["agents", "codex"], ["model", "gpt-future"], ["hardware", "gb10"],
    ] }),
  ];
  const [row] = sellerBoard(events, NOW);
  assert.equal(row.askSats, 77);
  assert.equal(row.accepting, "y");
  assert.equal(row.queueDepth, 1);
  assert.deepEqual(row.acceptedMints, ["https://mint"]);
  assert.deepEqual(row.advertisedAgents, ["codex"]);
  assert.deepEqual(row.advertisementTags.at(-1), { name: "hardware", values: ["gb10"] });
});

test("the seat name and full profile metadata resolve from kind-0", () => {
  const s = pk("e");
  const events = [
    ...trade("o1", { seller: s }),
    ev(PROFILE, { id: "p1", pubkey: s, at: NOW - 100,
      content: '{"name":"frogger","website":"https://frogger.example","about":"fast"}' }),
  ];
  const [row] = sellerBoard(events, NOW);
  assert.equal(row.name, "frogger");
  assert.equal(row.about, "fast");
  assert.equal(row.profile.website, "https://frogger.example", "details retain every advertised profile field");
  assert.equal(participantNames(events).get(s), "frogger");
});

/**
 * REGRESSION: a kind-0 ENRICHES a participant, it never creates one.
 *
 * When kind-0 first began arriving, the seller board's PROFILE arm called the
 * same row-creating getter as the heartbeat and advert arms, so every profile
 * on the relay became a runner: 13 of 24 rows were strangers with no claim, no
 * delivery, no advert and no heartbeat — including a pubkey whose only activity
 * was buying.
 */
test("REGRESSION: a kind-0 alone never creates a seller row", () => {
  const stranger = pk("9");
  const events = [
    ...trade("o1", { seller: pk("c") }),
    ev(PROFILE, { id: "pstranger", pubkey: stranger, at: NOW - 100, content: '{"name":"bob"}' }),
  ];
  const board = sellerBoard(events, NOW);
  assert.equal(board.some((r) => r.pubkey === stranger), false,
    "publishing profile metadata is not selling");
  assert.equal(board.length, 1, "only the seat that actually delivered holds a row");
});

test("REGRESSION: a buyer's kind-0 never creates a seller row", () => {
  const buyer = pk("d");
  const events = [
    ...trade("o1", { buyer, seller: pk("c") }),
    ev(PROFILE, { id: "pbuyer", pubkey: buyer, at: NOW - 100, content: '{"name":"sage"}' }),
  ];
  assert.equal(sellerBoard(events, NOW).some((r) => r.pubkey === buyer), false,
    "buying is not selling, whoever owns the profile");
  const [row] = buyerBoard(events, NOW);
  assert.equal(row.pubkey, buyer);
  assert.equal(row.name, "sage", "the buyer is named from its own kind-0");
});

/**
 * Relay order is not ours to choose, so naming must not depend on it.
 *
 * The case that discriminates is a seat earning its row from a HEARTBEAT or an
 * advert — evidence read in the same pass as kind-0, not in the trades pass
 * that runs before it. Most live seats are exactly this: heartbeating, adverts
 * up, nothing delivered yet. Enriching in-loop with a plain row lookup passes
 * when the profile happens to arrive second and drops the name when it arrives
 * first, which is why the names are applied after every row-creating pass.
 */
test("REGRESSION: a heartbeat-only seat is named whichever order kind-0 arrives in", () => {
  const s = pk("e");
  const profile = ev(PROFILE, { id: "pe", pubkey: s, at: NOW - 100, content: '{"name":"cherry"}' });
  const beat = ev(HEARTBEAT, { id: "hb", pubkey: s, at: NOW - 30, tags: [["d", "seller"]] });

  const rowFrom = (events) => sellerBoard(events, NOW).find((r) => r.pubkey === s);
  assert.equal(rowFrom([profile, beat])?.name, "cherry", "kind-0 before the heartbeat");
  assert.equal(rowFrom([beat, profile])?.name, "cherry", "kind-0 after the heartbeat");
  assert.equal(rowFrom([profile, beat])?.delivered, 0, "still a seat with no deliveries");
});

test("working state follows the awarded claim and ends at the selected runner's terminal event", () => {
  const buyer = pk("b"), winner = pk("c"), other = pk("d");
  const offer = ev(OFFER, { id: "work", pubkey: buyer, at: NOW - 300 });
  const otherClaim = ev(CLAIM, { id: "other-claim", pubkey: other, at: NOW - 290, tags: [root("work")] });
  const winningClaim = ev(CLAIM, { id: "winning-claim", pubkey: winner, at: NOW - 280, tags: [root("work")] });
  const award = ev(AWARD, { id: "work-award", pubkey: buyer, at: NOW - 270, tags: [
    root("work"), ["e", H("winning-claim")], ["p", buyer], ["p", winner],
  ] });
  // Deliberately shuffled: relay order cannot decide who appears to be working.
  const underway = [award, otherClaim, offer, winningClaim];
  assert.deepEqual(inProgressJobs(underway, NOW), [{
    offerId: H("work"), awardId: H("work-award"), claimId: H("winning-claim"),
    buyer, seller: winner, startedAt: NOW - 270,
    // This offer carries no deadline, so there is nothing to be late against.
    deadline: null, state: JOB_WORKING,
  }]);
  assert.equal(buyerBoard(underway, NOW)[0].inProgressJobs.length, 1);
  const sellers = Object.fromEntries(sellerBoard(underway, NOW).map((row) => [row.pubkey, row]));
  assert.equal(sellers[winner].inProgressJobs.length, 1, "the awarded runner is working");
  assert.equal(sellers[other].inProgressJobs.length, 0, "another claimant is not working");

  const otherResult = ev(RESULT, { id: "other-result", pubkey: other, at: NOW - 260, tags: [root("work")] });
  assert.equal(inProgressJobs([...underway, otherResult], NOW).length, 1,
    "a result from a non-selected claimant cannot stop the awarded runner");

  const terminals = [
    ev(RESULT, { id: "result", pubkey: winner, at: NOW - 250, tags: [root("work")] }),
    ev(ACCEPT, { id: "accept-working", pubkey: buyer, at: NOW - 240, tags: [root("work")] }),
    ev(RECEIPT, { id: "receipt-working", pubkey: buyer, at: NOW - 230, tags: [root("work")] }),
    // protocol-v1 §7.2: only a terminal CLASS ends the attempt. A bare feedback
    // used to be asserted terminal here, which locked in the early-clear bug.
    ev(FEEDBACK, { id: "release-working", pubkey: winner, at: NOW - 220,
      tags: [root("work"), ["status", "claim_released"]] }),
  ];
  for (const terminal of terminals) {
    assert.deepEqual(inProgressJobs([...underway, terminal], NOW), [], `kind ${terminal.kind} ends working state`);
  }
});

/**
 * #681. `inProgressJobs` used to take no clock, so the only exit from "working"
 * was a terminal event authored by the awarded seller. A seller that published
 * nothing produced no exit condition and stayed "working" for as long as the
 * award was in the window — which on a live board is indefinitely. The
 * Sage/Ember canary sat that way on production after the buyer had already
 * released its reservation privately, so a seat that delivered nothing rendered
 * as maximally busy.
 *
 * Overdue is DERIVED presentation state, not relay truth: no event says a job
 * stalled — that is #682. This is the site reading an absence, which is exactly
 * why the threshold is one exported constant and why every boundary below is
 * asserted against it rather than against a number spelled here.
 */
const DUE = NOW - 1000;
const lateJob = (extra = []) => [
  ev(OFFER, { id: "late", pubkey: pk("b"), at: DUE - 600, tags: [["param", "deadline", String(DUE)]] }),
  ev(CLAIM, { id: "late-claim", pubkey: pk("c"), at: DUE - 500, tags: [root("late")] }),
  ev(AWARD, { id: "late-award", pubkey: pk("b"), at: DUE - 400, tags: [
    root("late"), ["e", H("late-claim")], ["p", pk("b")], ["p", pk("c")],
  ] }),
  ...extra,
];
const stateAt = (events, at) => inProgressJobs(events, at).map((job) => job.state);

test("#681 case 1: delivery before the deadline finishes the job and never goes overdue", () => {
  const delivered = lateJob([ev(RESULT, { id: "late-result", pubkey: pk("c"), at: DUE - 60, tags: [root("late")] })]);
  assert.deepEqual(stateAt(delivered, DUE - 30), [], "finished before the deadline");
  assert.deepEqual(stateAt(delivered, DUE + STALLED_GRACE_SECONDS + 5000), [],
    "and it stays finished — delivered work cannot become overdue later");
});

test("#681 case 2: delivery inside the grace window finishes the job, it is not overdue", () => {
  const inGrace = DUE + Math.floor(STALLED_GRACE_SECONDS / 2);
  const delivered = lateJob([ev(RESULT, { id: "grace-result", pubkey: pk("c"), at: inGrace, tags: [root("late")] })]);
  assert.deepEqual(stateAt(delivered, inGrace), [], "delivered during grace is finished, not overdue");
  // The grace window is the whole point: without it, clock skew alone would
  // mark a job overdue that was delivered on time by the seller's own clock.
  assert.deepEqual(stateAt(lateJob(), DUE + STALLED_GRACE_SECONDS - 1), [JOB_WORKING],
    "one second inside the window is still working");
});

test("#681 case 3: no delivery past deadline plus grace is overdue, and the award survives", () => {
  const events = lateJob();
  assert.deepEqual(stateAt(events, DUE - 1), [JOB_WORKING], "before the deadline");
  assert.deepEqual(stateAt(events, DUE + STALLED_GRACE_SECONDS), [JOB_WORKING], "the boundary itself is not past it");
  assert.deepEqual(stateAt(events, DUE + STALLED_GRACE_SECONDS + 1), [JOB_OVERDUE], "one second past is overdue");

  // The award is preserved, not erased and not rewritten as completed.
  const [job] = inProgressJobs(events, DUE + STALLED_GRACE_SECONDS + 1);
  assert.equal(job.awardId, H("late-award"), "the award id is still there to link to");
  assert.equal(job.seller, pk("c"), "and it still names who was awarded");
  assert.equal(job.deadline, DUE, "the deadline it missed is carried, not just the verdict");

  // The discriminator the boards actually read. If these two were equal the
  // assertion above could pass while the lamp still counted a dead job.
  const at = DUE + STALLED_GRACE_SECONDS + 1;
  const seller = sellerBoard(events, at).find((r) => r.pubkey === pk("c"));
  assert.equal(seller.inProgressJobs.length, 1, "the detail view still sees the award");
  assert.equal(seller.workingJobs.length, 0, "the lamp does not count it as current work");
  const buyer = buyerBoard(events, at).find((r) => r.pubkey === pk("b"));
  assert.equal(buyer.inProgressJobs.length, 1);
  assert.equal(buyer.workingJobs.length, 0, "and neither does the racer board");
});

test("#681 case 4: a terminal event arriving after the job went overdue still ends it", () => {
  const at = DUE + STALLED_GRACE_SECONDS + 5000;
  assert.deepEqual(stateAt(lateJob(), at), [JOB_OVERDUE], "overdue first");
  for (const late of [
    ev(RESULT, { id: "very-late-result", pubkey: pk("c"), at: at - 1, tags: [root("late")] }),
    ev(ACCEPT, { id: "very-late-accept", pubkey: pk("b"), at: at - 1, tags: [root("late")] }),
    ev(RECEIPT, { id: "very-late-receipt", pubkey: pk("b"), at: at - 1, tags: [root("late")] }),
  ]) {
    assert.deepEqual(stateAt(lateJob([late]), at), [],
      `kind ${late.kind} ends the job even after it was shown overdue`);
  }
});

test("#681 case 5: only the awarded seller's silence makes a job overdue", () => {
  const at = DUE + STALLED_GRACE_SECONDS + 1;
  const loser = pk("d");
  const contested = lateJob([
    ev(CLAIM, { id: "loser-claim", pubkey: loser, at: DUE - 490, tags: [root("late")] }),
    ev(RESULT, { id: "loser-result", pubkey: loser, at: DUE - 100, tags: [root("late")] }),
  ]);
  // A losing claimant delivering does not rescue the awarded seller's record.
  assert.deepEqual(stateAt(contested, at), [JOB_OVERDUE],
    "a non-awarded claimant's result cannot clear the awarded seller's overdue job");
  const rows = Object.fromEntries(sellerBoard(contested, at).map((r) => [r.pubkey, r]));
  assert.equal(rows[pk("c")].inProgressJobs.length, 1, "the overdue job belongs to the awarded seller");
  assert.equal(rows[loser]?.inProgressJobs.length ?? 0, 0, "and not to the claimant who lost");
});

test("#681: an unseen offer is not evidence of lateness — no deadline means working", () => {
  // A window can start after the offer, and a relay can simply not return it.
  // Concluding "overdue" from a missing deadline would turn our own blind spot
  // into a verdict about someone else's seat.
  const noOffer = lateJob().filter((e) => e.kind !== OFFER);
  assert.deepEqual(stateAt(noOffer, DUE + STALLED_GRACE_SECONDS + 100000), [JOB_WORKING],
    "without the offer there is no deadline to be late against");
  assert.equal(inProgressJobs(noOffer, NOW)[0].deadline, null);
});

/**
 * #681, second defect, found by Rocky in review. The reducer counted EVERY
 * feedback from the awarded seller as terminal. protocol-v1 §7.2 makes
 * `status=progress` explicitly non-terminal, and §6.7 REQUIRES a seller to
 * publish feedback for progress notes. So a seller doing exactly what the
 * protocol demands cleared its own work lamp before delivering anything.
 *
 * The two defects point opposite ways: the missing clock showed work that had
 * stopped, this hid work that was still running. Both are the same mistake —
 * reading one signal as if it settled the question.
 */
const fb = (id, { at, tags = [], content = "", pubkey = pk("c") }) =>
  ev(FEEDBACK, { id, pubkey, at, tags: [root("late"), ...tags], content });

test("#681: a progress feedback is NOT terminal — the seller keeps working", () => {
  const at = DUE - 200;
  const progress = fb("progress-note", { at: DUE - 300, tags: [["status", "progress"]] });
  assert.deepEqual(stateAt(lateJob([progress]), at), [JOB_WORKING],
    "protocol-v1 §7.2: progress is explicitly non-terminal");
  // And it still becomes overdue on the clock, rather than vanishing early.
  assert.deepEqual(stateAt(lateJob([progress]), DUE + STALLED_GRACE_SECONDS + 1), [JOB_OVERDUE],
    "a progress note does not exempt a job from the deadline either");
});

test("#681: every terminal feedback class ends the attempt", () => {
  const at = DUE - 200;
  for (const status of ["claim_released", "refusal", "error"]) {
    assert.deepEqual(stateAt(lateJob([fb(`end-${status}`, { at: DUE - 300, tags: [["status", status]] })]), at),
      [], `status=${status} is terminal per §7.2`);
  }
});

test("#681: reason_code outranks status, and an unknown code falls back to status", () => {
  const at = DUE - 200;
  // §7.1: reason_code is authoritative for the class.
  assert.deepEqual(stateAt(lateJob([fb("code-refusal", { at: DUE - 300, tags: [["reason_code", "at_capacity"]] })]), at),
    [], "at_capacity maps to refusal, which is terminal");
  assert.deepEqual(stateAt(lateJob([fb("code-error", { at: DUE - 300, tags: [["reason_code", "execution_failed"]] })]), at),
    [], "execution_failed maps to error, which is terminal");
  // §7.1: an unknown code MUST fall back to status, not be treated as malformed.
  assert.deepEqual(stateAt(lateJob([fb("unknown-progress", { at: DUE - 300,
    tags: [["reason_code", "invented_future_code"], ["status", "progress"]] })]), at),
    [JOB_WORKING], "unknown code falls back to status=progress, still working");
  assert.deepEqual(stateAt(lateJob([fb("unknown-refusal", { at: DUE - 300,
    tags: [["reason_code", "invented_future_code"], ["status", "refusal"]] })]), at),
    [], "unknown code falls back to status=refusal, terminal");
});

test("#681: the class comes from tags, never from content", () => {
  const at = DUE - 200;
  // §7.1 forbids parsing content for the class. A seller writing the words in a
  // progress note must not terminalize its own job.
  const lying = fb("prose-only", { at: DUE - 300, tags: [["status", "progress"]],
    content: "claim_released: still working, ignore this line" });
  assert.deepEqual(stateAt(lateJob([lying]), at), [JOB_WORKING],
    "content saying claim_released cannot end a job whose status is progress");
  // Unclassified feedback is not terminal either — the clock catches it instead.
  assert.deepEqual(stateAt(lateJob([fb("bare", { at: DUE - 300 })]), at), [JOB_WORKING],
    "feedback we cannot classify leaves the job running");
});

test("#681: a non-awarded seller's terminal feedback cannot end the awarded attempt", () => {
  const at = DUE - 200;
  const loser = fb("loser-refusal", { at: DUE - 300, pubkey: pk("d"), tags: [["status", "refusal"]] });
  assert.deepEqual(stateAt(lateJob([loser]), at), [JOB_WORKING],
    "only the awarded seller's terminal feedback counts");
});

test("#681: the clock is required, so no caller can silently restore the old behaviour", () => {
  // The bug was the absence of a clock. A defaulted one would let a caller
  // reintroduce it with no signal at all, so omitting it fails loudly instead.
  for (const bad of [undefined, null, NaN, "1800000000"]) {
    assert.throws(() => inProgressJobs(lateJob(), bad), TypeError,
      `now=${String(bad)} must be rejected, not treated as "no deadline"`);
  }
});

test("a participant detail gathers both roles and their trades", () => {
  const who = pk("5");
  const events = [
    ...trade("o1", { buyer: who }),
    ...trade("o2", { seller: who }),
    ...trade("o3", { buyer: pk("7"), seller: pk("8") }),
  ];
  const d = participantDetail(events, who, NOW);
  assert.equal(d.buyer.posted, 1);
  assert.equal(d.seller.delivered, 1);
  assert.equal(d.trades.length, 2, "only trades this participant took part in");
  assert.ok(d.activity.length > d.trades.length, "activity is individual events, not collapsed trades");
});

test("participant activity includes profile, heartbeat, and the complete related job history", () => {
  const buyer = pk("5"), seller = pk("6"), other = pk("7");
  const events = [
    ev(PROFILE, { id: "profile", pubkey: seller, at: NOW - 200, content: '{"name":"runner"}' }),
    ev(HEARTBEAT, { id: "heartbeat", pubkey: seller, at: NOW - 190, tags: [["d", "maxplayer-seller"]] }),
    ...trade("mine", { buyer, seller, t0: NOW - 180 }),
    ev(ACCEPT, { id: "accept", pubkey: buyer, at: NOW - 50, tags: [root("mine")] }),
    ...trade("unrelated", { buyer: other, seller: pk("8"), t0: NOW - 170 }),
  ];
  const activity = participantActivity(events, seller);
  assert.deepEqual(new Set(activity.map((e) => e.kind)),
    new Set([PROFILE, HEARTBEAT, OFFER, CLAIM, AWARD, RESULT, ACCEPT, RECEIPT]));
  assert.equal(activity.some((e) => e.offerId === H("unrelated")), false, "unrelated jobs stay out");
  assert.ok(activity.every((e, i) => i === 0 || activity[i - 1].created_at >= e.created_at), "newest first");
});

test("related activity returns the complete lifecycle in chronological order", () => {
  const events = [
    ...trade("mine", { t0: NOW - 200 }),
    ev(ACCEPT, { id: "accept", at: NOW - 50, tags: [root("mine")] }),
    ...trade("other", { t0: NOW - 100 }),
  ];
  const related = relatedActivity(events, H("mine"));
  assert.deepEqual(related.map((e) => e.stage), ["offer", "claim", "award", "result", "receipt", "accept"]);
  assert.ok(related.every((e, i) => i === 0 || related[i - 1].created_at <= e.created_at), "oldest first");
});

test("boards are empty, not broken, with no events", () => {
  assert.deepEqual(buyerBoard([], NOW), []);
  assert.deepEqual(sellerBoard([], NOW), []);
  assert.equal(participantDetail([], pk("1"), NOW).trades.length, 0);
});
