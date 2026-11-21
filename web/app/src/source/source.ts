/**
 * The seam the relay upgrade swaps at.
 *
 * Everything downstream — store, engine, UI — consumes this interface and
 * cannot tell HOW events arrive. Today's relay never pushes post-EOSE, so the
 * default implementation polls; the day the relay streams, TRANSPORT flips to
 * "stream" in config.ts and nothing else changes.
 */
import type { RawEvent } from "../model/events.js";

export type Transport = "poll" | "stream";

export type SourceState =
  | "idle"        // not started
  | "connecting"  // socket opening
  | "history"     // walking stored events
  | "live"        // caught up; new events arrive by poll or push
  | "reconnecting"
  | "failed";

export interface SourceStatus {
  state: SourceState;
  detail?: string;
}

export interface SourceCallbacks {
  /** One raw event. May be a duplicate; the store dedupes. */
  onEvent(event: RawEvent): void;
  onStatus(status: SourceStatus): void;
  /**
   * History reading is over. `complete` is true only when every stream drained
   * genuinely; false means a backstop cut the read short, so the store holds a
   * hole and must not be treated as a floor for a later forward read.
   */
  onSynced(result: { complete: boolean }): void;
  /**
   * Optional: one history request has been answered in full (its EOSE). The relay answers newest first within a
   * request, but nothing promises the order of the events INSIDE one answer, so a reader that needs "everything above
   * this point is in" checks it here, between requests, not per event.
   */
  onPage?(): void;
}

export interface MarketSource {
  start(): void;
  stop(): void;
  readonly state: SourceState;
}
