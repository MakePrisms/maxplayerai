import assert from "node:assert/strict";
import test from "node:test";
import { AGENTS_ANCHOR, isRunning, renderStart, startFact } from "../src/challenge/hero.js";

const when = (secs: number) => `t=${secs}`;
const run = { starts: 1000, ends: 2000, status: "in_progress" };

test("before the work window the hero keeps 'Work starts' and the time", () => {
  assert.deepEqual(startFact({ ...run, status: "scheduled" }, 999, when), { label: "Work starts", value: "t=1000", href: null });
});

test("once the window opens the hero says 'Running now' and links the live agents list", () => {
  for (const status of ["scheduled", "in_progress"]) {
    for (const now of [1000, 1500, 1999]) {
      const fact = startFact({ ...run, status }, now, when);
      assert.equal(fact.label, "Running now", `${status} at ${now}`);
      assert.equal(fact.href, AGENTS_ANCHOR);
    }
  }
  assert.equal(AGENTS_ANCHOR, "#agents");
});

test("a finished or cancelled run never says 'Running now'; it shows what it showed before", () => {
  const ended = { label: "Work starts", value: "t=1000", href: null };
  assert.deepEqual(startFact(run, 2000, when), ended, "window over");
  assert.deepEqual(startFact({ ...run, status: "completed" }, 1500, when), ended, "completed early");
  assert.deepEqual(startFact({ ...run, status: "cancelled" }, 1500, when), ended, "cancelled");
  assert.equal(isRunning({ ...run, status: "cancelled" }, 1500), false);
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
  renderStart(doc, startFact(run, 1500, when));
  assert.equal(label.textContent, "Running now");
  const a = dd.querySelector("a")!;
  assert.equal(a.getAttribute("href"), "#agents");
  assert.ok(a.textContent.length > 0);
  renderStart(doc, startFact(run, 2500, when));
  assert.equal(label.textContent, "Work starts");
  assert.equal(dd.querySelector("a"), null);
  assert.equal(dd.textContent, "t=1000");
});
