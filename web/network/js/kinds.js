/**
 * SINGLE SOURCE OF TRUTH for every Nostr `kind` number the observatory touches.
 *
 * This file is the ONLY place a kind literal may appear — every other module imports
 * the named constants below and MUST NOT hard-code a kind number. A grep test
 * (test/kinds.test.mjs) enforces that: the marketplace kind digits appear nowhere in
 * js/ or scripts/ except here.
 */

/** NIP-01 profile metadata. Nostr-standard — carries no maxplayer tag. */
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
/**
 * Buyer accept — the pay-bind against one verified result, e-tagging the offer and the claim.
 * Its own kind, not a second AWARD: a job's normal steady state used to be two events of the
 * same kind, so award and accept were indistinguishable and every awarded-then-accepted job
 * rendered as a double award.
 */
export const ACCEPT = 3406;
/** Co-signed payment receipt — the settlement proof. */
export const RECEIPT = 3400;
/** NIP-89 seller handler announce (a seller capability advert). Carries no maxplayer tag. */
export const HANDLER = 31990;
/**
 * Seller liveness heartbeat. Addressable (parameterized-replaceable): keyed by
 * (author, kind, d) — resolve the current one by AUTHOR + KIND (+ d), taking the
 * newest created_at. NEVER look it up by a published event id (a replaceable event
 * is superseded, so by-id lookups go empty and read as a false "offline").
 */
export const HEARTBEAT = 30340;

/**
 * The `d` value scoping a seller's seat announcement within its author. A seat is addressed by
 * (pubkey, kind, d) — the pubkey alone is not the address.
 *
 * The `d` does NOT imply the tag shape. Measured on relay.maxplayer.ai 2026-08-21: a
 * `maxplayer-seller` announcement was carrying the pre-rename tag set (`mobee_agent`,
 * `protocol_versions`, no `v`), so a reader must detect the shape from the tags rather than infer
 * it from the address. The pre-rename `mobee-seller` address is also still live on the relay and is
 * not this one.
 */
export const SELLER_HEARTBEAT_D = "maxplayer-seller";

/** The maxplayer namespace tag value. Every trade event and the heartbeat carry `["t","maxplayer"]`. */
export const MAXPLAYER_TAG = "maxplayer";

/**
 * Seat capability tags the AWARD DECISION can read. The emitter puts exactly these on a claim.
 *
 * Two of the three carry a LIST IN ONE TAG — `["harness_family","claude-code","codex"]`,
 * `["capabilities","rust","node"]`. The third REPEATS, one tag per model, each holding a PAIR:
 * `["harness_model","claude-code","claude-opus-5"]`. Reading `harness_model` as a one-tag list
 * yields "a family plus a model" and drops every model past the first silently, so the two shapes
 * need different readers and never the same one.
 */
export const SEAT_FILTERABLE_TAGS = Object.freeze(["harness_family", "capabilities", "harness_model"]);

/**
 * Seat capability tags that exist for a human to read and NOTHING ELSE.
 *
 * These are beat-only: the emitter never puts them on a claim, and the award decision reads the
 * claim, so they are not merely unfiltered — they are absent from the filter's input. Two
 * consequences for a reader. Their absence on a claim is UNCONDITIONAL, so "not carried on claims"
 * and "this seat did not set it" are indistinguishable there and must not be reported as the
 * latter. And they are kept out of the filterable shape structurally rather than by convention, so
 * a predicate written later against that shape cannot reach them.
 *
 * `hardware` values contain commas and spaces (`"mac studio, 64GB"`) and model values contain
 * brackets (`"gpt-5.6-sol[low]"`). Split on the tag array. Never on the value text.
 */
export const SEAT_DISPLAY_ONLY_TAGS = Object.freeze(["harness_variant", "hardware"]);

/** Plain-English labels for a kind, for any place a kind must surface to a human. */
export const KIND_LABELS = Object.freeze({
  [PROFILE]: "profile",
  [OFFER]: "offer",
  [CLAIM]: "claim",
  [RESULT]: "result",
  [FEEDBACK]: "feedback",
  [AWARD]: "award",
  [ACCEPT]: "accept",
  [RECEIPT]: "receipt",
  [HANDLER]: "handler (NIP-89)",
  [HEARTBEAT]: "heartbeat",
});

/**
 * Marketplace kinds that carry `["t","maxplayer"]` — requested with a `#t:["maxplayer"]` filter.
 * The trade path plus the seller heartbeat all live in the maxplayer namespace.
 */
export const MAXPLAYER_TAGGED_KINDS = Object.freeze([
  OFFER,
  CLAIM,
  RESULT,
  FEEDBACK,
  AWARD,
  ACCEPT,
  RECEIPT,
  HEARTBEAT,
]);

/**
 * Marketplace kinds requested WITHOUT a t-tag filter — the NIP-89 handler announce is a
 * standard advert that carries no maxplayer tag, so a `#t` filter would hide it. Gift-wrap
 * (1059) stays dark either way.
 */
export const UNTAGGED_KINDS = Object.freeze([HANDLER]);
