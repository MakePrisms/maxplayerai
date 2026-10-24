/**
 * Filter-parity gate (issue #872).
 *
 * The probe (scripts/live-check.mjs) previously (a) failed to load because it imported an export
 * (SUBSCRIBE_KINDS) that js/kinds.js never provided, and (b) sent a single unscoped REQ that did
 * not match the two-filter REQ the app (js/relay.js) actually sends. This suite exercises the
 * CONSUMERS — it drives the real app client and the real probe script, capturing their actual
 * outbound REQ frames — and asserts intended filter equality, with the deliberate `limit`
 * difference between the probe (always a capped limit) and the app (resume by `since`) called
 * out as intended, not erased.
 *
 * Everything runs offline: globalThis.WebSocket is replaced with a recording mock before either
 * consumer is imported, so no real network connection is ever attempted.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import {
  ACCEPT,
  AWARD,
  CLAIM,
  FEEDBACK,
  HANDLER,
  HEARTBEAT,
  MAXPLAYER_TAG,
  MAXPLAYER_TAGGED_KINDS,
  OFFER,
  RECEIPT,
  RESULT,
  UNTAGGED_KINDS,
} from "../js/kinds.js";
import { buildMarketFilters } from "../js/filters.js";
import { HISTORY_LIMIT } from "../config.js";

const PROBE = fileURLToPath(new URL("../scripts/live-check.mjs", import.meta.url));
const RELAY = fileURLToPath(new URL("../js/relay.js", import.meta.url));
const FILTERS = fileURLToPath(new URL("../js/filters.js", import.meta.url));

/** Recording mock WebSocket: never dials the network; records every frame it is sent. */
class MockSocket {
  // Static ready-state constants the real WebSocket exposes — relay.js's safeSend compares
  // `readyState === WebSocket.OPEN`, so the mock must carry them.
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  static instances = [];
  constructor(url) {
    this.url = url;
    this.readyState = MockSocket.OPEN;
    this.sent = [];
    this._listeners = new Map();
    this.onopen = null;
    this.onmessage = null;
    this.onerror = null;
    this.onclose = null;
    MockSocket.instances.push(this);
  }
  send(frame) {
    this.sent.push(frame);
  }
  close() {}
  addEventListener(type, fn) {
    if (!this._listeners.has(type)) this._listeners.set(type, []);
    this._listeners.get(type).push(fn);
  }
  dispatch(type, arg) {
    for (const fn of this._listeners.get(type) || []) fn(arg);
    const prop = this["on" + type];
    if (prop) prop(arg);
  }
}

/** Load relay.js with the mock WebSocket installed; returns the recorded REQ array frames. */
async function loadAppReqFrames(connectHooks = null) {
  const origWs = globalThis.WebSocket;
  globalThis.WebSocket = MockSocket;
  MockSocket.instances = [];
  try {
    const { createRelayClient } = await import(RELAY);
    const client = createRelayClient(
      connectHooks || { onEvent() {}, onStatus() {} },
    );
    client.connect();
    const sock = MockSocket.instances.at(-1);
    sock.dispatch("open");
    const frames = sock.sent.map((s) => JSON.parse(s));
    client.disconnect();
    return frames;
  } finally {
    globalThis.WebSocket = origWs;
  }
}

/**
 * Load the probe script with the mock WebSocket installed and process.exit neutralised (so the
 * script's finish() doesn't kill the test runner). Returns the frames the probe sent.
 *
 * The probe connects at its top level, so dynamic import must be cache-busted to re-execute on
 * every call — otherwise the second consumer load would return the cached module and record no
 * fresh socket.
 */
let probeLoadSeq = 0;
async function loadProbeReqFrames() {
  const origWs = globalThis.WebSocket;
  const origExit = process.exit;
  globalThis.WebSocket = MockSocket;
  MockSocket.instances = [];
  process.exit = () => {};
  try {
    probeLoadSeq += 1;
    await import(`${PROBE}?cachebust=${probeLoadSeq}`); // runs the probe's top-level connect
    const sock = MockSocket.instances.at(-1);
    sock.dispatch("open"); // fire connection open → probe sends its REQ
    const frames = sock.sent.map((s) => JSON.parse(s));
    sock.dispatch("message", { data: JSON.stringify(["EOSE"]) }); // let it finish, no hang
    return frames;
  } finally {
    globalThis.WebSocket = origWs;
    process.exit = origExit;
  }
}

function marketReq(frames, subIdPrefix) {
  return frames.find((f) => f[0] === "REQ" && String(f[1]).startsWith(subIdPrefix));
}

/** The only scoping that must never drift: kinds + tag. limit/since are per-consumer. */
function scopeOf(filter) {
  return { kinds: [...filter.kinds], tag: filter["#t"] ?? null };
}

// -- Defect 1 regression: the entrypoint loads offline, and no dangling export reference remains --
test("offline: the live-check entrypoint imports and runs, and no SUBSCRIBE_KINDS reference survives", async () => {
  // This test would have thrown a SyntaxError at link time before the fix (missing export).
  const frames = await loadProbeReqFrames();
  assert.equal(frames.length > 0, true, "probe must have sent at least one frame");
  assert.ok(
    frames.some((f) => f[0] === "REQ"),
    "probe must have sent a REQ",
  );
  // The drifted/dangling export must be gone from the consumer source.
  const src = readFileSync(PROBE, "utf8");
  assert.ok(
    !src.includes("SUBSCRIBE_KINDS"),
    "live-check.mjs must not reference the vanished SUBSCRIBE_KINDS export",
  );
});

// -- Defect 2: the probe and the app share the identical kind+tag scoping --
test("probe and app market filters share EQUAL kind and #t scoping", async () => {
  const probeFrames = await loadProbeReqFrames();
  const appFrames = await loadAppReqFrames();

  const probeReq = marketReq(probeFrames, "live-check");
  const appReq = marketReq(appFrames, "maxplayer-net-m");
  assert.ok(probeReq && appReq, "both consumers must send a market REQ");

  const [probeTagged, probeUntagged] = probeReq.slice(2);
  const [appTagged, appUntagged] = appReq.slice(2);

  assert.deepEqual(scopeOf(probeTagged), scopeOf(appTagged), "tagged filter must match");
  assert.deepEqual(scopeOf(probeUntagged), scopeOf(appUntagged), "untagged filter must match");
});

// -- Positive assertions on tag scoping and kinds --
test("app + probe tag the marketplace kinds by #t and leave the handler announce untagged", async () => {
  const probeFrames = await loadProbeReqFrames();
  const appFrames = await loadAppReqFrames();
  const probeReq = marketReq(probeFrames, "live-check");
  const appReq = marketReq(appFrames, "maxplayer-net-m");
  const [probeTagged, probeUntagged] = probeReq.slice(2);
  const [appTagged, appUntagged] = appReq.slice(2);

  for (const [which, tagged, untagged] of [
    ["probe", probeTagged, probeUntagged],
    ["app", appTagged, appUntagged],
  ]) {
    assert.deepEqual(tagged["#t"], [MAXPLAYER_TAG], `${which}: tagged filter is #t-scoped`);
    assert.deepEqual(
      tagged.kinds,
      [...MAXPLAYER_TAGGED_KINDS],
      `${which}: tagged filter requests the maxplayer-namespaced kinds`,
    );
    assert.equal(untagged["#t"], undefined, `${which}: untagged filter carries no #t`);
    assert.deepEqual(
      untagged.kinds,
      [...UNTAGGED_KINDS],
      `${which}: untagged filter requests only the handler announce`,
    );
  }

  // The marketplace kinds actually requested.
  const marketKinds = [...MAXPLAYER_TAGGED_KINDS, ...UNTAGGED_KINDS];
  for (const k of [OFFER, CLAIM, RESULT, FEEDBACK, AWARD, ACCEPT, RECEIPT, HEARTBEAT, HANDLER]) {
    assert.ok(marketKinds.includes(k), `kind ${k} must be subscribed`);
  }
  // No kind 1059 anywhere in the subscription.
  assert.ok(!marketKinds.includes(1059), "kind 1059 must stay out of the subscription");
});

// -- The intended `limit` difference is asserted explicitly --
test("probe always caps by limit; the app's resume path switches to `since` (intended difference)", async () => {
  // Probe (one-shot census): always a `limit`, never `since`.
  const probeFrames = await loadProbeReqFrames();
  const probeReq = marketReq(probeFrames, "live-check");
  const [probeTagged, probeUntagged] = probeReq.slice(2);
  assert.equal(probeTagged.limit, HISTORY_LIMIT, "probe tagged filter caps by limit");
  assert.equal(probeUntagged.limit, HISTORY_LIMIT, "probe untagged filter caps by limit");
  assert.equal(probeTagged.since, undefined, "probe must not resume by since");
  assert.equal(probeUntagged.since, undefined, "probe must not resume by since");

  // App cold connect (no cursor): also a capped limit.
  const appFrames = await loadAppReqFrames();
  const appReq = marketReq(appFrames, "maxplayer-net-m");
  const [appTagged, appUntagged] = appReq.slice(2);
  assert.equal(appTagged.limit, HISTORY_LIMIT, "app cold tagged filter caps by limit");
  assert.equal(appUntagged.limit, HISTORY_LIMIT, "app cold untagged filter caps by limit");
  assert.equal(appTagged.since, undefined, "app cold connect must not resume by since");
  assert.equal(appUntagged.since, undefined, "app cold connect must not resume by since");

  // The app's resume path (a since cursor is set) uses `since`, dropping `limit` — the
  // INTENDED difference from the probe: the app keeps the subscription live by resuming from
  // the last seen created_at, while the probe is a one-shot bounded census. Assert the shared
  // helper threads `since` exactly as the app relies on it.
  const [resumeTagged, resumeUntagged] = buildMarketFilters({ since: 1234567890 });
  assert.equal(resumeTagged.since, 1234567890, "resume tagged filter uses since");
  assert.equal(resumeUntagged.since, 1234567890, "resume untagged filter uses since");
  assert.equal(resumeTagged.limit, undefined, "resume tagged filter drops limit");
  assert.equal(resumeUntagged.limit, undefined, "resume untagged filter drops limit");
});

// -- Drift / missing-export regression at the helper boundary --
test("regression: the shared construction is actually shared and the export exists", async () => {
  // The helper export must exist.
  assert.equal(typeof buildMarketFilters, "function", "filters.js must export buildMarketFilters");

  // Both consumers must route their market REQ through the shared helper — if either copies the
  // filter inline again (drift) or stops sharing, this fails.
  const relaySrc = readFileSync(RELAY, "utf8");
  const probeSrc = readFileSync(PROBE, "utf8");
  assert.ok(
    /from\s+["'].*filters\.js["']/.test(relaySrc),
    "relay.js (app) must import the shared filters helper",
  );
  assert.ok(
    /from\s+["'].*filters\.js["']/.test(probeSrc),
    "live-check.mjs (probe) must import the shared filters helper",
  );
  assert.ok(relaySrc.includes("buildMarketFilters"), "relay.js must call buildMarketFilters");
  assert.ok(probeSrc.includes("buildMarketFilters"), "live-check.mjs must call buildMarketFilters");

  // And neither may reintroduce an inline `#t`-scoped market filter (that construction now lives
  // only in filters.js). A re-inlined tagged filter would put the literal `"#t"` back in the
  // consumer and this fails — the drift regression.
  const forbiddenTag = /["']#t["']\s*:/;
  assert.ok(
    !forbiddenTag.test(relaySrc.replace(/\/\*[\s\S]*?\*\//g, "")),
    "relay.js must not inline a #t market filter; it must come from filters.js",
  );
  assert.ok(
    !forbiddenTag.test(probeSrc.replace(/\/\*[\s\S]*?\*\//g, "")),
    "probe must not inline a #t market filter; it must come from filters.js",
  );
});
