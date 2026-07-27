import assert from "node:assert/strict";
import { test } from "node:test";

import { classifyClosed, createRelayClient, historyFilter, historyStreams, liveFilters } from "../js/relay.js";
import { MOBEE_TAG } from "../js/kinds.js";

/** A scriptable stand-in for a relay socket. */
function fakeSocket() {
  const sent = [];
  const sock = {
    readyState: 1,
    sent,
    send: (raw) => sent.push(JSON.parse(raw)),
    close() { this.readyState = 3; },
    onopen: null, onmessage: null, onerror: null, onclose: null,
    open() { this.onopen && this.onopen(); },
    deliver(frame) { this.onmessage && this.onmessage({ data: JSON.stringify(frame) }); },
  };
  return sock;
}

function clientOn(sock, overrides = {}) {
  const events = [];
  const statuses = [];
  const client = createRelayClient({
    url: "wss://relay.test",
    openSocket: () => sock,
    now: () => 1_000_000,
    onEvent: (e) => events.push(e),
    onStatus: (s) => statuses.push(s),
    ...overrides,
  });
  return { client, events, statuses };
}

const evt = (id, created_at = 500) => ["EVENT", "sub", { id, kind: 3400, pubkey: "a".repeat(64), created_at, tags: [] }];
const reqs = (sock) => sock.sent.filter((f) => f[0] === "REQ");

test("classifyClosed separates our own CLOSE ack from a real refusal", () => {
  assert.equal(classifyClosed("", true), "acknowledged");
  assert.equal(classifyClosed("auth-required: need auth", true), "acknowledged");
  assert.equal(classifyClosed("auth-required: need auth", false), "retryable");
  assert.equal(classifyClosed("restricted: not a member", false), "refused");
  assert.equal(classifyClosed("", false), "unknown");
});

test("history requests carry the mobee tag filter and page limits", () => {
  const [tagged, untagged] = historyStreams();
  assert.deepEqual(tagged.filter["#t"], [MOBEE_TAG]);
  assert.ok(tagged.filter.limit > 0);
  assert.equal(historyFilter(tagged, null).until, undefined);
  assert.equal(untagged.filter["#t"], undefined, "the handler advert carries no mobee tag");
  assert.equal(historyFilter(tagged, 1234).until, 1234);
});

test("each filter is its own stream, so one REQ never mixes two cursors", () => {
  // A relay caps every filter in a REQ independently. Two filters sharing one
  // REQ run out at different depths, so they cannot share a cursor.
  const streams = historyStreams();
  assert.ok(streams.length >= 2);
  for (const s of streams) {
    assert.ok(Array.isArray(s.filter.kinds) && s.filter.kinds.length > 0, `${s.name} has kinds`);
  }
});

test("live subscription resumes from now, not from the beginning", () => {
  const [tagged] = liveFilters(999);
  assert.equal(tagged.since, 999);
  assert.equal(tagged.limit, undefined);
});

test("REGRESSION: the relay's ack of our own CLOSE must not abort the read", () => {
  // The relay echoes ["CLOSED", subid, ""] to confirm a CLOSE we sent. Reading
  // that as a refusal ends history at page one and silently loses the market.
  const sock = fakeSocket();
  const { client, statuses } = clientOn(sock);
  client.connect();
  sock.open();

  sock.deliver(evt("e1", 900));
  sock.deliver(["EOSE", "h1"]);
  const closeFrame = sock.sent.find((f) => f[0] === "CLOSE");
  assert.ok(closeFrame, "page should be closed before the next one opens");

  sock.deliver(["CLOSED", closeFrame[1], ""]);

  assert.ok(!statuses.some((s) => s.state === "failed"), "ack must not fail the read");
  assert.equal(reqs(sock).length, 2, "paging should continue to the next page");
});

test("an unsolicited CLOSED does fail the read", () => {
  const sock = fakeSocket();
  const { client, statuses } = clientOn(sock);
  client.connect();
  sock.open();
  sock.deliver(["CLOSED", "h1", "restricted: not a member"]);
  assert.ok(statuses.some((s) => s.state === "failed"), "a real refusal must surface");
});

test("paging walks backwards within a stream", () => {
  const sock = fakeSocket();
  const { client } = clientOn(sock);
  client.connect();
  sock.open();

  sock.deliver(evt("e1", 900));
  sock.deliver(evt("e2", 800));
  sock.deliver(["EOSE", "h1"]);

  const second = reqs(sock)[1];
  assert.equal(second[2].until, 799, "next page starts just below the oldest seen");
});

test("REGRESSION: a drained stream must not hand its cursor to the next one", () => {
  // The bug this guards: one shared cursor advanced from the globally-oldest
  // event skips everything a shallower filter has not delivered, and the read
  // ends early looking perfectly healthy.
  const sock = fakeSocket();
  const { client } = clientOn(sock);
  client.connect();
  sock.open();

  sock.deliver(evt("e1", 900));           // stream 1 reaches back to 900
  sock.deliver(["EOSE", "h1"]);
  sock.deliver(["EOSE", "h2"]);           // stream 1 drained

  const third = reqs(sock)[2];
  assert.notEqual(third[1], "live", "a second stream must still be read");
  assert.equal(third[2].until, undefined, "the new stream starts from the top, not at 899");
});

test("history ends and goes live only once every stream is drained", () => {
  const sock = fakeSocket();
  const { client } = clientOn(sock);
  client.connect();
  sock.open();

  const streamCount = historyStreams().length;
  for (let i = 0; i < streamCount; i++) sock.deliver(["EOSE", `h${i + 1}`]);

  const last = reqs(sock).at(-1);
  assert.equal(last[1], "live", "all streams drained => live");
  assert.equal(last[2].since, 1_000_000);
});

test("events reach the caller as they arrive", () => {
  const sock = fakeSocket();
  const { client, events } = clientOn(sock);
  client.connect();
  sock.open();
  sock.deliver(evt("e1"));
  sock.deliver(evt("e2"));
  assert.deepEqual(events.map((e) => e.id), ["e1", "e2"]);
});

test("the client never sends anything but reads", () => {
  const sock = fakeSocket();
  const { client } = clientOn(sock);
  client.connect();
  sock.open();
  sock.deliver(["AUTH", "challenge-string"]);
  sock.deliver(evt("e1"));
  sock.deliver(["EOSE", "h1"]);

  const verbs = new Set(sock.sent.map((f) => f[0]));
  assert.deepEqual([...verbs].sort(), ["CLOSE", "REQ"], "read-only: no AUTH, no EVENT");
});
