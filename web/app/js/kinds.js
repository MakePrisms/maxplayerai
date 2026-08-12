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
 * Buyer accept — the pay-bind against one verified result, e-tagging the offer and the claim.
 *
 * Its own kind, not a second AWARD. While the two shared 3405, two events per job was the
 * NORMAL steady state, so award and accept were indistinguishable here: every
 * awarded-then-accepted job counted as two awards, and no reader could tell a selection
 * from a pay-authorisation.
 */
export const ACCEPT = 3406;
/**
 * Co-signed payment receipt.
 *
 * OPTIONAL, and that is load-bearing: settlement can complete with no receipt
 * ever published, because payment itself travels as encrypted gift-wrap and the
 * wallet is the only complete record. Receipt-derived totals are therefore a
 * FLOOR on what settled, never the total. Anything user-facing must say so.
 */
export const RECEIPT = 3400;
/**
 * Seller capability + liveness announcement. Addressable
 * (parameterized-replaceable): keyed by (author, kind, d), newest
 * `created_at` wins. NEVER resolve one by event id — a superseded event goes
 * missing and reads as a false "offline".
 *
 * Protocol v1 retired kind-31990. Every live seat-level fact now comes from
 * this event, including current and future fields the UI does not know yet.
 */
export const HEARTBEAT = 30340;

/** The maxplayer namespace tag value. Every trade event and the heartbeat carry `["t","maxplayer"]`. */
export const MAXPLAYER_TAG = "maxplayer";

/**
 * A second `t` value marking an offer whose buyer operates the seller being
 * paid. Self-commissioned work is real work, but it is not market demand, and
 * a receipt cannot be told apart from an arms-length one after the fact.
 *
 * Events carry both values, so the `#t` filter for MAXPLAYER_TAG still matches.
 */
export const SELF_TRADE_TAG = "self-trade";

/**
 * Kinds that carry `["t","maxplayer"]` — requested with a `#t` filter.
 */
export const MAXPLAYER_TAGGED_KINDS = Object.freeze([
  OFFER, CLAIM, RESULT, FEEDBACK, AWARD, ACCEPT, RECEIPT, HEARTBEAT,
]);

/**
 * Kinds requested WITHOUT a t-tag filter. NIP-01 profile metadata carries no
 * maxplayer tag of its own, so a `#t` filter would hide it.
 *
 * PROFILE belongs here because it is the SINGLE publisher of a seat's display
 * name (§6.1 / #275). It was parsed, cached and read by the seller board while
 * appearing on no requested-kinds list at all, so the name had a reader and no
 * source and every card fell back to the short pubkey (#449).
 *
 * Gift-wrap stays dark either way and is never requested or decoded.
 */
export const UNTAGGED_KINDS = Object.freeze([PROFILE]);

/**
 * Kinds whose newest event per (author, kind, d) supersedes the rest.
 */
export const ADDRESSABLE_KINDS = Object.freeze([HEARTBEAT]);
/** Kinds whose newest event per (author, kind) supersedes the rest. */
export const REPLACEABLE_KINDS = Object.freeze([PROFILE]);

/**
 * The stage each kind represents in a trade's life. Kinds absent from this map
 * (profile and heartbeat) describe a participant, not a trade.
 */
export const TRADE_STAGES = Object.freeze({
  [OFFER]: "offer",
  [CLAIM]: "claim",
  [AWARD]: "award",
  [RESULT]: "result",
  [ACCEPT]: "accept",
  [RECEIPT]: "receipt",
  [FEEDBACK]: "feedback",
});

/**
 * The kinds above are the protocol. Figures derived from them describe the
 * market on the current protocol, which is narrower than the analytics
 * pipeline's view of every trade maxplayer has ever run — expect the counts to
 * differ, and say which one a number is whenever it reaches a reader.
 */

/** Plain-English label for a kind, wherever one must surface to a human. */
export const KIND_LABELS = Object.freeze({
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
