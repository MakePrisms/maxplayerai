/**
 * Live funnel + census probe against the pinned relay.
 * Usage: node scripts/live-check.mjs
 * Exit 0 on successful connect + EOSE (or timeout with ≥1 event).
 */
import { RELAY_URL, HISTORY_LIMIT } from "../config.js";
import { SUBSCRIBE_KINDS } from "../js/kinds.js";
import { parseEvent } from "../js/parse.js";
import { createStore } from "../js/store.js";

const TIMEOUT_MS = 15000;
const store = createStore();
const frames = { auth: 0, eose: 0, event: 0, notice: 0, other: 0 };
const kindCounts = new Map();

const report = {
  url: RELAY_URL,
  started_at: new Date().toISOString(),
  connected: false,
  auth_ignored: false,
  eose: false,
  error: null,
};

function finish(code) {
  const snap = store.snapshot();
  const out = {
    ...report,
    finished_at: new Date().toISOString(),
    frames,
    kind_counts: Object.fromEntries([...kindCounts.entries()].sort()),
    funnel: snap.funnel,
    census: snap.census,
    census_n: snap.census.length,
    latency_samples: snap.latency.samples,
    economics_receipts: snap.economics.rows.length,
  };
  console.log(JSON.stringify(out, null, 2));
  try {
    ws.close();
  } catch {
    /* ignore */
  }
  process.exit(code);
}

const ws = new WebSocket(RELAY_URL);
const timer = setTimeout(() => {
  report.error = frames.event > 0 ? null : "timeout waiting for events/EOSE";
  // If we got events without EOSE, still treat as success for live check.
  finish(frames.event > 0 || report.eose ? 0 : 2);
}, TIMEOUT_MS);

ws.addEventListener("open", () => {
  report.connected = true;
  const subId = "live-check-1";
  ws.send(
    JSON.stringify([
      "REQ",
      subId,
      { kinds: [...SUBSCRIBE_KINDS], limit: HISTORY_LIMIT },
    ]),
  );
});

ws.addEventListener("message", (msg) => {
  let data;
  try {
    data = JSON.parse(typeof msg.data === "string" ? msg.data : String(msg.data));
  } catch {
    frames.other += 1;
    return;
  }
  if (!Array.isArray(data) || !data.length) {
    frames.other += 1;
    return;
  }
  const type = data[0];
  if (type === "AUTH") {
    frames.auth += 1;
    report.auth_ignored = true;
    return; // ignore — do not treat as error
  }
  if (type === "EOSE") {
    frames.eose += 1;
    report.eose = true;
    clearTimeout(timer);
    finish(0);
    return;
  }
  if (type === "EVENT" && data[2]) {
    frames.event += 1;
    const ev = data[2];
    const k = ev?.kind;
    kindCounts.set(k, (kindCounts.get(k) || 0) + 1);
    store.ingest(parseEvent(ev));
    return;
  }
  if (type === "NOTICE") {
    frames.notice += 1;
    return;
  }
  frames.other += 1;
});

ws.addEventListener("error", () => {
  report.error = "websocket error";
});

ws.addEventListener("close", () => {
  if (!report.eose && frames.event === 0 && !report.error) {
    report.error = "socket closed before data";
    clearTimeout(timer);
    finish(2);
  }
});
