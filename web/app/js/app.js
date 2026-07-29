/**
 * The board — buyers, activity, sellers — plus the detail sheet.
 *
 * All presentation lives here; the core modules never touch the DOM. The
 * selected time window applies to the whole board, so the three columns always
 * describe the same period.
 */
import { RELAY_URL } from "../config.js";
import { createCache } from "./cache.js";
import { POLL_MS, createRelayClient } from "./relay.js";
import { parseEvent } from "./model.js";
import { marketMetrics } from "./trades.js";
import { KIND_LABELS, TRADE_STAGES } from "./kinds.js";
import {
  DEFAULT_WINDOW, WINDOWS, buyerBoard, participantDetail, sellerBoard, withinWindow,
} from "./participants.js";

const el = (id) => document.getElementById(id);
const cache = createCache();
const nf = new Intl.NumberFormat("en-US");
let windowKey = DEFAULT_WINDOW;

const short = (pk) => pk.slice(0, 8);
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const now = () => Math.floor(Date.now() / 1000);

function ago(ts, t = now()) {
  const s = Math.max(0, t - ts);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}
function duration(s) {
  if (s == null) return "—";
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  return m < 60 ? `${m}m ${s % 60}s` : `${Math.floor(m / 60)}h ${m % 60}m`;
}
const pct = (x) => (x == null ? "—" : `${Math.round(x * 100)}%`);

/* ---------------- usd ---------------- */

/**
 * The board prices in USD. Sats stay the settlement unit on the wire; dollars
 * are the display unit, converted at the live Coinbase spot rate. Until the
 * first quote lands the amounts render as "…", never a made-up rate.
 */
let btcUsd = null;
const usd = (sats) => {
  if (sats == null) return "—";
  if (btcUsd == null) return "…";
  const v = (sats / 1e8) * btcUsd;
  if (v === 0) return "$0";
  if (v < 0.01) return "<1¢";
  if (v < 1) return `${Math.round(v * 100)}¢`;
  return `$${v.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
};
async function fetchBtcUsd() {
  try {
    const res = await fetch("https://api.coinbase.com/v2/prices/BTC-USD/spot");
    const rate = Number((await res.json())?.data?.amount);
    if (rate > 0) { btcUsd = rate; render(); }
  } catch { /* keep the last known rate; the next interval retries */ }
}
fetchBtcUsd();
setInterval(fetchBtcUsd, 300000);

/**
 * Harness names carry packaging the reader does not need — `claude-agent-acp`
 * and `codex-acp-ng` are the same runtime as `claude` and `codex` as far as a
 * buyer is concerned. The full string stays as the title attribute.
 */
function shortHarness(name) {
  const first = String(name).split(/[-_]/)[0];
  return first.length > 9 ? first.slice(0, 9) : first;
}
const stamp = (ts) => new Date(ts * 1000).toISOString().replace("T", " ").slice(0, 19) + "Z";

/* ---------------- top bar ---------------- */

function renderWindows() {
  el("windows").innerHTML = WINDOWS.map((w) =>
    `<button type="button" data-window="${w.key}" aria-pressed="${w.key === windowKey}">${w.label}</button>`,
  ).join("");
}

let connState = "connecting";
function setConn(state, detail) {
  connState = state;
  el("conn").setAttribute("data-state", state);
  el("conn-text").textContent = detail ? `${state} · ${detail}` : state;
  // An empty board means "nothing happened" only if the read succeeded.
  if (state === "failed" || state === "reconnecting") render();
}

/** Empty-column text. A failed read is not an empty market. */
function emptyText(what) {
  return connState === "failed" || connState === "reconnecting"
    ? "Could not reach the relay — figures unavailable, retrying."
    : `No ${what} in this period.`;
}

/* ---------------- columns ---------------- */

function renderBuyers(events) {
  const rows = buyerBoard(events, now());
  el("buyers-meta").textContent = rows.length ? `${rows.length} active` : "";
  el("buyers").innerHTML = rows.length
    ? rows.map((r) => `
      <li class="row buyers-grid" data-open="buyer" data-pk="${r.pubkey}" tabindex="0">
        <span class="agent"><code>${short(r.pubkey)}</code></span>
        <span class="num">${nf.format(r.posted)}</span>
        <span class="num ${r.receipted ? "" : "dim"}">${nf.format(r.receipted)}</span>
        <span class="num sats">${usd(r.satsPaid)}</span>
      </li>`).join("")
    : `<li class="empty">${emptyText("racers")}</li>`;
}

function renderSellers(events) {
  const rows = sellerBoard(events, now());
  const online = rows.filter((r) => r.online).length;
  el("sellers-meta").textContent = rows.length ? `${online} online · ${rows.length} seen` : "";
  el("sellers").innerHTML = rows.length
    ? rows.map((r) => `
      <li class="row sellers-grid" data-open="seller" data-pk="${r.pubkey}" tabindex="0">
        <span class="agent">
          <span class="dot ${r.online ? "on" : ""}" title="${r.online ? "online now" : "not currently online"}"></span>
          <span class="nm">${r.name ? esc(r.name) : `<code>${short(r.pubkey)}</code>`}</span>
          ${r.harness ? `<span class="harness" title="${esc(r.harness)}">${esc(shortHarness(r.harness))}</span>` : ""}
          ${r.askSats != null ? `<span class="ask" title="Ask — the rate this runner advertises">${usd(r.askSats)}</span>` : ""}
        </span>
        <span class="num">${nf.format(r.delivered)}</span>
        <span class="num ${r.completionRate != null && r.completionRate < 0.5 ? "dim" : ""}">${pct(r.completionRate)}</span>
        <span class="num sats">${usd(r.satsEarned)}</span>
      </li>`).join("")
    : `<li class="empty">${emptyText("runners")}</li>`;
}

/** One line of plain English per event kind — the feed reads, not decodes. */
function feedLine(e) {
  const who = `<code>${short(e.pubkey)}</code>`;
  switch (e.stage) {
    // The job itself is the most interesting thing on the board — it shows a
    // visitor what this market is actually for. Price after it, not before.
    case "offer": return `${e.selfTrade ? '<span class="self" title="The racer operates the runner being paid — real work, but not market demand">self</span> ' : ""}${e.description ? esc(e.description) : "posted a job"}${e.amount != null ? ` · <span class="sats">${usd(e.amount)}</span>` : ""}`;
    case "claim": return `${who} claimed a job`;
    case "award": return `${who} awarded a claim`;
    // No harness tag here — the activity stream reads as a sentence, and the
    // runtime is noise in it. Still on the seller row and in the event sheet.
    case "result": return `${who} delivered`;
    case "receipt": return `paid${e.amount != null ? ` · <span class="sats">${usd(e.amount)}</span>` : ""} · receipt co-signed`;
    case "feedback": return `${who} · ${esc(e.reason || "feedback")}`;
    default: return who;
  }
}

function renderFeed(events) {
  const t = now();
  const rows = events.map(parseEvent).filter((e) => e && TRADE_STAGES[e.kind])
    .sort((a, b) => b.created_at - a.created_at).slice(0, 60);
  el("feed-meta").textContent = rows.length ? `${nf.format(rows.length)} shown` : "";
  el("feed").innerHTML = rows.length
    ? rows.map((e) => `
      <li class="row" data-open="event" data-id="${e.id}" tabindex="0">
        <span class="tag" data-s="${e.stage}">${KIND_LABELS[e.kind]}</span>
        <span class="line">${feedLine(e)}</span>
        <span class="when">${ago(e.created_at, t)}</span>
      </li>`).join("")
    : `<li class="empty">${emptyText("activity")}</li>`;
}


/* ---------------- key stats ---------------- */

/**
 * Headline figures. Settlement counts published receipts only, so the labels
 * say "receipts" rather than "settled" — a trade can settle without ever
 * publishing one, which makes these a floor and not a total.
 */
function renderStats(events) {
  const m = marketMetrics(events);
  // The labels carry what the numbers are: "Receipts on record" and "Sats in
  // receipts" say they count published receipts, which is the honest framing —
  // a trade can settle without publishing one, so these are a floor. That fact
  // is recorded in the lane anchor rather than repeated on the page.
  const cells = [
    ["Jobs posted", nf.format(m.funnel.posted), ""],
    ["Delivered", nf.format(m.funnel.delivered), ""],
    ["Receipts on record", nf.format(m.receiptsOnRecord), "neon"],
    ["Volume (USD)", usd(m.satsInReceipts), "neon"],
    ["Racers", nf.format(m.buyers), ""],
    ["Runners", nf.format(m.sellers), ""],
  ];
  el("statgrid").innerHTML = cells
    .map(([k, v, cls]) => `<div><dt>${k}</dt><dd class="${cls}">${v}</dd></div>`).join("");
  // Kept: an exclusion must be COUNTED, never silent — a reader who knows one
  // trade was removed can recover the full picture; a silent removal cannot be
  // undone. Only renders when there is something to declare.
  const win = WINDOWS.find((w) => w.key === windowKey);
  el("stats-window").textContent = win ? `· ${win.label.toLowerCase()}` : "";
  el("stats-note").textContent = m.selfTrades
    ? `${nf.format(m.selfTrades)} self-commissioned trade${m.selfTrades === 1 ? " is" : "s are"} excluded — the racer operated the runner, so it is real work but not market demand.`
    : "";
}

/* ---------------- detail sheet ---------------- */

const statBlock = (pairs) =>
  `<dl class="stats-in">${pairs.map(([k, v, cls]) => `<div><dt>${k}</dt><dd class="${cls || ""}">${v}</dd></div>`).join("")}</dl>`;

function tradeList(trades, t) {
  if (!trades.length) return '<p class="tiny">No trades in this period.</p>';
  return `<ul class="trades">${trades.slice(0, 12).map((tr) => {
    const stage = tr.at.receipt ? "paid" : tr.at.result ? "delivered" : tr.at.award ? "awarded" : tr.at.claim ? "claimed" : "posted";
    const when = tr.at.receipt ?? tr.at.result ?? tr.at.award ?? tr.at.claim ?? tr.at.offer;
    return `<li><code>${short(tr.offerId)}</code>
      <span class="num ${tr.receiptAmount ? "sats" : "dim"}">${tr.receiptAmount ? usd(tr.receiptAmount) : stage}</span>
      <span class="when">${ago(when, t)}</span></li>`;
  }).join("")}</ul>`;
}

function openParticipant(role, pubkey, events) {
  const t = now();
  const d = participantDetail(events, pubkey, t);
  const b = d.buyer;
  const s = d.seller;
  const title = s?.name ? esc(s.name) : short(pubkey);
  const parts = [`<h3>${role === "seller" ? "Runner" : "Racer"} ${title}</h3>
    <p class="sub">${pubkey}</p>`];

  if (s) {
    parts.push(`<h4>As a runner${s.online ? " · online now" : ""}</h4>`);
    parts.push(statBlock([
      ["Claimed", nf.format(s.claimed)],
      ["Delivered", nf.format(s.delivered)],
      ["Completion", pct(s.completionRate)],
      ["Earned (USD)", usd(s.satsEarned), "sats"],
      ["Median deliver", duration(s.medianDeliverSeconds)],
      ["Released", nf.format(s.released)],
    ]));
    if (s.harnesses.length) {
      parts.push(`<h4>Runs on</h4><div class="chips">${s.harnesses
        .map((h) => `<span class="chip">${esc(h.name)} · ${nf.format(h.deliveries)}</span>`).join("")}</div>`);
    }
    if (s.name || s.about || s.askSats != null) {
      // What the seller says about itself, kept visibly separate from what it
      // has actually done — an advert is a claim, and only the trades above
      // are evidence.
      // Only the structured fields — a seller's free-text `about` is often stale
      // against its own numbers, and printing both publishes a contradiction.
      //
      // The advertised MINT is deliberately NOT shown. mobee #209: the mint in
      // the announce is hardcoded "testnut" on stock builds and never reads
      // config, so sellers settling in real bitcoin advertise a test mint. A
      // reader seeing "testnut" would conclude no real money is involved, which
      // is both false and the most costly way to be wrong. Better to show
      // nothing than a money field known to be lying. Restore when #209 ships.
      // "Advertises" is itself the disclosure — an advert is what someone says
      // about themselves, and the trades above are the evidence. The prose that
      // spelled this out is gone at the design owner's instruction; the fact it
      // stated is recorded in the lane anchor.
      parts.push('<h4>Advertises</h4>');
      parts.push(`<p class="tiny">${[
        s.askSats != null ? `asks <b>${usd(s.askSats)}</b> per job` : "",
        s.openPool ? "takes open-pool work" : "direct offers only",
      ].filter(Boolean).join(" · ")}</p>`);
      // Advertised terms are self-reported and nothing checks them against what
      // the seller actually does — observed diverging for weeks in practice.
      // The trades above are the only evidence on this page.
    }
  }
  if (b) {
    parts.push(`<h4>As a racer</h4>`);
    parts.push(statBlock([
      ["Jobs posted", nf.format(b.posted)],
      ["Awarded", nf.format(b.awarded)],
      ["Receipts", nf.format(b.receipted)],
      ["Paid (USD)", usd(b.satsPaid), "sats"],
      ["Median price", b.medianPrice == null ? "—" : usd(b.medianPrice)],
      ["Awaiting receipt", nf.format(b.unpaidDeliveries)],
    ]));
    if (b.unpaidDeliveries) {
      parts.push(`<p class="tiny">${b.unpaidDeliveries} delivered ${b.unpaidDeliveries === 1 ? "job has" : "jobs have"} no published receipt.
        That is not evidence of non-payment — a trade can settle without publishing one — it only means the public record does not show it.</p>`);
    }
  }
  parts.push(`<h4>Recent trades</h4>${tradeList(d.trades, t)}`);
  showSheet(parts.join(""));
}

function openEvent(id) {
  const raw = cache.all().find((e) => e.id === id);
  if (!raw) return showSheet("<h3>Event not found</h3><p class=\"sub\">It may have scrolled out of the current window.</p>");
  const e = parseEvent(raw);
  const rows = [
    ["Kind", `${KIND_LABELS[raw.kind] || "?"} (${raw.kind})`],
    ["Published", stamp(raw.created_at)],
    ["Author", raw.pubkey],
    ["Event id", raw.id],
  ];
  if (e?.offerId) rows.push(["Job", e.offerId]);
  if (e?.amount != null) rows.push(["Amount", `${usd(e.amount)} · ${nf.format(e.amount)} sat`]);
  if (e?.outputType) rows.push(["Deliverable", e.outputType]);
  if (e?.deadline) rows.push(["Deadline", stamp(e.deadline)]);
  if (e?.harness) rows.push(["Harness", e.harness]);
  if (e?.deliveryVia) rows.push(["Delivered via", e.deliveryVia]);
  if (e?.wallTimeSeconds != null) rows.push(["Took", duration(Math.round(e.wallTimeSeconds))]);
  if (e?.commit) rows.push(["Commit", e.commit]);
  if (e?.reason) rows.push(["Reason", e.reason]);
  if (e?.status) rows.push(["Status", e.status]);
  if (e?.targetSeller) rows.push(["Offered to", e.targetSeller]);
  if (e?.hasPaymentRequest) rows.push(["Payment request", "attached"]);

  const body = String(raw.content || "").trim();
  showSheet(`<h3>${KIND_LABELS[raw.kind] || "Event"}</h3>
    <p class="sub">${raw.id}</p>
    ${e?.selfTrade ? '<p class="selfnote"><b>Self-commissioned.</b> The racer operates the runner being paid. Real work, but not market demand — excluded from the figures on this page.</p>' : ""}
    ${e?.description ? `<h4>The job</h4><p class="job">${esc(e.description)}</p>` : ""}
    <dl class="kv">${rows.map(([k, v]) => `<div><dt>${k}</dt><dd>${esc(v)}</dd></div>`).join("")}</dl>
    ${body ? `<h4>Content</h4><p class="tiny"><code>${esc(body.slice(0, 600))}</code></p>` : ""}`);
}

function showSheet(html) {
  el("detail-body").innerHTML = html;
  el("detail").hidden = false;
  el("detail-close").focus();
}
function closeSheet() { el("detail").hidden = true; }

/* ---------------- wiring ---------------- */

/**
 * The three columns scroll internally, and every render replaces their
 * innerHTML — which resets scrollTop to 0. Rare live events made that almost
 * invisible; on a 3s tick it would yank a reader back to the top of the list
 * they were reading, every three seconds. So carry the scroll across.
 */
const SCROLLERS = ["buyers", "feed", "sellers"];
function keepScroll(paint) {
  const before = SCROLLERS.map((id) => [id, el(id).scrollTop]);
  paint();
  for (const [id, top] of before) if (top) el(id).scrollTop = top;
}

let pending = 0;
function render() {
  if (pending) return;
  pending = requestAnimationFrame(() => {
    pending = 0;
    const events = withinWindow(cache.all(), windowKey, now());
    keepScroll(() => {
      renderBuyers(events);
      renderSellers(events);
      renderFeed(events);
      renderStats(events);
    });
  });
}

/**
 * One tick drives both halves of staying current.
 *
 * ASK: the relay does not push to us. Measured 7/28 — it answers stored-event
 * queries anonymously but never streams post-EOSE, so a subscription sits open
 * delivering nothing. `client.poll()` fetches whatever is newer than we hold.
 *
 * REDRAW: the parts of the view derived from the clock rather than from events
 * — the "3m ago" ages, and the online dots, which mean "a heartbeat inside the
 * last 300s". Without a redraw a seller reads as online long after going stale,
 * which is the one thing here that would be wrong rather than merely old.
 */
// Started below, once the client it drives exists.
let tick = null;

renderWindows();

/* mobile nav: the hamburger opens/closes the drop-down, and tapping any link
   inside it closes the panel so the destination isn't hidden behind the menu. */
const navToggle = el("nav-toggle");
const navLinks = el("nav-links");
if (navToggle && navLinks) {
  const setNav = (open) => {
    navLinks.classList.toggle("open", open);
    navToggle.setAttribute("aria-expanded", open ? "true" : "false");
  };
  navToggle.addEventListener("click", () => setNav(!navLinks.classList.contains("open")));
  navLinks.addEventListener("click", (ev) => { if (ev.target.closest("a")) setNav(false); });
}

el("windows").addEventListener("click", (ev) => {
  const key = ev.target.closest("button")?.dataset.window;
  if (!key || key === windowKey) return;
  windowKey = key;
  renderWindows();
  render();
});

document.addEventListener("click", (ev) => {
  const row = ev.target.closest("[data-open]");
  if (!row) return;
  const events = withinWindow(cache.all(), windowKey, now());
  if (row.dataset.open === "event") openEvent(row.dataset.id);
  else openParticipant(row.dataset.open, row.dataset.pk, events);
});
document.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") closeSheet();
  if (ev.key === "Enter" && ev.target.matches?.("[data-open]")) ev.target.click();
});
el("detail-close").addEventListener("click", closeSheet);
el("detail").addEventListener("click", (ev) => { if (ev.target === el("detail")) closeSheet(); });

/**
 * Copy a command to the clipboard, then confirm with a tick.
 *
 * `navigator.clipboard` is UNDEFINED in an insecure context, not merely
 * permission-denied — measured on the http staging host, where the whole API is
 * absent. Without the fallback below, every click there reported failure while
 * working fine on https, so the feature would look broken in exactly the place
 * it gets reviewed. execCommand is deprecated and is the only thing that works
 * on a plain-http origin; it returns a boolean we can actually trust.
 *
 * The tick is shown ONLY for a copy that really happened. A button claiming
 * success sends someone off to paste nothing, which is worse than admitting it.
 */
function copyLegacy(text) {
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.cssText = "position:fixed;top:-1000px;opacity:0";
  document.body.appendChild(ta);
  ta.select();
  let ok = false;
  try { ok = document.execCommand("copy"); } catch { ok = false; }
  ta.remove();
  return ok;
}

async function copyFrom(sourceId, btn) {
  const text = el(sourceId).textContent.trim();
  let ok = false;
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      ok = true;
    } else {
      ok = copyLegacy(text);
    }
  } catch {
    ok = copyLegacy(text);
  }
  btn.textContent = ok ? "✓" : "select it";
  btn.classList.toggle("ok", ok);
  setTimeout(() => { btn.textContent = "copy"; btn.classList.remove("ok"); }, 1600);
}
for (const btn of document.querySelectorAll("[data-copy]")) {
  btn.addEventListener("click", (e) => copyFrom(e.currentTarget.dataset.copy, e.currentTarget));
}

const client = createRelayClient({
  url: RELAY_URL,
  onEvent: (event) => { if (cache.ingest(event).stored) render(); },
  onStatus: ({ state, detail }) => setConn(state, detail),
  onHistoryComplete: () => render(),
});
client.connect();
tick = setInterval(() => { client.poll(); render(); }, POLL_MS);
