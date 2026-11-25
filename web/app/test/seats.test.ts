// The seller heartbeat's live load (`in_flight`, falling back to the deprecated `queue_depth`) and
// its seats (`slots` + `free_slots`), as the network page reads them.
import assert from "node:assert/strict";
import { test } from "node:test";

import { parseEvent, type RawEvent } from "../src/model/events.js";
import { HEARTBEAT } from "../src/model/kinds.js";
import { sellerBoard } from "../src/market/participants.js";
import { inFlightFromTags, seatSlotsFromTags } from "../src/model/seats.mjs";

const SELLER = "d".repeat(64);
let idCounter = 0x5e47;
const id = (): string => (idCounter++).toString(16).padStart(64, "0");
const beat = (created_at: number, tags: string[][]): RawEvent =>
  ({ id: id(), kind: HEARTBEAT, pubkey: SELLER, created_at, tags: [["d", "maxplayer-seller"], ["accepting", "y"], ...tags] });
const T0 = 1_700_000_000;

test("in_flight is preferred, queue_depth is the fallback", () => {
  assert.equal(inFlightFromTags([["in_flight", "2"], ["queue_depth", "2"]]), 2);
  assert.equal(inFlightFromTags([["in_flight", "2"], ["queue_depth", "9"]]), 2, "in_flight wins");
  assert.equal(inFlightFromTags([["queue_depth", "4"]]), 4, "an older seat publishes only queue_depth");
  assert.equal(inFlightFromTags([["in_flight", "x"], ["queue_depth", "3"]]), 3, "malformed in_flight falls back");
  assert.equal(inFlightFromTags([]), null);
  assert.equal(inFlightFromTags(undefined), null);
});

test("the seat pair is read together; absent or malformed is unknown", () => {
  assert.deepEqual(seatSlotsFromTags([["slots", "3"], ["free_slots", "1"]]), { total: 3, free: 1 });
  assert.deepEqual(seatSlotsFromTags([["slots", "2"], ["free_slots", "0"]]), { total: 2, free: 0 });
  assert.deepEqual(seatSlotsFromTags([["slots", " 3 "], ["free_slots", "3"]]), { total: 3, free: 3 });
  assert.equal(seatSlotsFromTags([]), null);
  assert.equal(seatSlotsFromTags([["slots", "3"]]), null, "slots alone");
  assert.equal(seatSlotsFromTags([["free_slots", "1"]]), null, "free_slots alone");
  assert.equal(seatSlotsFromTags([["slots", "0"], ["free_slots", "0"]]), null, "slots=0");
  assert.equal(seatSlotsFromTags([["slots", "2"], ["free_slots", "3"]]), null, "free > slots");
  for (const bad of ["", "-1", "+1", "1.5", "a", "1 2"]) {
    assert.equal(seatSlotsFromTags([["slots", "3"], ["free_slots", bad]]), null, JSON.stringify(bad));
    assert.equal(seatSlotsFromTags([["slots", bad], ["free_slots", "0"]]), null, JSON.stringify(bad));
  }
});

test("the network page takes load and seats from the newest beat", () => {
  const p = parseEvent(beat(T0, [["in_flight", "1"], ["queue_depth", "1"], ["slots", "3"], ["free_slots", "1"]]));
  assert.equal(p?.queueDepth, 1);
  assert.deepEqual(p?.seats, { total: 3, free: 1 });
  // The page reads the NEW tag: a beat whose two counts disagree, and one carrying only in_flight.
  assert.equal(parseEvent(beat(T0, [["in_flight", "1"], ["queue_depth", "5"]]))?.queueDepth, 1, "in_flight wins");
  assert.equal(parseEvent(beat(T0, [["in_flight", "2"]]))?.queueDepth, 2, "in_flight alone is read");

  const rows = sellerBoard([
    beat(T0, [["in_flight", "1"], ["queue_depth", "1"], ["slots", "3"], ["free_slots", "1"]]),
    beat(T0 + 60, [["in_flight", "0"], ["queue_depth", "0"], ["slots", "3"], ["free_slots", "0"]]),
  ], T0 + 120);
  const seat = rows.find((r) => r.pubkey === SELLER);
  assert.equal(seat?.queueDepth, 0);
  assert.deepEqual(seat?.seats, { total: 3, free: 0 }, "in_flight 0 with every seat reserved");

  const old = sellerBoard([beat(T0 + 200, [["queue_depth", "2"]])], T0 + 220).find((r) => r.pubkey === SELLER);
  assert.equal(old?.queueDepth, 2, "an older seat's queue_depth still reads");
  assert.equal(old?.seats, null, "an older seat states no seats");
});
