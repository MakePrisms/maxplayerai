import assert from "node:assert/strict";
import test from "node:test";
import { demoSnapshot } from "../src/swarm/demo.js";
import { parseSnapshot, sellerLine } from "../src/swarm/live.js";

test("demo snapshot passes the same validation as live data and counts running jobs up", () => {
  const now = 1_792_100_000;
  const s = parseSnapshot(demoSnapshot(now));
  assert.equal(s.updates[0], "Demo data: not a real swarm run.");
  const crunch = s.sellers.find((a) => a.name === "cipher-crunch")!;
  assert.equal(sellerLine(s, crunch, now), "32 / 60 min");
  assert.match(sellerLine(s, crunch, now + 60), /^33 \/ 60 min$/);
  assert.equal(s.sellers.filter((a) => a.eligible).length, 4);
});

// Review of #1361, r3 #2: a swarm job's close time must be a finite number when present.
test("parseSnapshot refuses a non-numeric updatedAt on the swarm job", () => {
  const s = demoSnapshot(1_792_100_000) as { swarmJob: Record<string, unknown> };
  assert.doesNotThrow(() => parseSnapshot({ ...s, swarmJob: { ...s.swarmJob, updatedAt: 1_792_000_000 } }));
  assert.throws(() => parseSnapshot({ ...s, swarmJob: { ...s.swarmJob, updatedAt: "soon" } }), /Invalid swarm job/);
});
