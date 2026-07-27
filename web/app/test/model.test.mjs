import assert from "node:assert/strict";
import { test } from "node:test";

import { feedbackReason, parseEvent, rootOfferId } from "../js/model.js";
import { AWARD, CLAIM, FEEDBACK, HEARTBEAT, OFFER, PROFILE, RECEIPT, RESULT } from "../js/kinds.js";

const pk = (c) => c.repeat(64);
const ev = (kind, { id = "x", pubkey = pk("a"), at = 1000, tags = [], content = "" }) =>
  ({ id, kind, pubkey, created_at: at, tags, content });

test("an offer is its own trade root and carries its price", () => {
  const p = parseEvent(ev(OFFER, { id: "o1", tags: [["amount", "21", "sat"], ["p", pk("c")]], content: "do a thing" }));
  assert.equal(p.stage, "offer");
  assert.equal(p.offerId, "o1");
  assert.equal(p.buyer, pk("a"));
  assert.equal(p.amount, 21);
  assert.equal(p.targetSeller, pk("c"));
  assert.equal(p.description, "do a thing");
});

test("rootOfferId prefers the root marker over an incidental e-tag", () => {
  assert.equal(rootOfferId({ tags: [["e", "claim-id"], ["e", "offer-id", "", "root"]] }), "offer-id");
  assert.equal(rootOfferId({ tags: [["e", "only-one"]] }), "only-one", "a lone e-tag is the root");
  assert.equal(rootOfferId({ tags: [] }), null);
});

test("an award keyed off the winning claim would split a trade — it does not", () => {
  const p = parseEvent(ev(AWARD, { id: "a1", tags: [["e", "claim-1"], ["e", "offer-1", "", "root"]] }));
  assert.equal(p.offerId, "offer-1");
  assert.equal(p.stage, "award");
});

test("a claim reports its seller and whether it carries a payment request", () => {
  const withReq = parseEvent(ev(CLAIM, { pubkey: pk("c"), tags: [["e", "o1", "", "root"], ["creq", "creq..."], ["status", "processing"]] }));
  assert.equal(withReq.seller, pk("c"));
  assert.equal(withReq.hasPaymentRequest, true);
  assert.equal(withReq.status, "processing");

  const without = parseEvent(ev(CLAIM, { tags: [["e", "o1", "", "root"]] }));
  assert.equal(without.hasPaymentRequest, false);
});

test("a receipt carries the settled amount against its offer", () => {
  const p = parseEvent(ev(RECEIPT, { tags: [["e", "o1", "", "root"], ["amount", "1505", "sat"]] }));
  assert.equal(p.stage, "receipt");
  assert.equal(p.offerId, "o1");
  assert.equal(p.amount, 1505);
});

test("a result belongs to its offer and names the delivering seller", () => {
  const p = parseEvent(ev(RESULT, { pubkey: pk("c"), tags: [["e", "o1", "", "root"]] }));
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
  for (const bad of [null, undefined, {}, { kind: OFFER }, { id: "x", kind: OFFER, pubkey: pk("a") }]) {
    assert.equal(parseEvent(bad), null);
  }
});
