/**
 * The deploy bake. Every case here ends in the same question: can this run
 * ship a file that LOOKS like a complete market but is not?
 *
 * Driven through an injectable socket and injectable writes, so the paths that
 * only appear against a real relay under load are reachable in a test.
 */
import assert from "node:assert/strict";
import test from "node:test";
import { bake, collect, writeAtomic, MAX_PAGES } from "../scripts/bake-snapshot.mjs";

/** A relay that answers REQs from a scripted supply of events per filter. */
function fakeRelay({ supply }) {
  const socket = {
    sent: [],
    onopen: null,
    onmessage: null,
    onerror: null,
    onclose: null,
    close() {},
    send(text) {
      const frame = JSON.parse(text);
      socket.sent.push(frame);
      if (frame[0] !== "REQ") return;
      const [, sub, filter] = frame;
      const key = filter.kinds.includes(0) ? "profiles" : "tagged";
      const remaining = supply[key];
      // One page: up to `limit` events, each older than the last.
      const page = remaining.splice(0, filter.limit);
      queueMicrotask(() => {
        for (const e of page) socket.onmessage({ data: JSON.stringify(["EVENT", sub, e]) });
        socket.onmessage({ data: JSON.stringify(["EOSE", sub]) });
      });
    },
  };
  return socket;
}

let seq = 0;
const ev = (created_at) => ({ id: String(++seq).padStart(64, "0"), created_at, sig: "deadbeef" });
/** `n` events, descending in time so `until` paging behaves like a relay. */
const supplyOf = (n, start = 2_000_000_000) => Array.from({ length: n }, (_, i) => ev(start - i));

function run(supply, extra = {}) {
  const socket = fakeRelay({ supply });
  const started = collect({ openSocket: () => socket, ...extra });
  socket.onopen();
  return started.then((result) => ({ result, socket }));
}

test("each stream gets its OWN page budget, so a deep stream cannot starve a sparse one", async () => {
  // THE BUG: one shared `pages` counter. The tagged stream spends the whole
  // allowance, then every later EOSE trips `pages >= MAX_PAGES` immediately —
  // the profiles stream is cut off after a single page and seats lose their
  // display names, while the bake still declares success.
  // The budget must be SPENT for this to mean anything: tagged uses all three
  // pages before draining, so a shared counter would be exhausted by the time
  // profiles starts.
  const { result } = await run({
    tagged: supplyOf(8),    // 2 pages of 4, then an empty page = drained
    profiles: supplyOf(8),  // the same again, on its own budget
  }, { maxPages: 3, streams: [
    { name: "tagged", filter: { kinds: [3401], limit: 4 } },
    { name: "profiles", filter: { kinds: [0], limit: 4 } },
  ] });

  const profiles = result.streams.find((s) => s.name === "profiles");
  assert.equal(profiles.pages, 3, "the profiles stream was paged on its own budget");
  assert.ok(profiles.drained, "and read to exhaustion");
  assert.equal(result.events.length, 16, "every event from both streams is present");
  assert.equal(result.complete, true);
});

test("hitting the page backstop is recorded as TRUNCATED, never as drained", async () => {
  const { result } = await run({
    tagged: supplyOf(100),
    profiles: supplyOf(4),
  }, { maxPages: 2, streams: [
    { name: "tagged", filter: { kinds: [3401], limit: 4 } },
    { name: "profiles", filter: { kinds: [0], limit: 4 } },
  ] });

  const tagged = result.streams.find((s) => s.name === "tagged");
  assert.equal(tagged.pages, 2, "it stopped at the backstop");
  assert.equal(tagged.drained, false, "the backstop is not exhaustion");
  assert.equal(result.complete, false, "and the run as a whole is not complete");
});

test("a truncated read WRITES NOTHING — a partial bake is a skip, not a file", async () => {
  // The dangerous outcome: a snapshot missing half the market, shipped as a
  // successful deploy and trusted by every first-time visitor's first paint.
  const writes = [];
  const result = await bake({
    openSocket: () => {
      const s = fakeRelay({ supply: { tagged: supplyOf(100), profiles: supplyOf(4) } });
      queueMicrotask(() => s.onopen());
      return s;
    },
    maxPages: 2,
    streams: [
      { name: "tagged", filter: { kinds: [3401], limit: 4 } },
      { name: "profiles", filter: { kinds: [0], limit: 4 } },
    ],
    destinations: ["would-be-snapshot.json"],
    write: (path, data) => writes.push([path, data]),
    rename: () => {},
  });

  assert.equal(result.written, false);
  assert.deepEqual(writes, [], "nothing was written");
  assert.match(result.detail, /tagged: 2p TRUNCATED/, "the report names which stream was short");
});

test("every subscription is CLOSEd once it has answered", async () => {
  // A relay caps concurrent subscriptions per connection (strfry: 20). A bake
  // runs to tens of pages, so without a CLOSE per page the relay drops the
  // read partway and the script blames a timeout.
  const { socket } = await run({
    tagged: supplyOf(6 * 2),
    profiles: supplyOf(2),
  }, { streams: [
    { name: "tagged", filter: { kinds: [3401], limit: 2 } },
    { name: "profiles", filter: { kinds: [0], limit: 2 } },
  ] });

  const reqs = socket.sent.filter((f) => f[0] === "REQ").map((f) => f[1]);
  const closes = socket.sent.filter((f) => f[0] === "CLOSE").map((f) => f[1]);
  assert.ok(reqs.length >= 4, "several pages were requested");
  assert.deepEqual(closes, reqs, "each REQ is closed, in order, exactly once");
});

test("an unsolicited CLOSED ends the run with its real cause, not a timeout", async () => {
  const socket = {
    sent: [], onopen: null, onmessage: null, onerror: null, onclose: null,
    close() {},
    send(text) {
      const frame = JSON.parse(text);
      socket.sent.push(frame);
      if (frame[0] !== "REQ") return;
      queueMicrotask(() => {
        socket.onmessage({ data: JSON.stringify(["CLOSED", frame[1], "rate-limited: slow down"]) });
      });
    },
  };
  const started = collect({ openSocket: () => socket, timeoutMs: 5000 });
  socket.onopen();
  const result = await started;

  assert.equal(result.complete, false);
  assert.match(result.reason, /rate-limited/, "the relay's own reason is reported");
});

test("writes are atomic: a temp file is renamed into place, never truncated in place", async () => {
  const calls = [];
  writeAtomic(["a.json", "b.json"], "payload", {
    write: (path, data) => calls.push(["write", path, data]),
    rename: (from, to) => calls.push(["rename", from, to]),
  });
  assert.deepEqual(calls, [
    ["write", "a.json.tmp", "payload"],
    ["rename", "a.json.tmp", "a.json"],
    ["write", "b.json.tmp", "payload"],
    ["rename", "b.json.tmp", "b.json"],
  ]);
});

test("a complete read writes every destination with one identical payload", async () => {
  const writes = [];
  const result = await bake({
    openSocket: () => {
      const s = fakeRelay({ supply: { tagged: supplyOf(2), profiles: supplyOf(2) } });
      queueMicrotask(() => s.onopen());
      return s;
    },
    streams: [
      { name: "tagged", filter: { kinds: [3401], limit: 4 } },
      { name: "profiles", filter: { kinds: [0], limit: 4 } },
    ],
    destinations: ["public/snapshot.json", "dist/snapshot.json"],
    write: (path, data) => writes.push([path, data]),
    rename: () => {},
  });

  assert.equal(result.written, true);
  assert.equal(result.count, 4);
  assert.equal(writes.length, 2);
  assert.equal(writes[0][1], writes[1][1], "one serialisation, written to both");
  assert.ok(!writes[0][1].includes("sig"), "signatures are stripped");
});
