import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { parseEvent } from "../js/parse.js";
import {
  buyerMetrics,
  relationshipPairs,
  resolveHeartbeats,
  resolveLiveness,
  sellerMetrics,
} from "../js/profiles.js";
import { groupEvents } from "../js/jobs.js";
import { HEARTBEAT, OFFER } from "../js/kinds.js";

/* Real relay chains for a heavy buyer (B) and a productive seller (S), recorded from
 * wss://relay.example. Note: the relay serves NO kind-30340 heartbeats yet, so
 * heartbeat liveness is exercised with synthetic events in its own test below. */
const RAW = JSON.parse(
  readFileSync(new URL("./fixtures/profiles.json", import.meta.url), "utf8"),
);
const NORMALIZED = RAW.map(parseEvent).filter(Boolean);

const B = "228493830039fc67240325e2e9b79b7b7b4b530281de2147a6449c5bd306d8af";
const S = "8784aec6ae3bdbaeec9c865d215c0f20a2267c6b97d1ed2771d55f5a3d15d58c";

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
const NOW = Math.max(...market.map((e) => e.created_at)) + 60;

test("seller metrics: completed jobs, sats earned, refusal rate, delivery time", () => {
  const m = sellerMetrics(market, S, profiles, NOW);
  assert.equal(m.jobsCompleted, 10, "delivered results");
  assert.equal(m.satsEarned, 32, "sats from receipts on delivered jobs");
  assert.equal(m.jobsEngaged, 12);
  assert.equal(m.refusals, 6);
  assert.equal(Number(m.refusalRate.toFixed(3)), 0.5);
  assert.equal(m.deliverySamples, 10);
  assert.equal(m.meanDeliverySec, 70, "mean claim→deliver seconds");
});

test("buyer metrics: jobs posted, sats paid, refusal rate, pay promptness", () => {
  const m = buyerMetrics(market, B, profiles, NOW);
  assert.equal(m.jobsPosted, 24);
  assert.equal(m.satsPaid, 32);
  assert.equal(m.refusals, 15);
  assert.equal(Number(m.refusalRate.toFixed(3)), Number((15 / 24).toFixed(3)));
  assert.equal(m.expiredUnpaid, 0);
  assert.equal(m.paySamples, 8, "jobs the buyer accepted and then paid");
  assert.ok(m.meanPayLatencySec >= 0, "accept→pay latency computed");
});

test("buyer metrics: a synthetic expired-unpaid job is counted", () => {
  const future = "e".repeat(64);
  const past = 1000;
  const offer = parseEvent({
    id: "ab".repeat(32),
    pubkey: B,
    kind: OFFER,
    created_at: past,
    tags: [["i", "task"], ["amount", "5", "sat"], ["param", "deadline", String(past + 10)], ["t", "mobee"], ["v", "2"]],
    content: "",
  });
  const m = buyerMetrics([offer], B, profiles, past + 1000); // now well past the deadline
  assert.equal(m.expiredUnpaid, 1);
  void future;
});

test("relationship pairs: repeat counterparties (2+ trades), single-trade excluded", () => {
  const groups = groupEvents(market);
  const sellerRels = relationshipPairs(groups, S, "seller", profiles);
  const buyerRels = relationshipPairs(groups, B, "buyer", profiles);

  const bFromSeller = sellerRels.find((r) => r.pubkey === B);
  assert.ok(bFromSeller, "buyer B is a repeat counterparty on seller S's profile");
  assert.ok(bFromSeller.trades >= 2);
  assert.equal(bFromSeller.otherRole, "buyer");

  const sFromBuyer = buyerRels.find((r) => r.pubkey === S);
  assert.ok(sFromBuyer, "seller S is a repeat counterparty on buyer B's profile");
  assert.equal(sFromBuyer.trades, bFromSeller.trades, "trade count agrees from both sides");
  assert.equal(sFromBuyer.otherRole, "seller");

  // every returned pair meets the 2+ threshold
  for (const r of [...sellerRels, ...buyerRels]) assert.ok(r.trades >= 2);
});

test("heartbeat liveness: resolved by author+kind+d, newest wins (never by id)", () => {
  const author = "aa".repeat(32);
  const mk = (id, created, d, content) =>
    parseEvent({
      id,
      pubkey: author,
      kind: HEARTBEAT,
      created_at: created,
      tags: [["d", d], ["t", "mobee"], ["status", "online"]],
      content,
    });
  // An OLDER event with a LATER id must not win over the newer one (id order is irrelevant).
  const older = mk("ff".repeat(32), 1000, "seller", "starting up");
  const newer = mk("00".repeat(32), 2000, "seller", "alive");
  const hbs = resolveHeartbeats([older, newer]);
  assert.equal(hbs.get(author).id, newer.id, "newest created_at wins per author+d");
  assert.equal(hbs.get(author).heartbeat.message, "alive");

  const now = 2000;
  assert.equal(resolveLiveness(author, hbs, null, now).state, "live");
  assert.equal(resolveLiveness(author, hbs, null, now + 60 * 60).state, "recent");
  assert.equal(resolveLiveness(author, hbs, null, now + 48 * 60 * 60).state, "stale");
  // heartbeat is preferred over activity as the liveness source
  assert.equal(resolveLiveness(author, hbs, 500, now).source, "heartbeat");
});

test("liveness falls back to last activity when no heartbeat exists", () => {
  const empty = resolveHeartbeats([]);
  const l = resolveLiveness(S, empty, NOW - 60, NOW);
  assert.equal(l.source, "activity");
  assert.equal(l.state, "live");
  const offline = resolveLiveness(S, empty, null, NOW);
  assert.equal(offline.state, "offline");
  assert.equal(offline.source, "none");
});
