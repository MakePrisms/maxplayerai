// Speed review of #1361: the changes keep what they claim.
import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import { createEngine } from "../src/market/engine.js";
import { PAGE_SIZE } from "../src/source/relay.js";

test("#3: only content-stamped assets are cached for a year; never a page or an unstamped file", () => {
  const { headers } = JSON.parse(readFileSync(new URL("../vercel.json", import.meta.url), "utf8"));
  const rules = headers.filter((h: { headers: { key: string }[] }) => h.headers.some((x) => x.key === "Cache-Control"));
  assert.equal(rules.length, 1);
  const [rule] = rules;
  assert.equal(rule.headers[0].value, "public, max-age=31536000, immutable");
  const path = new RegExp(`^${rule.source}$`), stamp = new RegExp(rule.has[0].value);
  assert.equal(rule.has[0].key, "v");
  for (const p of ["/site.css", "/market.js", "/fonts/saira-normal-400-100.woff2", "/assets/agents/codex.webp", "/assets/muse.jpg", "/assets/agents/openclaw.svg", "/assets/instinct.png"])
    assert.ok(path.test(p), p);
  for (const p of ["/", "/market", "/swarm", "/index.html", "/skill.md", "/snapshot.json", "/swarm/naddr1x"]) assert.ok(!path.test(p), p);
  assert.ok(stamp.test("0123456789ab"));
  for (const v of ["1", "3", "0123456789abc", "0123456789AB", "x0123456789ab"]) assert.ok(!stamp.test(v), v);
});

test("#5: history pages are 1000 events, the relay's cap", () => assert.equal(PAGE_SIZE, 1000));

