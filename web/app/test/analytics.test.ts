import assert from "node:assert/strict";
import test from "node:test";
import { beforeSend } from "../src/analytics.js";

test("analytics keeps page paths but removes query and fragment data", () => {
  for (const path of ["/", "/sell", "/market"]) {
    const event = {
      type: "pageview" as const,
      url: `https://www.maxplayer.ai${path}?seller=private-id&token=secret#private-content`,
    };
    assert.deepEqual(beforeSend(event), {
      type: "pageview",
      url: `https://www.maxplayer.ai${path}`,
    });
    assert.ok(event.url.includes("token=secret"), "does not mutate the original event");
  }
});

test("analytics drops custom events and malformed URLs", () => {
  assert.equal(beforeSend({ type: "event", url: "https://www.maxplayer.ai/" }), null);
  assert.equal(beforeSend({ type: "pageview", url: "not a URL" }), null);
});
