/**
 * The docks: three persistent terminal windows. Racers always open on the
 * LEFT, runners on the RIGHT, everything else (events) in the MIDDLE. Each
 * stays until its close button, Close All, or Escape. Open docks repaint from
 * the same MarketView the columns do, carrying scroll, focus, and each dock's
 * activity filter across the swap.
 */
import { ago, duration, esc, nf, now, pct, short, stamp } from "./format.js";
import { usd } from "./spot.js";
import { statusDot } from "./indicators.js";
import { feedLine } from "./board.js";
import { KIND_LABELS, PROFILE } from "../model/kinds.js";
import { parseEvent, type HarnessModel, type ParsedEvent } from "../model/events.js";
import {
  JOB_OVERDUE, participantDetail, relatedActivity,
} from "../market/participants.js";
import type { MarketView } from "../market/engine.js";

const RACER_ACTIVE_SECONDS = 86400;

const el = (id: string): HTMLElement => document.getElementById(id) as HTMLElement;

type DockKey = "left" | "mid" | "right";
type DockState =
  | { type: "participant"; role: "buyer" | "seller"; pubkey: string }
  | { type: "event"; id: string };

const DOCK_KEYS: DockKey[] = ["left", "mid", "right"];
const DOCK_FOR_ROLE: Record<string, DockKey> = { buyer: "left", seller: "right" };

const docks: Record<DockKey, DockState | null> = { left: null, mid: null, right: null };
/** Per-dock activity filter — a choice on one dock must not follow the reader. */
const dockFilters: Record<DockKey, string> = { left: "all", mid: "all", right: "all" };
/** Whether the event dock's job text is expanded past its 3-line clamp. */
const dockJobExpanded: Record<DockKey, boolean> = { left: false, mid: false, right: false };
let dockZ = 60;

/* ---------------- window manager ----------------
   Each dock is a floating window. Geometry is remembered per dock for the
   session, so a window you placed stays where you put it across closes and
   reopens; double-click the title bar to snap it home. On narrow screens the
   CSS takes over (full-screen, no drag/resize). */

interface Geom { x: number; y: number; w: number; h: number }
const dockGeom: Record<DockKey, Geom | null> = { left: null, mid: null, right: null };
/** Docks SPAWN pinned — the classic anchored layout. The pin button floats them. */
const dockPinned: Record<DockKey, boolean> = { left: true, mid: true, right: true };

/** Space reserved for the fixed chrome above the page. */
const CHROME_TOP = 46; // --nav-h

const isMobile = (): boolean => window.matchMedia("(max-width: 700px)").matches;

const clamp = (v: number, lo: number, hi: number): number => Math.max(lo, Math.min(hi, v));

function defaultGeom(key: DockKey): Geom {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const w = Math.min(key === "mid" ? 460 : 420, Math.floor(vw * 0.94));
  const h = Math.min(vh - CHROME_TOP - 16, 760);
  const x = key === "left" ? 8 : key === "right" ? vw - w - 8 : Math.round((vw - w) / 2);
  return { x, y: CHROME_TOP + 8, w, h };
}

function applyGeom(key: DockKey): void {
  if (isMobile() || dockPinned[key]) return; // CSS owns pinned/mobile geometry
  const box = el(`dock-${key}`);
  const g = dockGeom[key] ?? defaultGeom(key);
  box.style.left = `${g.x}px`;
  box.style.top = `${g.y}px`;
  box.style.width = `${g.w}px`;
  box.style.height = `${g.h}px`;
}

function raise(key: DockKey): void {
  el(`dock-${key}`).style.zIndex = String(++dockZ);
}

/**
 * Pin ↔ float. Pinning clears inline geometry so the CSS anchors take over;
 * unpinning pops the window out roughly where it stands, so nothing jumps
 * across the screen — it just comes loose in place.
 */
function setPinned(key: DockKey, pinned: boolean): void {
  const box = el(`dock-${key}`);
  dockPinned[key] = pinned;
  if (pinned) {
    box.classList.add("pinned");
    box.style.left = box.style.top = box.style.width = box.style.height = "";
  } else {
    const r = box.getBoundingClientRect();
    box.classList.remove("pinned");
    // ALWAYS pop out in place — never at some remembered spot from earlier,
    // which read as the window jumping across the screen. A slight inward
    // nudge shows it came loose, and the height comes up substantially so
    // the bottom-right resize handle is right there to grab.
    const g = {
      x: clamp(r.left + (key === "left" ? 16 : key === "right" ? -16 : 0), 8, Math.max(8, window.innerWidth - r.width - 8)),
      y: r.top + 12,
      w: r.width,
      h: Math.min(r.height - 120, 620),
    };
    dockGeom[key] = g;
    applyGeom(key);
  }
  const pinBtn = box.querySelector(".dock-pin") as HTMLElement | null;
  if (pinBtn) {
    pinBtn.innerHTML = pinned ? "&#x29C9;" : "&#x21F1;";
    pinBtn.title = pinned ? "Unpin — pop out into a floating window" : "Pin back to its edge";
    pinBtn.setAttribute("aria-label", pinned ? "Unpin window" : "Pin window");
  }
}

/** Drag by the title bar; native corner handle resizes (recorded below). */
function wireWindow(key: DockKey): void {
  const box = el(`dock-${key}`);
  const bar = box.querySelector(".dock-bar") as HTMLElement;
  bar.title = "Unpin to move · double-click to pin back";

  // Any interaction brings the window to the front, like a real desktop.
  box.addEventListener("pointerdown", () => raise(key));

  bar.addEventListener("pointerdown", (ev: PointerEvent) => {
    if (isMobile() || dockPinned[key]) return;
    if ((ev.target as HTMLElement).closest(".dock-close, .dock-pin")) return;
    ev.preventDefault();
    const rect = box.getBoundingClientRect();
    const start = { x: ev.clientX, y: ev.clientY, gx: rect.left, gy: rect.top };
    const move = (m: PointerEvent) => {
      // The title bar must stay reachable: clamp so the window can never be
      // dragged somewhere it cannot be dragged back from.
      const x = clamp(start.gx + m.clientX - start.x, 8 - rect.width + 120, window.innerWidth - 120);
      const y = clamp(start.gy + m.clientY - start.y, 48, window.innerHeight - 48);
      box.style.left = `${x}px`;
      box.style.top = `${y}px`;
    };
    const up = () => {
      bar.removeEventListener("pointermove", move);
      const r = box.getBoundingClientRect();
      dockGeom[key] = { x: r.left, y: r.top, w: r.width, h: r.height };
    };
    bar.setPointerCapture(ev.pointerId);
    bar.addEventListener("pointermove", move);
    bar.addEventListener("pointerup", up, { once: true });
    bar.addEventListener("pointercancel", up, { once: true });
  });

  // Double-click the bar of a floating window to pin it back to its edge.
  bar.addEventListener("dblclick", () => setPinned(key, true));

  // The native resize handle changes width/height behind our back — observe
  // and remember, so a reopened window keeps the size the reader chose.
  new ResizeObserver(() => {
    if (box.hidden || isMobile() || dockPinned[key]) return;
    const r = box.getBoundingClientRect();
    if (!r.width || !r.height) return;
    const g = dockGeom[key] ?? defaultGeom(key);
    dockGeom[key] = { ...g, w: r.width, h: r.height };
  }).observe(box);
}

/** Filter only the job lifecycle, in the order a successful trade occurs. */
const ACTIVITY_FILTER_ORDER = ["offer", "claim", "award", "result", "accept", "receipt"];

const nameOf = (view: MarketView, pubkey: string): string | null => view.names.get(pubkey) || null;

const dockBody = (key: DockKey): HTMLElement => el(`dock-${key}-body`);

/* ---------------- shared blocks ---------------- */

const statBlock = (pairs: [string, string, string?][]): string =>
  `<dl class="stats-in">${pairs.map(([k, v, cls]) => `<div><dt>${k}</dt><dd class="${cls || ""}">${v}</dd></div>`).join("")}</dl>`;

const fieldLabel = (name: string): string => ({
  d: "Seat", t: "Namespace", v: "Protocol version", rate: "Rate (sats)",
  accepting: "Accepting work", queue_depth: "Queue depth",
  accepted_mints: "Accepted mints", agents: "Agents",
  model: "Model", models: "Models", hardware: "Hardware",
} as Record<string, string>)[name] || String(name).replaceAll("_", " ").replace(/\b\w/g, (c) => c.toUpperCase());

function valueText(value: unknown): string {
  if (value == null || value === "") return "—";
  if (typeof value === "object") {
    try { return JSON.stringify(value); } catch { return String(value); }
  }
  return String(value);
}

const kvBlock = (pairs: [string, unknown][], cls = ""): string =>
  `<dl class="kv ${cls}">${pairs.map(([k, v]) =>
    `<div><dt>${esc(k)}</dt><dd>${esc(valueText(v))}</dd></div>`).join("")}</dl>`;

function profileRows(profile: Record<string, unknown> | undefined): [string, string][] {
  return Object.entries(profile || {})
    .filter(([, value]) => value != null && value !== "")
    .map(([key, value]) => [fieldLabel(key), valueText(value)]);
}

function activityLine(view: MarketView, e: ParsedEvent): string {
  const who = view.names.get(e.pubkey)
    ? `<span class="person">${esc(view.names.get(e.pubkey))}</span>`
    : `<code>${short(e.pubkey)}</code>`;
  if (e.stage === "offer") {
    return `${who} posted${e.description ? ` · ${esc(e.description)}` : " a job"}${e.amount != null ? ` · <span class="sats">${usd(e.amount)}</span>` : ""}`;
  }
  if (e.stage) return feedLine(view, e);
  if (e.kind === PROFILE) return `${who} updated their profile`;
  return `${who} updated runner availability`;
}

function activityList(view: MarketView, events: ParsedEvent[], t: number, currentId: string | null = null, filter = "all"): string {
  if (!events.length) return '<p class="tiny">No activity in this period.</p>';
  return `<ul class="detail-activity">${events.map((e) => {
    const type = KIND_LABELS[e.kind] || "event";
    // Applied HERE, not only by the click handler: the panel repaints on every
    // view update, and markup that came back unfiltered would silently undo
    // the reader's choice.
    const hide = filter !== "all" && type !== filter;
    return `<li class="activity-row ${e.id === currentId ? "current" : ""}" data-open="event" data-id="${e.id}" data-activity-type="${esc(type)}" tabindex="0"${hide ? " hidden" : ""}>
      <span class="tag" data-s="${e.stage || type}">${esc(type)}</span>
      <span class="line">${activityLine(view, e)}</span>
      <span class="when" data-ts="${e.created_at}">${ago(e.created_at, t)}</span>
    </li>`;
  }).join("")}</ul>`;
}

/* ---------------- participant sheet ---------------- */

/**
 * A Profile row: label, value, and an optional note. `title` explains what the
 * value is worth; `mark` is the visible tag the row wears, and `markClass`
 * selects its weight. A row with a title but no mark is annotated, not marked.
 */
export type ProfileRow = [string, string | null, { title: string; mark?: string; markClass?: string }?];

/**
 * One Profile row as markup. Exported so the marker rule is under test rather
 * than asserted by reading this file as text: which mark a value wears is a
 * claim about what a buyer sees, and a source grep cannot tell a rendered
 * marker from one that is built and dropped.
 *
 * The value goes through `esc` like every other relay string — `hardware` and
 * `harness_variant` are free text from an open relay, so they are an injection
 * path in exactly the way an enum-bound token is not.
 */
export function profileRowHtml([label, value, note]: ProfileRow): string {
  return `<div${note ? ` title="${esc(note.title)}"` : ""}><dt>${esc(label)}</dt><dd>${esc(String(value ?? ""))}${
    note?.mark ? ` <span class="${esc(note.markClass ?? "unverified")}">${esc(note.mark)}</span>` : ""}</dd></div>`;
}

/**
 * Operator-typed, nothing measures it, no filter reads it. Every row carrying
 * this marker is colour a human may find useful and a buyer must not price on.
 */
const OPERATOR_DECLARED = {
  mark: "operator-declared",
  markClass: "unverified",
  title: "Typed by the operator. Nothing measures or contradicts this value, and no buyer filter reads it.",
};

/**
 * The only capability field current as of the beat that carries it, and a
 * claim filter rather than a dispatch guarantee (protocol v1 §4.5.3, §4.5.4).
 *
 * The wording here is constrained for the same reason `LAST_OBSERVED`'s is.
 * Nothing at the seat reads `harness_family`: dispatch selects a harness by the
 * offer's `agent` preset alone. Any verb implying it binds execution — enforced,
 * pinned, guaranteed, exact-or-nothing at dispatch — states a fact no code
 * supports, and a multi-harness seat matching the filter can run a different
 * harness within the family.
 */
const FILTERS_CLAIMS_NOT_DISPATCH = {
  mark: "filters claims, not dispatch",
  markClass: "provenance",
  title: "Read from the live roster each time a beat is drafted, so it is current as of this beat. It decides which seats may claim a job, never which harness runs one — dispatch selects by the offer's `agent` preset alone, so a seat configured with several presets in this family can match and run a different one. A buyer that needs the execution guarantee must name the preset.",
};

/**
 * Last-observed, and never a promise about the next job.
 *
 * The wording here is constrained on purpose. This field carries what a harness
 * reported when it was last asked; nothing in the stack pins a model or reads
 * back what executed. Any stronger verb — ran, runs, will run, is serving,
 * guarantees — states a fact no code supports, and the tense is not what makes
 * it wrong. See `HARNESS_MODEL_TAG` beside the emitter.
 */
const LAST_OBSERVED = {
  mark: "last observed",
  markClass: "provenance",
  title: "What this harness said about itself when it was last asked, republished on every beat since. Not a promise about the next job. A result echoes a model id, so a divergence is visible in a buyer's own records — an inconsistency signal, never proof of what ran.",
};

/**
 * The stalest of the three, and the one a buyer commits sats on. Bounded by the
 * seat's UPTIME rather than by the beat cadence, and it drifts in both
 * directions — only one of which is safe (protocol v1 §4.5.4).
 */
const AS_OF_SEAT_START = {
  mark: "as of seat start",
  markClass: "provenance",
  title: "Probed once when the seat started and republished on every beat since, so a recent beat is NOT evidence of a recent measurement. A token means its probe command resolved in the job execution environment then: necessary for the work, not sufficient. A toolchain removed since start is still advertised, so this row can over-claim, and nothing on the filter path catches it.",
};

/**
 * What the runner says it can run, from the newest heartbeat.
 *
 * THE ROWS ARE NOT EQUAL, and the difference is provenance, not confidence.
 * Protocol v1 §4.5.3 is explicit that a reader MUST NOT read the three
 * filterable fields as three grades of proof, and that NONE of the three is an
 * enforcement: `harness_model` is ECHOED, so a divergence is at least visible
 * in a buyer's own records, while `harness_family` and `capabilities` are both
 * SILENCE — nothing at the seat reads the family, and no event carries a
 * capability back at all. One inconsistency signal and two silences. §4.5.4
 * then gives the three different freshness guarantees, and only
 * `harness_family` is current as of the beat carrying it.
 *
 * So every row wears its own mark, and the marks differ on what a reader can
 * DO with the value, not on how true it is. Two rows being equally unenforced
 * does not make them interchangeable: they differ in freshness and in what
 * they gate, and that is what the marks carry. Flush and unmarked, the stalest
 * row would read as solidly as the freshest, and a buyer commits sats on the
 * stalest of the three. The last two rows are free text an operator typed;
 * they stay marked apart from all three, because nothing pays out on them.
 *
 * Every row is an ANNOUNCEMENT either way. This reader sees the claim, never
 * the probe. A mark says what a claim is worth, not that anything verified it
 * here.
 *
 * An unstated field yields no row. A seat may state nothing — the stock Docker
 * runtime image proves no tokens at all — and an empty row would read as a
 * measured zero instead of a silence.
 */
export function capabilityRows(s: { harnessFamilies?: string[]; harnessModels?: HarnessModel[]; capabilities?: string[]; harnessVariant?: string | null; hardware?: string | null } | null): ProfileRow[] {
  if (!s) return [];
  return [
    ["Harness family", s.harnessFamilies?.length ? s.harnessFamilies.join(" · ") : null, FILTERS_CLAIMS_NOT_DISPATCH],
    ["Harness model", s.harnessModels?.length
      ? s.harnessModels.map((m) => `${m.family} ${m.model}`).join(" · ") : null, LAST_OBSERVED],
    ["Capabilities", s.capabilities?.length ? s.capabilities.join(" · ") : null, AS_OF_SEAT_START],
    ["Harness variant", s.harnessVariant || null, OPERATOR_DECLARED],
    ["Hardware", s.hardware || null, OPERATOR_DECLARED],
  ];
}

/**
 * Racer and runner details share one structure on purpose: headline name,
 * Profile, Stats (six boxes), Recent activity. A field the participant never
 * published is simply not shown — only the public key is guaranteed.
 */
function participantSheet(view: MarketView, role: "buyer" | "seller", pubkey: string, activityFilter = "all"): string {
  const t = now();
  const d = participantDetail(view.events, pubkey, t, view.allEvents);
  const b = d.buyer;
  const s = d.seller;
  const isSeller = role === "seller";
  const name = nameOf(view, pubkey);
  const title = name ? esc(name) : short(pubkey);

  // The headline wears the same streaks as the board row that opened it.
  let dot: string;
  if (isSeller) {
    dot = statusDot(!!s?.online, view.activeBySeller.get(pubkey) || []);
  } else {
    const lastAt = view.racerLastSeen.get(pubkey) || 0;
    const active = lastAt > 0 && t - lastAt <= RACER_ACTIVE_SECONDS;
    const context = active
      ? `Active in last 24 hours · last activity ${ago(lastAt, t)} ago`
      : (lastAt
          ? `No activity in last 24 hours · last activity ${ago(lastAt, t)} ago`
          : "No activity in last 24 hours");
    dot = statusDot(active, view.activeByBuyer.get(pubkey) || [], context);
  }
  const parts = [`<h3>${dot}<span>${isSeller ? "Runner" : "Racer"} ${title}</span></h3>`];

  /* Profile: identity plus what the participant advertises. "Accepting work:
     No" is a published value and stays; an absent advertisement does not. */
  const kind0 = view.profiles.get(pubkey);
  const about = kind0?.about ?? s?.about ?? b?.about;
  const accepting = s?.accepting;
  // The wire value is "y"/"n" (measured on the live relay 8/13 — 20 of 21
  // heartbeats say "y"). Accept the long forms too; an unknown value shows
  // raw rather than being silently misread as a refusal.
  const acceptingText = accepting == null ? null
    : ["y", "yes", "true", "1"].includes(String(accepting).toLowerCase()) ? "Yes"
    : ["n", "no", "false", "0"].includes(String(accepting).toLowerCase()) ? "No"
    : String(accepting);
  const profileRowsHtml = ([
    ["About", about ? String(about) : null],
    ["Min rate", s?.askSats == null ? null : `${usd(s.askSats)} · ${nf.format(s.askSats)} sat`],
    ["Accepting work", acceptingText],
    ["Accepted mints", s?.acceptedMints?.length ? s.acceptedMints.join(" · ") : null],
    ["Agents", s?.advertisedAgents?.length ? s.advertisedAgents.join(" · ") : null],
    ...capabilityRows(s),
  ] as ProfileRow[])
    .filter(([, v]) => v != null && v !== "")
    .map(profileRowHtml).join("");
  parts.push(`<h4>Profile</h4><dl class="kv profile-kv">
    <div><dt>Public key</dt><dd><button type="button" class="pk-copy" data-copy-text="${esc(pubkey)}" title="Click to copy the full public key" aria-label="Copy public key"><code>${short(pubkey)}…${esc(pubkey.slice(-8))}</code></button></dd></div>
    ${profileRowsHtml}</dl>`);

  /* Stats: six boxes, three per row, role-specific. Runner win rate counts
     LOSING claims too — the per-seller board can't, because a trade only
     records its winning seller — so both sides of the ratio come from the
     participant's raw activity. */
  if (isSeller) {
    const claimedOffers = new Set(d.activity.filter((e) => e.stage === "claim" && e.pubkey === pubkey && e.offerId).map((e) => e.offerId));
    const wonOffers = new Set(d.activity.filter((e) => e.stage === "award" && e.awardedSeller === pubkey && e.offerId).map((e) => e.offerId));
    const winRate = claimedOffers.size ? wonOffers.size / claimedOffers.size : null;
    parts.push(`<h4>Stats</h4>`);
    parts.push(statBlock([
      ["Claimed", nf.format(s?.claimed ?? 0)],
      ["Delivered", nf.format(s?.delivered ?? 0)],
      ["Completion", pct(s?.completionRate ?? null)],
      ["Win rate", pct(winRate)],
      ["Earned (USD)", usd(s?.satsEarned ?? 0), "sats"],
      ["Median deliver", duration(s?.medianDeliverSeconds ?? null)],
    ]));
  } else {
    // The racer's sixth box mirrors win rate from the buying side: how many of
    // the jobs they posted found a runner they awarded.
    const fillRate = b?.posted ? (b.awarded ?? 0) / b.posted : null;
    parts.push(`<h4>Stats</h4>`);
    parts.push(statBlock([
      ["Jobs posted", nf.format(b?.posted ?? 0)],
      ["Awarded", nf.format(b?.awarded ?? 0)],
      ["Receipts", nf.format(b?.receipted ?? 0)],
      ["Fill rate", pct(fillRate)],
      ["Paid (USD)", usd(b?.satsPaid ?? 0), "sats"],
      ["Median price", b?.medianPrice == null ? "—" : usd(b.medianPrice)],
    ]));
  }

  /* In-progress / overdue chips keep their place between Stats and the feed —
     the click-through behind the working streaks. */
  const trackedJobs = [...new Map([
    ...(b?.inProgressJobs || []), ...(s?.inProgressJobs || []),
  ].map((job) => [job.offerId, job])).values()];
  const working = trackedJobs.filter((job) => job.state !== JOB_OVERDUE);
  const overdue = trackedJobs.filter((job) => job.state === JOB_OVERDUE);
  if (working.length) {
    parts.push(`<h4>In progress · ${nf.format(working.length)} job${working.length === 1 ? "" : "s"}</h4>
      <div class="chips active-jobs">${working.map((job) =>
        `<button type="button" class="chip working-chip" data-open="event" data-id="${job.awardId}" title="Open job history">IN PROGRESS · ${short(job.offerId)}</button>`,
      ).join("")}</div>`);
  }
  // Overdue is the runner dock's section only: the runner owes the delivery.
  if (isSeller && overdue.length) {
    parts.push(`<h4>Overdue · ${nf.format(overdue.length)} job${overdue.length === 1 ? "" : "s"}</h4>
      <p class="job-note">Awarded, past the offer deadline, and no delivery has been published. The award stands; nothing here says it was paid or cancelled.</p>
      <div class="chips active-jobs">${overdue.map((job) =>
        `<button type="button" class="chip overdue-chip" data-open="event" data-id="${job.awardId}" title="${esc(`Deadline ${stamp(job.deadline ?? 0)} · no delivery published`)}">OVERDUE · ${short(job.offerId)}</button>`,
      ).join("")}</div>`);
  }

  parts.push(`<h4>Recent activity</h4>${filteredActivityHtml(view, d.activity, t, activityFilter)}`);
  return parts.join("");
}

/* ---------------- event sheet ---------------- */

/**
 * The event dock mirrors the participant docks' structure: bare event name as
 * the headline, then "Event details" first. Both parties are named there as
 * links into their own docks — racer left, runner right.
 */
function eventSheet(view: MarketView, id: string, jobExpanded = false): string {
  const raw = view.allEvents.find((e) => e.id === id);
  if (!raw) return "<h3>Event not found</h3><p class=\"sub\">It may have scrolled out of the current window.</p>";
  const e = parseEvent(raw);
  const sellerStages = new Set(["claim", "result", "feedback"]);
  const participant = participantDetail(view.allEvents, raw.pubkey, now());
  const authorRole = (e?.stage && sellerStages.has(e.stage)) || e?.advertisementTags?.length || (!e?.stage && participant.seller)
    ? "seller"
    : "buyer";
  const trade = e?.offerId ? view.trades.get(e.offerId) : null;
  const racerPk = e?.buyer || trade?.buyer || (authorRole === "buyer" ? raw.pubkey : null);
  const runnerPk = e?.awardedSeller || e?.targetSeller || trade?.seller || (authorRole === "seller" ? raw.pubkey : null);

  const personLink = (role: string, pk: string): string => {
    const label = nameOf(view, pk) || short(pk);
    return `<button type="button" class="detail-person" data-open="${role}" data-pk="${esc(pk)}" aria-label="Open ${esc(label)} details">${esc(label)}</button>`;
  };
  /** Truncated id, click copies the full value. */
  const copyId = (value: string): string => `<button type="button" class="pk-copy" data-copy-text="${esc(value)}" title="Click to copy" aria-label="Copy full id"><code>${esc(value.slice(0, 8))}…${esc(value.slice(-8))}</code></button>`;
  /** Same affordance for text values: head of the string, click copies all. */
  const copyTrunc = (value: string, max = 28): string => {
    const text = String(value);
    const head = text.length > max ? `${text.slice(0, max)}…` : text;
    return `<button type="button" class="pk-copy" data-copy-text="${esc(text)}" title="Click to copy" aria-label="Copy full value"><code>${esc(head)}</code></button>`;
  };

  const rows: [string, string][] = [];
  if (racerPk) rows.push(["Racer", personLink("buyer", racerPk)]);
  if (runnerPk) rows.push(["Runner", personLink("seller", runnerPk)]);
  rows.push(["Published", esc(stamp(raw.created_at))]);
  rows.push(["Event id", copyId(raw.id)]);
  if (e?.offerId) rows.push(["Job", copyId(e.offerId)]);
  if (e?.amount != null) rows.push(["Amount", esc(`${usd(e.amount)} · ${nf.format(e.amount)} sat`)]);
  if (e?.outputType) rows.push(["Deliverable", copyTrunc(e.outputType)]);
  if (e?.deadline) rows.push(["Deadline", esc(stamp(e.deadline))]);
  if (e?.harness) rows.push(["Harness", esc(e.harness)]);
  if (e?.model) rows.push(["Model", esc(e.model)]);
  if (e?.agents?.length) rows.push(["Agents", esc(e.agents.join(" · "))]);
  if (e?.deliveryVia) rows.push(["Delivered via", esc(e.deliveryVia)]);
  if (e?.wallTimeSeconds != null) rows.push(["Took", esc(duration(Math.round(e.wallTimeSeconds)))]);
  if (e?.commit) rows.push(["Commit", esc(e.commit)]);
  if (e?.reason) rows.push(["Reason", esc(e.reason)]);
  if (e?.status) rows.push(["Status", esc(e.status)]);
  if (e?.hasPaymentRequest) rows.push(["Payment request", "attached"]);
  const detailsKv = `<dl class="kv">${rows.map(([k, v]) => `<div><dt>${esc(k)}</dt><dd>${v}</dd></div>`).join("")}</dl>`;

  const body = String(raw.content || "").trim();
  const history = relatedActivity(view.allEvents, e?.offerId);
  const eventAdvert = e?.advertisementTags?.length
    ? `<h4>Advertisement</h4>${kvBlock(e.advertisementTags.map(({ name, values }) => [fieldLabel(name), values.join(" · ") || "—"]), "advertisement")}`
    : "";
  const eventProfile = e?.profile ? profileRows(e.profile) : [];
  // Streaks on the headline ONLY while its job is being worked.
  const workingJob = e?.offerId
    ? (view.activeBySeller.get(trade?.seller ?? "") || []).find((j) => j.offerId === e.offerId)
      || (view.activeByBuyer.get(trade?.buyer ?? "") || []).find((j) => j.offerId === e.offerId)
    : null;
  const workingDot = workingJob ? statusDot(true, [workingJob]) : "";
  return `<h3>${workingDot}<span>${esc(KIND_LABELS[raw.kind] || "Event")}</span></h3>
    ${e?.selfTrade ? '<p class="selfnote"><b>Self-commissioned.</b> The racer operates the runner being paid. Real work, but not market demand.</p>' : ""}
    <h4>Event details</h4>${detailsKv}
    ${e?.description ? `<h4>The job</h4><div class="job-wrap"><p class="job ${jobExpanded ? "" : "clamp"}">${esc(e.description)}</p><div class="chips">${e.description.length > 160 ? `<button type="button" class="chip show-chip" data-job-toggle>${jobExpanded ? "hide" : "show"}</button>` : ""}<button type="button" class="chip copy-chip" data-copy-text="${esc(e.description)}">copy</button></div></div>` : ""}
    ${eventAdvert}
    ${eventProfile.length ? `<h4>Profile advertisement</h4>${kvBlock(eventProfile, "advertisement")}` : ""}
    ${body ? `<h4>Content</h4><p class="tiny"><code>${esc(body.slice(0, 600))}</code></p>` : ""}
    ${history.length ? `<h4>Related job history</h4>${activityList(view, history, now(), raw.id)}` : ""}`;
}

/* ---------------- dock machinery ---------------- */

function dockHtml(view: MarketView, key: DockKey, state: DockState, filter: string): string {
  return state.type === "event"
    ? eventSheet(view, state.id, dockJobExpanded[key])
    : participantSheet(view, state.role, state.pubkey, filter);
}

/** Re-find a focused control after its markup is replaced, or give up quietly. */
function focusSelector(node: Element): string | null {
  const { activityFilter: filter, id } = (node as HTMLElement).dataset || {};
  if (filter) return `[data-activity-filter="${CSS.escape(filter)}"]`;
  // Tag-qualified: a chip and an activity row can carry the same event id.
  if (id) return `${node.localName}[data-id="${CSS.escape(id)}"]`;
  return null;
}

/** Repaint one dock in place, carrying focus across the innerHTML swap. */
function refreshDock(view: MarketView, key: DockKey): void {
  const state = docks[key];
  const box = el(`dock-${key}`);
  if (!state) { box.hidden = true; return; }
  const body = dockBody(key);
  const focused = document.activeElement;
  const focusKey = focused && body.contains(focused) ? focusSelector(focused) : null;
  const scrollTop = body.scrollTop;
  body.innerHTML = dockHtml(view, key, state, dockFilters[key]);
  box.hidden = false;
  if (scrollTop) body.scrollTop = scrollTop;
  if (focusKey) (body.querySelector(focusKey) as HTMLElement | null)?.focus();
}

export function refreshDocks(view: MarketView): void {
  for (const key of DOCK_KEYS) refreshDock(view, key);
  el("close-all").hidden = !DOCK_KEYS.some((key) => docks[key]);
}

/** Explicit open: reset the dock's filter, raise it, focus its close button. */
function openDock(view: MarketView, key: DockKey, state: DockState): void {
  // Phones hold ONE details screen: whatever opens takes the whole display
  // and replaces anything else. Desktop keeps its multi-window arrangement.
  if (isMobile()) for (const k of DOCK_KEYS) if (k !== key) closeDock(k);
  dockFilters[key] = "all";
  dockJobExpanded[key] = false;
  const wasOpen = docks[key] != null;
  docks[key] = state;
  const box = el(`dock-${key}`);
  raise(key);
  // Fresh opens spawn pinned; swapping content inside an open window must not
  // yank it back to the edge mid-arrangement.
  if (!wasOpen) setPinned(key, true);
  applyGeom(key);
  refreshDock(view, key);
  el("close-all").hidden = false;
  (box.querySelector(".dock-close") as HTMLElement | null)?.focus();
  dockBody(key).scrollTop = 0;
}

function closeDock(key: DockKey): void {
  docks[key] = null;
  el(`dock-${key}`).hidden = true;
  el("close-all").hidden = !DOCK_KEYS.some((k) => docks[k]);
}

export function closeAllDocks(): void {
  for (const key of DOCK_KEYS) closeDock(key);
}

/** Wire dock interactions. `currentView` defers to the latest engine output. */
export function wireDocks(currentView: () => MarketView | null): void {
  for (const key of DOCK_KEYS) wireWindow(key);
  document.addEventListener("click", (ev) => {
    const target = ev.target as HTMLElement;
    const closeBtn = target.closest("[data-dock-close]") as HTMLElement | null;
    if (closeBtn) return closeDock(closeBtn.dataset.dockClose as DockKey);
    const jobToggle = target.closest("[data-job-toggle]") as HTMLElement | null;
    if (jobToggle) {
      const dockNode = jobToggle.closest(".dock");
      const key = dockNode?.id?.replace("dock-", "") as DockKey | undefined;
      if (!key) return;
      // Recorded, not just applied: the dock repaints on every view update
      // and reads this back, so the expansion outlives the markup.
      dockJobExpanded[key] = !dockJobExpanded[key];
      const job = dockNode?.querySelector(".job");
      job?.classList.toggle("clamp", !dockJobExpanded[key]);
      jobToggle.textContent = dockJobExpanded[key] ? "hide" : "show";
      return;
    }
    const pinBtn = target.closest("[data-dock-pin]") as HTMLElement | null;
    if (pinBtn) {
      const key = pinBtn.dataset.dockPin as DockKey;
      return setPinned(key, !dockPinned[key]);
    }
    const keyBtn = target.closest("[data-copy-text]") as HTMLElement | null;
    if (keyBtn) return void copyKey(keyBtn);
    const filter = target.closest("[data-activity-filter]") as HTMLElement | null;
    if (filter) {
      // Recorded per dock, not just applied: the dock repaints on every view
      // update and reads this back, so the choice outlives the markup.
      const dockNode = filter.closest(".dock");
      const key = dockNode?.id?.replace("dock-", "") as DockKey | undefined;
      const selected = filter.dataset.activityFilter ?? "all";
      if (key) dockFilters[key] = selected;
      const scope = dockNode || document;
      for (const button of scope.querySelectorAll("[data-activity-filter]")) {
        button.setAttribute("aria-pressed", String(button === filter));
      }
      for (const row of scope.querySelectorAll<HTMLElement>("[data-activity-type]")) {
        row.hidden = selected !== "all" && row.dataset.activityType !== selected;
      }
      return;
    }
    const row = target.closest("[data-open]") as HTMLElement | null;
    if (!row) return;
    const view = currentView();
    if (!view) return;
    if (row.dataset.open === "event") {
      openDock(view, "mid", { type: "event", id: row.dataset.id as string });
    } else {
      const role = row.dataset.open as "buyer" | "seller";
      openDock(view, DOCK_FOR_ROLE[role] as DockKey, { type: "participant", role, pubkey: row.dataset.pk as string });
    }
  });
  document.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") closeAllDocks();
    if (ev.key === "Enter" && (ev.target as HTMLElement).matches?.("[data-open]")) (ev.target as HTMLElement).click();
  });
  el("close-all").addEventListener("click", closeAllDocks);
}

/* ---------------- copy ---------------- */

function copyLegacy(text: string): boolean {
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

export async function writeClipboard(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
    return copyLegacy(text);
  } catch {
    return copyLegacy(text);
  }
}

/**
 * Truncated ids flash inside their <code>; plain copy chips flash themselves.
 * The repaint may rebuild the button mid-flash, which quietly ends the flash —
 * never the copy.
 */
async function copyKey(btn: HTMLElement): Promise<void> {
  const ok = await writeClipboard(btn.dataset.copyText ?? "");
  btn.classList.toggle("ok", ok);
  const label = (btn.querySelector("code") as HTMLElement | null) || btn;
  const prev = label.textContent;
  label.textContent = ok ? "copied ✓" : "copy failed — select it";
  setTimeout(() => { label.textContent = prev; btn.classList.remove("ok"); }, 1200);
}

/**
 * The activity list plus its type-filter bar. A filtered type can fall out of
 * the window as newer activity arrives — fall back to All rather than render
 * a panel whose every row is hidden and whose filter bar offers nothing
 * pressed to explain why.
 */
function filteredActivityHtml(view: MarketView, activity: ParsedEvent[], t: number, activityFilter = "all"): string {
  const recent = activity.slice(0, 120);
  const availableTypes = new Set(recent.map((e) => KIND_LABELS[e.kind] || "event"));
  const types = ACTIVITY_FILTER_ORDER.filter((type) => availableTypes.has(type));
  const active = types.includes(activityFilter) ? activityFilter : "all";
  return `<div class="activity-tools">
      <span class="activity-label">Activity type</span>
      <span class="activity-count">${nf.format(recent.length)} shown · ${nf.format(activity.length)} total</span>
      <div class="activity-filters windows" role="group" aria-label="Filter activity by type">
        <button type="button" data-activity-filter="all" aria-pressed="${active === "all"}">All</button>${types
          .map((type) => `<button type="button" data-activity-filter="${esc(type)}" aria-pressed="${type === active}">${esc(type)}</button>`).join("")}
      </div>
    </div>${activityList(view, recent, t, null, active)}`;
}
