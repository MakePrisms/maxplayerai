// /swarm fast load (fix 1): the agents rows show once the challenge data is ready (the hero's gate),
// with placeholder job states until the run's trade data is complete. Never "Working now" early.
import assert from "node:assert/strict";
import test from "node:test";
import { paint } from "../src/challenge/page.js";
import type { Snapshot } from "../src/challenge/live.js";
// Literal (not imported) so this file runs, and fails at its assertions, against main too.
const ROSTER_ONLY = "Loading what each agent is doing…";
import { npubEncode } from "../src/trade/nip19.js";

const buyer = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
const seller = { pubkey: buyer, npub: npubEncode(buyer), name: "alice", minutesOffered: 60, minutesUsed: 32, minutesRemaining: 28, state: "working", eligible: true, decision: "accepted", currentJobStart: 1000 };
const leaver = { ...seller, pubkey: "b".repeat(64), npub: npubEncode("b".repeat(64)), name: "bob", state: "left", currentJobStart: null };
const s: Snapshot = { asOf: 1500, challenge: { id: "t", buyer, buyerNpub: npubEncode(buyer), title: "T", goal: "G", starts: 1000, ends: 2000, status: "in_progress", result: "" }, sellers: [seller, leaver], jobs: [{ id: "j", seller: buyer, start: 1000, deadline: 4600, end: null, running: true, usedMinutes: 32 }], budget: { totalSats: 1, committedSats: 0, paidSats: 0, unknownAmounts: 0, estimated: true, note: "" }, updates: [] };

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
const clock = (ms: number) => ({ nowMs: ms, now: 1500, when: (x: number) => `t=${x}`, status: "Live", budget: () => "$7.00" });
const all = (e: El): string => e.textContent + e.children.map(all).join("");
// The double's replaceChildren(fragment) keeps the fragment as the one child: rows are inside it.
const rows = (ids: Record<string, El>) => ids["challenge-sellers"]!.children[0]?.children ?? [];
const stateCell = (row: El) => row.children[3]!;
const placeholder = (row: El) => stateCell(row).textContent === "" && stateCell(row).children[0]?.className === "skel c-skel-state";
const ch = (o = {}) => ["wss://a", "wss://b", "wss://c"].map((url) => ({ role: "challenge", url, connected: true, historyComplete: true, error: null, ...o }));
const tradeRelay = (o = {}) => ({ role: "trade", url: "wss://t", connected: true, historyComplete: false, error: null, ...o });
// Challenge data complete, trade relay connected and authenticated but its window not read yet.
const roster = (o = {}, t = {}) => ({ connected: true, authenticated: true, historyComplete: false, windowComplete: false, windowSince: 0, error: null, quorum: { required: 2, complete: 3, total: 3 }, relays: [tradeRelay(t), ...ch()], ...o });
// The trade window (starting at or before the run) is read: the run's trade data is complete.
const windowDone = (o = {}) => roster({ windowComplete: true, windowSince: 900, ...o });

test("fastload: rows show with the hero, before the trade data; job states are placeholders, never 'Working now'", () => {
  const { doc, ids } = page();
  paint(doc, s, roster(), { wasLive: false, readyAt: null }, clock(0));
  assert.equal(ids["challenge-start-label"]!.textContent, "Running now", "hero");
  assert.equal(rows(ids).length, 2, "rows with the hero");
  assert.equal(rows(ids)[0]!.children[0]!.textContent, "alice");
  assert.ok(placeholder(rows(ids)[0]!), "the working seller's state is a placeholder");
  assert.equal(all(rows(ids)[0]!.children[1]!), " / 60 min", "minutes used is a placeholder; offered shows");
  assert.equal(rows(ids)[0]!.children[1]!.children[0]!.className, "skel c-skel-used");
  assert.equal(stateCell(rows(ids)[1]!).textContent, "Left", "a member who left (their own membership) shows");
  assert.equal(ids["seller-summary"]!.textContent, "2 agents", "count without 'working now'");
  assert.ok(!all(ids["challenge-sellers"]!).includes("Working now"));
  assert.ok(ids["agents-eyebrow"]!.cls.has("is-off"), "no Live eyebrow");
  assert.equal(ids["connection-status"]!.textContent, ROSTER_ONLY);
  assert.equal(ids["challenge-sellers"]!.getAttribute("aria-busy"), "true");
});

test("fastload: never 'Working now' before the run's trade data is complete, whatever else is ready", () => {
  const notReady = [
    roster(),
    roster({ authenticated: false }),
    roster({ historyComplete: false, windowComplete: true, windowSince: 1001 }), // window starts after the run: doesn't cover it
    roster({ windowComplete: true, windowSince: 1001, relays: [tradeRelay({ historyComplete: false }), ...ch()] }),
  ];
  for (const [i, r] of notReady.entries()) {
    const { doc, ids } = page();
    let t = { wasLive: false, readyAt: null as number | null };
    for (const ms of [0, 1000, 2000]) t = paint(doc, s, r, t, clock(ms));
    assert.equal(rows(ids).length, 2, `case ${i}`);
    assert.ok(!all(ids["challenge-sellers"]!).includes("Working now"), `case ${i}`);
    assert.ok(placeholder(rows(ids)[0]!), `case ${i}`);
    assert.ok(!ids["seller-summary"]!.textContent.includes("working now"), `case ${i}`);
    assert.equal(t.wasLive, false, `case ${i}: not latched`);
  }
});

test("fastload: the trade window completes -> states and count; the 15s poll after that never flickers", () => {
  const { doc, ids } = page();
  let t = paint(doc, s, roster(), { wasLive: false, readyAt: null }, clock(0));
  t = paint(doc, s, windowDone(), t, clock(1000));
  assert.equal(stateCell(rows(ids)[0]!).textContent, "Working now");
  assert.equal(ids["seller-summary"]!.textContent, "2 agents · 1 working now");
  assert.equal(ids["connection-status"]!.textContent, "Live");
  assert.ok(!ids["agents-eyebrow"]!.cls.has("is-off"));
  // Backfill and poll rounds: windowComplete/historyComplete drop briefly while still connected.
  const seen = new Set<string>();
  for (let ms = 2000; ms <= 40000; ms += 1000) {
    const poll = ms % 15000 < 2000;
    t = paint(doc, s, poll ? roster({ windowSince: 900 }) : windowDone({ historyComplete: ms > 20000 }), t, clock(ms));
    seen.add(`${stateCell(rows(ids)[0]!).textContent}|${ids["seller-summary"]!.textContent}|${ids["connection-status"]!.textContent}|${ids["agents-eyebrow"]!.cls.has("is-off")}`);
  }
  assert.deepEqual([...seen], ["Working now|2 agents · 1 working now|Live|false"], "one steady state across every tick");
});

test("fastload: placeholder rows are not rebuilt every tick (no restarted sheen), and are once the roster changes", () => {
  const { doc, ids } = page();
  let t = paint(doc, s, roster(), { wasLive: false, readyAt: null }, clock(0));
  const first = rows(ids)[0];
  for (const ms of [1000, 2000, 3000]) t = paint(doc, s, roster(), t, clock(ms));
  assert.equal(rows(ids)[0], first, "same row element");
  paint(doc, { ...s, sellers: [{ ...seller, name: "alice2" }, leaver] }, roster(), t, clock(4000));
  assert.notEqual(rows(ids)[0], first);
  assert.equal(rows(ids)[0]!.children[0]!.textContent, "alice2");
});

test("fastload: a challenge relay going down blanks the rows (as it does the hero); a trade reconnect waits for fresh trade data", () => {
  const { doc, ids } = page();
  let t = paint(doc, s, windowDone(), { wasLive: false, readyAt: null }, clock(0));
  assert.equal(stateCell(rows(ids)[0]!).textContent, "Working now");
  // Below the challenge quorum: no rows, no counts, hero back to skeletons.
  const down = roster({ windowComplete: true, windowSince: 900, relays: [tradeRelay(), ...ch().map((r, i) => (i ? { ...r, connected: false } : r))] });
  t = paint(doc, s, down, t, clock(1000));
  assert.equal(rows(ids).length, 0);
  assert.equal(ids["seller-summary"]!.textContent, "");
  assert.equal(ids["connection-status"]!.textContent, "Updating…");
  assert.equal(ids["challenge-facts"]!.getAttribute("aria-busy"), "true");
  // Back up and ready.
  t = paint(doc, s, windowDone(), t, clock(2000));
  assert.equal(stateCell(rows(ids)[0]!).textContent, "Working now");
  // The trade relay drops: the latch clears, job states go back to placeholders (challenge data still ready).
  t = paint(doc, s, roster({ connected: false, authenticated: false, windowComplete: false }, { connected: false }), t, clock(3000));
  assert.equal(t.wasLive, false);
  assert.ok(placeholder(rows(ids)[0]!));
  // Reconnected, window not read yet: still placeholders; read: states again.
  t = paint(doc, s, roster(), t, clock(4000));
  assert.ok(placeholder(rows(ids)[0]!), "reconnect waits for fresh trade history");
  assert.ok(!all(ids["challenge-sellers"]!).includes("Working now"));
  paint(doc, s, windowDone(), t, clock(5000));
  assert.equal(stateCell(rows(ids)[0]!).textContent, "Working now");
});

test("fastload: stale challenge data (no fresh ready read for over 5s) drops the placeholder rows too", () => {
  const { doc, ids } = page();
  const catching = roster({ relays: [tradeRelay(), ...ch().map((r) => ({ ...r, historyComplete: false }))] });
  let t = paint(doc, s, roster(), { wasLive: false, readyAt: null }, clock(0));
  t = paint(doc, s, catching, t, clock(5000));
  assert.equal(rows(ids).length, 2, "held through a poll");
  paint(doc, s, catching, t, clock(6000));
  assert.equal(rows(ids).length, 0, "stale: blank");
});
