import { connectionLine, isLive, parseSnapshot, renderSellers, UPDATING, type Snapshot } from "./challenge/live.js";
import { renderStart, startFact } from "./challenge/hero.js";
import { wireMore } from "./challenge/more.js";
import { demoSnapshot } from "./challenge/demo.js";
import { renderJob } from "./challenge/job.js";
import { buyerHex } from "./challenge/model.js";
import { writeClipboard } from "./ui/clipboard.js";
import { usd, onSpotChange, startSpot } from "./ui/spot.js";
import { EventStore } from "./challenge/shared/model.mjs";
import { ChallengeReader } from "./challenge/shared/relay.mjs";
import { RELAY_URL } from "./config.js";
declare const CHALLENGE_BUYER_PUBKEY: string;
declare const CHALLENGE_RELAYS: string[];

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

// Floating "See the job" pill, same as Buy's "Try it first": gone once the job section reaches it.
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
// Once the data has been live on this page load, a reconnect keeps showing the last counts.
let wasLive = false;

function render(s: Snapshot): void {
  const c = s.challenge;
  if (!c) return;
  renderJob(document, c.goal);
  jobMore.update();
  text("challenge-budget", usd(s.budget!.totalSats));
  text("buyer-npub", c.buyerNpub);
  text("buyer-hex", buyerHex(c.buyerNpub));
  text("apply-command", `maxplayer-challenges join --buyer ${c.buyerNpub} --minutes 60`);
  tick();
}
function tick(): void {
  if (!snapshot) return;
  const now = snapshot.asOf + Math.max(0, (performance.now() - receivedAt) / 1000);
  // "Work starts" becomes "Running now" (linking the live list) once the window opens (bob).
  if (snapshot.challenge) renderStart(document, startFact(snapshot.challenge, now, when));
  renderSellers(document, snapshot, now, !wasLive);
}
function receive(value: unknown, status: string): void {
  snapshot = parseSnapshot(value);
  receivedAt = performance.now();
  wasLive ||= isLive(snapshot.relay);
  render(snapshot);
  text("connection-status", connectionLine(snapshot.relay, status));
}
function unavailable(): void {
  text("connection-status", snapshot ? UPDATING : "Live view unavailable · check back soon");
}

function connect(): void {
  const store = new EventStore();
  const reader = new ChallengeReader({url: RELAY_URL, challengeRelays: CHALLENGE_RELAYS, buyer: CHALLENGE_BUYER_PUBKEY, store, onChange: () => {
    try { receive({...store.snapshot(CHALLENGE_BUYER_PUBKEY), relay: reader.status}, reader.status.authenticated ? "Live" : "Polling"); }
    catch { unavailable(); }
  }});
  reader.start();
  addEventListener("pagehide", (event) => { if (!event.persisted) reader.stop(); });
}

if (new URLSearchParams(location.search).has("demo")) receive(demoSnapshot(Date.now() / 1000), "Demo data · not a real challenge");
else void connect();
setInterval(tick, 1000);
// Budget shows in dollars at the live BTC-USD rate, like the rest of the site.
onSpotChange(() => { if (snapshot) render(snapshot); });
startSpot();
