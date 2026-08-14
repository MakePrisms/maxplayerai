/**
 * Relay source — reads maxplayer's market from the relay and keeps reading.
 *
 * Read-only by construction: never signs, never holds a key, never requests
 * gift-wrap. It walks history back to exhaustion (or forward from a `since`
 * hint when the store already holds the past), then keeps itself current in
 * one of two ways:
 *
 *   poll   — one `since: newest+1` REQ per tick, closed on its own EOSE.
 *            MEASURED 7/28 on the production relay: it answers stored-event
 *            queries anonymously but never streams post-EOSE (NIP-42 AUTH
 *            gates the live feed), so a long-lived REQ sits there looking
 *            healthy and delivering nothing.
 *   stream — one long-lived REQ left open; the relay pushes each new event
 *            the moment it lands. THE MODE TO SWITCH TO once the relay is
 *            upgraded — flip TRANSPORT in config.ts, nothing else changes.
 *
 * The socket is injectable so the whole lifecycle is testable without a network.
 */
import { MAXPLAYER_TAG, MAXPLAYER_TAGGED_KINDS, UNTAGGED_KINDS } from "../model/kinds.js";
import type { RawEvent } from "../model/events.js";
import type { MarketSource, SourceCallbacks, SourceState, Transport } from "./source.js";

/** Events per history page. The relay caps a REQ; we page under that cap. */
export const PAGE_SIZE = 500;
/** Stop paging after this many pages — a backstop, not an expected limit. */
export const MAX_PAGES = 40;
/** Poll cadence. Irrelevant in stream mode. */
export const POLL_MS = 3000;
/** Reconnect backoff in ms, then steady. */
const BACKOFF = [1000, 2000, 5000, 10000, 30000];

type Filter = Record<string, unknown>;
interface Stream { name: string; filter: Filter }

/**
 * History is read as independent streams, each paged with its OWN cursor.
 *
 * Not a stylistic choice: a relay caps each filter in a REQ separately, so
 * filters in one REQ run out at different depths. Advancing one shared `until`
 * from the globally-oldest event steps straight past everything the
 * more-truncated filter has not delivered — the read looks healthy, ends
 * early, and silently loses half the market. One cursor per filter, always.
 */
export function historyStreams(since: number | null): Stream[] {
  // A `since` hint (the newest event the cache already holds) turns a repeat
  // visit's history walk into a single tiny page of "what did I miss".
  const base: Filter = since != null ? { since } : {};
  return [
    { name: "tagged", filter: { ...base, kinds: [...MAXPLAYER_TAGGED_KINDS], limit: PAGE_SIZE, "#t": [MAXPLAYER_TAG] } },
    // One stream PER untagged kind: kinds sharing a filter share the relay's
    // cap on it, so a numerous kind truncates a sparse one and the sparse
    // one's absence looks like "the relay has none".
    ...UNTAGGED_KINDS.map((kind) => ({ name: `untagged-${kind}`, filter: { ...base, kinds: [kind], limit: PAGE_SIZE } })),
  ];
}

export function historyFilter(stream: Stream, until: number | null): Filter {
  const filter = { ...stream.filter };
  if (until != null) filter.until = until;
  return filter;
}

export function liveFilters(since: number): Filter[] {
  return [
    { kinds: [...MAXPLAYER_TAGGED_KINDS], "#t": [MAXPLAYER_TAG], since },
    // Split per kind here too — same cap-sharing reason as history.
    ...UNTAGGED_KINDS.map((kind) => ({ kinds: [kind], since })),
  ];
}

/**
 * Classify a CLOSED frame.
 *
 * A relay echoes `["CLOSED", subid, ""]` to ACKNOWLEDGE a CLOSE we sent —
 * routine, not a rejection; treating it as one silently ends the read
 * mid-history. An unsolicited CLOSED is a real refusal, and its reason decides
 * whether retrying can ever help: `auth-required:` may succeed later,
 * `restricted:` will not.
 */
export function classifyClosed(reason: unknown, weClosedIt: boolean): "acknowledged" | "retryable" | "refused" | "unknown" {
  if (weClosedIt) return "acknowledged";
  const text = String(reason || "");
  if (text.startsWith("auth-required:")) return "retryable";
  if (text.startsWith("restricted:")) return "refused";
  return text ? "refused" : "unknown";
}

export interface RelaySourceOptions {
  url: string;
  transport: Transport;
  /** Newest created_at already held by the cache; history resumes from here. */
  sinceHint?: number | null;
  /**
   * Whether the cached store is known to hold a COMPLETE history — i.e. a
   * previous walk ran to genuine exhaustion. The since-hint is an optimization
   * that assumes there is nothing below it; applied to a store with a hole,
   * every future read starts above that hole and the store can never repair
   * itself. Default false: re-walk unless completeness was actually proven.
   */
  storeComplete?: boolean;
  openSocket?: (url: string) => WebSocket;
  now?: () => number;
  setTimer?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearTimer?: (id: ReturnType<typeof setTimeout>) => void;
}

export function createRelaySource(
  {
    url,
    transport,
    sinceHint = null,
    storeComplete = false,
    openSocket = (u) => new WebSocket(u),
    now = () => Math.floor(Date.now() / 1000),
    setTimer = (fn, ms) => setTimeout(fn, ms),
    clearTimer = (id) => clearTimeout(id),
  }: RelaySourceOptions,
  callbacks: SourceCallbacks,
): MarketSource {
  let ws: WebSocket | null = null;
  let phase: SourceState = "idle";
  /**
   * Pages spent PER STREAM. One shared counter let the deep tagged stream spend
   * the whole allowance, after which every remaining stream went live having
   * read nothing — a truncated market reporting itself fully synced.
   */
  const pageCounts = new Map<string, number>();
  /** Streams that stopped on the backstop rather than running out. */
  const truncated = new Set<string>();
  /** A relay refusal, as opposed to a transient drop: retrying cannot help. */
  let refused = false;
  let seenThisPage = 0;
  let streams: Stream[] = [];
  let streamIndex = 0;
  /** Cursor for the stream being paged — reset when moving to the next one. */
  let oldestSeen: number | null = null;
  /** Forward cursor: the newest created_at we have ingested. */
  let newestSeen: number | null = sinceHint;
  /**
   * Per-stream backward cursors and drained marks, kept ACROSS reconnects.
   *
   * A dropped socket must resume each stream where that stream stopped. One
   * shared cursor cannot do it — streams run out at different depths, so
   * resuming them all from the globally-oldest event steps past everything a
   * shallower stream has not delivered. That is the same defect the per-filter
   * cursors above exist to prevent, and it has to survive the reconnect too.
   * Recorded at page boundaries (EOSE), so a drop mid-page costs a re-read of
   * one page and can never skip.
   */
  const cursors = new Map<string, number>();
  const drained = new Set<string>();
  /** Has a history walk ever run to genuine exhaustion? */
  let historyComplete = storeComplete;
  let subCounter = 0;
  let activeSub: string | null = null;
  /** The long-lived subscription in stream mode; never closed by us. */
  let liveSub: string | null = null;
  let attempt = 0;
  let retryTimer: ReturnType<typeof setTimeout> | null = null;
  let pollTimer: ReturnType<typeof setTimeout> | null = null;
  let stopped = false;
  const closedByUs = new Set<string>();

  const status = (state: SourceState, detail = "") => { phase = state; callbacks.onStatus({ state, detail }); };

  function send(frame: unknown[]) {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify(frame));
  }

  function requestHistory() {
    seenThisPage = 0;
    activeSub = `h${++subCounter}`;
    const stream = streams[streamIndex];
    if (!stream) return goLive(true);
    send(["REQ", activeSub, historyFilter(stream, oldestSeen == null ? null : oldestSeen - 1)]);
  }

  /** Move to the next stream, resuming it from its own cursor. */
  function nextStream() {
    streamIndex += 1;
    oldestSeen = cursors.get(streams[streamIndex]?.name ?? "") ?? null;
    if (streamIndex >= streams.length) return goLive(truncated.size === 0);
    requestHistory();
  }

  /** One incremental ask. A quiet market costs an empty round trip. */
  function pollOnce() {
    if (stopped) return;
    activeSub = `p${++subCounter}`;
    // +1 so an ingested event is not re-fetched every tick; `since` is
    // inclusive and this cursor only moves forward.
    send(["REQ", activeSub, ...liveFilters(newestSeen == null ? now() : newestSeen + 1)]);
  }

  function schedulePoll() {
    if (stopped || transport !== "poll") return;
    pollTimer = setTimer(() => { pollOnce(); schedulePoll(); }, POLL_MS);
  }

  /**
   * History reading is over — switch to staying current, per transport.
   *
   * `complete` says WHY it is over: every stream genuinely drained, or the
   * MAX_PAGES backstop cut a read short. Only the first proves the store holds
   * everything, and only then may a later visit skip the walk. Collapsing the
   * two is a green that cannot go red — a truncated read would report itself
   * fully synced and bake its own gap in.
   *
   * The distinction is deliberately NOT surfaced in the connection line: per
   * bob, the page says `live` either way. It still governs what the store may
   * claim about itself and still reaches the console, so the correctness
   * tracking is intact — only the display was dropped.
   */
  function goLive(complete: boolean) {
    if (complete) {
      historyComplete = true;
      cursors.clear();
      drained.clear();
    } else {
      console.warn(`[relay] history stopped at the ${MAX_PAGES}-page backstop; the store is not known complete`);
    }
    status("live");
    callbacks.onSynced({ complete });
    if (transport === "stream") {
      // One subscription, left open: the relay pushes every new event as it
      // lands. No timers at all in this mode.
      liveSub = `s${++subCounter}`;
      send(["REQ", liveSub, ...liveFilters(newestSeen == null ? now() : newestSeen + 1)]);
    } else {
      pollOnce();
      schedulePoll();
    }
  }

  function handleEose(subId: string) {
    if (phase === "history") {
      if (activeSub) {
        closedByUs.add(activeSub);
        send(["CLOSE", activeSub]);
      }
      const exhausted = seenThisPage === 0 || oldestSeen == null;
      // Record this stream's progress at the page boundary, so a drop before
      // the next EOSE resumes here instead of restarting or skipping ahead.
      const current = streams[streamIndex];
      if (current && oldestSeen != null) cursors.set(current.name, oldestSeen);
      if (current) pageCounts.set(current.name, (pageCounts.get(current.name) ?? 0) + 1);
      const spent = current ? (pageCounts.get(current.name) ?? 0) >= MAX_PAGES : true;
      if (exhausted || spent) {
        // Drained is an answer; the backstop is an admission. Recorded apart so
        // a short read cannot certify the store as complete.
        if (current) (exhausted ? drained : truncated).add(current.name);
        if (current && !exhausted) {
          console.warn(`[relay] stream ${current.name} stopped at the ${MAX_PAGES}-page backstop`);
        }
        return nextStream();
      }
      requestHistory();
      return;
    }
    // Stream mode: the live subscription's EOSE just means "caught up; now
    // pushing". It stays open. Poll mode: the round is done — CLOSE it, or
    // subscriptions accumulate until the relay drops us for holding too many.
    if (subId === liveSub) return;
    if (subId === activeSub && activeSub) {
      closedByUs.add(activeSub);
      send(["CLOSE", activeSub]);
    }
  }

  function handleFrame(frame: unknown[]) {
    const [type] = frame;
    if (type === "EVENT") {
      const event = frame[2] as RawEvent | undefined;
      seenThisPage += 1;
      if (event && (oldestSeen == null || event.created_at < oldestSeen)) oldestSeen = event.created_at;
      if (event && (newestSeen == null || event.created_at > newestSeen)) newestSeen = event.created_at;
      if (event) callbacks.onEvent(event);
      return;
    }
    if (type === "EOSE") { handleEose(String(frame[1])); return; }
    if (type === "CLOSED") {
      const subId = String(frame[1]);
      const verdict = classifyClosed(frame[2], closedByUs.has(subId));
      if (verdict === "acknowledged") { closedByUs.delete(subId); return; }
      // `restricted:` will not become permitted by asking again; anything else
      // stays retryable, so the ordinary reconnect path keeps working.
      if (verdict === "refused") refused = true;
      status("failed", `relay declined the read (${verdict})`);
      teardown();
      if (verdict === "retryable") scheduleRetry();
      return;
    }
    // AUTH: the relay may challenge. We are read-only and never answer it; the
    // historical read is served regardless. NOTICE is informational.
  }

  function teardown() {
    if (pollTimer) { clearTimer(pollTimer); pollTimer = null; }
    if (ws) {
      ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
      try { ws.close(); } catch { /* already gone */ }
      ws = null;
    }
  }

  function scheduleRetry() {
    if (stopped || retryTimer) return;
    const wait = BACKOFF[Math.min(attempt, BACKOFF.length - 1)] ?? 30000;
    attempt += 1;
    status("reconnecting", `retrying in ${Math.round(wait / 1000)}s`);
    retryTimer = setTimer(() => { retryTimer = null; connect(); }, wait);
  }

  function connect() {
    if (stopped) return;
    teardown();
    // The backstop is per connection, like the page counter it replaced: a
    // reconnect resumes deeper thanks to the cursors, so it makes progress
    // rather than re-spending the same allowance on the same pages.
    pageCounts.clear();
    truncated.clear();
    refused = false;
    // The forward read is only sound once a walk has genuinely finished:
    // `since` asserts there is nothing below it. Otherwise walk backward and
    // resume each stream from its own recorded cursor — reconnecting mid-read
    // must not step over history it has never seen.
    const forward = historyComplete && newestSeen != null;
    streams = historyStreams(forward ? newestSeen! + 1 : null);
    streamIndex = forward ? 0 : streams.findIndex((s) => !drained.has(s.name));
    if (streamIndex < 0) streamIndex = streams.length;
    oldestSeen = forward ? null : cursors.get(streams[streamIndex]?.name ?? "") ?? null;
    liveSub = null;
    closedByUs.clear();
    status("connecting");
    try { ws = openSocket(url); } catch { return scheduleRetry(); }

    ws.onopen = () => { attempt = 0; status("history"); requestHistory(); };
    ws.onmessage = (msg: MessageEvent) => {
      let frame: unknown;
      try { frame = JSON.parse(msg.data as string); } catch { return; }
      if (Array.isArray(frame)) handleFrame(frame);
    };
    ws.onerror = () => { if (phase !== "failed") status("failed", "connection error"); };
    // An error followed by a close is THE ordinary browser drop, and gating the
    // retry on `phase !== "failed"` meant onerror had already disqualified it —
    // the app sat disconnected forever. A deliberate teardown detaches this
    // handler first, so reaching here at all means the drop was involuntary;
    // only an outright refusal by the relay is worth not retrying.
    ws.onclose = () => { if (!stopped && !refused) scheduleRetry(); };
  }

  return {
    start: connect,
    stop() {
      stopped = true;
      if (retryTimer) clearTimer(retryTimer);
      retryTimer = null;
      teardown();
      status("idle");
    },
    get state() { return phase; },
  };
}
