/**
 * SINGLE SOURCE OF TRUTH for every Nostr `kind` the app touches.
 *
 * No other module may contain a kind literal — everything imports the named
 * constants below, and a test enforces it. A renumber stays a one-file change.
 */

/** NIP-01 profile metadata. Nostr-standard; carries no maxplayer tag. */
export const PROFILE = 0;

/** Job offer a buyer posts. Sellers claim it. */
export const OFFER = 3401;
/** Seller claim — carries the NUT-18 payment request. A bid to do the job. */
export const CLAIM = 3402;
/** Seller result — the delivery. */
export const RESULT = 3403;
/** Seller feedback — progress, error, or refusal on a job. */
export const FEEDBACK = 3404;
/** Buyer award — selects a claim, e-tagging the offer and the winning claim. */
export const AWARD = 3405;
/**
 * Buyer accept — the pay-bind against one verified result. Its own kind, not a
 * second AWARD: while the two shared 3405, every awarded-then-accepted job
 * counted as two awards and no reader could tell selection from pay-authorisation.
 */
export const ACCEPT = 3406;
/**
 * Co-signed payment receipt.
 *
 * OPTIONAL, and that is load-bearing: settlement can complete with no receipt
 * ever published — payment travels as encrypted gift-wrap and the wallet is
 * the only complete record. Receipt-derived totals are therefore a FLOOR on
 * what settled, never the total. Anything user-facing must say so.
 */
export const RECEIPT = 3400;
/**
 * Seller capability + liveness announcement. Addressable (parameterized-
 * replaceable): keyed by (author, kind, d), newest `created_at` wins. NEVER
 * resolve one by event id — a superseded event goes missing and reads as a
 * false "offline".
 */
export const HEARTBEAT = 30340;

/** The maxplayer namespace tag value. Every trade event and the heartbeat carry `["t","maxplayer"]`. */
export const MAXPLAYER_TAG = "maxplayer";

/**
 * A second `t` value marking an offer whose buyer operates the seller being
 * paid. Self-commissioned work is real work, but it is not market demand.
 * Events carry both values, so the `#t` filter for MAXPLAYER_TAG still matches.
 */
export const SELF_TRADE_TAG = "self-trade";

/** Kinds that carry `["t","maxplayer"]` — requested with a `#t` filter. */
export const MAXPLAYER_TAGGED_KINDS: readonly number[] = Object.freeze([
  OFFER, CLAIM, RESULT, FEEDBACK, AWARD, ACCEPT, RECEIPT, HEARTBEAT,
]);

/**
 * Kinds requested WITHOUT a t-tag filter. NIP-01 profile metadata carries no
 * maxplayer tag of its own, so a `#t` filter would hide it. PROFILE is the
 * SINGLE publisher of a seat's display name (§6.1 / #275).
 * Gift-wrap stays dark either way and is never requested or decoded.
 */
export const UNTAGGED_KINDS: readonly number[] = Object.freeze([PROFILE]);

/** Kinds whose newest event per (author, kind, d) supersedes the rest. */
export const ADDRESSABLE_KINDS: readonly number[] = Object.freeze([HEARTBEAT]);
/** Kinds whose newest event per (author, kind) supersedes the rest. */
export const REPLACEABLE_KINDS: readonly number[] = Object.freeze([PROFILE]);

export type Stage =
  | "offer" | "claim" | "award" | "result" | "accept" | "receipt" | "feedback";

/**
 * The stage each kind represents in a trade's life. Kinds absent from this map
 * (profile and heartbeat) describe a participant, not a trade.
 */
export const TRADE_STAGES: Readonly<Record<number, Stage>> = Object.freeze({
  [OFFER]: "offer",
  [CLAIM]: "claim",
  [AWARD]: "award",
  [RESULT]: "result",
  [ACCEPT]: "accept",
  [RECEIPT]: "receipt",
  [FEEDBACK]: "feedback",
});

/** Plain-English label for a kind, wherever one must surface to a human. */
export const KIND_LABELS: Readonly<Record<number, string>> = Object.freeze({
  [PROFILE]: "profile",
  [OFFER]: "offer",
  [CLAIM]: "claim",
  [RESULT]: "result",
  [FEEDBACK]: "feedback",
  [AWARD]: "award",
  [ACCEPT]: "accept",
  [RECEIPT]: "receipt",
  [HEARTBEAT]: "heartbeat",
});
