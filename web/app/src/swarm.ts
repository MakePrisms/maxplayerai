// /swarm: every buyer's swarms, read from our relay. /swarm/<naddr>: one swarm, its shown swarm
// job, members with today's minutes, and how to join (docs/specs/swarm.md §12 PR 3). No buyer key
// or relay list is built into the page.
import { parseSnapshot, relayStatusWord, renderLiveState, UPDATING, type Snapshot, type Tracker } from "./swarm/live.js";
import { paint } from "./swarm/page.js";
import { wireMore } from "./swarm/more.js";
import { coalesce } from "./swarm/coalesce.js";
import { demoSnapshot } from "./swarm/demo.js";
import { renderJob } from "./swarm/job.js";
import { renderIndex, renderSwarmJobs, route, shortNpub, swarmPath, firstLine, type SwarmRow } from "./swarm/index.js";
import { writeClipboard } from "./ui/clipboard.js";
import { usd, onSpotChange, spotUsd, startSpot } from "./ui/spot.js";
import { budgetValue } from "./swarm/hero.js";
import { SwarmStore, swarmIndex, swarmSnapshot } from "./swarm/shared/model.mjs";
import { SwarmReader } from "./swarm/shared/relay.mjs";
import { RELAY_URL } from "./config.js";

const el = (id: string) => document.getElementById(id)!;
const text = (id: string, value: string) => { el(id).textContent = value; };
const when = (secs: number) => new Intl.DateTimeFormat(undefined, {
  weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit", timeZoneName: "short",
}).format(new Date(secs * 1000));
const SKILL = "https://www.maxplayer.ai/.well-known/skills/swarm-join/skill.md";

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

const query = new URLSearchParams(location.search);
const demo = query.has("demo");
const where = demo ? null : route(location.pathname);
const RENDER_MS = 250; // at most one rebuild per RENDER_MS (see swarm/coalesce.ts)

if (where?.view === "index") index();
else swarmPage(where);

// ---- index ---------------------------------------------------------------------------------

function index(): void {
  const store = new SwarmStore();
  let reader: SwarmReader | null = null, wasReady = false;
  const draw = () => {
    const status = reader!.status;
    const ready = status.connected && status.historyComplete && !status.error;
    wasReady = status.connected && (wasReady || ready);
    const nowS = Math.floor(Date.now() / 1000);
    renderIndex(document, swarmIndex(store.events.values(), nowS) as SwarmRow[], wasReady ? "live" : "loading", when, RELAY_URL);
    text("index-status", !wasReady ? (status.error ? `Live view unavailable · ${status.error}` : UPDATING) : relayStatusWord(status));
  };
  const changed = coalesce(() => { try { draw(); } catch { text("index-status", "Live view unavailable · check back soon"); } }, RENDER_MS);
  reader = new SwarmReader({ url: RELAY_URL, store, onChange: changed });
  reader.start();
  addEventListener("pagehide", (event) => { if (!event.persisted) reader!.stop(); });
  setInterval(changed, 30_000); // "Running now" follows the clock between relay changes
}

// ---- one swarm -----------------------------------------------------------------------------

function swarmPage(target: Exclude<typeof where, { view: "index" }>): void {
  document.body.classList.add("view-swarm");
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
  // readyAt: last time the swarm data was fully ready (drives "Running now"). See live.ts observe.
  let tracker: Tracker = { wasLive: false, readyAt: null };
  let reader: SwarmReader | null = null;
  let demoStatus: string | null = null;
  let shownSwarm = "";

  function render(s: Snapshot): void {
    const w = s.swarm;
    if (w) {
      const key = JSON.stringify([w.buyer, w.id, w.name, w.purpose, w.buyerName]); // the name arrives after the swarm
      if (key !== shownSwarm) {
        shownSwarm = key;
        text("swarm-title", w.name);
        text("swarm-sub", firstLine(w.purpose));
        document.title = `${w.name} — Swarm — MAXPLAYER AI`;
        text("buyer-label", w.buyerName ? `${w.buyerName} (${shortNpub(w.buyerNpub)})` : w.buyerNpub);
        text("buyer-hex", w.buyer);
        const address = target?.view === "swarm" ? target.naddr : swarmPath(w.buyer, w.id, RELAY_URL).slice("/swarm/".length);
        text("cmd-join", `Read ${SKILL} and join the Maxplayer swarm ${address} for 60 minutes a day`);
        text("apply-command", `maxplayer swarm join ${address} --minutes 60`);
      }
    }
    if (s.swarmJob) { renderJob(document, s.swarmJob.goal); jobMore.update(); }
    renderSwarmJobs(document, s.swarmJobs ?? [], when);
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

  // Not found: only the hero's title and a way back to the index; no facts, buyer, job or join.
  function notFound(why: string): void {
    document.body.classList.remove("view-swarm");
    document.body.classList.add("view-missing");
    text("swarm-title", "Swarm not found");
    text("swarm-sub", `${why} Pick one from the list of swarms.`);
    const back = document.querySelector<HTMLAnchorElement>('a.h-button[data-view="index"]');
    if (back) { back.href = "/swarm"; back.firstElementChild!.textContent = "See the swarms"; }
  }
  if (demo) { demoStatus = "Demo data · not a real swarm"; receive(demoSnapshot(Date.now() / 1000, { privateTasks: query.get("demo") === "private" })); }
  else if (target?.view !== "swarm") notFound("That address isn't a swarm."); else {
    const store = new SwarmStore();
    const { buyer, swarm } = target;
    let missing = false;
    const changed = coalesce(() => {
      try {
        const s = swarmSnapshot(store.events.values(), { buyer, swarm }, Math.floor(Date.now() / 1000));
        const status = reader!.status;
        // Read in full and still no such swarm: say so instead of loading forever.
        if (!s.swarm && status.relays?.[0]?.historyComplete && !missing) {
          missing = true;
          notFound("No swarm with this address on our relay yet.");
        }
        receive({ ...s, relay: status });
      } catch { unavailable(); }
    }, RENDER_MS);
    reader = new SwarmReader({ url: RELAY_URL, store, swarm: { buyer, swarm }, onChange: changed });
    reader.start();
    addEventListener("pagehide", (event) => { if (!event.persisted) reader!.stop(); });
  }
  setInterval(tick, 1000);
  // Budget shows in dollars at the live BTC-USD rate, like the rest of the site.
  onSpotChange(() => { if (snapshot) render(snapshot); });
  startSpot();
}
