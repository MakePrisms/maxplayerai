/**
 * The BTC-USD quote is the one third-party value on the render path, and every
 * dollar figure on the page derives from it. A wrong rate is not a degraded
 * display — it is a wrong number presented as a fact.
 *
 * Dollars remain the display unit and the "…" loading behaviour is unchanged;
 * this pins the guard only.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { fetchSpot, isPlausibleRate, spotUsd, usd } from "../src/ui/spot.js";

const quote = (amount: unknown): typeof fetch =>
  (async () => new Response(JSON.stringify({ data: { amount } }), { status: 200 })) as unknown as typeof fetch;

test("`> 0` is not a rate check — the implausible is refused", () => {
  for (const bad of [0, -1, 0.00001, 1e12, Number.NaN, Number.POSITIVE_INFINITY]) {
    assert.equal(isPlausibleRate(bad), false, `${bad} must be refused`);
  }
  // The band is a garbage filter, not a forecast: anything a real market could
  // print has to pass, in both directions.
  for (const ok of [1_000, 27_500, 118_432.19, 10_000_000]) {
    assert.equal(isPlausibleRate(ok), true, `${ok} must be accepted`);
  }
});

test("a garbage quote never becomes a displayed figure", async () => {
  // Order matters: no rate has been accepted yet, so this proves the bad quote
  // does not become the FIRST rate. usd() must still be withholding.
  await fetchSpot(quote("0.00000001"));
  assert.equal(spotUsd(), null, "an absurd quote is not adopted");
  assert.equal(usd(100_000), "…", "and no dollar figure is invented from it");

  await fetchSpot(quote("999999999999"));
  assert.equal(spotUsd(), null, "nor is one at the other extreme");

  await fetchSpot(quote(undefined));
  assert.equal(spotUsd(), null, "a missing field parses to NaN and is refused");
});

test("a real quote is adopted and renders", async () => {
  await fetchSpot(quote("118432.19"));
  assert.equal(spotUsd(), 118_432.19);
  assert.equal(usd(100_000), "$118.43", "sats convert at the live rate, dollars unchanged");
});

test("a later garbage quote cannot overwrite a good rate", async () => {
  const good = spotUsd();
  assert.ok(good, "precondition: a good rate is held");
  await fetchSpot(quote("0"));
  assert.equal(spotUsd(), good, "the last known rate survives a bad refresh");
});
