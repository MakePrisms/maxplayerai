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
import { mintLabel, rate, seller, timeLeft } from "./trade/format.js";
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

function wireWindows(onChange: () => void): void {
  const box = el("windows");
  box.innerHTML = WINDOWS.map((w) => `<button type="button" data-w="${w.key}" aria-pressed="${w.key === windowKey}">${w.label}</button>`).join("");
  box.addEventListener("click", (ev) => {
    const b = (ev.target as HTMLElement).closest<HTMLElement>("button[data-w]");
    if (!b) return;
    windowKey = b.dataset.w ?? windowKey;
    for (const x of box.querySelectorAll("button")) x.setAttribute("aria-pressed", String(x === b));
    onChange();
  });
}

function doneItem(r: LotRow, t: number): KeyedItem {
  const s = seller(r.lot.maker);
  return {
    key: r.id,
    className: "row done-grid",
    tabIndex: 0,
    data: { lot: r.id },
    html:
      `<span class="when" data-ts="${r.updated_at}" title="${esc(stamp(r.updated_at))}">${ago(r.updated_at, t)}</span>` +
      `<span class="leg">${legHtml(r.lot.give.net, r.lot.give.mint_url)}</span>` +
      `<span class="leg">${legHtml(r.lot.want.net, r.lot.want.mint_url)}</span>` +
      `<span class="num">${esc(rate(r.price))}</span>` +
      `<span class="person col-seller" title="${esc(s.npub)}">${esc(s.short)}</span>` +
      `<span class="num">${esc(duration(r.updated_at - r.lot.created_at))}</span>`,
  };
}

function renderStats(v: BookView, t: number, answered: boolean): void {
  const w = WINDOWS.find((x) => x.key === windowKey) ?? WINDOWS[0]!;
  const c = completedStats(v, t, w.seconds);
  const cells: [string, string, boolean][] = [
    ["Trades", nf.format(c.trades), true],
    ["Sats sold", nf.format(c.giveSats), true],
    ["Sats received", nf.format(c.wantSats), false],
    ["Sellers", nf.format(c.sellers), false],
    ["Mint pairs", nf.format(c.pairs), false],
    ["Median fill", c.medianFill == null ? "—" : duration(c.medianFill), false],
  ];
  el("statgrid").innerHTML = cells.map(([k, val, neon]) => `<div><dt>${k}</dt><dd${neon ? ' class="neon"' : ""}>${esc(val)}</dd></div>`).join("");
  el("done-meta").textContent = `${c.trades} · ${w.label.toLowerCase()}`;
  if (c.rows.length) reconcileList(el("done"), c.rows.slice(0, 500).map((r) => doneItem(r, t)));
  else if (answered) reconcileList(el("done"), [{ key: "-empty", className: "empty", html: "No completed trades in this period." }]);
  const notes: string[] = [`${c.cancelled} cancelled and ${c.expired} expired in this period`];
  if (v.quarantined.length) notes.push(`${v.quarantined.length} listing${v.quarantined.length === 1 ? "" : "s"} hidden: broken or forked status history`);
  if (v.rejected) notes.push(`${v.rejected} invalid listing${v.rejected === 1 ? "" : "s"} ignored`);
  if (v.pending) notes.push(`${v.pending} still loading`);
  el("stats-note").textContent =
    "A trade counts as completed when its seller signs the lot sold; settlement itself is private. " +
    "Every listing and status is signature-checked in your browser with the trade CLI's rules. " + notes.join(" · ") + ".";
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

function render(v: BookView, answered: boolean): void {
  lastView = v;
  const t = now();
  // An empty book is a conclusion; the skeletons hold until a relay has answered.
  if (!answered && v.open.length + v.closed.length === 0) return;
  renderStats(v, t, answered);
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
  const live = () => [...states].filter(([, s]) => s === "live").map(([u]) => u);
  const paint = () => render(book.view(now(), live()), answered);
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
      onRound: (relay) => { if (states.get(relay) === "live") answered = true; schedule(); },
    },
  );
  reader.start();

  const pick = (ev: Event) => {
    const row = (ev.target as HTMLElement).closest<HTMLElement>("[data-lot]");
    if (!row) return;
    if (ev instanceof KeyboardEvent && ev.key !== "Enter" && ev.key !== " ") return;
    ev.preventDefault();
    selected = row.dataset.lot ?? null;
    renderDetail();
    el("trade-detail").scrollIntoView({ block: "nearest", behavior: "smooth" });
  };
  for (const id of ["lots", "recent", "done"]) {
    el(id).addEventListener("click", pick);
    el(id).addEventListener("keydown", pick);
  }
  el("trade-detail-close").addEventListener("click", () => { selected = null; renderDetail(); });

  // Time left and ages tick in place; a full re-derive each minute moves
  // lots across the expiry line without waiting for an event.
  setInterval(() => {
    const t = now();
    for (const n of document.querySelectorAll<HTMLElement>("[data-expires]")) n.textContent = timeLeft(Number(n.dataset.expires) - t);
    for (const n of document.querySelectorAll<HTMLElement>("#recent [data-ts], #done [data-ts]")) n.textContent = ago(Number(n.dataset.ts), t);
  }, 1000);
  setInterval(paint, 60_000);
}

if (WEB_ANALYTICS_ENABLED) startAnalytics();
boot();
