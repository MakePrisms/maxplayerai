import { KIND_LABELS } from "./kinds.js";
import { STATUS_LABELS } from "./jobs.js";

/* ═══════════════════════ connection banner ═══════════════════════ */

export function renderConnection(node, conn) {
  const label =
    conn.state === "connected"
      ? "connected"
      : conn.state === "connecting"
        ? "connecting…"
        : conn.state === "reconnecting"
          ? "reconnecting…"
          : "disconnected";
  node.dataset.state = conn.state;
  node.textContent = `${label} · ${conn.url}${conn.detail ? " · " + conn.detail : ""}`;
}

/* ═══════════════════════ pulse strip ═══════════════════════ */

export function renderPulse(node, pulse) {
  node.innerHTML = "";
  node.append(
    pulseStat(fmtSats(pulse.satsSettledToday), "sats settled today", "money"),
    pulseStat(String(pulse.openOffers), "open offers", ""),
    pulseStat(String(pulse.activeSellers), "active sellers", ""),
  );
}

function pulseStat(value, label, variant) {
  return el("div", { class: variant ? `pulse-stat ${variant}` : "pulse-stat" }, [
    el("div", { class: "pulse-value" }, [text(value)]),
    el("div", { class: "pulse-label" }, [text(label)]),
  ]);
}

/* ═══════════════════════ job-card feed ═══════════════════════ */

/**
 * @param {HTMLElement} root
 * @param {any[]} jobs
 * @param {Set<string>} expanded job ids currently open
 * @param {(id: string) => void} onToggle
 * @param {number} now unix-seconds, for relative timestamps
 */
export function renderFeed(root, jobs, expanded, onToggle, now) {
  root.innerHTML = "";
  if (!jobs.length) {
    root.append(el("p", { class: "empty-feed" }, [text("No jobs yet — waiting for the relay.")]));
    return;
  }
  renderJobList(root, jobs, expanded, onToggle, now);
}

/** Append a list of job cards. Shared by the feed and profile "recent jobs". */
export function renderJobList(root, jobs, expanded, onToggle, now) {
  for (const job of jobs) {
    root.append(jobCard(job, expanded.has(job.id), onToggle, now));
  }
}

function jobCard(job, isOpen, onToggle, now) {
  const card = el("article", { class: "job", dataset: { status: job.status } }, []);
  // Identities sit OUTSIDE the toggle button so they can be links (buyer/seller profiles).
  card.append(partiesRow(job));

  const head = el("button", { class: "job-head", type: "button" }, [
    el("div", { class: "job-line" }, [
      el("span", { class: "status-badge", dataset: { status: job.status } }, [text(job.status)]),
      job.amount_sats != null
        ? el("span", { class: "amount" }, [text(fmtSats(job.amount_sats))])
        : null,
      el("span", { class: "when" }, [text(fmtAgo(job.last_activity, now))]),
      el("span", { class: "chev" }, [text(isOpen ? "▾" : "▸")]),
    ]),
    job.task ? el("div", { class: "job-task" }, [text(job.task)]) : null,
  ]);
  head.addEventListener("click", () => onToggle(job.id));
  card.append(head);

  const timeline = el("div", { class: isOpen ? "job-timeline" : "job-timeline hidden" }, [
    el("p", { class: "status-line" }, [text(STATUS_LABELS[job.status] || job.status)]),
    timelineList(job, now),
  ]);
  card.append(timeline);
  return card;
}

/** Buyer on the LEFT, seller(s) on the RIGHT — the rule holds across the whole page. */
function partiesRow(job) {
  const seller =
    job.sellers.length === 0
      ? el("span", { class: "party-none" }, [text("no seller yet")])
      : el(
          "span",
          { class: "party-list" },
          job.sellers.slice(0, 3).map((s) => partyChip(s, "seller")),
        );
  const extra =
    job.sellers.length > 3
      ? el("span", { class: "party-more" }, [text(`+${job.sellers.length - 3}`)])
      : null;

  return el("div", { class: "parties" }, [
    el("div", { class: "party buyer" }, [
      partyChip(job.buyer, "buyer"),
      el("span", { class: "role-tag" }, [text("buyer")]),
    ]),
    el("div", { class: "party-arrow" }, [text("→")]),
    el("div", { class: "party seller" }, [
      el("span", { class: "role-tag" }, [text("seller")]),
      seller,
      extra,
    ]),
  ]);
}

/** Hash route to a party's profile — `#/seller/<pk>` or `#/buyer/<pk>`. */
export function profileHash(role, pubkey) {
  return `#/${role}/${pubkey}`;
}

/**
 * A colored identicon dot + short label for one party. When a role is given and a pubkey
 * is known, it is a link to that party's profile page.
 */
function partyChip(party, role) {
  if (!party || !party.pubkey) {
    return el("span", { class: "chip" }, [dot(null), el("span", { class: "chip-label" }, [text("unknown")])]);
  }
  const label = party.profile?.display_name || party.profile?.name || shortPk(party.pubkey);
  const kids = [dot(party.pubkey), el("span", { class: "chip-label" }, [text(label)])];
  if (role) {
    return el("a", { class: "chip chip-link", href: profileHash(role, party.pubkey), title: party.pubkey }, kids);
  }
  return el("span", { class: "chip", title: party.pubkey }, kids);
}

function dot(pubkey) {
  const d = el("span", { class: "dot" }, []);
  d.style.background = pubkeyColor(pubkey);
  return d;
}

function timelineList(job, now) {
  const ol = el("ol", { class: "timeline" }, []);
  for (const ev of job.timeline) {
    const who = ev.actor === "buyer" ? "Buyer" : ev.actor === "seller" ? "Seller" : "";
    const whoLabel = who ? `${who} ${shortPk(ev.pubkey)}` : shortPk(ev.pubkey);
    const whoNode =
      (ev.actor === "buyer" || ev.actor === "seller") && ev.pubkey
        ? el("a", { class: "tl-who tl-link", href: profileHash(ev.actor, ev.pubkey), title: ev.pubkey }, [text(whoLabel)])
        : el("span", { class: "tl-who" }, [text(whoLabel)]);
    ol.append(
      el("li", { class: `tl-entry actor-${ev.actor}` }, [
        el("span", { class: "tl-dot" }, [dotInline(ev.pubkey)]),
        el("span", { class: "tl-text" }, [whoNode, text(` ${ev.text}`)]),
        el("time", { class: "tl-time" }, [text(fmtAgo(ev.at, now))]),
      ]),
    );
  }
  if (!job.timeline.length) {
    ol.append(el("li", { class: "tl-entry" }, [text("no events recorded")]));
  }
  return ol;
}

function dotInline(pubkey) {
  const d = el("span", { class: "dot sm" }, []);
  d.style.background = pubkeyColor(pubkey);
  return d;
}

/* ═══════════════════════ profile pages ═══════════════════════ */

export function renderSellerProfile(root, m, expanded, onToggle, now) {
  root.innerHTML = "";
  root.append(
    profileHeader(m, "Seller", now),
    metricGrid([
      ["jobs completed", String(m.jobsCompleted)],
      ["sats earned", fmtSats(m.satsEarned)],
      ["refusal rate", fmtRate(m.refusalRate, m.jobsEngaged)],
      ["mean delivery", fmtDur(m.meanDeliverySec)],
      ["jobs engaged", String(m.jobsEngaged)],
    ]),
    relationshipStrip(m.relationships, "Repeat buyers"),
    recentJobsBlock(m.recentJobs, expanded, onToggle, now),
  );
}

export function renderBuyerProfile(root, m, expanded, onToggle, now) {
  root.innerHTML = "";
  root.append(
    profileHeader(m, "Buyer", now),
    metricGrid([
      ["jobs posted", String(m.jobsPosted)],
      ["sats paid", fmtSats(m.satsPaid)],
      ["pay promptness", fmtDur(m.meanPayLatencySec)],
      ["refusal rate", fmtRate(m.refusalRate, m.jobsPosted)],
      ["expired unpaid", String(m.expiredUnpaid)],
    ]),
    relationshipStrip(m.relationships, "Repeat sellers"),
    recentJobsBlock(m.recentJobs, expanded, onToggle, now),
  );
}

function profileHeader(m, roleLabel, now) {
  const name = m.profile?.display_name || m.profile?.name || shortPk(m.pubkey);
  const kids = [
    el("a", { class: "back-link", href: "#/" }, [text("← back to feed")]),
    el("div", { class: "profile-id" }, [
      bigDot(m.pubkey),
      el("div", {}, [
        el("h2", { class: "profile-name" }, [text(name)]),
        el("div", { class: "profile-sub" }, [
          el("span", { class: "role-tag" }, [text(roleLabel)]),
          el("span", { class: "profile-pk", title: m.pubkey }, [text(shortPk(m.pubkey))]),
        ]),
      ]),
    ]),
  ];
  // Liveness only surfaces on seller profiles (it's a seller heartbeat).
  if (m.liveness) kids.push(livenessBadge(m.liveness, now));
  return el("header", { class: "profile-header" }, kids);
}

function livenessBadge(liveness, now) {
  const label =
    liveness.state === "live"
      ? "live"
      : liveness.state === "recent"
        ? "seen recently"
        : liveness.state === "stale"
          ? "stale"
          : "no signal";
  const seen = liveness.lastSeen != null ? ` · last seen ${fmtAgo(liveness.lastSeen, now)}` : "";
  const src = liveness.source === "heartbeat" ? "" : liveness.source === "activity" ? " (from activity)" : "";
  return el("div", { class: "liveness", dataset: { state: liveness.state } }, [
    el("span", { class: "live-dot" }, []),
    el("span", {}, [text(`${label}${seen}${src}`)]),
  ]);
}

function metricGrid(pairs) {
  return el(
    "div",
    { class: "metric-grid" },
    pairs.map(([label, value]) =>
      el("div", { class: "metric-card" }, [
        el("div", { class: "metric-value" }, [text(value)]),
        el("div", { class: "metric-label" }, [text(label)]),
      ]),
    ),
  );
}

function relationshipStrip(rels, title) {
  const body =
    !rels || !rels.length
      ? [el("p", { class: "meta" }, [text("No repeat counterparties yet (2+ trades).")])]
      : rels.map((r) => {
          const name = r.profile?.display_name || r.profile?.name || shortPk(r.pubkey);
          return el(
            "a",
            { class: "rel-chip", href: profileHash(r.otherRole, r.pubkey), title: r.pubkey },
            [
              dot(r.pubkey),
              el("span", { class: "chip-label" }, [text(name)]),
              el("span", { class: "rel-count" }, [text(`${r.trades}×`)]),
            ],
          );
        });
  return el("section", { class: "rel-strip" }, [
    el("h3", {}, [text(title)]),
    el("div", { class: "rel-list" }, body),
  ]);
}

function recentJobsBlock(jobs, expanded, onToggle, now) {
  const list = el("div", { class: "feed-panel" }, []);
  if (!jobs || !jobs.length) {
    list.append(el("p", { class: "meta" }, [text("No jobs on record.")]));
  } else {
    renderJobList(list, jobs, expanded, onToggle, now);
  }
  return el("section", { class: "recent-jobs" }, [el("h3", {}, [text("Recent jobs")]), list]);
}

function bigDot(pubkey) {
  const d = el("span", { class: "dot big" }, []);
  d.style.background = pubkeyColor(pubkey);
  return d;
}

/* ═══════════════════════ stats tab (demoted event analytics) ═══════════════════════ */

export function renderStats(root, snap) {
  root.innerHTML = "";
  root.append(
    el("section", { class: "view", id: "funnel" }, [
      h2("Funnel"),
      p("Offers → claims → results → receipts. Leaks are the product signal."),
      renderFunnel(snap.funnel),
    ]),
    el("section", { class: "view", id: "latency" }, [
      h2("Latency"),
      p("Time-to-claim and time-to-result (seconds)."),
      renderLatency(snap.latency),
    ]),
    el("section", { class: "view", id: "economics" }, [
      h2("Economics"),
      p(
        "From receipts / usage adjunct. measured_cost_tokens may be absent — view degrades, never blanks.",
      ),
      renderEconomics(snap.economics),
    ]),
    el("section", { class: "view", id: "census" }, [
      h2("Seller census"),
      p("Seller handler announces (NIP-89) — harness_name + version."),
      renderCensus(snap.census),
    ]),
    el("section", { class: "view", id: "tail" }, [
      h2("Raw live tail"),
      p("Newest events, kind-labeled. Expand a row for JSON."),
      renderTail(snap.tail),
    ]),
  );
}

function renderFunnel(f) {
  const leaks = f.leaks || { unclaimed: 0, unresulted: 0, unpaid: 0 };
  return el("div", { class: "funnel" }, [
    el("div", { class: "funnel-steps" }, [
      metric("offers", f.offers),
      arrow(),
      metric("claimed", f.claimed),
      arrow(),
      metric("resulted", f.resulted),
      arrow(),
      metric("receipted", f.receipted),
    ]),
    el("div", { class: "leaks" }, [
      leak("unclaimed offers", leaks.unclaimed),
      leak("unresulted claims", leaks.unresulted),
      leak("unpaid results", leaks.unpaid),
    ]),
    el("p", { class: "meta" }, [
      text(
        `${f.events} events ingested · ${f.profiles || 0} profiles · ${f.parseSkips} parse skips (malformed dropped)`,
      ),
    ]),
  ]);
}

function renderLatency(lat) {
  return el("div", { class: "latency" }, [
    latBlock("time-to-claim", lat.toClaim),
    latBlock("time-to-result", lat.toResult),
  ]);
}

function latBlock(title, s) {
  return el("div", { class: "lat-block" }, [
    el("h3", {}, [text(title)]),
    el("dl", { class: "stats" }, [
      stat("n", s.n),
      stat("p50", fmtSec(s.p50)),
      stat("p90", fmtSec(s.p90)),
      stat("p99", fmtSec(s.p99)),
      stat("min", fmtSec(s.min)),
      stat("max", fmtSec(s.max)),
    ]),
  ]);
}

function renderEconomics(eco) {
  const groupRows = eco.groups.map(([key, g]) => {
    const [family, transport] = key.split("|");
    return el("tr", {}, [
      td(family),
      td(transport),
      td(String(g.n)),
      td(g.withCost ? avg(g.sumCost, g.withCost) : "—"),
      td(g.sumPaidTok ? avg(g.sumPaidTok, g.n) : "—"),
      td(g.sumPaidSat ? avg(g.sumPaidSat, g.n) : "—"),
    ]);
  });

  const detailRows = eco.rows.slice(0, 40).map((r) =>
    el("tr", {}, [
      td(shortId(r.id)),
      td(r.source || "—"),
      td(r.harness_family || "—"),
      td(r.usage_transport || "—"),
      td(fmtNum(r.total_tokens)),
      td(fmtNum(r.input_tokens)),
      td(fmtNum(r.output_tokens)),
      td(fmtNum(r.measured_cost_tokens)),
      td(fmtNum(r.paid_price_tokens)),
      td(fmtNum(r.paid_price_sats)),
    ]),
  );

  return el("div", { class: "economics" }, [
    el("h3", {}, [text("by harness_family × usage_transport")]),
    el("div", { class: "table-wrap" }, [
      el("table", {}, [
        el("thead", {}, [
          el("tr", {}, [
            th("harness_family"),
            th("usage_transport"),
            th("n"),
            th("avg measured_cost_tokens"),
            th("avg paid_price_tokens"),
            th("avg paid sats"),
          ]),
        ]),
        el("tbody", {}, groupRows.length ? groupRows : [emptyRow(6)]),
      ]),
    ]),
    el("h3", {}, [text("recent settlements")]),
    el("div", { class: "table-wrap" }, [
      el("table", {}, [
        el("thead", {}, [
          el("tr", {}, [
            th("id"),
            th("source"),
            th("harness_family"),
            th("usage_transport"),
            th("total_tokens"),
            th("input_tokens"),
            th("output_tokens"),
            th("measured_cost_tokens"),
            th("paid_price_tokens"),
            th("paid sats"),
          ]),
        ]),
        el("tbody", {}, detailRows.length ? detailRows : [emptyRow(10)]),
      ]),
    ]),
  ]);
}

function renderCensus(rows) {
  const body = rows.map((r) =>
    el("tr", {}, [
      td(r.harness_name),
      td(r.version),
      el("td", {}, [authorCell(r.pubkey, r.profile)]),
      td(r.k.join(", ") || "—"),
      td(fmtTime(r.created_at)),
    ]),
  );
  return el("div", { class: "table-wrap" }, [
    el("table", {}, [
      el("thead", {}, [
        el("tr", {}, [th("harness_name"), th("version"), th("seller"), th("k"), th("seen")]),
      ]),
      el("tbody", {}, body.length ? body : [emptyRow(5)]),
    ]),
  ]);
}

function renderTail(events) {
  const list = el("div", { class: "tail" }, []);
  for (const ev of events) {
    const summary = el("button", { class: "tail-row", type: "button" }, [
      el("span", { class: "kind" }, [text(KIND_LABELS[ev.kind] || String(ev.kind))]),
      el("span", { class: "id" }, [text(shortId(ev.id))]),
      el("span", { class: "pk" }, [authorCell(ev.pubkey, ev.profile)]),
      el("span", { class: "ts" }, [text(fmtTime(ev.created_at))]),
    ]);
    const pre = el("pre", { class: "tail-json hidden" }, [
      text(
        JSON.stringify(
          {
            id: ev.id,
            pubkey: ev.pubkey,
            kind: ev.kind,
            created_at: ev.created_at,
            tags: ev.tags,
            content: ev.content,
            profile: ev.profile
              ? { name: ev.profile.name, display_name: ev.profile.display_name, picture: ev.profile.picture }
              : null,
          },
          null,
          2,
        ),
      ),
    ]);
    summary.addEventListener("click", () => pre.classList.toggle("hidden"));
    list.append(el("div", { class: "tail-item" }, [summary, pre]));
  }
  if (!events.length) {
    list.append(el("p", { class: "meta" }, [text("No events yet.")]));
  }
  return list;
}

/**
 * Prefer display_name / name; fall back to short pubkey — never blank.
 * @param {string} pubkey
 * @param {{name?:string|null, display_name?:string|null, picture?:string|null}|null} profile
 */
function authorCell(pubkey, profile) {
  const label = profile?.display_name || profile?.name || shortId(pubkey) || "—";
  const kids = [];
  if (profile?.picture) {
    kids.push(
      el("img", {
        class: "avatar",
        src: profile.picture,
        alt: "",
        width: "16",
        height: "16",
        loading: "lazy",
        referrerpolicy: "no-referrer",
      }),
    );
  }
  kids.push(el("span", { class: "author-label", title: pubkey || "" }, [text(label)]));
  return el("span", { class: "author" }, kids);
}

/* ═══════════════════════ DOM helpers ═══════════════════════ */

function el(tag, attrs = {}, children = []) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") node.className = v;
    else if (k === "dataset") Object.assign(node.dataset, v);
    else node.setAttribute(k, v);
  }
  for (const child of children) {
    if (child != null) node.append(child);
  }
  return node;
}

function text(s) {
  return document.createTextNode(String(s));
}
function h2(s) {
  return el("h2", {}, [text(s)]);
}
function p(s) {
  return el("p", { class: "lede" }, [text(s)]);
}
function metric(label, value) {
  return el("div", { class: "metric" }, [
    el("div", { class: "metric-value" }, [text(String(value))]),
    el("div", { class: "metric-label" }, [text(label)]),
  ]);
}
function arrow() {
  return el("div", { class: "arrow" }, [text("→")]);
}
function leak(label, value) {
  return el("div", { class: value > 0 ? "leak loud" : "leak" }, [
    el("span", { class: "leak-value" }, [text(String(value))]),
    el("span", { class: "leak-label" }, [text(label)]),
  ]);
}
function stat(k, v) {
  return el("div", { class: "stat" }, [
    el("dt", {}, [text(k)]),
    el("dd", {}, [text(v == null ? "—" : String(v))]),
  ]);
}
function th(s) {
  return el("th", {}, [text(s)]);
}
function td(s) {
  return el("td", {}, [text(s)]);
}
function emptyRow(cols) {
  return el("tr", {}, [el("td", { colspan: String(cols), class: "empty" }, [text("no data yet")])]);
}

/* ═══════════════════════ formatting ═══════════════════════ */

function fmtSats(v) {
  if (v == null) return "—";
  return `${Number(v).toLocaleString("en-US")} sats`;
}
/** Rate as a percent; dash when there were no samples to divide by. */
function fmtRate(rate, denom) {
  if (rate == null || !denom) return "—";
  return `${Math.round(rate * 100)}%`;
}
/** Compact duration: 45s / 1m10s / 2h5m. Null → dash. */
function fmtDur(sec) {
  if (sec == null) return "—";
  const s = Math.round(sec);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${String(s % 60).padStart(2, "0")}s`;
  const h = Math.floor(s / 3600);
  return `${h}h${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
}
function fmtSec(v) {
  if (v == null) return "—";
  return `${v.toFixed(1)}s`;
}
function fmtNum(v) {
  if (v == null) return "—";
  return String(v);
}
function avg(sum, n) {
  if (!n) return "—";
  return (sum / n).toFixed(1);
}
function shortId(id) {
  if (!id) return "—";
  return id.length > 12 ? `${id.slice(0, 8)}…` : id;
}
function shortPk(pk) {
  if (!pk) return "unknown";
  return pk.length > 8 ? `${pk.slice(0, 6)}…` : pk;
}
function fmtTime(ts) {
  if (ts == null) return "—";
  try {
    return new Date(ts * 1000).toISOString().replace("T", " ").replace(/\.\d+Z$/, "Z");
  } catch {
    return String(ts);
  }
}

/** Compact relative age: 12s / 5m / 3h / 2d. */
function fmtAgo(ts, now) {
  if (ts == null) return "—";
  const s = Math.max(0, (now ?? Math.floor(Date.now() / 1000)) - ts);
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

/**
 * Deterministic identicon color from a pubkey — same key, same hue everywhere.
 * Not security-sensitive; just a stable visual handle.
 */
function pubkeyColor(pubkey) {
  if (!pubkey) return "hsl(0 0% 55%)";
  let h = 0;
  for (let i = 0; i < pubkey.length; i += 1) {
    h = (h * 31 + pubkey.charCodeAt(i)) % 360;
  }
  return `hsl(${h} 62% 52%)`;
}
