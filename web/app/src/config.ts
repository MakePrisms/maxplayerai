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

/**
 * How far BELOW the newest event we hold every forward ask reaches, in seconds.
 *
 * The forward cursor is a single high-water mark raised by every event. Asked
 * from `mark + 1`, two things fall under it for good: a second event stamped
 * in the mark's own second that the relay had not returned yet (agents fire
 * an offer and its claim in the same second routinely), and an event whose
 * publisher's clock runs slow — the relay accepts a wide skew, and a receipt
 * stamped a minute in the past is already below the mark when it lands. One
 * missed RECEIPT or ACCEPT is a working lamp that never goes out.
 *
 * So the REQ floor trails the mark by this much; the mark itself stays
 * monotonic. Everything re-delivered is dropped by id in the cache (and a
 * re-delivered replaceable event loses the tie to the incumbent), so the only
 * cost is bytes: MEASURED 2026-09-04 19:34Z on the production relay, the
 * trailing two minutes held 2–11 events (≈1.4–7.5 KB), almost all heartbeats,
 * which the relay stores one-per-seat regardless of window length.
 */
export const POLL_OVERLAP_SECONDS = 120;

/**
 * How far back a boot may re-walk the relay to heal a job the store still
 * shows open, in seconds.
 *
 * A browser that lost a terminal event long ago cannot be reached by the
 * overlap above: its mark is far past the miss. So at boot the first forward
 * walk starts from the oldest job this browser still holds open (see
 * source/recovery.ts) — bounded here, because an award nobody ever delivers
 * against stays "open" forever and must not turn every boot into a full
 * history walk. Two days covers the whole natural life of a working lamp
 * (ACTIVE_GRACE_SECONDS after its last stamp) with margin.
 */
export const RECOVERY_MAX_SECONDS = 2 * 86400;

/** IndexedDB database name for the event cache. */
export const DB_NAME = "maxplayer-terminal";

/** Baked market snapshot served next to the page (static file, optional). */
export const SNAPSHOT_URL = "./snapshot.json";

/**
 * Deadline for the snapshot fetch. It is a first-paint optimization, so a slow
 * or stalled request must lose to the relay rather than delay it.
 */
export const SNAPSHOT_TIMEOUT_MS = 5000;
