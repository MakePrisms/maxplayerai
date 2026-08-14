/**
 * Deploy-tunable constants. Kind numbers live in src/model/kinds.ts — never here.
 */
import type { Transport } from "./source/source.js";

/** The launch relay, baked in. Read-only: no key is ever loaded. */
export const RELAY_URL = "wss://relay.maxplayer.ai";

/**
 * THE SWITCH. Today's relay answers stored-event queries but never streams
 * post-EOSE, so we poll. The day the relay is upgraded to push, change this
 * one word to "stream" — the source holds its subscription open and every
 * event renders the instant it lands. Nothing else in the app changes.
 */
export const TRANSPORT: Transport = "poll";

/** IndexedDB database name for the event cache. */
export const DB_NAME = "maxplayer-terminal";

/** Baked market snapshot served next to the page (static file, optional). */
export const SNAPSHOT_URL = "./snapshot.json";

/**
 * Deadline for the snapshot fetch. It is a first-paint optimization, so a slow
 * or stalled request must lose to the relay rather than delay it.
 */
export const SNAPSHOT_TIMEOUT_MS = 5000;
