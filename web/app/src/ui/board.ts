/**
 * The board — buyers, activity, sellers, stats, ticker — rendered from a
 * MarketView through the keyed reconciler. All presentation; no derivation.
 */
import { ago, esc, nf, now, shortHarness, short } from "./format.js";
import { usd } from "./spot.js";
import { posMark, statusDot } from "./indicators.js";
import { reconcileList, type KeyedItem } from "./reconcile.js";
import { HEARTBEAT, KIND_LABELS } from "../model/kinds.js";
import type { MarketView } from "../market/engine.js";
import { parseEvent, type ParsedEvent } from "../model/events.js";
import { WINDOWS } from "../market/participants.js";

const RACER_ACTIVE_SECONDS = 86400;

const el = (id: string): HTMLElement => document.getElementById(id) as HTMLElement;

const nameOf = (names: Map<string, string>, pubkey: string): string | null =>
  names.get(pubkey) || null;

const identity = (names: Map<string, string>, pubkey: string): string => {
  const name = nameOf(names, pubkey);
  return name ? `<span class="person">${esc(name)}</span>` : `<code>${short(pubkey)}</code>`;
};

/**
 * A row's display name. Names come from the FULL event history, not the
 * board's window: kind-0 profiles are published once and rarely re-published,
 * so a week window would strip the name off anyone whose profile predates it
 * and leave a hex stub beside a fully named activity feed.
 */
function label(view: MarketView, r: { pubkey: string; name: string | null }): string {
  const name = view.names.get(r.pubkey) ?? r.name;
  return name ? esc(name) : `<code>${short(r.pubkey)}</code>`;
}

/** The other side of an event: named, or null when the record doesn't say. */
function counterparty(view: MarketView, e: ParsedEvent, want: "buyer" | "seller"): string | null {
  const t = e.offerId ? view.trades.get(e.offerId) : null;
  const pk = want === "buyer"
    ? (e.buyer || t?.buyer)
    : (e.awardedSeller || e.targetSeller || t?.seller);
  return pk && pk !== e.pubkey ? identity(view.names, pk) : null;
}

/** One line of plain English per event kind — the feed reads, not decodes. */
export function feedLine(view: MarketView, e: ParsedEvent): string {
  const who = identity(view.names, e.pubkey);
  switch (e.stage) {
    // The job itself is the most interesting thing on the board. Price after
    // it, not before.
    case "offer": return `${who} · ${e.selfTrade ? '<span class="self" title="The racer operates the runner being paid — real work, but not market demand">self</span> ' : ""}${e.description ? esc(e.description) : "posted a job"}${e.amount != null ? ` · <span class="sats">${usd(e.amount)}</span>` : ""}`;
    case "claim": { const from = counterparty(view, e, "buyer"); return `${who} claimed a job${from ? ` from ${from}` : ""}`; }
    // "awarded the job", not "awarded a claim" — the claim is the mechanism,
    // the job is what the reader understands changed hands.
    case "award": { const to = counterparty(view, e, "seller"); return `${who} awarded the job${to ? ` to ${to}` : ""}`; }
    case "result": { const to = counterparty(view, e, "buyer"); return `${who} delivered${to ? ` to ${to}` : ""}`; }
    // "accepted the delivery", not "authorised payment": it sits directly
    // above "paid" in the feed.
    case "accept": { const from = counterparty(view, e, "seller"); return `${who} accepted the delivery${from ? ` from ${from}` : ""}`; }
    case "receipt": { const to = counterparty(view, e, "seller"); return `${who} paid${to ? ` ${to}` : ""}${e.amount != null ? ` · <span class="sats">${usd(e.amount)}</span>` : ""}`; }
    case "feedback": return `${who} · ${esc(e.reason || "feedback")}`;
    default: return who;
  }
}

export function renderBuyers(view: MarketView): void {
  const t = now();
  const rows = view.buyers;
  el("buyers-meta").textContent = rows.length ? `${rows.length} active` : "";
  if (!rows.length) {
    reconcileList(el("buyers"), [{ key: "-empty", className: "empty", html: "No racers in this period." }]);
    return;
  }
  const items: KeyedItem[] = rows.map((r, i) => {
    const lastAt = view.racerLastSeen.get(r.pubkey) || 0;
    const active = lastAt > 0 && t - lastAt <= RACER_ACTIVE_SECONDS;
    const context = active
      ? `Active in last 24 hours · last activity ${ago(lastAt, t)} ago`
      : (lastAt
          ? `No activity in last 24 hours · last activity ${ago(lastAt, t)} ago`
          : "No activity in last 24 hours");
    return {
      key: r.pubkey,
      className: "row buyers-grid",
      tabIndex: 0,
      data: { open: "buyer", pk: r.pubkey },
      html: `<span class="agent">
          ${posMark(view.buyerClimbs.get(r.pubkey), i + 1)}
          ${statusDot(active, view.activeByBuyer.get(r.pubkey) || [], context)}
          <span class="nm">${label(view, r)}</span>
        </span>
        <span class="num">${nf.format(r.posted)}</span>
        <span class="num ${r.receipted ? "" : "dim"}">${nf.format(r.receipted)}</span>
        <span class="num sats">${usd(r.satsPaid)}</span>`,
    };
  });
  reconcileList(el("buyers"), items);
}

export function renderSellers(view: MarketView): void {
  const rows = view.sellers;
  const online = rows.filter((r) => r.online).length;
  el("sellers-meta").textContent = rows.length ? `${online} online · ${rows.length} seen` : "";
  if (!rows.length) {
    reconcileList(el("sellers"), [{ key: "-empty", className: "empty", html: "No runners in this period." }]);
    return;
  }
  const items: KeyedItem[] = rows.map((r, i) => ({
    key: r.pubkey,
    className: "row sellers-grid",
    tabIndex: 0,
    data: { open: "seller", pk: r.pubkey },
    html: `<span class="agent">
        ${posMark(view.sellerClimbs.get(r.pubkey), i + 1)}
        ${statusDot(r.online, view.activeBySeller.get(r.pubkey) || [])}
        <span class="nm">${label(view, r)}</span>
        ${r.harness ? `<span class="harness" title="${esc(r.harness)}">${esc(shortHarness(r.harness))}</span>` : ""}
      </span>
      <span class="num">${nf.format(r.delivered)}</span>
      <span class="num ${r.askSats == null ? "dim" : ""}" title="Minimum price advertised by this runner">${r.askSats == null ? "—" : usd(r.askSats)}</span>
      <span class="num sats">${usd(r.satsEarned)}</span>`,
  }));
  reconcileList(el("sellers"), items);
}

export function renderFeed(view: MarketView): void {
  const t = now();
  const rows = view.feed.slice(0, 60);
  el("feed-meta").textContent = rows.length
    ? `${nf.format(rows.length)} shown · ${nf.format(view.feed.length)} total`
    : "";
  if (!rows.length) {
    reconcileList(el("feed"), [{ key: "-empty", className: "empty", html: "No activity in this period." }]);
    return;
  }
  const items: KeyedItem[] = rows.map((e) => ({
    key: e.id,
    className: "row",
    tabIndex: 0,
    data: { open: "event", id: e.id },
    html: `<span class="tag" data-s="${e.stage}">${KIND_LABELS[e.kind]}</span>
      <span class="line">${feedLine(view, e)}</span>
      <span class="when" data-ts="${e.created_at}">${ago(e.created_at, t)}</span>`,
  }));
  reconcileList(el("feed"), items);
}

/**
 * Headline figures. Settlement counts published receipts only, so the labels
 * say "receipts" — a trade can settle without publishing one, which makes
 * these a floor and not a total.
 */
export function renderStats(view: MarketView): void {
  const m = view.metrics;
  const cells: [string, string, string][] = [
    ["Jobs posted", nf.format(m.funnel.posted), ""],
    ["Delivered", nf.format(m.funnel.delivered), ""],
    ["Receipts", nf.format(m.receiptsOnRecord), "neon"],
    ["Volume", usd(m.satsInReceipts), "neon"],
    ["Racers", nf.format(m.buyers), ""],
    ["Runners", nf.format(m.sellers), ""],
  ];
  el("statgrid").innerHTML = cells
    .map(([k, v, cls]) => `<div><dt>${k}</dt><dd class="${cls}">${v}</dd></div>`).join("");
  const win = WINDOWS.find((w) => w.key === view.windowKey);
  el("stats-window").textContent = win ? `· ${win.label.toLowerCase()}` : "";
  // An exclusion must be COUNTED, never silent.
  el("stats-note").textContent = m.selfTrades
    ? `${nf.format(m.selfTrades)} self-commissioned trade${m.selfTrades === 1 ? " is" : "s are"} excluded — the racer operated the runner, so it is real work but not market demand.`
    : "";
}

/**
 * The tape is a CONVEYOR, not a repainted ribbon. Item nodes scroll left; the
 * moment one fully exits the left edge it is recycled to the tail (or dropped
 * if it expired), and NEW items are appended at the tail — they simply arrive
 * with the flow. The DOM is append-only between recycles, so the glide can
 * never be interrupted by an update: there is nothing to rebuild.
 */
const TAPE_SPEED_PX_S = 20;
let tapeOffset = 0;
let tapePaused = false;
let tapeFrozen = false;
/** Live item nodes by entry id — the diff target for renderTicker. */
const tapeNodes = new Map<string, HTMLElement>();

export function startTape(): void {
  const node = el("ticker");
  const tape = node.parentElement as HTMLElement;
  tape.addEventListener("mouseenter", () => { tapePaused = true; });
  tape.addEventListener("mouseleave", () => { tapePaused = false; });
  // Click freezes the tape outright until clicked again — hover-pause is
  // momentary, the freeze is a decision.
  tape.addEventListener("click", () => { tapeFrozen = !tapeFrozen; });
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
  let last = 0;
  const frame = (t: number) => {
    if (last && !tapePaused && !tapeFrozen && node.firstElementChild) {
      tapeOffset += ((t - last) / 1000) * TAPE_SPEED_PX_S;
      // Recycle: when the head item has fully scrolled out, move it to the
      // tail and subtract its width — visually seamless, no width remeasure
      // of anything else. Expired items are dropped here, at the exact moment
      // they are invisible anyway.
      let head = node.firstElementChild as HTMLElement | null;
      while (head && tapeOffset >= head.offsetWidth) {
        tapeOffset -= head.offsetWidth;
        if (head.dataset.expired) head.remove();
        else node.appendChild(head); // moves the node; no clone, no rebuild
        head = node.firstElementChild as HTMLElement | null;
      }
      node.style.transform = `translateX(${-tapeOffset}px)`;
    }
    last = t;
    requestAnimationFrame(frame);
  };
  requestAnimationFrame(frame);
}

/** Build one tape item element. */
function tapeItem(id: string, html: string): HTMLElement {
  const span = document.createElement("span");
  span.className = "tape-item";
  span.dataset.tapeId = id;
  span.innerHTML = html;
  return span;
}
/** The tape covers the last 24 hours, every item type alike. */
const TAPE_WINDOW_SECONDS = 24 * 3600;

export function renderTicker(view: MarketView): void {
  const node = el("ticker");
  if (!node) return;
  const t = now();

  // One pass over history: each participant's first market action (their
  // arrival) and their latest one (used to date their overtakes).
  const firstSeen = new Map<string, number>();
  const lastSeen = new Map<string, number>();
  for (const raw of view.allEvents) {
    const e = parseEvent(raw);
    if (!e) continue;
    if (!e.stage && e.kind !== HEARTBEAT) continue;
    const first = firstSeen.get(e.pubkey);
    if (first == null || e.created_at < first) firstSeen.set(e.pubkey, e.created_at);
    if (e.stage) {
      const last = lastSeen.get(e.pubkey);
      if (last == null || e.created_at > last) lastSeen.set(e.pubkey, e.created_at);
    }
  }
  const welcomes = [...firstSeen.entries()]
    .filter(([, at]) => t - at <= TAPE_WINDOW_SECONDS)
    .map(([pk, at]) => ({
      ts: at,
      id: `w:${pk}`,
      html: `<span class="tape-item">welcome <span class="t-new">${esc(nameOf(view.names, pk) || short(pk))}</span> <span class="t-sep">//</span> <span data-ts="${at}">${ago(at, t)}</span></span>`,
    }));

  // Settlements: who paid whom, how much, when.
  const payments = view.feed
    .filter((e) => e.stage === "receipt" && t - e.created_at <= TAPE_WINDOW_SECONDS)
    .slice(0, 12)
    .map((e) => {
      const buyer = nameOf(view.names, e.pubkey) || short(e.pubkey);
      const sellerPk = e.offerId ? view.trades.get(e.offerId)?.seller : null;
      const seller = sellerPk ? (nameOf(view.names, sellerPk) || short(sellerPk)) : null;
      const amt = e.amount != null ? ` <span class="t-amt">${usd(e.amount)}</span>` : "";
      return {
        ts: e.created_at,
        id: e.id,
        html: `<span class="tape-item">${esc(buyer)} paid${seller ? ` ${esc(seller)}` : ""}${amt} <span class="t-sep">//</span> <span data-ts="${e.created_at}">${ago(e.created_at, t)}</span></span>`,
      };
    });

  // Overtakes, dated by the winner's latest move — the action that plausibly
  // made the pass — so they sort into the timeline like everything else.
  const overtakes = view.overtakes
    .map((o) => ({ ...o, at: lastSeen.get(o.winner) ?? 0 }))
    .filter((o) => t - o.at <= TAPE_WINDOW_SECONDS)
    .slice(0, 4)
    .map((o) => ({
      ts: o.at,
      id: `o:${o.winner}:${o.loser}`,
      html: `<span class="tape-item"><span class="t-pass">${esc(nameOf(view.names, o.winner) || short(o.winner))}</span> passes ${esc(nameOf(view.names, o.loser) || short(o.loser))} <span class="t-sep">//</span> <span data-ts="${o.at}">${ago(o.at, t)}</span></span>`,
    }));

  const entries = [...overtakes, ...welcomes, ...payments].sort((a, b) => b.ts - a.ts).slice(0, 14);
  if (!entries.length) return;
  const wanted = new Set(entries.map((x) => x.id));

  // Diff against the conveyor: NEW entries are appended at the tail — they
  // arrive with the flow. Departed entries are only MARKED; the recycler
  // drops them when they next scroll out of view, never mid-screen. The spot
  // rate arriving late upgrades "…" amounts in place, keyed by data-ts-safe
  // innerHTML swap on that node alone.
  for (const entry of entries) {
    const existing = tapeNodes.get(entry.id);
    if (!existing) {
      const item = tapeItem(entry.id, entry.html.replace(/^<span class="tape-item">|<\/span>$/g, ""));
      tapeNodes.set(entry.id, item);
      node.appendChild(item);
    } else {
      delete existing.dataset.expired;
      // Amounts render "…" until the first BTC quote — patch that one case
      // in place without touching the node otherwise.
      if (existing.innerHTML.includes("…") && !entry.html.includes("…")) {
        existing.innerHTML = entry.html.replace(/^<span class="tape-item">|<\/span>$/g, "");
      }
    }
  }
  for (const [id, itemNode] of tapeNodes) {
    if (!wanted.has(id)) {
      itemNode.dataset.expired = "1";
      tapeNodes.delete(id);
    }
  }
  // First real content: lower the strip into view.
  (node.parentElement as HTMLElement).classList.add("live");
}
