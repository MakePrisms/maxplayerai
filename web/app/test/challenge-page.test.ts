import assert from "node:assert/strict";
import test from "node:test";
import { paint } from "../src/challenge/page.js";
import type { Snapshot } from "../src/challenge/live.js";
import { npubEncode } from "../src/trade/nip19.js";

const buyer = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
const seller = { pubkey: buyer, npub: npubEncode(buyer), name: "a", minutesOffered: 60, minutesUsed: 32, minutesRemaining: 28, state: "working", eligible: true, decision: "accepted", currentJobStart: 1000 };
const s: Snapshot = { asOf: 1500, challenge: { id: "t", buyer, buyerNpub: npubEncode(buyer), title: "T", goal: "G", starts: 1000, ends: 2000, status: "in_progress", result: "" }, sellers: [seller], jobs: [{ id: "j", seller: buyer, start: 1000, deadline: 4600, end: null, running: true, usedMinutes: 32 }], budget: { totalSats: 1, committedSats: 0, paidSats: 0, unknownAmounts: 0, estimated: true, note: "" }, updates: [] };

class El { textContent = ""; children: El[] = []; attrs: Record<string, string> = {}; cls = new Set<string>(); tag: string;
  constructor(tag = "#text") { this.tag = tag; }
  className = ""; set href(v: string) { this.attrs.href = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  querySelector(sel: string) { return this.children.find((c) => c.tag === sel) ?? null; }
  classList = { toggle: (c: string, on: boolean) => { on ? this.cls.add(c) : this.cls.delete(c); } };
  append(...c: El[]) { this.children.push(...c); }
  replaceChildren(...c: El[]) { this.children = c; this.textContent = c.map((n) => n.textContent).join(""); } }
function page() {
  const ids: Record<string, El> = {};
  const doc = { getElementById: (id: string) => (ids[id] ??= new El(id)), createElement: (t: string) => new El(t), createTextNode: (t: string) => Object.assign(new El(), { textContent: t }), createDocumentFragment: () => new El() } as unknown as Document;
  return { doc, ids };
}
const live = { connected: true, authenticated: true, historyComplete: true, error: null };
const clock = (ms: number) => ({ nowMs: ms, now: 1500, when: (x: number) => `t=${x}`, status: "Live" });

test("page: loading -> live -> 15s poll stays live -> disconnect shows Updating and drops rows and Running now", () => {
  const { doc, ids } = page();
  let t = paint(doc, s, { ...live, historyComplete: false }, { wasLive: false, readyAt: null }, clock(0));
  assert.equal(ids["connection-status"]!.textContent, "Updating…");
  assert.equal(ids["challenge-start-label"]!.textContent, "Loading…");
  assert.ok(ids["agents-eyebrow"]!.cls.has("is-off"));
  assert.equal(ids["challenge-sellers"]!.children.length, 0);

  t = paint(doc, s, live, t, clock(1000));
  assert.equal(ids["connection-status"]!.textContent, "Live");
  assert.equal(ids["challenge-start-label"]!.textContent, "Running now");
  assert.ok(!ids["agents-eyebrow"]!.cls.has("is-off"));
  assert.equal(ids["seller-summary"]!.textContent, "1 agent · 1 working now");

  // Every tick of a routine poll round (history briefly incomplete, still connected).
  for (const ms of [2000, 3000, 4000]) {
    t = paint(doc, s, { ...live, historyComplete: false }, t, clock(ms));
    assert.equal(ids["connection-status"]!.textContent, "Live", `poll at ${ms}`);
    assert.equal(ids["challenge-start-label"]!.textContent, "Running now", `poll at ${ms}`);
    assert.equal(ids["seller-summary"]!.textContent, "1 agent · 1 working now");
  }
  t = paint(doc, s, live, t, clock(5000));

  t = paint(doc, s, { ...live, connected: false, authenticated: false, historyComplete: false }, t, clock(6000));
  assert.equal(ids["connection-status"]!.textContent, "Updating…");
  assert.equal(ids["challenge-start-label"]!.textContent, "Loading…");
  assert.ok(ids["agents-eyebrow"]!.cls.has("is-off"));
  assert.equal(ids["challenge-sellers"]!.children.length, 0);
  assert.equal(ids["seller-summary"]!.textContent, "");
});

test("page: stuck incomplete after being live keeps Live but drops Running now once the ready read is stale", () => {
  const { doc, ids } = page();
  let t = paint(doc, s, live, { wasLive: false, readyAt: null }, clock(0));
  t = paint(doc, s, { ...live, historyComplete: false }, t, clock(5000));
  assert.equal(ids["challenge-start-label"]!.textContent, "Running now");
  paint(doc, s, { ...live, historyComplete: false }, t, clock(6000));
  assert.equal(ids["challenge-start-label"]!.textContent, "Loading…");
  assert.equal(ids["connection-status"]!.textContent, "Live");
});

test("page: after a disconnect, the reconnect waits for a fresh complete history before Live and the rows return", () => {
  const { doc, ids } = page();
  let t = paint(doc, s, live, { wasLive: false, readyAt: null }, clock(0));
  assert.equal(ids["connection-status"]!.textContent, "Live");
  t = paint(doc, s, { ...live, connected: false, authenticated: false, historyComplete: false }, t, clock(1000));
  assert.equal(ids["connection-status"]!.textContent, "Updating…");
  // Reconnected, catch-up still running: not live yet, no pre-drop rows.
  for (const ms of [2000, 3000]) {
    t = paint(doc, s, { ...live, historyComplete: false }, t, clock(ms));
    assert.equal(ids["connection-status"]!.textContent, "Updating…", `catch-up at ${ms}`);
    assert.ok(ids["agents-eyebrow"]!.cls.has("is-off"));
    assert.equal(ids["challenge-sellers"]!.children.length, 0);
    assert.equal(ids["seller-summary"]!.textContent, "");
    assert.equal(ids["challenge-start-label"]!.textContent, "Loading…");
  }
  t = paint(doc, s, live, t, clock(4000));
  assert.equal(ids["connection-status"]!.textContent, "Live");
  assert.ok(!ids["agents-eyebrow"]!.cls.has("is-off"));
  assert.equal(ids["seller-summary"]!.textContent, "1 agent · 1 working now");
  // And the latch is back: a routine poll afterwards stays Live.
  paint(doc, s, { ...live, historyComplete: false }, t, clock(5000));
  assert.equal(ids["connection-status"]!.textContent, "Live");
});

test("page: while a run is on, loading never shows 'Work starts', and 'Running now' doesn't wait for trade auth or trade history", () => {
  const { doc, ids } = page();
  const ch = (o = {}) => ["wss://a", "wss://b", "wss://c"].map((url) => ({ role: "challenge", url, connected: true, historyComplete: false, error: null, ...o }));
  const trade = { role: "trade", url: "wss://t", connected: true, historyComplete: false, error: null };
  const seen: string[] = [];
  // Every tick of the load: challenge relays catching up, then complete; trade unauthenticated throughout.
  let t = { wasLive: false, readyAt: null as number | null };
  const steps = [
    { relays: [{ ...trade, connected: false }, ...ch({ connected: false })], connected: false },
    { relays: [trade, ...ch()], connected: true },
    { relays: [trade, ...ch().map((r, i) => (i ? r : { ...r, historyComplete: true }))], connected: true },
    { relays: [trade, ...ch({ historyComplete: true })], connected: true },
  ];
  steps.forEach((st, i) => {
    t = paint(doc, s, { connected: st.connected, authenticated: false, historyComplete: false, error: null, relays: st.relays }, t, clock(i * 1000));
    seen.push(ids["challenge-start-label"]!.textContent);
  });
  assert.deepEqual(seen, ["Loading…", "Loading…", "Loading…", "Running now"]);
  assert.equal(ids["challenge-start"]!.querySelector("a")?.getAttribute("href"), "#agents");
  // The agents section still waits for the full history; only the hero is earlier.
  assert.equal(ids["connection-status"]!.textContent, "Updating…");
});
