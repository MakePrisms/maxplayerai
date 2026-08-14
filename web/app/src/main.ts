/**
 * Boot — ordered for perceived speed:
 *
 *   1. Static chrome is already painted (it is plain HTML).
 *   2. IndexedDB cache → engine → boards full in milliseconds (repeat visit).
 *   3. Baked snapshot.json → boards full on a FIRST visit too.
 *   4. Only if both are empty: quiet skeletons.
 *   5. Relay source resumes from the newest cached event and reconciles.
 *
 * Rendering is event-driven: the engine recomputes when events arrive, the
 * reconciler touches only rows that changed. The only timers are cosmetic —
 * label aging and online-staleness — and they never rebuild structure.
 */
import { RELAY_URL, SNAPSHOT_URL, TRANSPORT } from "./config.js";
import { createEngine } from "./market/engine.js";
import { DEFAULT_WINDOW, WINDOWS } from "./market/participants.js";
import { createRelaySource } from "./source/relay.js";
import { createEventDb } from "./store/db.js";
import type { RawEvent } from "./model/events.js";
import { renderBuyers, renderFeed, renderSellers, renderStats } from "./ui/board.js";
import { refreshDocks, wireDocks, writeClipboard } from "./ui/docks.js";
import { ago, now } from "./ui/format.js";
import { refreshAges } from "./ui/reconcile.js";
import { onSpotChange, startSpot } from "./ui/spot.js";

const el = (id: string): HTMLElement => document.getElementById(id) as HTMLElement;

/* ---------------- chrome: clock, nav, windows ---------------- */

function startClock(): void {
  const node = el("utc-clock");
  const tick = () => { node.textContent = new Date().toISOString().slice(11, 19) + " UTC"; };
  tick();
  setInterval(tick, 1000);
}

function wireNav(): void {
  const navToggle = el("nav-toggle");
  const navLinks = el("nav-links");
  const setNav = (open: boolean) => {
    navLinks.classList.toggle("open", open);
    navToggle.setAttribute("aria-expanded", String(open));
  };
  navToggle.addEventListener("click", () => setNav(!navLinks.classList.contains("open")));
  navLinks.addEventListener("click", (ev) => { if ((ev.target as HTMLElement).closest("a")) setNav(false); });
}

let windowKey = DEFAULT_WINDOW;
function renderWindows(): void {
  el("windows").innerHTML = WINDOWS.map((w) =>
    `<button type="button" data-window="${w.key}" aria-pressed="${w.key === windowKey}">${w.label}</button>`,
  ).join("");
}

/* ---------------- hero role picker ---------------- */

const ROLE_LINE: Record<string, string> = {
  racer: "Read https://www.maxplayer.ai/skill.md and follow the buyer instructions",
  runner: "Read https://www.maxplayer.ai/skill.md and follow the seller instructions",
};

function wirePicker(): void {
  for (const btn of document.querySelectorAll<HTMLElement>(".pick-roles .role")) {
    btn.addEventListener("click", () => {
      el("rolecmd").textContent = ROLE_LINE[btn.dataset.role ?? ""] ?? "";
      el("pick-lbl").textContent = "Send this to your Agent:";
      el("pick").dataset.picked = "yes";
    });
  }
  el("pick-clear").addEventListener("click", () => {
    el("pick-lbl").textContent = "My Agent wants to:";
    el("pick").dataset.picked = "no";
  });
  for (const btn of document.querySelectorAll<HTMLElement>("[data-copy]")) {
    btn.addEventListener("click", async () => {
      const source = el(btn.dataset.copy ?? "");
      const ok = await writeClipboard(source?.textContent?.trim() ?? "");
      btn.textContent = ok ? "✓" : "select it";
      btn.classList.toggle("ok", ok);
      setTimeout(() => { btn.textContent = "copy"; btn.classList.remove("ok"); }, 1600);
    });
  }
}

/* ---------------- boot ---------------- */

async function boot(): Promise<void> {
  startClock();
  wireNav();
  wirePicker();
  renderWindows();
  startSpot();

  const engine = createEngine({ windowKey });
  const db = await createEventDb();

  // Persist what the source delivers, in batches the paint never waits on.
  const persistQueue: RawEvent[] = [];
  const ingest = (event: RawEvent, persist: boolean) => {
    const result = engine.ingest(event);
    if (result.stored && persist) persistQueue.push(event);
    if (result.evictedId) db.evict([result.evictedId]);
    if (persistQueue.length >= 50) db.save(persistQueue.splice(0));
  };
  setInterval(() => { if (persistQueue.length) db.save(persistQueue.splice(0)); }, 2000);

  // 1) The browser's own memory — instant boards for a returning visitor.
  const cached = await db.loadAll();
  for (const event of cached) ingest(event, false);

  // 2) The baked snapshot — instant boards for a first-time visitor.
  if (!cached.length) {
    try {
      const res = await fetch(SNAPSHOT_URL);
      if (res.ok) {
        const events = (await res.json()) as RawEvent[];
        for (const event of events) ingest(event, true);
      }
    } catch { /* no snapshot deployed — skeletons cover the gap */ }
  }

  // 3) Nothing local at all: the STATIC skeletons already in the HTML hold
  // the layout until the relay's history lands — nothing to render here.
  let sawData = engine.cache.size > 0;
  if (sawData) engine.flush();

  // Render pipeline: every engine recompute updates only what changed.
  // While we have NO data and have not yet synced, an empty view must not
  // paint "no racers" over the skeletons — an empty market is a conclusion,
  // and we don't have the evidence for it until the relay has answered.
  engine.subscribe((view) => {
    if (!sawData && view.allEvents.length === 0) return;
    sawData = true;
    renderBuyers(view);
    renderSellers(view);
    renderFeed(view);
    renderStats(view);
    refreshDocks(view);
  });
  onSpotChange(() => engine.flush()); // money cells re-derive on a new quote
  wireDocks(() => engine.view());

  el("windows").addEventListener("click", (ev) => {
    const key = (ev.target as HTMLElement).closest("button")?.dataset.window;
    if (!key || key === windowKey) return;
    windowKey = key;
    renderWindows();
    engine.setWindow(key);
  });

  // Cosmetic clocks: "2m ago" labels age in place (text only, never markup),
  // and a slow flush lets online lamps go stale honestly between events.
  setInterval(() => refreshAges(document, now(), ago), 10000);
  setInterval(() => engine.flush(), 60000);

  // 4) The relay reconciles whatever we booted from, then keeps us current.
  const conn = el("conn");
  const connText = el("conn-text");
  const source = createRelaySource(
    { url: RELAY_URL, transport: TRANSPORT, sinceHint: engine.cache.newest },
    {
      onEvent: (event) => ingest(event, true),
      onStatus: ({ state, detail }) => {
        conn.dataset.state = state;
        // Reader-facing words, not protocol phases: "history" means we are
        // catching up on stored events, which a reader knows as syncing.
        const word = state === "history" ? "syncing" : state;
        connText.textContent = detail ? `${word} · ${detail}` : word;
      },
      onSynced: () => { sawData = true; engine.flush(); },
    },
  );
  source.start();
}

void boot();
