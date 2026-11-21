// The /swarm index and the routing between the two views. Remote text only ever reaches
// textContent; links are built from validated keys and slugs, never from event text.
import { naddrDecode, naddrEncode } from "./shared/naddr.mjs";
import { SWARM } from "../model/kinds.js";

export interface SwarmRow {
  buyer: string; buyerNpub: string; buyerName: string; id: string; name: string; purpose: string;
  status: string; tasks: string; members: number; openJobs: number; createdAt?: number; jobsDone?: number;
  running: { id: string; title: string; starts: number; ends: number } | null;
  next: { id: string; title: string; starts: number; ends: number } | null;
}
export type Route = { view: "index" } | { view: "swarm"; buyer: string; swarm: string; naddr: string } | { view: "missing"; raw: string };

const SLUG = /^[a-z0-9-]{1,64}$/;
const HEX = /^[0-9a-f]{64}$/;
/** The swarm a /swarm path names: /swarm (index) or /swarm/<naddr1… of a swarm>. */
export function route(pathname: string): Route {
  const rest = pathname.replace(/^\/swarm(?:\.html)?\/?/, "");
  if (!rest) return { view: "index" };
  let raw = rest.replace(/\/+$/, "");
  try { raw = decodeURIComponent(raw); } catch { return { view: "missing", raw }; }
  const d = naddrDecode(raw);
  if (!d || d.kind !== SWARM || !HEX.test(d.pubkey) || !SLUG.test(d.identifier)) return { view: "missing", raw };
  return { view: "swarm", buyer: d.pubkey, swarm: d.identifier, naddr: raw };
}
/** The board's link to a swarm: the same naddr the CLI prints for our relay. */
export const swarmPath = (buyer: string, swarm: string, relay: string) =>
  `/swarm/${naddrEncode({ kind: SWARM, pubkey: buyer, identifier: swarm, relays: [relay] })}`;

export const shortNpub = (npub: string) => npub.length > 20 ? `${npub.slice(0, 12)}…${npub.slice(-6)}` : npub;
// The first line of prose; a leading "# Heading" (usually the swarm's name again) only when there is nothing else.
export const firstLine = (text: string, max = 280) => {
  const lines = text.replace(/\r\n?/g, "\n").split("\n").map((l) => l.trim()).filter(Boolean);
  const line = (lines.find((l) => !l.startsWith("#")) ?? lines[0] ?? "").replace(/^#+\s*/, "");
  return line.length > max ? `${line.slice(0, max - 1)}…` : line;
};

/** "Running now: <title>", "Next job <when>: <title>" or "No job scheduled". */
export function jobLine(r: SwarmRow, when: (secs: number) => string): string {
  if (r.running) return `Running now: ${r.running.title}`;
  if (r.next) return `Next job ${when(r.next.starts)}: ${r.next.title}`;
  return "No job scheduled";
}

/**
 * The board's order (Bob, 2026-11-21): swarms with a job running now first, then the rest; newest swarm first in
 * each group (swarmIndex already sorts by the swarm event's time, newest first, and the sort is stable).
 */
export const boardOrder = (rows: SwarmRow[]): SwarmRow[] => [...rows].sort((a, b) => Number(!!b.running) - Number(!!a.running));

/** The four numbers over the board. */
export function indexTotals(rows: SwarmRow[]): { live: number; open: number; members: number; done: number } {
  return { live: rows.filter((r) => r.running).length, open: rows.length,
    members: rows.reduce((n, r) => n + r.members, 0), done: rows.reduce((n, r) => n + (r.jobsDone ?? 0), 0) };
}

/** A swarm's age, as on /market's activity list: now, 5m, 3h, 12d. */
export function age(createdAt: number | undefined, now: number): string {
  if (!createdAt) return "";
  const s = Math.max(0, now - createdAt);
  return s < 60 ? "now" : s < 3600 ? `${Math.floor(s / 60)}m` : s < 86400 ? `${Math.floor(s / 3600)}h` : `${Math.floor(s / 86400)}d`;
}

/** The status column: Live, the next job's start, or nothing scheduled. */
export function statusText(r: SwarmRow, when: (secs: number) => string): string {
  if (r.running) return "Live";
  if (r.next) return `Starts ${when(r.next.starts)}`;
  return "No job scheduled";
}

// The board, in the rows of /market's lists. mode 'loading': grey placeholder rows only, no counts (nothing reads as
// current before the relay is read). A row is a link to the swarm (a real address: it opens in a new tab and can be
// shared); a click opens it in the panel instead (swarm.ts).
export function renderIndex(doc: Document, rows: SwarmRow[], mode: "loading" | "live", when: (secs: number) => string, relay: string, now = Math.floor(Date.now() / 1000)): void {
  const list = doc.getElementById("swarm-list")!;
  list.setAttribute("aria-busy", String(mode === "loading"));
  const count = doc.getElementById("swarm-count")!;
  const el = (tag: string, cls: string, text?: string) => { const e = doc.createElement(tag); e.className = cls; if (text !== undefined) e.textContent = text; return e; };
  if (mode === "loading") {
    if (!list.querySelector(".sk")) {
      const fragment = doc.createDocumentFragment();
      for (let i = 0; i < 4; i++) { const li = el("li", "row sk"); li.setAttribute("aria-hidden", "true"); li.append(el("i", `sk${i % 3}`)); fragment.append(li); }
      list.replaceChildren(fragment);
    }
    count.textContent = "";
    return;
  }
  const fragment = doc.createDocumentFragment();
  for (const r of boardOrder(rows)) {
    const li = el("li", "row g-s go");
    const lamp = el("i", r.running ? "lamp on" : "lamp");
    lamp.setAttribute("role", "img");
    lamp.setAttribute("aria-label", r.running ? "Live: a job is running" : "No job running");
    const nm = el("span", "nm");
    const a = doc.createElement("a") as HTMLAnchorElement;
    a.className = "who";
    a.href = swarmPath(r.buyer, r.id, relay);
    a.textContent = r.name;
    nm.append(a);
    const purpose = firstLine(r.purpose, 140);
    if (purpose) nm.append(el("span", "pp", purpose));
    li.append(lamp, nm, el("span", r.running ? "st on" : "st", statusText(r, when)), el("span", "num", String(r.members)),
      el("span", "num", String(r.jobsDone ?? 0)), el("span", "num age", age(r.createdAt, now)));
    fragment.append(li);
  }
  if (!rows.length) fragment.append(el("li", "note", "No open swarms yet. Check back soon."));
  list.replaceChildren(fragment);
  count.textContent = rows.length ? `${rows.length} open ${rows.length === 1 ? "swarm" : "swarms"}` : "No open swarms yet. Check back soon.";
}

/** This swarm's jobs, newest first: title, window, status and task count. */
export function renderSwarmJobs(doc: Document, jobs: { id: string; title: string; starts: number; status: string; tasks: number }[], when: (secs: number) => string): void {
  const list = doc.getElementById("swarm-jobs")!;
  const fragment = doc.createDocumentFragment();
  const status: Record<string, string> = { scheduled: "Scheduled", in_progress: "Running", completed: "Done", cancelled: "Cancelled" };
  for (const j of jobs.slice(0, 20)) {
    const li = doc.createElement("li");
    // Bob (2026-11-21): no task count (it read "0 tasks" on finished jobs); "Done" is said by the heading.
    li.textContent = j.status === "completed" ? `${when(j.starts)} · ${j.title}` : `${when(j.starts)} · ${j.title} · ${status[j.status] ?? j.status}`;
    fragment.append(li);
  }
  if (!jobs.length) { const li = doc.createElement("li"); li.className = "empty"; li.textContent = "No finished jobs yet."; fragment.append(li); }
  list.replaceChildren(fragment);
}
