// /swarm agents table (bob, 2026-10-20): a member who left is listed only if they had at least one
// job in the run shown. Driven through the real shared projection (EventStore.snapshot, the one the
// runner and CLI also read) and the page's paint(); the snapshot itself keeps every left seller.
import assert from "node:assert/strict";
import test from "node:test";
import { schnorr } from "@noble/curves/secp256k1";
import { eventId } from "../src/challenge/shared/events.mjs";
import { EventStore, snapshot as project } from "../src/challenge/shared/model.mjs";
import { paint } from "../src/challenge/page.js";
import { parseSnapshot, type Snapshot } from "../src/challenge/live.js";
import { OFFER, CLAIM, AWARD, RESULT, ACCEPT, CHALLENGE, MEMBERSHIP, DECISION, PROFILE } from "../src/model/kinds.js";

const key = (n: number) => n.toString(16).padStart(64, "0");
const pub = (k: string) => Buffer.from(schnorr.getPublicKey(k)).toString("hex");
const BUYER = key(1), WORKER = key(2), LEAVER = key(3), OLDHAND = key(4), EXPIRED = key(5);
const buyer = pub(BUYER);
type Ev = { id: string; sig: string; pubkey: string; kind: number; created_at: number; tags: string[][]; content: string };
function sign(k: string, kind: number, created_at: number, tags: string[][], content = ""): Ev {
  const e = { pubkey: pub(k), kind, created_at, tags, content } as Ev;
  e.id = eventId(e); e.sig = Buffer.from(schnorr.sign(e.id, k)).toString("hex"); return e;
}
const ns = [["t", "maxplayer"], ["v", "1"]];
const run = (d: string, starts: number, ends: number, t: number) => sign(BUYER, CHALLENGE, t, [...ns, ["d", d], ["title", d], ["starts", String(starts)], ["ends", String(ends)], ["budget", "0", "sat"], ["status", "in_progress"]]);
const member = (k: string, t: number, status = "joined") => sign(k, MEMBERSHIP, t, [...ns, ["d", buyer], ["p", buyer], ["since", "50"], ["minutes", "60"], ["status", status]]);
const accepted = (k: string) => sign(BUYER, DECISION, 60, [...ns, ["d", pub(k)], ["p", pub(k)], ["decision", "accepted"]]);
const profile = (k: string, name: string) => sign(k, PROFILE, 70, [], JSON.stringify({ name }));
// One awarded job for seller k in run d at time t: offer, claim, award, and optionally result + accept.
function job(k: string, d: string, t: number, finished: boolean): Ev[] {
  const seller = pub(k);
  const offer = sign(BUYER, OFFER, t, [...ns, ["p", seller], ["i", `[swarm:${buyer}:${d}]\nTask`], ["amount", "0", "sat"], ["param", "payment", "none"], ["param", "deadline", String(t + 600)]]);
  const claim = sign(k, CLAIM, t + 1, [...ns, ["e", offer.id, "", "root"], ["p", buyer], ["status", "processing"]]);
  const award = sign(BUYER, AWARD, t + 2, [...ns, ["e", claim.id], ["e", offer.id, "", "root"], ["p", buyer], ["p", seller], ["status", "accepted"]]);
  if (!finished) return [offer, claim, award];
  const result = sign(k, RESULT, t + 60, [...ns, ["e", offer.id, "", "root"], ["p", buyer]], "Done");
  const accept = sign(BUYER, ACCEPT, t + 70, [...ns, ["e", claim.id], ["e", offer.id, "", "root"], ["p", buyer], ["p", seller], ["status", "accepted"]]);
  return [offer, claim, award, result, accept];
}
// Two runs: "old" (1000-2000) then "now" (3000-9000, shown by default).
// worker:  joined, working in "now".            leaver:  left, never had a job anywhere.
// oldhand: left, worked only in "old".          expired: left, one job in "now" that ran out unfinished.
const oldRun = run("old", 1000, 2000, 900), nowRun = run("now", 3000, 9000, 2900);
const events = [oldRun, nowRun,
  ...[WORKER, LEAVER, OLDHAND, EXPIRED].map(accepted),
  member(WORKER, 100), member(LEAVER, 2500, "left"), member(OLDHAND, 2500, "left"), member(EXPIRED, 3700, "left"),
  profile(WORKER, "worker"), profile(LEAVER, "leaver"), profile(OLDHAND, "oldhand"), profile(EXPIRED, "expired"),
  ...job(OLDHAND, "old", 1100, true), ...job(EXPIRED, "now", 3100, false), ...job(WORKER, "now", 3500, false)];
const NOW = 3800;
function store(evs = events) { const s = new EventStore(); for (const e of evs) s.add(e); return s; }

class El { textContent = ""; children: El[] = []; attrs: Record<string, string> = {}; cls = new Set<string>(); tag: string;
  constructor(tag = "#text") { this.tag = tag; }
  className = ""; set href(v: string) { this.attrs.href = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  querySelector(sel: string) { return this.children.find((c) => c.tag === sel) ?? null; }
  classList = { toggle: (c: string, on: boolean) => { on ? this.cls.add(c) : this.cls.delete(c); } };
  append(...c: El[]) { this.children.push(...c); }
  replaceChildren(...c: El[]) { this.children = c; this.textContent = c.map((n) => n.textContent).join(""); } }
function page() {
  const ids: Record<string, El> = {};
  const doc = { getElementById: (id: string) => (ids[id] ??= new El(id)), createElement: (t: string) => new El(t), createTextNode: (t: string) => Object.assign(new El(), { textContent: t }), createDocumentFragment: () => new El() } as unknown as Document;
  return { doc, ids };
}
const clock = (ms: number) => ({ nowMs: ms, now: NOW, when: (x: number) => `t=${x}`, status: "Live", budget: () => "$0.00" });
const rows = (ids: Record<string, El>) => ids["challenge-sellers"]!.children[0]?.children ?? [];
// name -> state cell ("" while a placeholder)
const table = (ids: Record<string, El>) => Object.fromEntries(rows(ids).map((r) => [r.children[0]!.textContent, r.children[3]!.textContent]));
const ch = () => ["wss://a", "wss://b", "wss://c"].map((url) => ({ role: "challenge", url, connected: true, historyComplete: true, error: null }));
const relay = (o = {}) => ({ connected: true, authenticated: true, historyComplete: false, windowComplete: false, windowSince: 0, error: null, quorum: { required: 2, complete: 3, total: 3 }, relays: [{ role: "trade", url: "wss://t", connected: true, historyComplete: false, error: null }, ...ch()], ...o });
const roster = () => relay();
const live = (o = {}) => relay({ windowComplete: true, windowSince: 900, ...o });
const snap = (s: unknown) => parseSnapshot(s) as Snapshot;
const fresh = { wasLive: false, readyAt: null };

test("hide-left: the shared snapshot still carries every left seller (runner and CLI unchanged)", () => {
  const s = store().snapshot(buyer, NOW);
  assert.deepEqual(s.sellers.filter((a: { state: string }) => a.state === "left").map((a: { name: string }) => a.name).sort(), ["expired", "leaver", "oldhand"]);
});

test("hide-left: live, a left member with no job is hidden and not counted", () => {
  const { doc, ids } = page();
  paint(doc, snap(store().snapshot(buyer, NOW)), live(), fresh, clock(0));
  assert.ok(!("leaver" in table(ids)), "left without working: not listed");
  assert.equal(ids["seller-summary"]!.textContent, "2 agents · 1 working now");
});

test("hide-left: live, a left member with a job in the shown run is listed as Left; an expired, unfinished job counts", () => {
  const { doc, ids } = page();
  paint(doc, snap(store().snapshot(buyer, NOW)), live(), fresh, clock(0));
  assert.deepEqual(table(ids), { worker: "Working now", expired: "Left" });
  const row = rows(ids).find((r) => r.children[0]!.textContent === "expired")!;
  assert.equal(row.className, "off");
});

test("hide-left: a left member who only had jobs in other runs is hidden for this run, and listed for the run they worked in", () => {
  const { doc, ids } = page();
  paint(doc, snap(store().snapshot(buyer, NOW)), live(), fresh, clock(0));
  assert.ok(!("oldhand" in table(ids)), "worked only in the old run: not listed for this one");
  // The same rule per run: the old run (shown through the projection's run selection) lists oldhand.
  const past = page();
  paint(past.doc, snap(project(events, buyer, NOW, oldRun as never)), live(), fresh, clock(0));
  assert.equal(table(past.ids).oldhand, "Left", "listed for the run they worked in");
  assert.ok(!("leaver" in table(past.ids)) && !("expired" in table(past.ids)), "no job in the old run: not listed");
  assert.equal(past.ids["seller-summary"]!.textContent, "2 agents · 0 working now");
});

test("hide-left: roster state hides every left member and the count excludes them", () => {
  const { doc, ids } = page();
  paint(doc, snap(store().snapshot(buyer, NOW)), roster(), fresh, clock(0));
  assert.deepEqual(Object.keys(table(ids)), ["worker"], "only the member still in; a worker who left waits for the trade data");
  assert.equal(table(ids).worker, "", "job state is a placeholder");
  assert.equal(ids["seller-summary"]!.textContent, "1 agent");
  assert.ok(!ids["challenge-sellers"]!.textContent.includes("Left"));
});

test("hide-left: roster -> live adds the worked leaver once; the 15s poll never adds, drops or relabels a row", () => {
  const { doc, ids } = page();
  const s = snap(store().snapshot(buyer, NOW));
  let t = paint(doc, s, roster(), fresh, clock(0));
  t = paint(doc, s, roster(), t, clock(1000));
  assert.deepEqual(table(ids), { worker: "" });
  t = paint(doc, s, live(), t, clock(2000));
  const seen = new Set<string>();
  for (let ms = 3000; ms <= 47000; ms += 1000) {
    // Poll rounds: windowComplete/historyComplete drop for about a second while still connected.
    const poll = ms % 15000 < 2000;
    t = paint(doc, s, poll ? roster() : live({ historyComplete: ms > 20000 }), t, clock(ms));
    seen.add(`${JSON.stringify(table(ids))}|${ids["seller-summary"]!.textContent}`);
  }
  assert.deepEqual([...seen], ['{"expired":"Left","worker":"Working now"}|2 agents · 1 working now'], "one steady table (rows in npub order) across every tick");
});
