// The hero's first fact. Before the day's work starts it is the start time; while the run is
// on it reads "Running now" and links to the live agents list on the same page (bob). After the
// run: "Work ended" and the end time once the job is over (completed, cancelled or past its window; Bob,
// 2026-11-21), else "Work started" and the start time (started but not on, e.g. a stale view).
// A job closed early ends when the buyer closed it: updatedAt, the shown job event's created_at. Kind 30341 has no
// ended_at, so a later republish of a closed job moves that time later (never past ends); the date can only err late.
//
// "On" matches the swarm engine: status in_progress (a scheduled day is not on) and the
// half-open window starts <= now < ends, read from swarm data that was ready within
// FRESH_S (see observe in live.ts). Residual: the window follows the browser clock.
//
// Both facts (start and budget, labels and values) are skeletons until everything in them is
// known, then revealed together (bob: no "Work starts" flash, no half-loaded block). Known =
// swarm data fresh, and the budget in dollars; if the BTC-USD rate isn't in SPOT_WAIT_MS
// after load, the budget shows in sats and switches to dollars when the rate lands. A routine
// poll keeps the facts (readiness is held through it). Stale data or the swarm reader down
// puts the whole block back to skeletons, so "Running now" never shows from stale data and the
// page never shows "Work starts" for a run that is on.
export const AGENTS_ANCHOR = "#agents";
export const FRESH_S = 5;
export const SPOT_WAIT_MS = 4000;
export interface StartFact { label: string; value: string; href: string | null }
type Run = { starts: number; ends: number; status: string; updatedAt?: number };

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

// No swarm job published yet (data ready, nothing found): the start is to be announced.
export function startFact(c: Run | null, now: number, when: (secs: number) => string, readyAgeS: number | null): StartFact {
  if (!c) return { label: "Work starts", value: "To be announced", href: null };
  if (isRunning(c, now, readyAgeS)) return { label: "Running now", value: "See the agents", href: AGENTS_ANCHOR };
  const closed = c.status === "completed" || c.status === "cancelled";
  const closedAt = closed && c.updatedAt !== undefined && Number.isFinite(c.updatedAt) ? c.updatedAt : null;
  // Called off before it began (the daemon lets a buyer cancel a scheduled job early; review of #1361, r3 #2).
  if (c.status === "cancelled" && (closedAt !== null ? closedAt < c.starts : now < c.starts))
    return { label: "Cancelled", value: when(closedAt ?? c.starts), href: null };
  if (closed || now >= c.ends) {
    const end = closedAt !== null ? Math.max(c.starts, Math.min(c.ends, closedAt)) : c.ends;
    return { label: "Work ended", value: when(end), href: null };
  }
  return { label: now >= c.starts ? "Work started" : "Work starts", value: when(c.starts), href: null };
}

// Called every tick; touches the DOM only when the fact changes.
export function renderStart(doc: Document, fact: StartFact): void {
  const label = doc.getElementById("swarm-start-label")!;
  const dd = doc.getElementById("swarm-start")!;
  const link = dd.querySelector("a");
  if (label.textContent === fact.label && dd.textContent === fact.value && (link?.getAttribute("href") ?? null) === fact.href) return;
  label.textContent = fact.label;
  if (!fact.href) { dd.replaceChildren(doc.createTextNode(fact.value)); return; }
  const a = doc.createElement("a");
  a.href = fact.href;
  a.textContent = fact.value;
  dd.replaceChildren(a);
}

// Skeleton spans reuse styles.css .skel (sheen, reduced motion); swarm.css sizes them to
// one line of each fact's font so the reveal can't shift layout.
function skeleton(doc: Document, el: HTMLElement, cls: string): void {
  const span = doc.createElement("span");
  span.className = `skel ${cls}`;
  el.replaceChildren(span);
}
export function renderFacts(doc: Document, f: Facts | null): void {
  const dl = doc.getElementById("swarm-facts")!;
  const busy = f === null;
  if (busy) {
    if (dl.getAttribute("aria-busy") === "true") return;
    skeleton(doc, doc.getElementById("swarm-start-label")!, "c-skel-dt");
    skeleton(doc, doc.getElementById("swarm-start")!, "c-skel-dd");
    skeleton(doc, doc.getElementById("swarm-budget-label")!, "c-skel-dt");
    skeleton(doc, doc.getElementById("swarm-budget")!, "c-skel-dd");
    dl.setAttribute("aria-busy", "true");
    return;
  }
  renderStart(doc, f.start);
  const label = doc.getElementById("swarm-budget-label")!, value = doc.getElementById("swarm-budget")!;
  if (label.textContent !== "Budget") label.replaceChildren(doc.createTextNode("Budget"));
  if (value.textContent !== f.budget) value.replaceChildren(doc.createTextNode(f.budget));
  dl.setAttribute("aria-busy", "false");
}
