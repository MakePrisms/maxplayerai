/** src/site/styled.ts: the page scripts wait for the stylesheets, but never for good. */
import assert from "node:assert/strict";
import { test } from "node:test";
import { stylesLoaded } from "../src/site/styled.js";

function fakeLinks(links: { sheet: unknown }[]): void {
  const withEvents = links.map((l) => ({ ...l, addEventListener: () => { /* a failed link never fires again */ } }));
  (globalThis as { document?: unknown }).document = { querySelectorAll: () => withEvents };
}

test("a stylesheet that already failed does not hold the page script for good", async () => {
  fakeLinks([{ sheet: {} }, { sheet: null }]);
  const t0 = Date.now();
  await stylesLoaded(30);
  assert.ok(Date.now() - t0 < 1000, "it resolves at the cap");
});

test("nothing pending resolves at once", async () => {
  fakeLinks([{ sheet: {} }]);
  await stylesLoaded(60_000);
});
