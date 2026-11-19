/**
 * When /tokens paints and what its relay status says. Pure; tokens.ts wires it.
 */
import type { RelayState } from "./relays.js";

/** After the first relay answers, wait at most this long for the others before painting. */
export const SETTLE_MS = 2500;

/**
 * bob: skeleton -> data in ONE swap. Paint once every relay has finished (or
 * failed) its first read, or SETTLE_MS after the first live answer, so one
 * slow relay never holds the page. Never before any relay answered.
 */
export function boardSettled(answered: boolean, firstReads: number, total: number, graceOver: boolean): boolean {
  return answered && (firstReads >= total || graceOver);
}

/** The nav status: "live · n/N relays" while any relay is live. */
export function relaySummary(states: ReadonlyMap<string, RelayState>, total: number): { state: "live" | "connecting" | "failed"; text: string } {
  const n = [...states.values()].filter((s) => s === "live").length;
  const any = (s: RelayState) => [...states.values()].includes(s);
  const state = n > 0 ? "live" : any("syncing") || any("connecting") || states.size < total ? "connecting" : "failed";
  return { state, text: n > 0 ? `live · ${n}/${total} relays` : state === "failed" ? "relays unreachable" : "syncing" };
}
