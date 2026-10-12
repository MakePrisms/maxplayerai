/**
 * SINGLE SOURCE OF TRUTH for the market subscription filters.
 *
 * Both the ACTUAL APP (js/relay.js) and the PROBE (scripts/live-check.mjs) build the
 * outbound REQ's filter list from this one helper, so the probe can never drift from the
 * subscription the app actually sends. The helper returns the two-filter shape the relay
 * serves: the maxplayer-namespaced kinds scoped by `#t`, plus the untagged kinds (the NIP-89
 * handler announce carries no maxplayer tag, so it must ride an unscoped filter).
 *
 * `limit` and `since` are deliberately per-consumer parameters, not baked in here: the app
 * cold-connects with a `limit` then resumes by `since`; the probe (a one-shot census) always
 * sets a `limit`. They MAY legitimately differ — that difference is intended and is asserted
 * explicitly in test/filter-parity.test.mjs rather than erased. The shared contract is the
 * kind + tag scoping, which is what must never drift.
 */
import { MAXPLAYER_TAG, MAXPLAYER_TAGGED_KINDS, UNTAGGED_KINDS } from "./kinds.js";

/**
 * Build the two market filters for a REQ. `since` (resume cursor) and `limit` are mutually
 * exclusive: if `since` is given the relay resumes from it, else `limit` caps the historical
 * fetch.
 *
 * @param {{ since?: number|null, limit?: number|null }} [opts]
 * @returns {[Record<string, unknown>, Record<string, unknown>]} [tagged, untagged]
 */
export function buildMarketFilters({ since = null, limit = null } = {}) {
  /** @type {Record<string, unknown>} */
  const tagged = { kinds: [...MAXPLAYER_TAGGED_KINDS], "#t": [MAXPLAYER_TAG] };
  /** @type {Record<string, unknown>} */
  const untagged = { kinds: [...UNTAGGED_KINDS] };
  if (since != null) {
    tagged.since = since;
    untagged.since = since;
  } else {
    tagged.limit = limit;
    untagged.limit = limit;
  }
  return [tagged, untagged];
}
