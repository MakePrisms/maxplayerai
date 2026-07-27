import assert from "node:assert/strict";
import { test } from "node:test";

import { createCache } from "../js/cache.js";
import { HEARTBEAT, PROFILE, RECEIPT } from "../js/kinds.js";

const pk = (c) => c.repeat(64);
const ev = (kind, { id, pubkey = pk("a"), at = 1000, tags = [] }) =>
  ({ id, kind, pubkey, created_at: at, tags, content: "" });

test("an event is stored once", () => {
  const cache = createCache();
  const e = ev(RECEIPT, { id: "r1" });
  assert.equal(cache.ingest(e).stored, true);
  assert.equal(cache.ingest(e).stored, false, "second delivery is a duplicate");
  assert.equal(cache.size, 1);
});

test("malformed events are rejected without throwing", () => {
  const cache = createCache();
  for (const bad of [null, undefined, {}, { id: 1, kind: 3400 }, { id: "x" }]) {
    assert.equal(cache.ingest(bad).stored, false);
  }
  assert.equal(cache.size, 0);
});

test("an addressable event resolves by author+kind+d, newest wins", () => {
  const cache = createCache();
  const older = ev(HEARTBEAT, { id: "hb1", at: 1000, tags: [["d", "seat-1"], ["status", "idle"]] });
  const newer = ev(HEARTBEAT, { id: "hb2", at: 2000, tags: [["d", "seat-1"], ["status", "busy"]] });

  cache.ingest(older);
  cache.ingest(newer);

  assert.equal(cache.size, 1, "the superseded copy is evicted, not accumulated");
  assert.equal(cache.slot(HEARTBEAT, pk("a"), "seat-1").id, "hb2");
  assert.equal(cache.has("hb1"), false);
});

test("a stale replacement is ignored, and a tie keeps the incumbent", () => {
  const cache = createCache();
  cache.ingest(ev(HEARTBEAT, { id: "hb2", at: 2000, tags: [["d", "s"]] }));
  assert.equal(cache.ingest(ev(HEARTBEAT, { id: "hb1", at: 1000, tags: [["d", "s"]] })).stored, false);
  assert.equal(cache.ingest(ev(HEARTBEAT, { id: "hb3", at: 2000, tags: [["d", "s"]] })).stored, false);
  assert.equal(cache.slot(HEARTBEAT, pk("a"), "s").id, "hb2");
});

test("different d values are different slots for the same author", () => {
  const cache = createCache();
  cache.ingest(ev(HEARTBEAT, { id: "a1", at: 1000, tags: [["d", "seat-1"]] }));
  cache.ingest(ev(HEARTBEAT, { id: "a2", at: 1000, tags: [["d", "seat-2"]] }));
  assert.equal(cache.size, 2);
  assert.equal(cache.slot(HEARTBEAT, pk("a"), "seat-1").id, "a1");
  assert.equal(cache.slot(HEARTBEAT, pk("a"), "seat-2").id, "a2");
});

test("a replaceable profile is keyed by author alone", () => {
  const cache = createCache();
  cache.ingest(ev(PROFILE, { id: "p1", at: 1000 }));
  cache.ingest(ev(PROFILE, { id: "p2", at: 2000 }));
  assert.equal(cache.size, 1);
  assert.equal(cache.slot(PROFILE, pk("a")).id, "p2");
});

test("the cache tracks its span so paging knows where to resume", () => {
  const cache = createCache();
  assert.equal(cache.oldest, null);
  cache.ingest(ev(RECEIPT, { id: "r1", at: 5000 }));
  cache.ingest(ev(RECEIPT, { id: "r2", at: 1000 }));
  cache.ingest(ev(RECEIPT, { id: "r3", at: 9000 }));
  assert.equal(cache.oldest, 1000);
  assert.equal(cache.newest, 9000);
});

test("byKinds returns matching events newest first", () => {
  const cache = createCache();
  cache.ingest(ev(RECEIPT, { id: "r1", at: 1000 }));
  cache.ingest(ev(RECEIPT, { id: "r2", at: 3000 }));
  cache.ingest(ev(PROFILE, { id: "p1", at: 2000 }));
  assert.deepEqual(cache.byKinds([RECEIPT]).map((e) => e.id), ["r2", "r1"]);
});
