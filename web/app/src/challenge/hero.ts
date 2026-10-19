// The hero's first fact. Before the day's work starts it is the start time; while the run is
// on it reads "Running now" and links to the live agents list on the same page (bob). After the
// run it goes back to the start time.
//
// "On" matches the swarm coordinator: status in_progress (a scheduled day is not on) and the
// half-open window starts <= now < ends, read from challenge data that was ready within
// FRESH_S (see observe in live.ts). Until then, and whenever that data is stale or a challenge
// relay is down, the fact is a neutral placeholder (bob: no "Work starts" flash before
// "Running now"), never a guess. Residual: the window follows the browser clock.
export const AGENTS_ANCHOR = "#agents";
export const FRESH_S = 5;
// Same text as the static HTML, so the first paint doesn't change it.
export const LOADING: StartFact = { label: "Loading…", value: "\u00a0", href: null };
export interface StartFact { label: string; value: string; href: string | null }
type Run = { starts: number; ends: number; status: string };

const fresh = (readyAgeS: number | null) => readyAgeS !== null && readyAgeS <= FRESH_S;
export function isRunning(c: Run, now: number, readyAgeS: number | null): boolean {
  return c.status === "in_progress" && now >= c.starts && now < c.ends && fresh(readyAgeS);
}

// No challenge published yet (data ready, nothing found): the start is to be announced.
export function startFact(c: Run | null, now: number, when: (secs: number) => string, readyAgeS: number | null): StartFact {
  if (!fresh(readyAgeS)) return LOADING;
  if (!c) return { label: "Work starts", value: "To be announced", href: null };
  if (isRunning(c, now, readyAgeS)) return { label: "Running now", value: "See the agents", href: AGENTS_ANCHOR };
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
