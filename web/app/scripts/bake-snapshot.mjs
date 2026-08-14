/**
 * Bake a market snapshot into a static file.
 *
 * Run at deploy time (or on a schedule) so a FIRST-TIME visitor's boards are
 * full on first paint — the client loads snapshot.json instantly and the
 * relay reconciles the tail. Zero hosting: the snapshot is just another
 * static file next to index.html.
 *
 * Node 20 needs: node --experimental-websocket scripts/bake-snapshot.mjs
 *
 * Failure here must never fail a deploy: the client treats a missing
 * snapshot.json as "first load, show skeletons" and reconciles from the relay
 * as always. The deploy command runs this best-effort.
 */
import { writeFileSync } from "node:fs";

if (typeof WebSocket === "undefined") {
  console.error("no global WebSocket (Node < 22 without --experimental-websocket) — skipping bake");
  process.exit(1);
}

const RELAY_URL = "wss://relay.maxplayer.ai";
const TAGGED_KINDS = [3401, 3402, 3403, 3404, 3405, 3406, 3400, 30340];
const PAGE = 500;
const MAX_PAGES = 40;

const events = new Map();
const ws = new WebSocket(RELAY_URL);
ws.onerror = () => { console.error("relay unreachable — skipping bake"); process.exit(1); };
let pages = 0;
let oldest = null;
let seenThisPage = 0;
let streamIndex = 0;
let sub = 0;

const streams = [
  { kinds: TAGGED_KINDS, limit: PAGE, "#t": ["maxplayer"] },
  { kinds: [0], limit: PAGE },
];

function page() {
  seenThisPage = 0;
  const filter = { ...streams[streamIndex] };
  if (oldest != null) filter.until = oldest - 1;
  ws.send(JSON.stringify(["REQ", `s${++sub}`, filter]));
}

ws.onopen = () => page();
ws.onmessage = (msg) => {
  const frame = JSON.parse(msg.data);
  if (frame[0] === "EVENT") {
    const e = frame[2];
    seenThisPage += 1;
    if (oldest == null || e.created_at < oldest) oldest = e.created_at;
    events.set(e.id, e);
  } else if (frame[0] === "EOSE") {
    pages += 1;
    if (pages >= MAX_PAGES || seenThisPage === 0) {
      streamIndex += 1;
      oldest = null;
      if (streamIndex >= streams.length) {
        // Strip signatures: the client renders the snapshot, it never
        // re-verifies or re-publishes it — ~12% smaller for free. The relay
        // reconciliation that follows uses full signed events as always.
        const list = [...events.values()].map(({ sig, ...rest }) => rest);
        writeFileSync("public/snapshot.json", JSON.stringify(list));
        writeFileSync("dist/snapshot.json", JSON.stringify(list));
        console.log(`baked ${list.length} events → public/snapshot.json + dist/snapshot.json`);
        process.exit(0);
      }
    }
    page();
  }
};
setTimeout(() => { console.error("timed out"); process.exit(1); }, 60000);
