import assert from "node:assert/strict";
import test from "node:test";
import { AGENTS_ANCHOR, FRESH_S, isRunning, LOADING, renderStart, startFact } from "../src/challenge/hero.js";

const when = (secs: number) => `t=${secs}`;
const run = { starts: 1000, ends: 2000, status: "in_progress" };
const ready = 0; // relay fully ready this second
const starts = { label: "Work starts", value: "t=1000", href: null };

test("before the work window the hero keeps 'Work starts' and the time", () => {
  assert.deepEqual(startFact(run, 999, when, ready), starts);
});

test("in_progress inside the window with fresh, ready data says 'Running now' and links the live agents list", () => {
  for (const now of [1000, 1500, 1999]) {
    const fact = startFact(run, now, when, ready);
    assert.equal(fact.label, "Running now", `at ${now}`);
    assert.equal(fact.href, AGENTS_ANCHOR);
  }
  assert.equal(startFact(run, 1500, when, FRESH_S).label, "Running now", "ready within FRESH_S");
  assert.equal(AGENTS_ANCHOR, "#agents");
});

test("with ready data and no challenge published, the start is to be announced; not ready, still the placeholder", () => {
  assert.deepEqual(startFact(null, 1500, when, ready), { label: "Work starts", value: "To be announced", href: null });
  assert.deepEqual(startFact(null, 1500, when, null), LOADING);
});

test("a scheduled day inside its window is not running (the coordinator does not assign work)", () => {
  assert.deepEqual(startFact({ ...run, status: "scheduled" }, 1500, when, ready), starts);
  assert.equal(isRunning({ ...run, status: "scheduled" }, 1500, ready), false);
});

test("until the challenge data is ready (or once it is stale) the fact is a neutral placeholder, never 'Work starts' or 'Running now'", () => {
  for (const c of [run, { ...run, status: "scheduled" }, { ...run, status: "completed" }])
    for (const now of [999, 1500, 2500]) {
      assert.deepEqual(startFact(c, now, when, null), LOADING, `${c.status} at ${now}, not ready`);
      assert.deepEqual(startFact(c, now, when, FRESH_S + 0.5), LOADING, `${c.status} at ${now}, stale`);
    }
  assert.equal(LOADING.label, "Loading…");
  assert.equal(LOADING.href, null);
});

test("a finished or cancelled run never says 'Running now'; it shows what it showed before", () => {
  assert.deepEqual(startFact(run, 2000, when, ready), starts, "window over");
  assert.deepEqual(startFact({ ...run, status: "completed" }, 1500, when, ready), starts, "completed early");
  assert.deepEqual(startFact({ ...run, status: "cancelled" }, 1500, when, ready), starts, "cancelled");
  assert.equal(isRunning({ ...run, status: "cancelled" }, 1500, ready), false);
});

// Minimal DOM double: text only ever reaches textContent; the link is a real <a href>.
class Node {
  textContent = ""; children: Node[] = []; attrs: Record<string, string> = {}; tag: string;
  constructor(tag = "#text") { this.tag = tag; }
  set href(v: string) { this.attrs.href = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  querySelector(sel: string) { return this.children.find((c) => c.tag === sel) ?? null; }
  replaceChildren(...c: Node[]) { this.children = c; this.textContent = c.map((n) => n.textContent).join(""); }
}
test("renderStart swaps the label and puts an in-page link in the value, then back", () => {
  const label = new Node("dt"), dd = new Node("dd");
  const doc = {
    getElementById: (id: string) => (id === "challenge-start-label" ? label : dd),
    createElement: (tag: string) => new Node(tag),
    createTextNode: (t: string) => Object.assign(new Node(), { textContent: t }),
  } as unknown as Document;
  renderStart(doc, startFact(run, 1500, when, ready));
  assert.equal(label.textContent, "Running now");
  const a = dd.querySelector("a")!;
  assert.equal(a.getAttribute("href"), "#agents");
  assert.ok(a.textContent.length > 0);
  renderStart(doc, startFact(run, 2500, when, ready));
  assert.equal(label.textContent, "Work starts");
  assert.equal(dd.querySelector("a"), null);
  assert.equal(dd.textContent, "t=1000");
});
