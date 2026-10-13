declare const WEB_ANALYTICS_ENABLED: boolean;
/**
 * /tokens — the token-trade market, read-only and unlisted (team link only:
 * no nav, footer or llms.txt entry, and served noindex).
 *
 * Its own bundle (tokens.js): the jobs board's terminal.js is untouched and
 * this page never opens the production relay. It reads listings and status
 * chains from the trade CLI's public relays (trade/relays.ts), validates every
 * event with the CLI's rules (trade/validate.ts), and renders the book
 * (trade/book.ts). No wallet, no keys, no trading: nothing here can sign.
 */
import { startAnalytics } from "./analytics.js";
import { WINDOWS, completedStats, createBook, type BookView, type LotRow } from "./trade/book.js";
import { dockSide, mintLabel, rate, seller, timeLeft } from "./trade/format.js";
import { DEFAULT_TRADE_RELAYS, createTradeReader, type RelayState } from "./trade/relays.js";
import { ago, duration, esc, nf, now, stamp } from "./ui/format.js";
import { reconcileList, type KeyedItem } from "./ui/reconcile.js";

const el = (id: string): HTMLElement => document.getElementById(id) as HTMLElement;

function wireNav(): void {
  const toggle = el("nav-toggle");
  const links = el("nav-links");
  const set = (open: boolean) => { links.classList.toggle("open", open); toggle.setAttribute("aria-expanded", String(open)); };
  toggle.addEventListener("click", () => set(!links.classList.contains("open")));
  links.addEventListener("click", (ev) => { if ((ev.target as HTMLElement).closest("a")) set(false); });
}

function startClock(): void {
  const node = el("utc-clock");
  const tick = () => { node.textContent = new Date().toISOString().slice(11, 19) + " UTC"; };
  tick();
  setInterval(tick, 1000);
}

const legHtml = (net: number, mint: string) =>
  `<span class="sats">${nf.format(net)}</span> <span class="dim" title="${esc(mint)}">${esc(mintLabel(mint))}</span>`;

function openItem(r: LotRow, t: number): KeyedItem {
  const s = seller(r.lot.maker);
  return {
    key: r.id,
    className: "row lots-grid",
    tabIndex: 0,
    data: { lot: r.id },
    html:
      `<span class="leg">${legHtml(r.lot.give.net, r.lot.give.mint_url)}</span>` +
      `<span class="leg">${legHtml(r.lot.want.net, r.lot.want.mint_url)}</span>` +
      `<span class="num" title="${esc(rate(1 / r.price))} give per want">${esc(rate(r.price))}</span>` +
      `<span class="person col-seller" title="${esc(s.npub)}">${esc(s.short)}</span>` +
      `<span class="num" data-expires="${r.lot.expires_at}">${esc(timeLeft(r.lot.expires_at - t))}</span>`,
  };
}

function closedItem(r: LotRow, t: number): KeyedItem {
  return {
    key: r.id,
    className: "row recent-grid",
    tabIndex: 0,
    data: { lot: r.id },
    html:
      `<span class="tag" data-s="${r.state}">${r.state}</span>` +
      `<span class="line">${legHtml(r.lot.give.net, r.lot.give.mint_url)} <span class="dim">→</span> ${legHtml(r.lot.want.net, r.lot.want.mint_url)}</span>` +
      `<span class="when" data-ts="${r.updated_at}">${ago(r.updated_at, t)}</span>`,
  };
}

let windowKey = "24h";
/** After the first relay answers, wait at most this long for the others before painting. */
const SETTLE_MS = 2500;

/** The buttons are static HTML (painted before JS); this only wires them. */
function wireWindows(onChange: () => void): void {
  const box = el("windows");
  box.addEventListener("click", (ev) => {
    const b = (ev.target as HTMLElement).closest<HTMLElement>("button[data-w]");
    if (!b) return;
    windowKey = b.dataset.w ?? windowKey;
    for (const x of box.querySelectorAll("button")) x.setAttribute("aria-pressed", String(x === b));
    onChange();
  });
}

/** Fills the static stat cells in place: labels never re-render, so nothing moves. */
function renderStats(v: BookView, t: number): void {
  const w = WINDOWS.find((x) => x.key === windowKey) ?? WINDOWS[0]!;
  const c = completedStats(v, t, w.seconds);
  const values: Record<string, string> = {
    trades: nf.format(c.trades),
    give: nf.format(c.giveSats),
    want: nf.format(c.wantSats),
    sellers: nf.format(c.sellers),
    pairs: nf.format(c.pairs),
    fill: c.medianFill == null ? "—" : duration(c.medianFill),
  };
  for (const dd of el("statgrid").querySelectorAll<HTMLElement>("dd[data-stat]")) {
    const val = values[dd.dataset.stat ?? ""];
    if (val != null && dd.textContent !== val) dd.textContent = val;
  }
}

let selected: string | null = null;
let lastView: BookView | null = null;

function renderDetail(): void {
  const box = el("trade-detail");
  const all = lastView ? [...lastView.open, ...lastView.closed, ...lastView.quarantined] : [];
  const r = all.find((x) => x.id === selected);
  if (!r) { box.hidden = true; return; }
  const s = seller(r.lot.maker);
  const kv = (k: string, v: string) => `<div><dt>${k}</dt><dd>${v}</dd></div>`;
  box.hidden = false;
  el("trade-detail-body").innerHTML =
    `<dl class="kv">` +
    kv("Status", `<span class="tag" data-s="${r.state}">${r.state}</span>${r.detail ? ` <span class="dim">${esc(r.detail)}</span>` : ""}`) +
    kv("Gives", `${nf.format(r.lot.give.net)} sat · <code>${esc(r.lot.give.mint_url)}</code>`) +
    kv("Wants", `${nf.format(r.lot.want.net)} sat · <code>${esc(r.lot.want.mint_url)}</code>`) +
    kv("Price", `${esc(rate(r.price))} want per give · ${esc(rate(1 / r.price))} give per want`) +
    kv("Seller", `<code>${esc(s.npub)}</code>`) +
    kv("Listed", esc(stamp(r.lot.created_at))) +
    kv("Expires", esc(stamp(r.lot.expires_at))) +
    kv("Listing id", `<code>${esc(r.id)}</code>`) +
    (r.chain.length ? kv("Status chain", r.chain.map((id) => `<code>${esc(id.slice(0, 12))}</code>`).join(" → ")) : "") +
    `</dl>`;
}

function render(v: BookView, answered: boolean, settled: boolean): void {
  lastView = v;
  const t = now();
  // bob: skeleton -> data in ONE swap. Painting each relay's answer as it
  // lands reshuffled the rows two or three times in the first second.
  if (!settled) return;
  renderStats(v, t);
  const recent = v.closed.filter((r) => r.updated_at >= t - 2 * 86400);
  el("lots-meta").textContent = `${v.open.length} open`;
  el("recent-meta").textContent = `${recent.length} in 48h`;
  if (v.open.length) reconcileList(el("lots"), v.open.map((r) => openItem(r, t)));
  else if (answered) reconcileList(el("lots"), [{ key: "-empty", className: "empty", html: "No open lots right now." }]);
  if (recent.length) reconcileList(el("recent"), recent.slice(0, 120).map((r) => closedItem(r, t)));
  else if (answered) reconcileList(el("recent"), [{ key: "-empty", className: "empty", html: "Nothing sold, cancelled or expired in the last 48 hours." }]);
  renderDetail();
}

function boot(): void {
  wireNav();
  startClock();
  const book = createBook();
  const states = new Map<string, RelayState>();
  let answered = false;
  /** Relays that have finished (or failed) their first read. */
  const firstRead = new Set<string>();
  let graceOver = false;
  const settled = () => answered && (firstRead.size >= DEFAULT_TRADE_RELAYS.length || graceOver);
  const live = () => [...states].filter(([, s]) => s === "live").map(([u]) => u);
  const paint = () => render(book.view(now(), live()), answered, settled());
  wireWindows(() => paint());

  const conn = el("conn");
  const connText = el("conn-text");
  const showConn = () => {
    const n = live().length;
    const total = DEFAULT_TRADE_RELAYS.length;
    const any = (s: RelayState) => [...states.values()].includes(s);
    const state = n > 0 ? "live" : any("syncing") || any("connecting") || states.size < total ? "connecting" : "failed";
    conn.dataset.state = state;
    connText.textContent = n > 0 ? `live · ${n}/${total} relays` : state === "failed" ? "relays unreachable" : "syncing";
    conn.title = [...states].map(([u, s]) => `${u.replace("wss://", "")}: ${s}`).join("\n");
  };

  let queued = false;
  const schedule = () => { if (!queued) { queued = true; requestAnimationFrame(() => { queued = false; paint(); }); } };

  const reader = createTradeReader(
    { knownLots: () => book.known() },
    {
      onEvent: (e) => { if (book.ingest(e)) schedule(); },
      onStatusesRead: (ids, relay) => { book.markHistoryRead(ids, relay); schedule(); },
      onRelayState: (relay, state) => { states.set(relay, state); showConn(); },
      onRound: (relay) => {
        firstRead.add(relay);
        if (states.get(relay) === "live" && !answered) {
          answered = true;
          // A slow relay must not hold the page: show what we have after this.
          setTimeout(() => { graceOver = true; schedule(); }, SETTLE_MS);
        }
        schedule();
      },
    },
  );
  reader.start();

  const pick = (ev: Event) => {
    const row = (ev.target as HTMLElement).closest<HTMLElement>("[data-lot]");
    if (!row) return;
    if (ev instanceof KeyboardEvent && ev.key !== "Enter" && ev.key !== " ") return;
    ev.preventDefault();
    selected = row.dataset.lot ?? null;
    const list = row.closest("ol")?.id ?? "";
    const side = dockSide(list);
    const box = el("trade-detail");
    box.classList.toggle("dock-left", side === "left");
    box.classList.toggle("dock-right", side === "right");
    renderDetail();
    // Opened like /market's docks: the popup takes focus at the top.
    el("trade-detail-body").scrollTop = 0;
    el("trade-detail-close").focus();
  };
  for (const id of ["lots", "recent"]) {
    el(id).addEventListener("click", pick);
    el(id).addEventListener("keydown", pick);
  }
  const closeDetail = () => {
    if (selected == null) return;
    selected = null;
    renderDetail();
  };
  el("trade-detail-close").addEventListener("click", closeDetail);
  document.addEventListener("keydown", (ev) => { if (ev.key === "Escape") closeDetail(); });

  // Time left and ages tick in place; a full re-derive each minute moves
  // lots across the expiry line without waiting for an event.
  setInterval(() => {
    const t = now();
    for (const n of document.querySelectorAll<HTMLElement>("[data-expires]")) n.textContent = timeLeft(Number(n.dataset.expires) - t);
    for (const n of document.querySelectorAll<HTMLElement>("#recent [data-ts]")) n.textContent = ago(Number(n.dataset.ts), t);
  }, 1000);
  setInterval(paint, 60_000);
}

if (WEB_ANALYTICS_ENABLED) startAnalytics();
boot();
