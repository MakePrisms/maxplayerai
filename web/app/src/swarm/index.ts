// The /swarm index and the routing between the two views. Remote text only ever reaches
// textContent; links are built from validated keys and slugs, never from event text.
import { naddrDecode, naddrEncode } from "./shared/naddr.mjs";
import { SWARM } from "../model/kinds.js";

export interface SwarmRow {
  buyer: string; buyerNpub: string; buyerName: string; id: string; name: string; purpose: string;
  status: string; tasks: string; members: number; openJobs: number;
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

// mode 'loading': skeleton rows only, no counts (nothing reads as current before the relay is read).
export function renderIndex(doc: Document, rows: SwarmRow[], mode: "loading" | "live", when: (secs: number) => string, relay: string): void {
  const list = doc.getElementById("swarm-list")!;
  list.setAttribute("aria-busy", String(mode === "loading"));
  const count = doc.getElementById("swarm-count")!;
  if (mode === "loading") {
    if (!list.querySelector(".skel")) {
      const fragment = doc.createDocumentFragment();
      for (let i = 0; i < 3; i++) { const li = doc.createElement("li"); const s = doc.createElement("span"); s.className = "skel"; li.append(s); fragment.append(li); }
      list.replaceChildren(fragment);
    }
    count.textContent = "";
    return;
  }
  const fragment = doc.createDocumentFragment();
  for (const r of rows) {
    const li = doc.createElement("li");
    const a = doc.createElement("a");
    a.href = swarmPath(r.buyer, r.id, relay);
    a.textContent = r.name;
    const purpose = doc.createElement("p"); purpose.className = "c-purpose"; purpose.textContent = firstLine(r.purpose);
    const meta = doc.createElement("span"); meta.className = "c-meta";
    const job = doc.createElement("span"); job.textContent = jobLine(r, when);
    if (r.running) job.className = "c-on";
    meta.append(job, doc.createTextNode(` · ${r.members} ${r.members === 1 ? "member" : "members"} · by ${r.buyerName || shortNpub(r.buyerNpub)}${r.tasks === "private" ? " · private tasks" : ""}`));
    li.append(a, purpose, meta);
    fragment.append(li);
  }
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
    li.textContent = `${when(j.starts)} · ${j.title} · ${status[j.status] ?? j.status} · ${j.tasks} ${j.tasks === 1 ? "task" : "tasks"}`;
    fragment.append(li);
  }
  list.replaceChildren(fragment);
}
