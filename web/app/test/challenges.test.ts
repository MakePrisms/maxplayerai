import assert from "node:assert/strict";
import test from "node:test";
import { buyerHex, joinState, validateChallenge, whitelistInstruction, type Challenge } from "../src/challenge/model.js";
import { npubEncode } from "../src/trade/nip19.js";
import { challenge } from "../src/challenge/content.js";

const hex = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
const fixture: Challenge = {
  title: "Example challenge", goal: "Two sellers research a problem.", buyerNpub: npubEncode(hex),
  startsAt: "2026-10-16T19:30:00.000Z",
  budgetSats: 6000, status: "scheduled", update: "Example only, not a live challenge.",
};

test("published content is either unannounced or complete and valid", () => {
  if (challenge) validateChallenge(challenge);
});

test("whitelist instruction preserves config and uses hex for the exact displayed npub", () => {
  assert.equal(buyerHex(fixture.buyerNpub), hex);
  assert.match(whitelistInstruction(fixture), new RegExp(hex));
  assert.match(whitelistInstruction(fixture), /without replacing other entries/);
  assert.match(whitelistInstruction(fixture), /not only this challenge/);
  assert.throws(() => buyerHex(fixture.buyerNpub.slice(0, -1) + "x"));
  assert.throws(() => buyerHex("nsec1invalid"));
  assert.throws(() => buyerHex(npubEncode("00".repeat(32))), "checksum alone does not prove a usable buyer key");
});

test("joining remains open before and after work starts", () => {
  const open = Date.parse(fixture.startsAt);
  assert.equal(joinState(fixture, open - 1), "Open to join");
  assert.equal(joinState(fixture, open), "Open to join");
  assert.equal(joinState(fixture, open + 3_600_000 - 1), "Open to join");
  assert.equal(joinState(fixture, open + 3_600_000), "Open to join");
  assert.equal(joinState({ ...fixture, status: "completed" }, open), "Open to join");
  assert.equal(joinState({ ...fixture, status: "cancelled" }, open), "Cancelled");
});

test("invalid money and ambiguous dates cannot publish; zero budget is not hidden", () => {
  assert.equal(validateChallenge({ ...fixture, budgetSats: 0 }).budgetSats, 0);
  for (const budgetSats of [-1, 1.5, NaN, Number.MAX_SAFE_INTEGER + 1]) {
    assert.throws(() => validateChallenge({ ...fixture, budgetSats }));
  }
  for (const startsAt of ["2026-10-16T19:30", "2026-02-30T19:30:00.000Z"]) {
    assert.throws(() => validateChallenge({ ...fixture, startsAt }));
  }
});
