import { parseSnapshot, relayStatusWord, renderLiveState, TRADE_WINDOW_MARGIN_S, UPDATING, type Snapshot, type Tracker } from "./challenge/live.js";
import { paint } from "./challenge/page.js";
import { wireMore } from "./challenge/more.js";
import { coalesce } from "./challenge/coalesce.js";
import { demoSnapshot } from "./challenge/demo.js";
import { renderJob } from "./challenge/job.js";
import { buyerHex } from "./challenge/model.js";
import { writeClipboard } from "./ui/clipboard.js";
import { usd, onSpotChange, spotUsd, startSpot } from "./ui/spot.js";
import { budgetValue } from "./challenge/hero.js";
import { EventStore } from "./challenge/shared/model.mjs";
import { ChallengeReader } from "./challenge/shared/relay.mjs";
import { RELAY_URL } from "./config.js";
declare const CHALLENGE_BUYER_PUBKEY: string;
declare const CHALLENGE_RELAYS: string[];
declare const CHALLENGE_QUORUM: number;

const el = (id: string) => document.getElementById(id)!;
const text = (id: string, value: string) => { el(id).textContent = value; };
const when = (secs: number) => new Intl.DateTimeFormat(undefined, {
  weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit", timeZoneName: "short",
}).format(new Date(secs * 1000));

// Shared chrome, same behaviour as /sell. No wallet or job posting here.
const toggle = el("nav-toggle");
const nav = el("nav-links");
toggle.addEventListener("click", () => toggle.setAttribute("aria-expanded", String(nav.classList.toggle("open"))));
nav.addEventListener("click", () => { nav.classList.remove("open"); toggle.setAttribute("aria-expanded", "false"); });
for (const btn of document.querySelectorAll<HTMLButtonElement>("[data-copy]")) {
  btn.addEventListener("click", async () => {
    const ok = await writeClipboard(el(btn.dataset.copy!).textContent?.trim() ?? "");
    btn.textContent = ok ? "✓" : "select it";
    btn.classList.toggle("ok", ok);
    setTimeout(() => { btn.textContent = "copy"; btn.classList.remove("ok"); }, 1600);
  });
}

// Floating "See the job" pill: gone once the job section reaches it.
const jobSection = el("job");
const jobFloat = el("job-float");
let floatQueued = false;
const placeFloat = () => {
  floatQueued = false;
  jobFloat.classList.toggle("is-gone", jobSection.getBoundingClientRect().top <= jobFloat.getBoundingClientRect().bottom);
};
const scheduleFloat = () => { if (!floatQueued) { floatQueued = true; requestAnimationFrame(placeFloat); } };
addEventListener("scroll", scheduleFloat, { passive: true });
addEventListener("resize", scheduleFloat);
placeFloat();
jobFloat.addEventListener("click", (e) => {
  e.preventDefault();
  history.pushState(null, "", "#job");
  jobSection.scrollIntoView({ behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth", block: "start" });
});

// The job is capped at about one screen with a "See more" toggle (bob). Collapsing scrolls the
// section back into view so the reader isn't left below it.
const jobMore = wireMore(el("job-body"), el("job-more") as HTMLButtonElement, () => {
  if (jobSection.getBoundingClientRect().top < 0) jobSection.scrollIntoView({ block: "start" });
});
addEventListener("resize", () => jobMore.update());
void document.fonts?.ready.then(() => jobMore.update());

let snapshot: Snapshot | null = null;
let receivedAt = 0;
// wasLive: the data has been live on this page load, so a routine poll or catch-up stays live.
// readyAt: last time the relay was fully ready (drives "Running now"). See live.ts observe.
let tracker: Tracker = { wasLive: false, readyAt: null };
let reader: ChallengeReader | null = null;
let demoStatus: string | null = null;

function render(s: Snapshot): void {
  const c = s.challenge;
  if (!c) return;
  renderJob(document, c.goal);
  jobMore.update();
  text("buyer-npub", c.buyerNpub);
  text("buyer-hex", buyerHex(c.buyerNpub));
  text("apply-command", `maxplayer-swarm join --buyer ${c.buyerNpub} --minutes 60`);
  tick();
}
function tick(): void {
  if (!snapshot) return;
  // The reader's current status, read every tick (not only the one copied into the last snapshot).
  const relay = (reader ? reader.status : snapshot.relay) as Snapshot["relay"];
  const status = demoStatus ?? relayStatusWord(relay);
  const now = snapshot.asOf + Math.max(0, (performance.now() - receivedAt) / 1000);
  const budget = (sats: number) => budgetValue(sats, spotUsd() === null ? null : usd(sats), performance.now());
  tracker = paint(document, snapshot, relay, tracker, { nowMs: Date.now(), now, when, status, budget });
}
function receive(value: unknown): void {
  snapshot = parseSnapshot(value);
  receivedAt = performance.now();
  render(snapshot);
}
function unavailable(): void {
  tracker = { ...tracker, readyAt: null };
  renderLiveState(document, "updating");
  text("connection-status", snapshot ? UPDATING : "Live view unavailable · check back soon");
}

// At most one snapshot rebuild per RENDER_MS (see challenge/coalesce.ts).
const RENDER_MS = 250;
function connect(): void {
  const store = new EventStore();
  const changed = coalesce(() => {
    try { receive({...store.snapshot(CHALLENGE_BUYER_PUBKEY), relay: live.status}); }
    catch { unavailable(); }
  }, RENDER_MS);
  // tradeWindowMarginS: read the current run's trade events first, older ones afterwards (relay.mjs window mode).
  const live = new ChallengeReader({url: RELAY_URL, challengeRelays: CHALLENGE_RELAYS, quorum: CHALLENGE_QUORUM, buyer: CHALLENGE_BUYER_PUBKEY, store, onChange: changed, tradeWindowMarginS: TRADE_WINDOW_MARGIN_S});
  reader = live;
  live.start();
  addEventListener("pagehide", (event) => { if (!event.persisted) live.stop(); });
}

if (new URLSearchParams(location.search).has("demo")) { demoStatus = "Demo data · not a real swarm run"; receive(demoSnapshot(Date.now() / 1000)); }
else void connect();
setInterval(tick, 1000);
// Budget shows in dollars at the live BTC-USD rate, like the rest of the site.
onSpotChange(() => { if (snapshot) render(snapshot); });
startSpot();
