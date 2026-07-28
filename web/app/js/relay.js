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

/** How often to ask for new events, since the relay will not push them to us. */
export const POLL_MS = 3000;

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
  // Injectable so a test can drive the retry itself. A test that waits out a
  // real backoff is asserting against a clock, which rots the moment any
  // machine or dependency changes speed.
  setTimer = (fn, ms) => setTimeout(fn, ms),
  clearTimer = (id) => clearTimeout(id),
} = {}) {
  let ws = null;
  let phase = "idle";           // idle | history | live | failed
  let pages = 0;
  let seenThisPage = 0;
  let streams = [];
  let streamIndex = 0;
  /** Cursor for the stream being paged — reset when moving to the next one. */
  let oldestSeen = null;
  /** Forward cursor for polling: the newest created_at we have ingested. */
  let newestSeen = null;
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

  /**
   * Poll for new events. We do NOT get pushed to.
   *
   * MEASURED 7/28: this relay answers stored-event queries anonymously but never
   * streams post-EOSE. Held a `since:T` subscription for 90s, then asked for
   * stored events in that same window — 4 existed, 0 had been pushed. It sends a
   * NIP-42 AUTH challenge on connect; reading history without a key is allowed,
   * receiving a live feed is not. A long-lived REQ therefore sits there looking
   * healthy and delivering nothing, which is how the board claimed "live" for
   * days while showing a snapshot from page load.
   *
   * So: ask, don't wait. Each tick is one REQ for `since: newest+1`, closed on
   * its own EOSE. Incremental, so a quiet market costs an empty round trip.
   */
  function pollOnce() {
    if (stopped) return;
    activeSub = `p${++subCounter}`;
    // +1 so an event already ingested is not re-fetched every tick. `since` is
    // inclusive, and this cursor only moves forward.
    send(["REQ", activeSub, ...liveFilters(newestSeen == null ? now() : newestSeen + 1)]);
  }

  /**
   * The CLIENT does not own a clock. The app already ticks every POLL_MS to
   * refresh clock-derived parts of the view, so it calls `poll()` on that same
   * tick — one ticker, not two that can drift apart or double after a
   * reconnect. It also keeps this module free of a repeating timer, which is
   * what a caller has to remember to cancel and a test suite hangs on.
   */
  function startPolling() {
    status("watching");
    onHistoryComplete({ pages });
    pollOnce();
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
    if (pages >= MAX_PAGES) return startPolling();
    if (exhausted) {
      // This stream is drained; the next one starts from its own beginning.
      streamIndex += 1;
      oldestSeen = null;
      if (streamIndex >= streams.length) return startPolling();
    }
    requestHistory();
  }

  function handleFrame(frame) {
    const [type] = frame;
    if (type === "EVENT") {
      const event = frame[2];
      seenThisPage += 1;
      if (event && (oldestSeen == null || event.created_at < oldestSeen)) oldestSeen = event.created_at;
      if (event && (newestSeen == null || event.created_at > newestSeen)) newestSeen = event.created_at;
      onEvent(event);
      return;
    }
    if (type === "EOSE") {
      if (phase === "history") { handleEose(); return; }
      // A poll's EOSE means that round is done. CLOSE it, or subscriptions
      // accumulate at one every POLL_MS — twenty a minute, none of them ever
      // ended — until the relay drops us for holding too many.
      if (frame[1] === activeSub) {
        closedByUs.add(activeSub);
        send(["CLOSE", activeSub]);
      }
      return;
    }
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
    retryTimer = setTimer(() => { retryTimer = null; connect(); }, wait);
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
    /** Ask for anything newer than we hold. No-op unless we are past history. */
    poll() { if (phase === "watching") pollOnce(); },
    stop() { stopped = true; if (retryTimer) clearTimer(retryTimer); retryTimer = null; teardown(); status("idle"); },
    get phase() { return phase; },
    get pagesRead() { return pages; },
  };
}
