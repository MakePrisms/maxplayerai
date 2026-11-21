// Speed review of #1361: the changes keep what they claim.
import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import { createEngine } from "../src/market/engine.js";
import { PAGE_SIZE } from "../src/source/relay.js";

test("#3: files with a ?v= stamp are cached for a year; nothing else gets that header", () => {
  const { headers } = JSON.parse(readFileSync(new URL("../vercel.json", import.meta.url), "utf8"));
  const rules = headers.filter((h: { headers: { key: string }[] }) => h.headers.some((x) => x.key === "Cache-Control"));
  assert.equal(rules.length, 1);
  assert.deepEqual(rules[0].has, [{ type: "query", key: "v" }]);
  assert.equal(rules[0].headers[0].value, "public, max-age=31536000, immutable");
});

test("#5: history pages are 1000 events, the relay's cap", () => assert.equal(PAGE_SIZE, 1000));

test("#1: an engine with shifts off computes no climbs or overtakes", () => {
  const engine = createEngine({ shifts: false });
  engine.flush();
  const v = engine.view()!;
  assert.equal(v.buyerClimbs.size, 0);
  assert.equal(v.sellerClimbs.size, 0);
  assert.deepEqual(v.overtakes, []);
});
