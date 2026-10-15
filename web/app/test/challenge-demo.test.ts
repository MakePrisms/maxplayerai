import assert from "node:assert/strict";
import test from "node:test";
import { demoSnapshot } from "../src/challenge/demo.js";
import { parseSnapshot, sellerLine } from "../src/challenge/live.js";

test("demo snapshot passes the same validation as live data and counts running jobs up", () => {
  const now = 1_792_100_000;
  const s = parseSnapshot(demoSnapshot(now));
  assert.equal(s.updates[0], "Demo data: not a real challenge.");
  const crunch = s.sellers.find((a) => a.name === "cipher-crunch")!;
  assert.match(sellerLine(s, crunch, now), /^32 of 60 min · 1 running$/);
  assert.match(sellerLine(s, crunch, now + 60), /^33 of 60 min/);
  assert.equal(s.sellers.filter((a) => a.eligible).length, 4);
});
