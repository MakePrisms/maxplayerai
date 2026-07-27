import assert from "node:assert/strict";
import { test } from "node:test";

import { feedbackReason, parseEvent, rootOfferId } from "../js/model.js";
import { AWARD, CLAIM, FEEDBACK, HEARTBEAT, OFFER, PROFILE, RECEIPT, RESULT } from "../js/kinds.js";


/** Fixtures use readable labels; the wire uses 32 bytes of hex. Map one to the other. */
const _ids = new Map();
const H = (label) => {
  if (!_ids.has(label)) _ids.set(label, (_ids.size + 1).toString(16).padStart(64, "0"));
  return _ids.get(label);
};

const pk = (c) => c.repeat(64);
const ev = (kind, { id = "x", pubkey = pk("a"), at = 1000, tags = [], content = "" }) =>
  ({ id: H(id), kind, pubkey, created_at: at, tags, content });

// Tag shapes below are copied from real offers on the relay, not invented.
// An earlier version of these fixtures supplied a `content` description that
// live offers never carry, so the parser read a field that was always empty
// and the tests still passed.
test("an offer is its own trade root and carries its price and the job", () => {
  const p = parseEvent(ev(OFFER, {
    id: "o1",
    tags: [
      ["i", "Create a file rebind2.txt containing exactly one line"],
      ["output", "text/plain"],
      ["amount", "21", "sat"],
      ["param", "deadline", "1785184881"],
      ["p", pk("c")],
    ],
  }));
  assert.equal(p.stage, "offer");
  assert.equal(p.offerId, H("o1"));
  assert.equal(p.buyer, pk("a"));
  assert.equal(p.amount, 21);
  assert.equal(p.targetSeller, pk("c"));
  assert.equal(p.description, "Create a file rebind2.txt containing exactly one line");
  assert.equal(p.outputType, "text/plain");
  assert.equal(p.deadline, 1785184881);
});

test("the job description comes from the i tag, never from content", () => {
  // Every live offer has empty content; reading it gives a field with no value.
  const p = parseEvent(ev(OFFER, { id: "o1", tags: [["i", "the actual job"]], content: "" }));
  assert.equal(p.description, "the actual job");

  const none = parseEvent(ev(OFFER, { id: "o2", tags: [["amount", "5", "sat"]] }));
  assert.equal(none.description, "", "an offer with no i tag has no description, not undefined");
});

test("a result reports what did the work and how it was handed over", () => {
  const p = parseEvent(ev(RESULT, {
    pubkey: pk("c"),
    tags: [
      ["e", H("o1"), "", "root"], ["delivery", "git"], ["harness", "grok"],
      ["commit", "42b8115deae26731523dbe1686ef00a008b66414"],
      ["amount", "10", "sat"], ["wall_time", "137"],
    ],
  }));
  assert.equal(p.harness, "grok");
  assert.equal(p.deliveryVia, "git");
  assert.equal(p.commit, "42b8115deae26731523dbe1686ef00a008b66414");
  assert.equal(p.wallTimeSeconds, 137);
});

test("rootOfferId prefers the root marker over an incidental e-tag", () => {
  assert.equal(rootOfferId({ tags: [["e", H("claim-id")], ["e", H("offer-id"), "", "root"]] }), H("offer-id"));
  assert.equal(rootOfferId({ tags: [["e", H("only-one")]] }), H("only-one"), "a lone e-tag is the root");
  assert.equal(rootOfferId({ tags: [] }), null);
});

test("an award keyed off the winning claim would split a trade — it does not", () => {
  const p = parseEvent(ev(AWARD, { id: "a1", tags: [["e", H("claim-1")], ["e", H("offer-1"), "", "root"]] }));
  assert.equal(p.offerId, H("offer-1"));
  assert.equal(p.stage, "award");
});

test("a claim reports its seller and whether it carries a payment request", () => {
  const withReq = parseEvent(ev(CLAIM, { pubkey: pk("c"), tags: [["e", H("o1"), "", "root"], ["creq", "creq..."], ["status", "processing"]] }));
  assert.equal(withReq.seller, pk("c"));
  assert.equal(withReq.hasPaymentRequest, true);
  assert.equal(withReq.status, "processing");

  const without = parseEvent(ev(CLAIM, { tags: [["e", H("o1"), "", "root"]] }));
  assert.equal(without.hasPaymentRequest, false);
});

test("a receipt carries the settled amount against its offer", () => {
  const p = parseEvent(ev(RECEIPT, { tags: [["e", H("o1"), "", "root"], ["amount", "1505", "sat"]] }));
  assert.equal(p.stage, "receipt");
  assert.equal(p.offerId, H("o1"));
  assert.equal(p.amount, 1505);
});

test("a result belongs to its offer and names the delivering seller", () => {
  const p = parseEvent(ev(RESULT, { pubkey: pk("c"), tags: [["e", H("o1"), "", "root"]] }));
  assert.equal(p.stage, "result");
  assert.equal(p.seller, pk("c"));
});

test("a feedback reason is the code before the colon, not the prose", () => {
  assert.equal(feedbackReason({ content: "claim_released: the seller withdrew" }), "claim_released");
  assert.equal(feedbackReason({ content: "git_fork_failed" }), "git_fork_failed");
  assert.equal(feedbackReason({ content: "something went badly wrong, at length, with commas" }), "unspecified");
  assert.equal(feedbackReason({ content: "" }), "unspecified");
});

test("a heartbeat exposes the slot it fills", () => {
  const p = parseEvent(ev(HEARTBEAT, { tags: [["d", "seat-1"], ["status", "idle"]] }));
  assert.equal(p.d, "seat-1");
  assert.equal(p.status, "idle");
  assert.equal(p.stage, null, "a heartbeat describes a seller, not a trade");
});

test("a profile parses its metadata and survives malformed json", () => {
  assert.equal(parseEvent(ev(PROFILE, { content: '{"name":"turtle"}' })).name, "turtle");
  assert.equal(parseEvent(ev(PROFILE, { content: "not json at all" })).name, null);
});

test("unmodelled and malformed events yield null rather than throwing", () => {
  assert.equal(parseEvent(ev(1, {})), null, "an unmodelled kind is not our business");
  for (const bad of [null, undefined, {}, { kind: OFFER }, { id: H("x"), kind: OFFER, pubkey: pk("a") }]) {
    assert.equal(parseEvent(bad), null);
  }
});
