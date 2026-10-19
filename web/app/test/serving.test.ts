// R3-2: the seller heartbeat's `serving` / `status=not_serving` statement, as every reader sees it.
import assert from "node:assert/strict";
import { test } from "node:test";

import { parseEvent, type RawEvent } from "../src/model/events.js";
import { HEARTBEAT } from "../src/model/kinds.js";
import { sellerBoard } from "../src/market/participants.js";
import { parseServingCount, servingFromTags } from "../src/model/serving.mjs";

const SELLER = "c".repeat(64);
let idCounter = 0x3232;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");
const beat = (created_at: number, tags: string[][]): RawEvent =>
  ({ id: id(), kind: HEARTBEAT, pubkey: SELLER, created_at, tags: [["d", "maxplayer-seller"], ["accepting", "y"], ...tags] });
const T0 = 1_700_000_000;

test("each serving state is read off the beat", () => {
  assert.deepEqual(servingFromTags([["serving", "1/1"]]), { count: { available: 1, configured: 1 }, state: "serving" });
  assert.deepEqual(servingFromTags([["serving", "1/2"]]), { count: { available: 1, configured: 2 }, state: "serving" });
  assert.deepEqual(servingFromTags([["serving", "0/1"], ["status", "not_serving"]]),
    { count: { available: 0, configured: 1 }, state: "not_serving" });
  // Either signal alone is enough.
  assert.equal(servingFromTags([["serving", "0/2"]]).state, "not_serving");
  assert.equal(servingFromTags([["status", "not_serving"]]).state, "not_serving");
  assert.equal(servingFromTags([["serving", "1/1"], ["status", "not_serving"]]).state, "not_serving",
    "a stated not_serving wins over a contradicting count");
});

test("absent or malformed is unknown, never not serving", () => {
  assert.deepEqual(servingFromTags([]), { count: null, state: "unknown" });
  assert.deepEqual(servingFromTags(undefined), { count: null, state: "unknown" });
  for (const bad of ["", "1", "1/", "/1", "2/1", "0/0", "a/b", "-1/2", "1/2/3", " 1 / 2", "+1/2"]) {
    assert.equal(parseServingCount(bad), null, JSON.stringify(bad));
    assert.equal(servingFromTags([["serving", bad]]).state, "unknown", JSON.stringify(bad));
  }
  assert.deepEqual(parseServingCount(" 1/2 "), { available: 1, configured: 2 });
  assert.equal(servingFromTags([["status", "something_else"]]).state, "unknown");
});

test("the network page parses serving and takes it from the newest beat", () => {
  const p = parseEvent(beat(T0, [["serving", "0/1"], ["status", "not_serving"]]));
  assert.deepEqual(p?.serving, { available: 0, configured: 1 });
  assert.equal(p?.servingState, "not_serving");

  const rows = sellerBoard([
    beat(T0, [["serving", "0/1"], ["status", "not_serving"]]),
    beat(T0 + 60, [["serving", "1/1"]]),
  ], T0 + 120);
  const seat = rows.find((r) => r.pubkey === SELLER);
  assert.deepEqual(seat?.serving, { available: 1, configured: 1 });
  assert.equal(seat?.servingState, "serving");

  const old = sellerBoard([beat(T0 + 200, [])], T0 + 220).find((r) => r.pubkey === SELLER);
  assert.equal(old?.serving, null, "an older seat states no count");
  assert.equal(old?.servingState, "unknown");
});
