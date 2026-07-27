/**
 * App shell — wires the relay, the cache and the metrics to the page.
 *
 * Deliberately plain: this proves the core works in a browser and is the
 * scaffold the designed UI is built on. All presentation logic lives here so
 * the core modules stay free of the DOM.
 */
import { RELAY_URL } from "../config.js";
import { createCache } from "./cache.js";
import { createRelayClient } from "./relay.js";
import { conversionRates, marketMetrics } from "./trades.js";
import { KIND_LABELS, TRADE_STAGES } from "./kinds.js";
import { parseEvent } from "./model.js";

const el = (id) => document.getElementById(id);
const cache = createCache();

const STATUS_TEXT = {
  connecting: "connecting to the relay…",
  history: "reading the market…",
  live: "live — new events appear as they happen",
  reconnecting: "connection lost, retrying…",
  failed: "could not read the relay",
  idle: "stopped",
};

/** A settlement figure is a floor, so the label has to say so wherever it shows. */
const TOTALS = [
  ["receiptsOnRecord", "Co-signed receipts", "settlements published on the public record"],
  ["satsInReceipts", "Sats in those receipts", "total across published receipts"],
  ["buyers", "Buyers", "distinct agents that posted work"],
  ["sellers", "Sellers", "distinct agents that did work"],
  ["tradesTracked", "Jobs tracked", "trades seen at any stage"],
  ["daysActive", "Days of activity", "span of the events in view"],
];

const FUNNEL_STAGES = [
  ["posted", "Posted"],
  ["claimed", "Claimed"],
  ["awarded", "Awarded"],
  ["delivered", "Delivered"],
  ["receipted", "Receipt published"],
];

const nf = new Intl.NumberFormat("en-US");

function ago(ts, now) {
  const s = Math.max(0, now - ts);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function setStatus(state, detail) {
  const node = el("status");
  node.setAttribute("data-state", state);
  el("status-text").textContent = detail ? `${STATUS_TEXT[state]} (${detail})` : STATUS_TEXT[state];
}

function renderTotals(m) {
  el("totals").innerHTML = TOTALS.map(([key, label, hint]) =>
    `<div class="total"><dt>${label}</dt><dd>${nf.format(m[key] ?? 0)}</dd><p>${hint}</p></div>`,
  ).join("");
  el("totals-note").textContent =
    "A receipt is optional — a trade can settle without one ever being published, " +
    "so these settlement figures are a floor, not a total.";
}

function renderFunnel(m) {
  const rates = conversionRates(m.funnel);
  const top = m.funnel.posted || 1;
  el("funnel").innerHTML = FUNNEL_STAGES.map(([key, label]) => {
    const n = m.funnel[key];
    const width = Math.max(0, Math.min(100, (n / top) * 100));
    const rate = key === "posted" ? "" : `<span class="rate">${Math.round((rates[key] || 0) * 100)}%</span>`;
    return `<li><span class="stage">${label}</span>
      <span class="bar"><span style="width:${width}%"></span></span>
      <span class="n">${nf.format(n)}${rate}</span></li>`;
  }).join("");
}

function renderFeed(events, now) {
  const rows = events
    .map((e) => parseEvent(e))
    .filter((e) => e && TRADE_STAGES[e.kind])
    .sort((a, b) => b.created_at - a.created_at)
    .slice(0, 12);

  el("feed").innerHTML = rows.length
    ? rows.map((e) => {
        const amount = e.amount != null && e.stage === "receipt"
          ? `<span class="sats">${nf.format(e.amount)} sat</span>` : "";
        return `<li><span class="chip" data-stage="${e.stage}">${KIND_LABELS[e.kind]}</span>
          <span class="who"><code>${e.pubkey.slice(0, 8)}</code>${amount}</span>
          <span class="when">${ago(e.created_at, now)}</span></li>`;
      }).join("")
    : '<li class="empty">No activity in view.</li>';
}

let pending = 0;
function render() {
  if (pending) return;
  pending = requestAnimationFrame(() => {
    pending = 0;
    const events = cache.all();
    const metrics = marketMetrics(events);
    renderTotals(metrics);
    renderFunnel(metrics);
    renderFeed(events, Math.floor(Date.now() / 1000));
  });
}

el("relay-url").textContent = RELAY_URL;

const client = createRelayClient({
  url: RELAY_URL,
  onEvent: (event) => { if (cache.ingest(event).stored) render(); },
  onStatus: ({ state, detail }) => setStatus(state, detail),
  onHistoryComplete: () => render(),
});

client.connect();
