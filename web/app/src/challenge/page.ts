// One paint of the live parts of /challenges: hero fact, agents eyebrow, rows, summary and status
// line. Called on every snapshot and every 1s tick with the reader's current relay status, so the
// readiness tracker is refreshed between the reader's 15s polls.
import { renderStart, startFact } from "./hero.js";
import { connectionLine, observe, renderLiveState, renderSellers, type Snapshot, type Tracker } from "./live.js";

export interface PaintClock { nowMs: number; now: number; when: (secs: number) => string; status: string }

export function paint(doc: Document, s: Snapshot, relay: Snapshot["relay"], t: Tracker, clock: PaintClock): Tracker {
  const o = observe(t, relay, clock.nowMs);
  if (s.challenge) renderStart(doc, startFact(s.challenge, clock.now, clock.when, o.readyAgeS));
  renderLiveState(doc, o.state);
  renderSellers(doc, s, clock.now, o.state !== "live");
  doc.getElementById("connection-status")!.textContent = connectionLine(relay, clock.status, o.wasLive);
  return { wasLive: o.wasLive, readyAt: o.readyAt };
}
