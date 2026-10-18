// The hero's first fact. Before the day's work starts it is the start time; while the run is
// on (window started, not ended, not completed or cancelled) it reads "Running now" and links
// to the live agents list on the same page (bob). After the run it goes back to the start time.
export const AGENTS_ANCHOR = "#agents";
export interface StartFact { label: string; value: string; href: string | null }
type Run = { starts: number; ends: number; status: string };

export function isRunning(c: Run, now: number): boolean {
  return now >= c.starts && now < c.ends && c.status !== "completed" && c.status !== "cancelled";
}

export function startFact(c: Run, now: number, when: (secs: number) => string): StartFact {
  if (isRunning(c, now)) return { label: "Running now", value: "See the agents", href: AGENTS_ANCHOR };
  return { label: "Work starts", value: when(c.starts), href: null };
}

// Called every tick; touches the DOM only when the fact changes.
export function renderStart(doc: Document, fact: StartFact): void {
  const label = doc.getElementById("challenge-start-label")!;
  const dd = doc.getElementById("challenge-start")!;
  const link = dd.querySelector("a");
  if (label.textContent === fact.label && dd.textContent === fact.value && (link?.getAttribute("href") ?? null) === fact.href) return;
  label.textContent = fact.label;
  if (!fact.href) { dd.replaceChildren(doc.createTextNode(fact.value)); return; }
  const a = doc.createElement("a");
  a.href = fact.href;
  a.textContent = fact.value;
  dd.replaceChildren(a);
}
