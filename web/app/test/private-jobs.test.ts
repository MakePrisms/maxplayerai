import assert from "node:assert/strict";
import test from "node:test";
import { parseEvent, type RawEvent } from "../src/model/events.js";
let id = 0x9900;
const offer = (tags: string[][]): RawEvent => ({
  id: (++id).toString(16).padStart(64, "0"), pubkey: "aa".repeat(32), kind: 3401,
  created_at: 1_800_000_000, content: "ciphertext-is-not-a-summary",
  tags: [["t", "maxplayer"], ["v", "2"], ...tags],
});
test("public market never renders a targeted private task or injected input", () => {
  const parsed = parseEvent(offer([["visibility", "private"], ["discovery", "targeted"], ["p", "bb".repeat(32)], ["i", "injected task"]]));
  assert.equal(parsed?.description, "Private task");
  assert.equal(parsed?.executionVisibility, "private");
});
test("open private discovery shows only the deliberate public task", () => {
  const tags = [["visibility", "private"], ["discovery", "open"]];
  assert.equal(parseEvent(offer([...tags, ["i", JSON.stringify({schema:"maxplayer.public-task.v2", text:"Public discovery task", requested_output:"text/plain", dispatch:{}})]]))?.description, "Public discovery task");
  for (const raw of ["malformed", '{"schema":"other","text":"injected"}']) {
    assert.equal(parseEvent(offer([...tags, ["i", raw]]))?.description, "Public task unavailable");
  }
  assert.equal(parseEvent(offer([...tags, ["p", "bb".repeat(32)], ["i", "injected"]]))?.description, "Private task");
});
test("explicit public v2 offers keep their public description", () => {
  const parsed = parseEvent(offer([["visibility", "public"], ["i", "Public task"]]));
  assert.equal(parsed?.description, "Public task");
  assert.equal(parsed?.executionVisibility, "public");
});
