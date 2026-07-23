/**
 * SINGLE SOURCE OF TRUTH for every Nostr `kind` number the observatory touches.
 *
 * This file is the ONLY place a kind literal may appear — every other module imports
 * the named constants below and MUST NOT hard-code a kind number. A grep test
 * (test/kinds.test.mjs) enforces that: the marketplace kind digits appear nowhere in
 * js/ or scripts/ except here.
 */

/** NIP-01 profile metadata. Nostr-standard — carries no mobee tag. */
export const PROFILE = 0;

/** Job offer the buyer posts. Sellers claim it. */
export const OFFER = 3401;
/** Seller claim — carries the NUT-18 payment request (`creq`). The seller bids to do the job. */
export const CLAIM = 3402;
/** Seller result — the delivery. */
export const RESULT = 3403;
/** Seller feedback — a progress / error / refusal note on a job. */
export const FEEDBACK = 3404;
/** Buyer award — selects a claim, e-tagging the offer and the winning claim. */
export const AWARD = 3405;
/** Co-signed payment receipt — the settlement proof. */
export const RECEIPT = 3400;
/** NIP-89 seller handler announce (a seller capability advert). Carries no mobee tag. */
export const HANDLER = 31990;
/**
 * Seller liveness heartbeat. Addressable (parameterized-replaceable): keyed by
 * (author, kind, d) — resolve the current one by AUTHOR + KIND (+ d), taking the
 * newest created_at. NEVER look it up by a published event id (a replaceable event
 * is superseded, so by-id lookups go empty and read as a false "offline").
 */
export const HEARTBEAT = 30340;

/** The mobee namespace tag value. Every trade event and the heartbeat carry `["t","mobee"]`. */
export const MOBEE_TAG = "mobee";

/** Plain-English labels for a kind, for any place a kind must surface to a human. */
export const KIND_LABELS = Object.freeze({
  [PROFILE]: "profile",
  [OFFER]: "offer",
  [CLAIM]: "claim",
  [RESULT]: "result",
  [FEEDBACK]: "feedback",
  [AWARD]: "award",
  [RECEIPT]: "receipt",
  [HANDLER]: "handler (NIP-89)",
  [HEARTBEAT]: "heartbeat",
});

/**
 * Marketplace kinds that carry `["t","mobee"]` — requested with a `#t:["mobee"]` filter.
 * The trade path plus the seller heartbeat all live in the mobee namespace.
 */
export const MOBEE_TAGGED_KINDS = Object.freeze([
  OFFER,
  CLAIM,
  RESULT,
  FEEDBACK,
  AWARD,
  RECEIPT,
  HEARTBEAT,
]);

/**
 * Marketplace kinds requested WITHOUT a t-tag filter — the NIP-89 handler announce is a
 * standard advert that carries no mobee tag, so a `#t` filter would hide it. Gift-wrap
 * (1059) stays dark either way.
 */
export const UNTAGGED_KINDS = Object.freeze([HANDLER]);
