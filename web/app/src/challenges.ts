import { parseSnapshot, renderSellers, type Snapshot } from "./challenge/live.js";
import { demoSnapshot } from "./challenge/demo.js";
import { renderJob } from "./challenge/job.js";
import { buyerHex } from "./challenge/model.js";
import { writeClipboard } from "./ui/clipboard.js";
import { usd, onSpotChange, startSpot } from "./ui/spot.js";
declare const COMMUNITY_APP_URL: string;

const el = (id: string) => document.getElementById(id)!;
const text = (id: string, value: string) => { el(id).textContent = value; };
const when = (secs: number) => new Intl.DateTimeFormat(undefined, {
  weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit", timeZoneName: "short",
}).format(new Date(secs * 1000));

// Shared chrome, same behaviour as /sell. No wallet, relay connection or job posting here.
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

let snapshot: Snapshot | null = null;
let receivedAt = 0;

function render(s: Snapshot): void {
  const c = s.challenge;
  if (!c) return;
  renderJob(document, c.goal);
  text("join-open", when(c.joinOpens));
  text("challenge-start", when(c.starts));
  text("challenge-budget", usd(s.budget!.totalSats));
  text("buyer-npub", c.buyerNpub);
  text("buyer-hex", buyerHex(c.buyerNpub));
  text("apply-command", `maxplayer challenge join --buyer ${c.buyerNpub} --minutes 60`);
  tick();
}
function tick(): void {
  if (!snapshot) return;
  renderSellers(document, snapshot, snapshot.asOf + Math.max(0, (performance.now() - receivedAt) / 1000));
}
function receive(value: unknown, status: string): void {
  snapshot = parseSnapshot(value);
  receivedAt = performance.now();
  render(snapshot);
  const r = snapshot.relay;
  text("connection-status", r && (!r.connected || !r.authenticated) ? "Reconnecting · showing last known" : r && !r.historyComplete ? "Catching up · totals may be low" : status);
}
function unavailable(): void {
  text("connection-status", snapshot ? "Reconnecting · showing last known" : "Live view unavailable · check back soon");
}

async function connect(): Promise<void> {
  const base = COMMUNITY_APP_URL.replace(/\/$/, "");
  try {
    const response = await fetch(`${base}/api/challenges/current`, { signal: AbortSignal.timeout(8000) });
    if (!response.ok) throw Error("Unavailable");
    receive(await response.json(), "Live");
  } catch { unavailable(); }
  const stream = new EventSource(`${base}/api/challenges/current/stream`);
  stream.onmessage = (e) => { try { receive(JSON.parse(e.data), "Live"); } catch { unavailable(); } };
  stream.onerror = unavailable;
}

if (new URLSearchParams(location.search).has("demo")) receive(demoSnapshot(Date.now() / 1000), "Demo data · not a real challenge");
else void connect();
setInterval(tick, 1000);
// Budget shows in dollars at the live BTC-USD rate, like the rest of the site.
onSpotChange(() => { if (snapshot) render(snapshot); });
startSpot();
