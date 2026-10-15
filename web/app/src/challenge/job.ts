// The job description is the challenge event's content (signed by the challenge buyer).
// Format, kept deliberately small: an optional first line "# Headline", then paragraphs
// separated by blank lines; a paragraph whose lines all start with "- " is a list.
// Rendering only ever uses textContent and https-only anchors: no HTML from the event.
export type Block = { kind: "p"; text: string } | { kind: "ul"; items: string[] };
export interface Job { headline: string | null; blocks: Block[] }

export function parseJob(content: string): Job {
  const lines = content.replace(/\r\n?/g, "\n").trim().split("\n");
  let headline: string | null = null;
  if (lines[0]?.startsWith("# ")) headline = lines.shift()!.slice(2).trim() || null;
  const blocks: Block[] = [];
  for (const chunk of lines.join("\n").split(/\n\s*\n/)) {
    const rows = chunk.split("\n").map((l) => l.trim()).filter(Boolean);
    if (!rows.length) continue;
    if (rows.every((r) => r.startsWith("- "))) blocks.push({ kind: "ul", items: rows.map((r) => r.slice(2).trim()) });
    else blocks.push({ kind: "p", text: rows.join(" ") });
  }
  return { headline, blocks };
}

const URL_RE = /https:\/\/[^\s<>"')\]]+/g;
function inline(doc: Document, parent: HTMLElement, text: string): void {
  let at = 0;
  for (const m of text.matchAll(URL_RE)) {
    const url = m[0].replace(/[.,;:!?]+$/, "");
    if (m.index! > at) parent.append(doc.createTextNode(text.slice(at, m.index)));
    const a = doc.createElement("a");
    a.href = url; a.textContent = url.replace(/^https:\/\//, ""); a.target = "_blank"; a.rel = "noopener";
    parent.append(a);
    at = m.index! + url.length;
  }
  if (at < text.length) parent.append(doc.createTextNode(text.slice(at)));
}

export function renderJob(doc: Document, content: string): void {
  const job = parseJob(content);
  doc.getElementById("job-title")!.textContent = job.headline ?? "The job.";
  const body = doc.getElementById("job-body")!;
  const fragment = doc.createDocumentFragment();
  for (const b of job.blocks) {
    if (b.kind === "p") { const p = doc.createElement("p"); inline(doc, p, b.text); fragment.append(p); }
    else {
      const ul = doc.createElement("ul");
      for (const item of b.items) { const li = doc.createElement("li"); inline(doc, li, item); ul.append(li); }
      fragment.append(ul);
    }
  }
  body.replaceChildren(fragment);
}
