// R3-2: the seller heartbeat's serving statement (protocol-v1 §4.2). ONE reader shared by the
// network page (events.ts) and the challenge model/runner (challenge/shared/model.mjs), mirroring
// `serving_from_tags` in crates/maxplayer-core/src/heartbeat.rs.
//
//   ["serving", "<available>/<configured>"]  harnesses able to run a job now / resolved at boot
//   ["status", "not_serving"]                emitted exactly when available is 0
//
// ABSENT OR MALFORMED IS UNKNOWN, NEVER "NOT SERVING": every seat built before the tag publishes
// neither, and a reader that refused them would drop every existing seller.

/** @typedef {{available:number, configured:number}} ServingCount */
/** @typedef {'serving'|'not_serving'|'unknown'} ServingState */

const first = (tags, name) => {
  for (const t of tags ?? []) if (Array.isArray(t) && t[0] === name && typeof t[1] === 'string') return t[1];
  return null;
};

/**
 * Parse `"a/n"`: two decimal integers, a <= n, n > 0. Anything else is null (unknown).
 * @param {string|null|undefined} value
 * @returns {ServingCount|null}
 */
export function parseServingCount(value) {
  const m = /^(\d+)\/(\d+)$/.exec(String(value ?? '').trim());
  if (!m) return null;
  const available = Number(m[1]), configured = Number(m[2]);
  if (!Number.isSafeInteger(available) || !Number.isSafeInteger(configured)) return null;
  return configured > 0 && available <= configured ? { available, configured } : null;
}

/**
 * The serving statement on a heartbeat's tags. `not_serving` wins over a contradicting count.
 * @param {string[][]|undefined} tags
 * @returns {{count: ServingCount|null, state: ServingState}}
 */
export function servingFromTags(tags) {
  const count = parseServingCount(first(tags, 'serving'));
  const notServing = (first(tags, 'status') ?? '').trim() === 'not_serving';
  const state = notServing || count?.available === 0 ? 'not_serving' : count ? 'serving' : 'unknown';
  return { count, state };
}
