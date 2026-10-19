// The hero's first fact. Before the day's work starts it is the start time; while the run is
// on it reads "Running now" and links to the live agents list on the same page (bob). After the
// run it goes back to the start time.
//
// "On" matches the swarm coordinator: status in_progress (a scheduled day is not on) and the
// half-open window starts <= now < ends, read from challenge data that was ready within
// FRESH_S (see observe in live.ts). Residual: the window follows the browser clock.
//
// Both facts (start and budget, labels and values) are skeletons until everything in them is
// known, then revealed together (bob: no "Work starts" flash, no half-loaded block). Known =
// challenge data fresh, and the budget in dollars; if the BTC-USD rate isn't in SPOT_WAIT_MS
// after load, the budget shows in sats and switches to dollars when the rate lands. A routine
// poll keeps the facts (readiness is held through it). Stale data or a challenge relay down
// puts the whole block back to skeletons, so "Running now" never shows from stale data and the
// page never shows "Work starts" for a run that is on.
export const AGENTS_ANCHOR = "#agents";
export const FRESH_S = 5;
export const SPOT_WAIT_MS = 4000;
export interface StartFact { label: string; value: string; href: string | null }
type Run = { starts: number; ends: number; status: string };

const fresh = (readyAgeS: number | null) => readyAgeS !== null && readyAgeS <= FRESH_S;
export interface Facts { start: StartFact; budget: string }
// usd: the dollar text, or null while the rate isn't in; waitedMs: time since page load.
export function budgetValue(sats: number, usd: string | null, waitedMs: number): string | null {
  if (usd !== null) return usd;
  return waitedMs >= SPOT_WAIT_MS ? `${sats.toLocaleString("en-US")} sats` : null;
}
// null = not everything is known yet: render both facts as skeletons.
export function heroFacts(c: Run | null, now: number, when: (secs: number) => string, readyAgeS: number | null, budget: string | null): Facts | null {
  if (!fresh(readyAgeS) || budget === null) return null;
  return { start: startFact(c, now, when, readyAgeS), budget };
}
export function isRunning(c: Run, now: number, readyAgeS: number | null): boolean {
  return c.status === "in_progress" && now >= c.starts && now < c.ends && fresh(readyAgeS);
}

// No challenge published yet (data ready, nothing found): the start is to be announced.
export function startFact(c: Run | null, now: number, when: (secs: number) => string, readyAgeS: number | null): StartFact {
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

// Skeleton spans reuse styles.css .skel (sheen, reduced motion); challenges.css sizes them to
// one line of each fact's font so the reveal can't shift layout.
function skeleton(doc: Document, el: HTMLElement, cls: string): void {
  const span = doc.createElement("span");
  span.className = `skel ${cls}`;
  el.replaceChildren(span);
}
export function renderFacts(doc: Document, f: Facts | null): void {
  const dl = doc.getElementById("challenge-facts")!;
  const busy = f === null;
  if (busy) {
    if (dl.getAttribute("aria-busy") === "true") return;
    skeleton(doc, doc.getElementById("challenge-start-label")!, "c-skel-dt");
    skeleton(doc, doc.getElementById("challenge-start")!, "c-skel-dd");
    skeleton(doc, doc.getElementById("challenge-budget-label")!, "c-skel-dt");
    skeleton(doc, doc.getElementById("challenge-budget")!, "c-skel-dd");
    dl.setAttribute("aria-busy", "true");
    return;
  }
  renderStart(doc, f.start);
  const label = doc.getElementById("challenge-budget-label")!, value = doc.getElementById("challenge-budget")!;
  if (label.textContent !== "Budget") label.replaceChildren(doc.createTextNode("Budget"));
  if (value.textContent !== f.budget) value.replaceChildren(doc.createTextNode(f.budget));
  dl.setAttribute("aria-busy", "false");
}
