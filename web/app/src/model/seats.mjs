// The seller heartbeat's load and seat statement (protocol-v1 §4.2). ONE reader for the network
// page (events.ts), mirroring `in_flight_from_tags` / `seat_slots_from_tags` in
// crates/maxplayer-core/src/heartbeat.rs.
//
//   ["in_flight", n]     jobs Awarded/Executing — live load, NOT free capacity
//   ["queue_depth", n]   DEPRECATED name for in_flight (same value); the only one older seats publish
//   ["slots", n]         the seat's total execution seats
//   ["free_slots", n]    seats free now (claim-time reservations included) — the claim gate's number
//
// ABSENT OR MALFORMED SEATS ARE UNKNOWN, NEVER "FULL" OR "FREE": every seat built before the tags
// publishes neither, and the pair is read together.

/** @typedef {{total:number, free:number}} SeatSlots */

const first = (tags, name) => {
  for (const t of tags ?? []) if (Array.isArray(t) && t[0] === name && typeof t[1] === 'string') return t[1];
  return null;
};

const decimal = (value) => {
  const m = /^\d+$/.exec(String(value ?? '').trim());
  if (!m) return null;
  const n = Number(m[0]);
  return Number.isSafeInteger(n) ? n : null;
};

/**
 * Live load: `in_flight` when it parses, else the deprecated `queue_depth`. null when neither does.
 * @param {string[][]|undefined} tags
 * @returns {number|null}
 */
export function inFlightFromTags(tags) {
  return decimal(first(tags, 'in_flight')) ?? decimal(first(tags, 'queue_depth'));
}

/**
 * The seat pair, or null (unknown) unless both parse, slots > 0 and free_slots <= slots.
 * @param {string[][]|undefined} tags
 * @returns {SeatSlots|null}
 */
export function seatSlotsFromTags(tags) {
  const total = decimal(first(tags, 'slots')), free = decimal(first(tags, 'free_slots'));
  if (total === null || free === null) return null;
  return total > 0 && free <= total ? { total, free } : null;
}
