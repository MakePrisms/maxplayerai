// One paint of the live parts of /swarm: hero fact, agents eyebrow, rows, summary and status
// line. Called on every snapshot and every 1s tick with the reader's current relay status, so the
// readiness tracker is refreshed between the reader's 15s polls.
import { heroFacts, renderFacts } from "./hero.js";
import { connectionLine, observe, renderLiveState, renderSellers, ROSTER_ONLY, UPDATING, type Snapshot, type Tracker } from "./live.js";

// budget: the displayed budget for a sats amount, or null while it isn't known yet (see budgetValue).
// quiet: the status line under the members says nothing while it loads or is simply live (Bob, 2026-11-21: the grey
// placeholders already say it is loading); it speaks only for a problem, polling, or demo data.
export interface PaintClock { nowMs: number; now: number; when: (secs: number) => string; status: string; budget: (sats: number) => string | null; quiet?: boolean }

export function paint(doc: Document, s: Snapshot, relay: Snapshot["relay"], t: Tracker, clock: PaintClock): Tracker {
  const o = observe(t, relay, clock.nowMs, s.swarmJob);
  const budget = s.swarmJob ? clock.budget(s.budget!.totalSats) : "To be announced";
  renderFacts(doc, heroFacts(s.swarmJob, clock.now, clock.when, o.readyAgeS, budget));
  renderLiveState(doc, o.state);
  // Rows show once the swarm data is ready (o.state "roster", the hero's gate); job states and
  // the working count once the run's trade data is complete too ("live"). See live.ts.
  renderSellers(doc, s, clock.now, o.state === "live" ? "live" : o.state === "roster" ? "roster" : "loading");
  const line = o.state === "roster" ? ROSTER_ONLY : o.state === "updating" ? UPDATING : connectionLine(relay, clock.status, o.wasLive, s.swarmJob);
  doc.getElementById("connection-status")!.textContent = clock.quiet && (line === ROSTER_ONLY || line === UPDATING || line === "Live") ? "" : line;
  return { wasLive: o.wasLive, readyAt: o.readyAt };
}
