/**
 * The deploy-baked snapshot — a first-visit optimization, never a dependency.
 *
 * It is a static file of a few megabytes fetched on the boot path, so the
 * failure that matters is not a rejection but a STALL: a request that neither
 * resolves nor rejects holds the boot open forever, and no `catch` ever runs.
 * Hence an explicit deadline, and an outcome the caller can act on instead of
 * an empty array that means three different things.
 */
import type { RawEvent } from "../model/events.js";

export type SnapshotOutcome =
  /** Events were read and parsed. */
  | "loaded"
  /** No snapshot is deployed, or it could not be reached in time. */
  | "absent"
  /** A snapshot was served but is not readable — a truncated or invalid bake. */
  | "unreadable";

export interface SnapshotResult {
  events: RawEvent[];
  outcome: SnapshotOutcome;
}

export interface LoadSnapshotOptions {
  url: string;
  timeoutMs: number;
  fetchImpl?: typeof fetch;
}

export async function loadSnapshot(
  { url, timeoutMs, fetchImpl = fetch }: LoadSnapshotOptions,
): Promise<SnapshotResult> {
  let res: Response;
  try {
    res = await fetchImpl(url, { signal: AbortSignal.timeout(timeoutMs) });
  } catch (err) {
    // Includes the timeout firing: a stalled fetch now ends as a reachable
    // failure rather than an open promise nothing is watching.
    console.warn("[snapshot] not available; the relay fills the boards", err);
    return { events: [], outcome: "absent" };
  }
  if (!res.ok) {
    console.warn(`[snapshot] not available (HTTP ${res.status}); the relay fills the boards`);
    return { events: [], outcome: "absent" };
  }
  try {
    const events = (await res.json()) as RawEvent[];
    if (!Array.isArray(events)) throw new TypeError("snapshot is not an array of events");
    return { events, outcome: "loaded" };
  } catch (err) {
    // Deliberately distinct from "absent": this one says a DEPLOY shipped a
    // bad file, which is a different thing to go and fix.
    console.warn("[snapshot] served but unreadable; skipped", err);
    return { events: [], outcome: "unreadable" };
  }
}
