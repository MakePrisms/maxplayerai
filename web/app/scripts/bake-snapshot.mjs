/**
 * Bake a market snapshot into a static file.
 *
 * Run at deploy time (or on a schedule) so a FIRST-TIME visitor's boards are
 * full on first paint — the client loads snapshot.json instantly and the relay
 * reconciles the tail. Zero hosting: the snapshot is just another static file
 * next to index.html.
 *
 * Node 22+ for the global WebSocket. Node 20: --experimental-websocket.
 *
 * Failure here must never fail a deploy: the client treats a missing
 * snapshot.json as "first load, show skeletons" and reconciles from the relay
 * as always. The deploy command runs this best-effort.
 *
 * WHAT MUST NEVER HAPPEN is the other one: shipping a snapshot that is missing
 * data while reporting a successful deploy. A partial read is indistinguishable
 * from a complete one once it is a file on disk, and the client trusts it as a
 * first paint. So a bake that did not genuinely drain every stream WRITES
 * NOTHING — no snapshot is a known state the client already handles, a quietly
 * incomplete one is not.
 */
import { renameSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

export const RELAY_URL = "wss://relay.maxplayer.ai";
export const TAGGED_KINDS = [3401, 3402, 3403, 3404, 3405, 3406, 3400, 30340];
export const PAGE = 500;
/** Per-stream backstop, not a shared allowance. */
export const MAX_PAGES = 40;
export const TIMEOUT_MS = 60_000;

/**
 * Each stream is paged with its OWN cursor AND its own page budget.
 *
 * A relay caps each filter separately, so streams run out at different depths.
 * Sharing one budget lets a deep stream spend the whole allowance and leave the
 * next one a single page — the profiles stream is the sparse one, so seats
 * silently lose their display names while the bake still reports success.
 */
export function defaultStreams() {
  return [
    { name: "tagged", filter: { kinds: TAGGED_KINDS, limit: PAGE, "#t": ["maxplayer"] } },
    { name: "profiles", filter: { kinds: [0], limit: PAGE } },
  ];
}

/**
 * Read every stream to exhaustion over one socket.
 *
 * Resolves with a report; never throws and never writes. The caller decides
 * what a partial read means — which is the point: the decision has to be
 * visible, not buried in the reader.
 */
export function collect({
  openSocket,
  streams = defaultStreams(),
  maxPages = MAX_PAGES,
  setTimer = setTimeout,
  clearTimer = clearTimeout,
  timeoutMs = TIMEOUT_MS,
} = {}) {
  return new Promise((resolve) => {
    const events = new Map();
    const report = streams.map((s) => ({ name: s.name, pages: 0, drained: false }));
    let index = 0;
    let oldest = null;
    let seenThisPage = 0;
    let subCounter = 0;
    let activeSub = null;
    let settled = false;
    const closedByUs = new Set();
    let ws;

    const timer = setTimer(() => finish("timed out waiting for the relay"), timeoutMs);

    function finish(reason) {
      if (settled) return;
      settled = true;
      clearTimer(timer);
      try { ws.close(); } catch { /* already gone */ }
      resolve({
        events: [...events.values()],
        streams: report,
        complete: report.every((s) => s.drained),
        reason,
      });
    }

    function requestPage() {
      seenThisPage = 0;
      activeSub = `s${++subCounter}`;
      const filter = { ...streams[index].filter };
      if (oldest != null) filter.until = oldest - 1;
      ws.send(JSON.stringify(["REQ", activeSub, filter]));
    }

    function nextStream() {
      index += 1;
      oldest = null;
      if (index >= streams.length) return finish(null);
      requestPage();
    }

    function onEose() {
      const stream = report[index];
      stream.pages += 1;
      // CLOSE every subscription as soon as it has answered. A relay caps how
      // many one connection may hold (strfry's default is 20), and a bake runs
      // to tens of pages — without this the relay CLOSEs us mid-read and the
      // run dies at the timeout, reporting the wrong cause.
      if (activeSub) {
        closedByUs.add(activeSub);
        ws.send(JSON.stringify(["CLOSE", activeSub]));
      }
      if (seenThisPage === 0) {
        // Genuine exhaustion: the relay had nothing more for this filter.
        stream.drained = true;
        return nextStream();
      }
      if (stream.pages >= maxPages) {
        // The backstop. NOT exhaustion — recorded as the distinct state it is,
        // so the caller can refuse to ship a partial read.
        return nextStream();
      }
      requestPage();
    }

    try {
      ws = openSocket(RELAY_URL);
    } catch (err) {
      settled = true;
      clearTimer(timer);
      resolve({ events: [], streams: report, complete: false, reason: `relay unreachable: ${err}` });
      return;
    }

    ws.onerror = () => finish("relay unreachable");
    ws.onclose = () => finish("relay closed the connection");
    ws.onopen = () => requestPage();
    ws.onmessage = (msg) => {
      let frame;
      try { frame = JSON.parse(msg.data); } catch { return; }
      if (!Array.isArray(frame)) return;
      if (frame[0] === "EVENT") {
        const e = frame[2];
        seenThisPage += 1;
        if (oldest == null || e.created_at < oldest) oldest = e.created_at;
        events.set(e.id, e);
        return;
      }
      if (frame[0] === "EOSE") return onEose();
      if (frame[0] === "CLOSED") {
        const subId = String(frame[1]);
        // Our own CLOSE being acknowledged is routine. Anything else is the
        // relay refusing the read, and must not look like a finished one.
        if (closedByUs.has(subId)) { closedByUs.delete(subId); return; }
        finish(`relay closed the subscription: ${frame[2] ?? "no reason given"}`);
      }
    };
  });
}

/**
 * Write one payload to several paths, atomically.
 *
 * writeFileSync truncates in place, so a process killed mid-write leaves a
 * valid-looking half file that the shell still reports as a successful deploy.
 * Write beside the target and rename — rename is atomic, so a reader sees the
 * whole file or the previous one.
 */
export function writeAtomic(paths, payload, { write = writeFileSync, rename = renameSync } = {}) {
  for (const path of paths) {
    const tmp = `${path}.tmp`;
    write(tmp, payload);
    rename(tmp, path);
  }
}

export const DESTINATIONS = ["public/snapshot.json", "dist/snapshot.json"];

/** Read, then write only if the read was genuinely complete. */
export async function bake(options = {}) {
  const result = await collect(options);
  const detail = result.streams.map((s) => `${s.name}: ${s.pages}p ${s.drained ? "drained" : "TRUNCATED"}`).join(", ");
  if (!result.complete) {
    return {
      written: false,
      count: result.events.length,
      reason: result.reason ?? "at least one stream hit the page backstop",
      detail,
    };
  }
  // Strip signatures: the client renders the snapshot, it never re-verifies or
  // re-publishes it — ~12% smaller for free. The relay reconciliation that
  // follows uses full signed events as always.
  const payload = JSON.stringify(result.events.map(({ sig, ...rest }) => rest));
  writeAtomic(options.destinations ?? DESTINATIONS, payload, options);
  return { written: true, count: result.events.length, reason: null, detail };
}

const invokedDirectly =
  process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;

if (invokedDirectly) {
  if (typeof WebSocket === "undefined") {
    console.error("no global WebSocket (Node < 22 without --experimental-websocket) — skipping bake");
    process.exit(1);
  }
  const result = await bake({ openSocket: (url) => new WebSocket(url) });
  if (!result.written) {
    console.error(`skipping bake — ${result.reason} (${result.detail})`);
    process.exit(1);
  }
  console.log(`baked ${result.count} events → ${DESTINATIONS.join(" + ")} (${result.detail})`);
  process.exit(0);
}
