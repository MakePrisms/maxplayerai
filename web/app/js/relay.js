/**
 * Relay client — reads mobee's market from the relay and keeps reading.
 *
 * Read-only by construction: it never signs, never holds a key, and never
 * requests gift-wrap. It walks history back to exhaustion, then stays
 * subscribed so new events arrive without a reload.
 *
 * The socket is injectable so the whole lifecycle is testable without a network.
 */
import { MOBEE_TAG, MOBEE_TAGGED_KINDS, UNTAGGED_KINDS } from "./kinds.js";

/** Events per history page. The relay caps a REQ; we page under that cap. */
export const PAGE_SIZE = 500;
/** Stop paging after this many pages — a backstop, not an expected limit. */
export const MAX_PAGES = 40;

/** Reconnect backoff in ms, then steady. */
const BACKOFF = [1000, 2000, 5000, 10000, 30000];

/**
 * History is read as independent streams, each paged with its OWN cursor.
 *
 * This is not a stylistic choice. A relay caps each filter in a REQ separately,
 * so filters in the same REQ run out at different depths. Advancing one shared
 * `until` from the globally-oldest event then steps straight past everything the
 * more-truncated filter has not delivered yet — the read looks healthy, ends
 * early, and silently loses half the market. One cursor per filter, always.
 */
export function historyStreams() {
  return [
    { name: "tagged", filter: { kinds: [...MOBEE_TAGGED_KINDS], limit: PAGE_SIZE, "#t": [MOBEE_TAG] } },
    { name: "untagged", filter: { kinds: [...UNTAGGED_KINDS], limit: PAGE_SIZE } },
  ];
}

export function historyFilter(stream, until) {
  const filter = { ...stream.filter };
  if (until != null) filter.until = until;
  return filter;
}

export function liveFilters(since) {
  const tagged = { kinds: [...MOBEE_TAGGED_KINDS], "#t": [MOBEE_TAG], since };
  const untagged = { kinds: [...UNTAGGED_KINDS], since };
  return [tagged, untagged];
}

/**
 * Classify a CLOSED frame.
 *
 * A relay echoes `["CLOSED", subid, ""]` to ACKNOWLEDGE a CLOSE we sent — that
 * is routine, not a rejection, and treating it as one silently ends the read
 * mid-history. An unsolicited CLOSED is a real refusal, and its reason decides
 * whether retrying can ever help: `auth-required:` may succeed later,
 * `restricted:` will not. The two are otherwise indistinguishable.
 */
export function classifyClosed(reason, weClosedIt) {
  if (weClosedIt) return "acknowledged";
  const text = String(reason || "");
  if (text.startsWith("auth-required:")) return "retryable";
  if (text.startsWith("restricted:")) return "refused";
  return text ? "refused" : "unknown";
}

export function createRelayClient({
  url,
  onEvent = () => {},
  onStatus = () => {},
  onHistoryComplete = () => {},
  openSocket = (u) => new WebSocket(u),
  now = () => Math.floor(Date.now() / 1000),
} = {}) {
  let ws = null;
  let phase = "idle";           // idle | history | live | failed
  let pages = 0;
  let seenThisPage = 0;
  let streams = [];
  let streamIndex = 0;
  /** Cursor for the stream being paged — reset when moving to the next one. */
  let oldestSeen = null;
  let subCounter = 0;
  let activeSub = null;
  let attempt = 0;
  let retryTimer = null;
  let stopped = false;
  const closedByUs = new Set();

  const status = (state, detail = "") => { phase = state; onStatus({ state, detail, pages, url }); };

  function send(frame) {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify(frame));
  }

  function requestHistory() {
    seenThisPage = 0;
    activeSub = `h${++subCounter}`;
    send(["REQ", activeSub, historyFilter(streams[streamIndex], oldestSeen == null ? null : oldestSeen - 1)]);
  }

  function goLive() {
    activeSub = "live";
    send(["REQ", activeSub, ...liveFilters(now())]);
    status("live");
    onHistoryComplete({ pages });
  }

  function handleEose() {
    pages += 1;
    // Close the page's subscription; remember it so its CLOSED ack is not read
    // as a refusal.
    if (activeSub) {
      closedByUs.add(activeSub);
      send(["CLOSE", activeSub]);
    }
    const exhausted = seenThisPage === 0 || oldestSeen == null;
    if (pages >= MAX_PAGES) return goLive();
    if (exhausted) {
      // This stream is drained; the next one starts from its own beginning.
      streamIndex += 1;
      oldestSeen = null;
      if (streamIndex >= streams.length) return goLive();
    }
    requestHistory();
  }

  function handleFrame(frame) {
    const [type] = frame;
    if (type === "EVENT") {
      const event = frame[2];
      seenThisPage += 1;
      if (event && (oldestSeen == null || event.created_at < oldestSeen)) oldestSeen = event.created_at;
      onEvent(event);
      return;
    }
    if (type === "EOSE") { if (phase === "history") handleEose(); return; }
    if (type === "CLOSED") {
      const verdict = classifyClosed(frame[2], closedByUs.has(frame[1]));
      if (verdict === "acknowledged") { closedByUs.delete(frame[1]); return; }
      status("failed", `relay declined the read (${verdict})`);
      teardown();
      if (verdict === "retryable") scheduleRetry();
      return;
    }
    // AUTH: the relay may challenge. We are read-only and never answer it; the
    // historical read is served regardless. NOTICE is informational.
  }

  function teardown() {
    if (ws) {
      ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
      try { ws.close(); } catch { /* already gone */ }
      ws = null;
    }
  }

  function scheduleRetry() {
    if (stopped || retryTimer) return;
    const wait = BACKOFF[Math.min(attempt, BACKOFF.length - 1)];
    attempt += 1;
    status("reconnecting", `retrying in ${Math.round(wait / 1000)}s`);
    retryTimer = setTimeout(() => { retryTimer = null; connect(); }, wait);
  }

  function connect() {
    if (stopped) return;
    teardown();
    pages = 0;
    oldestSeen = null;
    streams = historyStreams();
    streamIndex = 0;
    closedByUs.clear();
    status("connecting");
    try { ws = openSocket(url); } catch { return scheduleRetry(); }

    ws.onopen = () => { attempt = 0; status("history"); requestHistory(); };
    ws.onmessage = (msg) => {
      let frame;
      try { frame = JSON.parse(msg.data); } catch { return; }
      if (Array.isArray(frame)) handleFrame(frame);
    };
    ws.onerror = () => { if (phase !== "failed") { status("failed", "connection error"); } };
    ws.onclose = () => { if (!stopped && phase !== "failed") scheduleRetry(); };
  }

  return {
    connect,
    stop() { stopped = true; if (retryTimer) clearTimeout(retryTimer); retryTimer = null; teardown(); status("idle"); },
    get phase() { return phase; },
    get pagesRead() { return pages; },
  };
}
