import assert from "node:assert/strict";
import test from "node:test";
import { parseJob, renderJob } from "../src/challenge/job.js";

test("job content: optional headline, paragraphs and lists", () => {
  assert.deepEqual(parseJob("# Crack it.\n\nFirst line\nwraps.\n\n- one\n- two\n\nEnd"), {
    headline: "Crack it.",
    blocks: [{ kind: "p", text: "First line wraps." }, { kind: "ul", items: ["one", "two"] }, { kind: "p", text: "End" }],
  });
  assert.deepEqual(parseJob("Just a goal."), { headline: null, blocks: [{ kind: "p", text: "Just a goal." }] });
});

// Minimal DOM double: event text must only reach textContent; only https URLs become links.
class Node { textContent = ""; href = ""; target = ""; rel = ""; children: Node[] = []; constructor(public tag: string) {} append(...c: Node[]) { this.children.push(...c); } replaceChildren(...c: Node[]) { this.children = c; } }
test("job rendering never interprets HTML and links only https", () => {
  const title = new Node("h2"), body = new Node("div");
  const doc = { getElementById: (id: string) => (id === "job-title" ? title : body), createElement: (t: string) => new Node(t), createDocumentFragment: () => new Node("#f"),
    createTextNode: (t: string) => Object.assign(new Node("#text"), { textContent: t }) };
  renderJob(doc as unknown as Document, "<b>x</b> see https://example.com/a. and javascript:alert(1)");
  assert.equal(title.textContent, "The job.");
  const p = body.children[0]!.children[0]!; // fragment → p
  assert.deepEqual(p.children.map((c) => [c.tag, c.textContent]), [["#text", "<b>x</b> see "], ["a", "example.com/a"], ["#text", ". and javascript:alert(1)"]]);
  assert.equal(p.children[1]!.href, "https://example.com/a");
});
