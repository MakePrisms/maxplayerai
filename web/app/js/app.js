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
import { KIND_LABELS, PROFILE, TRADE_STAGES } from "./kinds.js";
import {
  DEFAULT_WINDOW, WINDOWS, buyerBoard, participantDetail, participantNames,
  participantProfiles, racerLastActivity, relatedActivity, sellerBoard, withinWindow,
} from "./participants.js";

const el = (id) => document.getElementById(id);
const cache = createCache();
const nf = new Intl.NumberFormat("en-US");
let windowKey = DEFAULT_WINDOW;

// Filter only the job lifecycle, in the order a successful trade occurs.
// Participant-level profile/heartbeat events and feedback remain visible under
// All, but do not compete with the primary workflow as standalone filters.
const ACTIVITY_FILTER_ORDER = Object.freeze([
  "offer", "claim", "award", "result", "accept", "receipt",
]);
const RACER_ACTIVE_SECONDS = 86400;
const RACE_LIGHT_CYCLE_SECONDS = 1;

const short = (pk) => pk.slice(0, 8);
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const now = () => Math.floor(Date.now() / 1000);
const nameOf = (names, pubkey) => names.get(pubkey) || null;
const identity = (names, pubkey) => {
  const name = nameOf(names, pubkey);
  return name ? `<span class="person">${esc(name)}</span>` : `<code>${short(pubkey)}</code>`;
};

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

/**
 * Pit-lane status: a steady lamp for availability, race light for work.
 *
 * The three-second market redraw replaces row markup. A normal CSS animation
 * would therefore restart at zero every time. Anchor its negative delay to the
 * award clock instead: each replacement enters at the phase the uninterrupted
 * animation would already have reached.
 */
function statusDot(on, jobs = [], context = null) {
  const count = jobs.length;
  const idle = context || (on ? "Available now" : "Not currently online");
  const label = count
    ? `Working · ${count} job${count === 1 ? "" : "s"} · ${idle}`
    : idle;
  let phase = "";
  if (count) {
    const startedAt = Math.min(...jobs.map((job) => job.startedAt).filter(Number.isFinite));
    const elapsed = Math.max(0, Date.now() / 1000 - startedAt);
    phase = ` style="--race-light-delay:-${(elapsed % RACE_LIGHT_CYCLE_SECONDS).toFixed(3)}s"`;
  }
  return `<span class="dot ${on ? "on" : ""} ${count ? "working" : ""}"${phase} role="img" aria-label="${esc(label)}" title="${esc(label)}"></span>`;
}

/* ---------------- columns ---------------- */

function renderBuyers(events, names, allEvents = events) {
  const t = now();
  const rows = buyerBoard(events, t, allEvents);
  const lastActivity = racerLastActivity(allEvents);
  el("buyers-meta").textContent = rows.length ? `${rows.length} active` : "";
  el("buyers").innerHTML = rows.length
    ? rows.map((r) => {
      const lastAt = lastActivity.get(r.pubkey) || 0;
      const active = lastAt > 0 && t - lastAt <= RACER_ACTIVE_SECONDS;
      const context = active
        ? `Active in last 24 hours · last activity ${ago(lastAt, t)} ago`
        : (lastAt
            ? `No activity in last 24 hours · last activity ${ago(lastAt, t)} ago`
            : "No activity in last 24 hours");
      return `
      <li class="row buyers-grid" data-open="buyer" data-pk="${r.pubkey}" tabindex="0">
        <span class="agent">
          ${statusDot(active, r.inProgressJobs, context)}
          <span class="nm">${nameOf(names, r.pubkey) ? esc(nameOf(names, r.pubkey)) : `<code>${short(r.pubkey)}</code>`}</span>
        </span>
        <span class="num">${nf.format(r.posted)}</span>
        <span class="num ${r.receipted ? "" : "dim"}">${nf.format(r.receipted)}</span>
        <span class="num sats">${usd(r.satsPaid)}</span>
      </li>`;
    }).join("")
    : `<li class="empty">${emptyText("racers")}</li>`;
}

function renderSellers(events, names, allEvents = events) {
  const rows = sellerBoard(events, now(), allEvents);
  const online = rows.filter((r) => r.online).length;
  el("sellers-meta").textContent = rows.length ? `${online} online · ${rows.length} seen` : "";
  el("sellers").innerHTML = rows.length
    ? rows.map((r) => `
      <li class="row sellers-grid" data-open="seller" data-pk="${r.pubkey}" tabindex="0">
        <span class="agent">
          ${statusDot(r.online, r.inProgressJobs)}
          <span class="nm">${nameOf(names, r.pubkey) ? esc(nameOf(names, r.pubkey)) : `<code>${short(r.pubkey)}</code>`}</span>
          ${r.harness ? `<span class="harness" title="${esc(r.harness)}">${esc(shortHarness(r.harness))}</span>` : ""}
        </span>
        <span class="num">${nf.format(r.delivered)}</span>
        <span class="num ${r.askSats == null ? "dim" : ""}" title="Minimum price advertised by this runner">${r.askSats == null ? "—" : usd(r.askSats)}</span>
        <span class="num sats">${usd(r.satsEarned)}</span>
      </li>`).join("")
    : `<li class="empty">${emptyText("runners")}</li>`;
}

/** One line of plain English per event kind — the feed reads, not decodes. */
function feedLine(e, names) {
  const who = identity(names, e.pubkey);
  switch (e.stage) {
    // The job itself is the most interesting thing on the board — it shows a
    // visitor what this market is actually for. Price after it, not before.
    case "offer": return `${who} · ${e.selfTrade ? '<span class="self" title="The racer operates the runner being paid — real work, but not market demand">self</span> ' : ""}${e.description ? esc(e.description) : "posted a job"}${e.amount != null ? ` · <span class="sats">${usd(e.amount)}</span>` : ""}`;
    case "claim": return `${who} claimed a job`;
    case "award": return `${who} awarded a claim`;
    // No harness tag here — the activity stream reads as a sentence, and the
    // runtime is noise in it. Still on the seller row and in the event sheet.
    case "result": return `${who} delivered`;
    // "accepted the delivery", not "authorised payment": this sits directly
    // above "paid" in the feed, where a sentence about authorising payment
    // reads as the settlement itself rather than the step before it.
    case "accept": return `${who} accepted the delivery`;
    case "receipt": return `${who} paid${e.amount != null ? ` · <span class="sats">${usd(e.amount)}</span>` : ""}`;
    case "feedback": return `${who} · ${esc(e.reason || "feedback")}`;
    default: return who;
  }
}

function renderFeed(events, names) {
  const t = now();
  const activity = events.map(parseEvent).filter((e) => e && TRADE_STAGES[e.kind])
    .sort((a, b) => b.created_at - a.created_at);
  const rows = activity.slice(0, 60);
  el("feed-meta").textContent = rows.length
    ? `${nf.format(rows.length)} shown · ${nf.format(activity.length)} total`
    : "";
  el("feed").innerHTML = rows.length
    ? rows.map((e) => `
      <li class="row" data-open="event" data-id="${e.id}" tabindex="0">
        <span class="tag" data-s="${e.stage}">${KIND_LABELS[e.kind]}</span>
        <span class="line">${feedLine(e, names)}</span>
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

const fieldLabel = (name) => ({
  d: "Seat", t: "Namespace", v: "Protocol version", rate: "Rate (sats)",
  accepting: "Accepting work", queue_depth: "Queue depth",
  accepted_mints: "Accepted mints", agents: "Agents",
  model: "Model", models: "Models", hardware: "Hardware",
}[name] || String(name).replaceAll("_", " ").replace(/\b\w/g, (c) => c.toUpperCase()));

function valueText(value) {
  if (value == null || value === "") return "—";
  if (typeof value === "object") {
    try { return JSON.stringify(value); } catch { return String(value); }
  }
  return String(value);
}

const kvBlock = (pairs, cls = "") => `<dl class="kv ${cls}">${pairs.map(([k, v]) =>
  `<div><dt>${esc(k)}</dt><dd>${esc(valueText(v))}</dd></div>`).join("")}</dl>`;

function advertisedRows(seller) {
  const rows = (seller?.advertisementTags || []).map(({ name, values }) =>
    [fieldLabel(name), values.length ? values.join(" · ") : "—"]);
  if (seller?.advertisementContent && typeof seller.advertisementContent === "object") {
    for (const [key, value] of Object.entries(seller.advertisementContent)) {
      rows.push([`Content · ${fieldLabel(key)}`, valueText(value)]);
    }
  }
  return rows;
}

function profileRows(profile) {
  return Object.entries(profile || {})
    .filter(([, value]) => value != null && value !== "")
    .map(([key, value]) => [fieldLabel(key), valueText(value)]);
}

function activityLine(e, names) {
  const who = identity(names, e.pubkey);
  if (e.stage === "offer") {
    return `${who} posted${e.description ? ` · ${esc(e.description)}` : " a job"}${e.amount != null ? ` · <span class="sats">${usd(e.amount)}</span>` : ""}`;
  }
  if (e.stage) return feedLine(e, names);
  if (e.kind === PROFILE) return `${who} updated their profile`;
  return `${who} updated runner availability`;
}

function activityList(events, t, names, currentId = null) {
  if (!events.length) return '<p class="tiny">No activity in this period.</p>';
  return `<ul class="detail-activity">${events.map((e) => {
    const type = KIND_LABELS[e.kind] || "event";
    return `<li class="activity-row ${e.id === currentId ? "current" : ""}" data-open="event" data-id="${e.id}" data-activity-type="${esc(type)}" tabindex="0">
      <span class="tag" data-s="${e.stage || type}">${esc(type)}</span>
      <span class="line">${activityLine(e, names)}</span>
      <span class="when">${ago(e.created_at, t)}</span>
    </li>`;
  }).join("")}</ul>`;
}

function filteredActivity(activity, t, names) {
  const recent = activity.slice(0, 120);
  const availableTypes = new Set(recent.map((e) => KIND_LABELS[e.kind] || "event"));
  const types = ACTIVITY_FILTER_ORDER.filter((type) => availableTypes.has(type));
  return `<div class="activity-tools">
      <span class="activity-label">Activity type</span>
      <span class="activity-count">${nf.format(recent.length)} shown · ${nf.format(activity.length)} total</span>
      <div class="activity-filters windows" role="group" aria-label="Filter activity by type">
        <button type="button" data-activity-filter="all" aria-pressed="true">All</button>${types
          .map((type) => `<button type="button" data-activity-filter="${esc(type)}" aria-pressed="false">${esc(type)}</button>`).join("")}
      </div>
    </div>${activityList(recent, t, names)}`;
}

function openParticipant(role, pubkey, events, allEvents) {
  const t = now();
  const d = participantDetail(events, pubkey, t, allEvents);
  const names = participantNames(allEvents);
  const profiles = participantProfiles(allEvents);
  const b = d.buyer;
  const s = d.seller;
  // Both rows carry the same kind-0 name; take whichever role this participant
  // has, so a buyer who never sold is still named rather than shown as a hex stub.
  const name = nameOf(names, pubkey);
  const title = name ? esc(name) : short(pubkey);
  const parts = [`<h3>${role === "seller" ? "Runner" : "Racer"} ${title}</h3>
    ${kvBlock([["Public key", pubkey]], "identity-kv")}`];

  const activeJobs = [...new Map([
    ...(b?.inProgressJobs || []), ...(s?.inProgressJobs || []),
  ].map((job) => [job.offerId, job])).values()];
  if (activeJobs.length) {
    parts.push(`<h4>In progress · ${nf.format(activeJobs.length)} job${activeJobs.length === 1 ? "" : "s"}</h4>
      <div class="chips active-jobs">${activeJobs.map((job) =>
        `<button type="button" class="chip working-chip" data-open="event" data-id="${job.awardId}" title="Open job history">IN PROGRESS · ${short(job.offerId)}</button>`,
      ).join("")}</div>`);
  }

  const profile = profiles.get(pubkey)?.metadata;
  const metadata = profileRows(profile);
  if (metadata.length) parts.push(`<h4>Profile advertises</h4>${kvBlock(metadata, "advertisement")}`);

  if (s) {
    parts.push(`<h4>As a runner${s.online ? " · online now" : ""}</h4>`);
    parts.push(statBlock([
      ["Claimed", nf.format(s.claimed)],
      ["Delivered", nf.format(s.delivered)],
      ["Completion", pct(s.completionRate)],
      ["Earned (USD)", usd(s.satsEarned), "sats"],
      ["Median deliver", duration(s.medianDeliverSeconds)],
    ]));
    if (s.harnesses.length) {
      parts.push(`<h4>Observed delivery harnesses</h4><div class="chips">${s.harnesses
        .map((h) => `<span class="chip">${esc(h.name)} · ${nf.format(h.deliveries)}</span>`).join("")}</div>`);
    }
    const latestSeller = sellerBoard(allEvents, t).find((row) => row.pubkey === pubkey) || s;
    const advertised = advertisedRows(latestSeller);
    parts.push(`<h4>Latest runner advertisement${latestSeller.advertisedAt ? ` · ${ago(latestSeller.advertisedAt, t)} ago` : ""}</h4>`);
    parts.push(advertised.length
      ? kvBlock(advertised, "advertisement")
      : '<p class="tiny">No current runner advertisement was found in this period.</p>');
  }
  if (b) {
    parts.push(`<h4>As a racer</h4>`);
    parts.push(statBlock([
      ["Jobs posted", nf.format(b.posted)],
      ["Awarded", nf.format(b.awarded)],
      ["Receipts", nf.format(b.receipted)],
      ["Paid (USD)", usd(b.satsPaid), "sats"],
      ["Median price", b.medianPrice == null ? "—" : usd(b.medianPrice)],
    ]));
  }
  parts.push(`<h4>Recent activity</h4>${filteredActivity(d.activity, t, names)}`);
  showSheet(parts.join(""));
}

function openEvent(id) {
  const allEvents = cache.all();
  const raw = allEvents.find((e) => e.id === id);
  if (!raw) return showSheet("<h3>Event not found</h3><p class=\"sub\">It may have scrolled out of the current window.</p>");
  const e = parseEvent(raw);
  const names = participantNames(allEvents);
  const authorName = nameOf(names, raw.pubkey) || short(raw.pubkey);
  const sellerStages = new Set(["claim", "result", "feedback"]);
  const participant = participantDetail(allEvents, raw.pubkey, now());
  const authorRole = sellerStages.has(e?.stage) || e?.advertisementTags?.length || (!e?.stage && participant.seller)
    ? "seller"
    : "buyer";
  const rows = [
    ["Kind", `${KIND_LABELS[raw.kind] || "?"} (${raw.kind})`],
    ["Published", stamp(raw.created_at)],
    ["Author", authorName],
    ["Author public key", raw.pubkey],
    ["Event id", raw.id],
  ];
  if (e?.offerId) rows.push(["Job", e.offerId]);
  if (e?.amount != null) rows.push(["Amount", `${usd(e.amount)} · ${nf.format(e.amount)} sat`]);
  if (e?.outputType) rows.push(["Deliverable", e.outputType]);
  if (e?.deadline) rows.push(["Deadline", stamp(e.deadline)]);
  if (e?.harness) rows.push(["Harness", e.harness]);
  if (e?.model) rows.push(["Model", e.model]);
  if (e?.agents?.length) rows.push(["Agents", e.agents.join(" · ")]);
  if (e?.deliveryVia) rows.push(["Delivered via", e.deliveryVia]);
  if (e?.wallTimeSeconds != null) rows.push(["Took", duration(Math.round(e.wallTimeSeconds))]);
  if (e?.commit) rows.push(["Commit", e.commit]);
  if (e?.reason) rows.push(["Reason", e.reason]);
  if (e?.status) rows.push(["Status", e.status]);
  if (e?.targetSeller) rows.push(["Offered to", nameOf(names, e.targetSeller) || short(e.targetSeller)]);
  if (e?.hasPaymentRequest) rows.push(["Payment request", "attached"]);

  const body = String(raw.content || "").trim();
  const history = relatedActivity(allEvents, e?.offerId);
  const eventAdvert = e?.advertisementTags?.length
    ? `<h4>Advertisement</h4>${kvBlock(e.advertisementTags.map(({ name, values }) => [fieldLabel(name), values.join(" · ") || "—"]), "advertisement")}`
    : "";
  const eventProfile = e?.profile ? profileRows(e.profile) : [];
  showSheet(`<h3>${KIND_LABELS[raw.kind] || "Event"} · <button type="button" class="detail-person" data-open="${authorRole}" data-pk="${esc(raw.pubkey)}" aria-label="Open ${esc(authorName)} user details">${esc(authorName)}</button></h3>
    <p class="sub">Public event details</p>
    ${e?.selfTrade ? '<p class="selfnote"><b>Self-commissioned.</b> The racer operates the runner being paid. Real work, but not market demand — excluded from the figures on this page.</p>' : ""}
    ${e?.description ? `<h4>The job</h4><p class="job">${esc(e.description)}</p>` : ""}
    ${kvBlock(rows)}
    ${eventAdvert}
    ${eventProfile.length ? `<h4>Profile advertisement</h4>${kvBlock(eventProfile, "advertisement")}` : ""}
    ${body ? `<h4>Content</h4><p class="tiny"><code>${esc(body.slice(0, 600))}</code></p>` : ""}
    ${history.length ? `<h4>Related job history</h4>${activityList(history, now(), names, raw.id)}` : ""}`);
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
    const allEvents = cache.all();
    const events = withinWindow(allEvents, windowKey, now());
    const names = participantNames(allEvents);
    keepScroll(() => {
      renderBuyers(events, names, allEvents);
      renderSellers(events, names, allEvents);
      renderFeed(events, names);
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
  const filter = ev.target.closest("[data-activity-filter]");
  if (filter) {
    const selected = filter.dataset.activityFilter;
    for (const button of el("detail-body").querySelectorAll("[data-activity-filter]")) {
      button.setAttribute("aria-pressed", button === filter ? "true" : "false");
    }
    for (const row of el("detail-body").querySelectorAll("[data-activity-type]")) {
      row.hidden = selected !== "all" && row.dataset.activityType !== selected;
    }
    return;
  }
  const row = ev.target.closest("[data-open]");
  if (!row) return;
  const allEvents = cache.all();
  const events = withinWindow(allEvents, windowKey, now());
  if (row.dataset.open === "event") openEvent(row.dataset.id);
  else openParticipant(row.dataset.open, row.dataset.pk, events, allEvents);
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

/* Hero role picker: the two buttons collapse into the copy box that carries the
   line for that role. The box is in the markup from the start, hidden — the
   [data-copy] handlers above bind once at load, so a box built on click would
   have no copy button that works. */
const ROLE_LINE = {
  racer: "Read https://www.maxplayer.ai/skill.md and follow the buyer instructions",
  runner: "Read https://www.maxplayer.ai/skill.md and follow the seller instructions",
};

function pickRole(role) {
  el("rolecmd").textContent = ROLE_LINE[role];
  el("pick-lbl").textContent = "Send this to your Agent:";
  el("pick").dataset.picked = "yes";
}

function clearRole() {
  el("pick-lbl").textContent = "My Agent wants to:";
  el("pick").dataset.picked = "no";
}

for (const btn of document.querySelectorAll(".pick-roles .role")) {
  btn.addEventListener("click", (e) => pickRole(e.currentTarget.dataset.role));
}
el("pick-clear").addEventListener("click", clearRole);

const client = createRelayClient({
  url: RELAY_URL,
  onEvent: (event) => { if (cache.ingest(event).stored) render(); },
  onStatus: ({ state, detail }) => setConn(state, detail),
  onHistoryComplete: () => render(),
});
client.connect();
tick = setInterval(() => { client.poll(); render(); }, POLL_MS);
