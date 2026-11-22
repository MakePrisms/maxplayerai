// /swarm, in the look of /market (Bob, 2026-11-21): the call to join, then every open swarm in one list (live ones
// first, newest first), How it works, FAQs, footer. A swarm opens in /market's panel: its description, status and
// facts (live stats while a job runs), how to join (a line to copy), the current job, its members and its finished
// jobs. /swarm/<naddr> is the same page with that swarm's panel open, so a swarm still has an address to share.
// The data is read from our relay by the same reader and model as before (src/swarm/shared); no buyer key or relay
// list is built into the page. Remote text only ever reaches textContent.
import { parseSnapshot, relayStatusWord, renderLiveState, type Snapshot, type Tracker } from "./swarm/live.js";
import { paint } from "./swarm/page.js";
import { wireMore } from "./swarm/more.js";
import { coalesce } from "./swarm/coalesce.js";
import { demoSnapshot } from "./swarm/demo.js";
import { renderAbout, renderJob } from "./swarm/job.js";
import { indexTotals, renderIndex, renderSwarmJobs, route, shortNpub, swarmPath, type Route, type SwarmRow } from "./swarm/index.js";
import { isRunning, budgetValue } from "./swarm/hero.js";
import { writeClipboard } from "./ui/clipboard.js";
import { usd, onSpotChange, spotUsd, startSpot } from "./ui/spot.js";
import { SwarmStore, swarmIndex, swarmSnapshot } from "./swarm/shared/model.mjs";
import { SwarmReader } from "./swarm/shared/relay.mjs";
import { RELAY_URL } from "./config.js";
import { wireMenu } from "./site/menu.js";
import { refocus } from "./site/refocus.js";
import { stylesLoaded } from "./site/styled.js";
import { startMarket, type MarketHandle } from "./site/market-data.js";
import { fillBuyer, fillExpert, wireNpubChip } from "./site/profile.js";

const el = (id: string) => document.getElementById(id)!;
const text = (id: string, value: string) => { const e = el(id); if (e.textContent !== value) e.textContent = value; };
const when = (secs: number) => new Intl.DateTimeFormat(undefined, {
  weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit", timeZoneName: "short",
}).format(new Date(secs * 1000));
const SKILL = "https://www.maxplayer.ai/.well-known/skills/swarm-join/skill.md";
const RENDER_MS = 250; // at most one rebuild per RENDER_MS (see swarm/coalesce.ts)
const nf = new Intl.NumberFormat("en-US");
const query = new URLSearchParams(location.search);
const demo = query.has("demo");

const setMenu = wireMenu();

/* ---------- copy ---------- */
for (const btn of document.querySelectorAll<HTMLButtonElement>("[data-copy]")) {
  btn.addEventListener("click", async () => {
    const label = btn.dataset.label ?? (btn.dataset.label = btn.textContent ?? "Copy");
    const code = el(btn.dataset.copy!);
    const ok = await writeClipboard(code.textContent?.trim() ?? "");
    // Bob (2026-11-21): clicking Copy moves no text. The button keeps one width (swarm.css) and only ever says Copy
    // or Copied; if the browser refuses to copy, the line is selected instead, ready for the user to copy.
    if (ok) { btn.textContent = "Copied"; btn.classList.add("ok"); }
    else { const range = document.createRange(); range.selectNodeContents(code); getSelection()?.removeAllRanges(); getSelection()?.addRange(range); }
    clearTimeout(Number(btn.dataset.t));
    btn.dataset.t = String(setTimeout(() => { btn.textContent = label; btn.classList.remove("ok"); }, 1800));
  });
}

/* ---------- FAQ: one answer open at a time (as on /market) ---------- */
el("faq-list").addEventListener("click", (e) => {
  const b = (e.target as HTMLElement).closest<HTMLButtonElement>(".fq button");
  if (!b) return;
  const item = b.closest(".fq")!, open = !item.classList.contains("open");
  for (const f of document.querySelectorAll("#faq-list .fq")) { f.classList.remove("open"); f.querySelector("button")!.setAttribute("aria-expanded", "false"); }
  if (open) { item.classList.add("open"); b.setAttribute("aria-expanded", "true"); }
});

/* ---------- the board ---------- */
const BOARD_ROWS = 8;           // the rows the box shows; "See all" in its bottom bar shows the rest
let showAll = false;
function board(): void {
  const store = new SwarmStore();
  let wasReady = false;
  const reader: SwarmReader = new SwarmReader({ url: RELAY_URL, store, onChange: () => changed(), queueMs: 25 });
  // 4 small REQs: 1.4 s -> 0.57 s ready (speed review #2). It scales with buyers (4 REQs each, 40/s): if there are
  // ever hundreds, batch the profiles into one authors REQ instead (review of #1361, r5).
  const draw = () => {
    const status = reader.status;
    const ready = status.connected && status.historyComplete && !status.error;
    wasReady = status.connected && (wasReady || ready);
    const nowS = Math.floor(Date.now() / 1000);
    const rows = swarmIndex(store.events.values(), nowS) as SwarmRow[];
    renderIndex(document, rows, wasReady ? "live" : "loading", when, RELAY_URL, nowS, showAll ? Infinity : BOARD_ROWS);
    const more = el("swarm-more") as HTMLButtonElement;
    more.hidden = !wasReady || rows.length <= BOARD_ROWS;
    text("swarm-more", showAll ? "Show fewer" : `See all ${rows.length} swarms`);
    more.setAttribute("aria-expanded", String(showAll));
    const t = wasReady ? indexTotals(rows) : null;
    for (const [id, v] of [["st-live", t?.live], ["st-open", t?.open], ["st-members", t?.members], ["st-done", t?.done]] as const) {
      text(id, v === undefined ? "" : nf.format(v));
      el(id).classList.toggle("sk", v === undefined);
    }
    // Bob (2026-11-21): no "Live" under the board, and no "Updating…" while it loads (the grey rows say that); the
    // line speaks only when something is wrong.
    const word = relayStatusWord(status);
    text("index-status", !wasReady ? (status.error ? `Live view unavailable · ${status.error}` : "") : word === "Live" ? "" : word);
  };
  const changed = coalesce(() => { try { draw(); } catch { text("index-status", "Live view unavailable · check back soon"); } }, RENDER_MS);
  el("swarm-more").addEventListener("click", () => { showAll = !showAll; draw(); });
  reader.start();
  addEventListener("pagehide", (event) => { if (!event.persisted) reader.stop(); });
  setInterval(changed, 30_000); // "Live" and the ages follow the clock between relay changes
}

/* ---------- one swarm, in the panel ---------- */
const sheet = el("sheet"), main = document.querySelector<HTMLElement>(".mk-main")!, nav = document.querySelector<HTMLElement>(".nav")!;
const root = document.documentElement;
const jobSection = el("job");
const jobMore = wireMore(el("job-body"), el("job-more") as HTMLButtonElement, () => jobSection.scrollIntoView({ block: "start" }));

interface Open { key: string; reader: SwarmReader | null; stop: () => void; from: HTMLElement | null; pushed: boolean }
let open: Open | null = null;
let snapshot: Snapshot | null = null, receivedAt = 0, tracker: Tracker = { wasLive: false, readyAt: null };
let demoStatus: string | null = null, shownSwarm = "", swarmName = "Swarm";

/* ---------- a member's or the buyer's profile, in the same panel (Bob, 2026-11-21) ----------
   The market's reader (the one /market uses) starts the first time a profile is opened: a profile counts all of a
   key's market history, so it shows placeholders until that is read (quick when /market was visited before, since
   both pages keep it in the same browser store). Back returns to the swarm. */
let profile: { key: string; kind: "expert" | "buyer"; from: HTMLElement | null } | null = null;
let market: MarketHandle | null = null, marketState = "connecting", marketFailed = false, marketStarted = false, pageGone = false;
function drawProfile(): void {
  if (!profile) return;
  const pf = el("pf"), title = el("swarm-title");
  const x = market ? (profile.kind === "buyer" ? market.buyer(profile.key, 6) : market.expert(profile.key, 6)) : null;
  if (x) { pf.dataset.state = "ok"; (profile.kind === "buyer" ? fillBuyer : fillExpert)(x, title); return; }
  // Not found only once the market's whole history and names are in (r2 #2); "live" alone comes before the names.
  const r = market?.ready(), down = marketFailed || marketState === "failed", none = !down && marketState === "live" && (r === "all" || r === "partial");
  const who = profile.kind === "buyer" ? "Buyer" : "Expert";
  title.textContent = none ? `${who} not found` : who; title.classList.remove("hex");
  pf.dataset.state = none || down ? "none" : "wait";
  el("pf-note").textContent = down ? "The live market is not reachable right now."
    : !none ? "" : r === "partial" ? "Not found in the part of the market's history that could be read."
    : profile.kind === "buyer" ? "This buyer hasn't posted a job on the market yet." : "This agent hasn't worked on the market yet.";
}
function openProfile(key: string, kind: "expert" | "buyer", from: HTMLElement | null): void {
  profile = { key, kind, from: profile?.from ?? from };
  sheet.dataset.mode = "profile";
  el("sheet-back").hidden = false;
  el("pf").dataset.state = "wait";
  el("pf").scrollTop = 0;
  // (Re)started before the first draw, so a retry after a failure shows placeholders, not "not reachable" (r3 #3).
  if (!marketStarted) {
    marketStarted = true; marketFailed = false; marketState = "connecting";
    startMarket({ onView: () => drawProfile(), onStatus: (s) => { marketState = s; drawProfile(); } })
      .then((h) => { if (pageGone) { h.stop(); return; } market = h; drawProfile(); }, (err) => {
        console.warn("[swarm] the market reader did not start", err);
        marketStarted = false; marketFailed = true; drawProfile();     // the next profile opened tries again
      });
  }
  drawProfile();
  sheet.focus({ preventScroll: true });
}
function backToSwarm(focus = true): void {
  if (!profile) return;
  const was = profile.from;
  // The members list is redrawn every second, so the link that opened the profile may be a new element by now.
  const from = was?.isConnected ? was : was?.dataset.pk ? el("swp").querySelector<HTMLElement>(`[data-pk="${CSS.escape(was.dataset.pk)}"]`) : null;
  profile = null;
  sheet.dataset.mode = "swarm";
  el("sheet-back").hidden = true;
  const title = el("swarm-title");
  title.textContent = swarmName; title.classList.remove("hex");
  if (!focus) return;
  if (from) refocus(from);
  if (document.activeElement !== from) sheet.focus({ preventScroll: true });     // gone or hidden: the panel itself
}
el("sheet-back").addEventListener("click", () => backToSwarm());
// A member's name, the buyer's name, and any name in a profile's recent jobs open that profile here.
sheet.addEventListener("click", (e) => {
  const a = (e.target as HTMLElement).closest<HTMLElement>("[data-pk]");
  if (!a || e.defaultPrevented || e.button || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;   // a new tab goes to /market
  e.preventDefault();
  openProfile(a.dataset.pk!, a.dataset.kind === "buyer" ? "buyer" : "expert", a);
});
wireNpubChip(writeClipboard);

function state(s: "wait" | "ok" | "none", note = ""): void {
  el("swp").dataset.state = s;
  text("sw-none", note);
}

function render(s: Snapshot): void {
  const w = s.swarm;
  if (!w) return;
  state("ok");
  const key = JSON.stringify([w.buyer, w.id, w.name, w.purpose, w.buyerName]); // the name arrives after the swarm
  if (key !== shownSwarm) {
    shownSwarm = key;
    swarmName = w.name;
    if (!profile) text("swarm-title", w.name);
    document.title = `Maxplayer - Swarm - ${w.name}`;
    // Bob (2026-11-21): the buyer's name links to its profile on /market.
    text("buyer-label", w.buyerName || shortNpub(w.buyerNpub));
    el("buyer-label").setAttribute("href", `/market?buyer=${w.buyerNpub}`);
    el("buyer-label").dataset.pk = w.buyer;
    el("buyer-label").dataset.kind = "buyer";
    renderAbout(document, el("swarm-about"), w.purpose, w.name);
    const address = swarmPath(w.buyer, w.id, RELAY_URL).slice("/swarm/".length);
    text("cmd-join", `Read ${SKILL} and join the Maxplayer swarm ${address} for 120 minutes a day`);
  }
  // The current job: the one the swarm shows, while it is scheduled or running.
  const job = s.swarmJob && (s.swarmJob.status === "scheduled" || s.swarmJob.status === "in_progress") ? s.swarmJob : null;
  jobSection.hidden = !job;
  if (job) {
    jobSection.querySelector(".pf-h")!.textContent = job.status === "in_progress" ? "Current job" : "Next job";
    renderJob(document, job.goal);
    if (el("job-title").textContent === "The job.") el("job-title").textContent = job.title;
    jobMore.update();
  }
  // Past jobs: the ones it finished, newest first.
  renderSwarmJobs(document, (s.swarmJobs ?? []).filter((j) => j.status === "completed"), when);
  tick();
}

function tick(): void {
  if (!snapshot || !open) return;
  const relay = (open.reader ? open.reader.status : snapshot.relay) as Snapshot["relay"];
  const status = demoStatus ?? relayStatusWord(relay);
  const now = snapshot.asOf + Math.max(0, (performance.now() - receivedAt) / 1000);
  const budget = (sats: number) => budgetValue(sats, spotUsd() === null ? null : usd(sats), performance.now());
  tracker = paint(document, snapshot, relay, tracker, { nowMs: Date.now(), now, when, status, budget, quiet: true });
  // The status line at the top of the panel.
  const fresh = tracker.readyAt === null ? null : Math.max(0, Date.now() - tracker.readyAt) / 1000;
  const live = !!snapshot.swarmJob && isRunning(snapshot.swarmJob, now, demoStatus ? 0 : fresh);
  const closed = snapshot.swarm?.status === "closed";
  el("sw-lamp").className = live ? "lamp on" : "lamp";
  text("sw-st", live ? "Live: a job is running" : closed ? "Closed" : snapshot.swarmJob?.status === "scheduled" && now < snapshot.swarmJob.ends ? "Open · next job scheduled" : "Open · no job running");
}

function receive(value: unknown): void {
  snapshot = parseSnapshot(value);
  receivedAt = performance.now();
  render(snapshot);
}

function resetPanel(): void {
  backToSwarm(false);
  jobMore.reset();
  swarmName = "Swarm";
  snapshot = null; tracker = { wasLive: false, readyAt: null }; shownSwarm = ""; demoStatus = null;
  text("swarm-title", "Swarm");
  el("swarm-about").replaceChildren();
  el("swarm-jobs").replaceChildren();
  jobSection.hidden = true;
  renderLiveState(document, "updating");
  text("connection-status", "");
  state("wait");
}

function showSheet(): void {
  root.classList.add("sheet-open");
  sheet.setAttribute("aria-hidden", "false");
  main.inert = true; nav.inert = true;
  setMenu(false);
  sheet.focus({ preventScroll: true });
  el("swp").scrollTop = 0;
}

/** Open one swarm. `where`: its route; `pushed`: the address was changed to it by this page (Close goes back). */
function openSwarm(where: Route, from: HTMLElement | null, pushed: boolean): void {
  const key = where.view === "swarm" ? `${where.buyer}/${where.swarm}` : demo ? "demo" : "missing";
  if (open?.key === key) return;
  open?.stop();
  resetPanel();
  open = { key, reader: null, stop: () => {}, from, pushed };
  showSheet();
  if (demo && where.view !== "swarm") {
    demoStatus = "Demo data · not a real swarm";
    receive(demoSnapshot(Date.now() / 1000, { privateTasks: query.get("demo") === "private" }));
    return;
  }
  if (where.view !== "swarm") { text("swarm-title", "Swarm not found"); state("none", "That address isn't a swarm. Pick one from the list."); return; }
  const store = new SwarmStore();
  const { buyer, swarm } = where;
  const mine = open;
  const changed = coalesce(() => {
    if (open !== mine) return;
    try {
      const s = swarmSnapshot(store.events.values(), { buyer, swarm }, Math.floor(Date.now() / 1000));
      const status = reader.status;
      // Read in full and still no such swarm: say so instead of loading forever.
      if (!s.swarm) {
        if (status.relays?.[0]?.historyComplete) { text("swarm-title", "Swarm not found"); state("none", "No swarm with this address on our relay yet. Pick one from the list."); }
        return;
      }
      receive({ ...s, relay: status });
    } catch {
      tracker = { ...tracker, readyAt: null };
      renderLiveState(document, "updating");
      text("connection-status", snapshot ? "" : "Live view unavailable · check back soon");
    }
  }, RENDER_MS);
  const reader: SwarmReader = new SwarmReader({ url: RELAY_URL, store, swarm: { buyer, swarm }, onChange: changed });
  mine.reader = reader;
  mine.stop = () => reader.stop();
  reader.start();
}

function closeSwarm(fromHistory = false): void {
  if (!open) return;
  backToSwarm(false);
  const was = open;
  open.stop();
  open = null;
  // Focus leaves the panel before it is hidden from assistive tech (r3 nit): the page becomes usable again, focus
  // goes back to the swarm's link (found again if the list was redrawn), and only then is the panel aria-hidden.
  root.classList.remove("sheet-open");
  main.inert = false; nav.inert = false;
  const href = was.from?.getAttribute("href");
  const back = was.from?.isConnected ? was.from : href ? document.querySelector<HTMLElement>(`#swarm-list a.who[href="${CSS.escape(href)}"]`) : null;
  if (back) refocus(back);
  if (sheet.contains(document.activeElement)) (document.activeElement as HTMLElement).blur();   // no opener (opened from a link), or a hidden one
  sheet.setAttribute("aria-hidden", "true");
  document.title = "Maxplayer - Swarm";
  if (!fromHistory) {
    if (was.pushed) history.back();                       // popstate finds the panel already closed
    else history.replaceState(null, "", "/swarm" + location.search);
  }
}

// A row, or its name, opens the swarm in the panel. The name is a real link: a new tab goes to the link itself.
el("swarm-list").addEventListener("click", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>(".row.go");
  if (!row || e.defaultPrevented || e.button || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  const a = row.querySelector<HTMLAnchorElement>("a.who");
  if (!a) return;
  e.preventDefault();
  const path = new URL(a.href).pathname;
  history.pushState(null, "", path + location.search);
  openSwarm(route(path), a, true);
});
el("sheet-x").addEventListener("click", () => closeSwarm());
el("sheet-bg").addEventListener("click", () => closeSwarm());
addEventListener("keydown", (e) => { if (e.key === "Escape") closeSwarm(); });
addEventListener("popstate", () => {
  const where = route(location.pathname);
  if (where.view === "index" && !demo) closeSwarm(true);
  else if (where.view !== "index") openSwarm(where, null, false);
});

setInterval(() => { if (!document.hidden) tick(); }, 1000);     // nothing to redraw in a hidden tab (speed review #9)
document.addEventListener("visibilitychange", () => { if (!document.hidden) tick(); });
onSpotChange(() => { if (snapshot) render(snapshot); });   // the budget shows in dollars at the live BTC-USD rate
startSpot();
board();
addEventListener("pagehide", (event) => { if (!event.persisted) { pageGone = true; market?.stop(); open?.stop(); } });

// Shown once the stylesheets and fonts are in, as on /market (market.css hides the page until body.ready).
let shown = false;
function show(): void {
  if (shown) return;
  shown = true;
  document.body.classList.add("ready");
  const where = route(location.pathname);
  if (where.view !== "index" || demo) openSwarm(where, null, false);
}
void stylesLoaded().then(() => {
  const faces = ['400 16px "Instrument Sans"', '500 16px "Instrument Sans"', 'italic 500 16px "Work Sans Logo"'];
  if (document.fonts?.load) Promise.all(faces.map((f) => document.fonts.load(f))).then(show, show);
  setTimeout(show, 800);
});
