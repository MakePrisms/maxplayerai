/**
 * The board's two moving signals: speed streaks (activity) and climb chevrons
 * (rank gained). Both are phase-anchored so a repaint re-enters an animation
 * mid-flight instead of restarting it.
 */
import { esc } from "./format.js";
import type { ActiveJob } from "../market/engine.js";

/** MUST match .dot.working i's animation-duration in styles.css. */
export const RACE_LIGHT_CYCLE_SECONDS = 2.2;

/**
 * Status streaks: three slanted bars. Working = one light sweeps left to
 * right each second, dwelling in each bar proportional to its size — a long
 * pass through the leader, a quick flick through the small chaser. Anchored
 * to the job clock so repaints re-enter the sweep mid-flight.
 */
export function statusDot(on: boolean, jobs: ActiveJob[] = [], context: string | null = null): string {
  const count = jobs.length;
  const idle = context || (on ? "Available now" : "Not currently online");
  const label = count ? `Working · ${count} job${count === 1 ? "" : "s"} · ${idle}` : idle;
  let phase = "";
  if (count) {
    // Anchored to the WALL CLOCK, not the job clock: every working lamp on
    // the page shares one phase, so they all sweep in unison — and a repaint
    // still re-enters the cycle mid-flight instead of restarting it.
    phase = ` style="--race-light-delay:-${((Date.now() / 1000) % RACE_LIGHT_CYCLE_SECONDS).toFixed(3)}s"`;
  }
  return `<span class="dot ${on ? "on" : ""} ${count ? "working" : ""}"${phase} role="img" aria-label="${esc(label)}" title="${esc(label)}"><i></i><i></i><i></i></span>`;
}

/**
 * The rank gutter: the position number — replaced by solid red climb chevrons
 * while its holder is up on the last 24 hours. The stack counts the move: one
 * chevron per place gained, capped at three.
 */
export function posMark(up: number | undefined, rank: number): string {
  if (!up) return `<span class="pos" title="P${rank}">${rank}</span>`;
  const chevrons = "<i></i>".repeat(Math.min(up, 3));
  return `<span class="pos up" role="img" aria-label="P${rank} · up ${up} place${up === 1 ? "" : "s"} in the last 24 hours" title="P${rank} · up ${up} in the last 24 hours">${chevrons}</span>`;
}
