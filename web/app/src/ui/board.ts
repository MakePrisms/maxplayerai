/**
 * The board — buyers, activity, sellers, stats — rendered from a
 * MarketView through the keyed reconciler. All presentation; no derivation.
 */
import { ago, esc, nf, now, shortHarness, short } from "./format.js";
import { usd } from "./spot.js";
import { posMark, statusDot } from "./indicators.js";
import { reconcileList, type KeyedItem } from "./reconcile.js";
import { KIND_LABELS } from "../model/kinds.js";
import type { MarketView } from "../market/engine.js";
import type { ParsedEvent } from "../model/events.js";
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

/**
 * Named when the buyer-signed records disagree on the winner. The "paid" and
 * "accepted the delivery from" lines must not name a runner the trade could not
 * resolve — a wrong name beside "paid" is exactly the attribution this join
 * guards against.
 */
const CONFLICTED_SELLER =
  '<span class="unknown" title="The buyer-signed award, accept and receipt for this job name different runners — the winner cannot be determined from the public record.">an undetermined runner</span>';

/** The other side of an event: named, or null when the record doesn't say. */
function counterparty(view: MarketView, e: ParsedEvent, want: "buyer" | "seller"): string | null {
  const t = e.offerId ? view.trades.get(e.offerId) : null;
  if (want === "buyer") {
    const pk = e.buyer || t?.buyer;
    return pk && pk !== e.pubkey ? identity(view.names, pk) : null;
  }
  // An award states its own winner directly; accept/receipt lines defer to the
  // trade's resolved winner. A conflicted trade names no one — it says so.
  const pk = e.awardedSeller || e.targetSeller || t?.seller;
  if (pk && pk !== e.pubkey) return identity(view.names, pk);
  return t?.sellerConflict ? CONFLICTED_SELLER : null;
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
