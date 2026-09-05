/**
 * Self-healing for a store that lost a terminal event.
 *
 * The forward read resumes from the newest cached event, so an ACCEPT or
 * RECEIPT that fell below that mark (same-second race, slow publisher clock)
 * is never asked for again — the job stays "working" on THIS browser only,
 * across reloads, while a fresh browser renders it done. The relay has the
 * event; the store just never re-asks.
 *
 * The cure is one number: the oldest moment any job this store still holds
 * open began. The first forward walk of the boot starts from there instead of
 * from the mark, re-reads the window that must contain the miss, and the
 * cache drops everything it already holds by id. Bounded by
 * RECOVERY_MAX_SECONDS so an award nobody ever delivers against cannot turn
 * every boot into a full history walk. Pure: no network, no DOM.
 */
import { RECOVERY_MAX_SECONDS } from "../config.js";
import { activeTradeJobs } from "../market/engine.js";
import { inProgressJobs } from "../market/participants.js";
import type { RawEvent } from "../model/events.js";

/**
 * The `since` floor the boot's forward walk must reach down to so every job
 * still shown open — lamp (activeTradeJobs) or in-progress row
 * (inProgressJobs) — gets its terminal events re-asked for. Null when nothing
 * is open: the ordinary forward read is then already correct.
 */
export function recoveryFloor(events: RawEvent[], now: number, maxSeconds = RECOVERY_MAX_SECONDS): number | null {
  let floor: number | null = null;
  const reach = (t: number) => { if (floor == null || t < floor) floor = t; };
  const active = activeTradeJobs(events, now);
  for (const jobs of [...active.byBuyer.values(), ...active.bySeller.values()]) {
    for (const job of jobs) reach(job.startedAt);
  }
  for (const job of inProgressJobs(events, now)) reach(job.startedAt);
  if (floor == null) return null;
  return Math.max(floor, now - maxSeconds);
}
