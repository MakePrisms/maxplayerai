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
  /** History exhausted — everything stored has been delivered once. */
  onSynced(): void;
}

export interface MarketSource {
  start(): void;
  stop(): void;
  readonly state: SourceState;
}
