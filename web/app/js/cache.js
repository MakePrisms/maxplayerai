/**
 * Event cache — the app's memory of the relay.
 *
 * Holds every event once, resolves superseded ones, and answers "what do I
 * already have?" so a re-read never re-fetches history it is holding. Pure data:
 * no network, no DOM, so it is directly testable.
 */
import { ADDRESSABLE_KINDS, REPLACEABLE_KINDS } from "./kinds.js";

const ADDRESSABLE = new Set(ADDRESSABLE_KINDS);
const REPLACEABLE = new Set(REPLACEABLE_KINDS);

/** The `d` tag identifies which of an author's addressable events this is. */
function dTag(event) {
  const tags = event.tags || [];
  for (const t of tags) if (t[0] === "d") return t[1] || "";
  return "";
}

/**
 * Replaceable events are keyed by who published them and which slot they fill —
 * never by event id, because a superseded event's id stops resolving.
 */
function slotKey(event) {
  if (ADDRESSABLE.has(event.kind)) return `${event.kind}:${event.pubkey}:${dTag(event)}`;
  if (REPLACEABLE.has(event.kind)) return `${event.kind}:${event.pubkey}`;
  return null;
}

export function createCache() {
  /** id -> event, for every event currently considered live. */
  const byId = new Map();
  /** slot key -> event id, for the winner of each replaceable slot. */
  const slots = new Map();
  let oldest = null;
  let newest = null;

  /**
   * Take one event.
   *
   * Returns what actually changed so a caller can avoid pointless re-renders:
   * `stored` false means it was a duplicate or an older version of something we
   * already hold. A newer version of a replaceable event evicts the old one, so
   * the cache never accumulates stale copies of the same slot.
   */
  function ingest(event) {
    if (!event || typeof event.id !== "string" || typeof event.kind !== "number") {
      return { stored: false, reason: "malformed" };
    }
    if (byId.has(event.id)) return { stored: false, reason: "duplicate" };

    const key = slotKey(event);
    if (key) {
      const currentId = slots.get(key);
      const current = currentId ? byId.get(currentId) : null;
      // Ties go to the incumbent: a re-delivered event must not churn the slot.
      if (current && current.created_at >= event.created_at) {
        return { stored: false, reason: "superseded" };
      }
      if (current) byId.delete(current.id);
      slots.set(key, event.id);
    }

    byId.set(event.id, event);
    if (oldest === null || event.created_at < oldest) oldest = event.created_at;
    if (newest === null || event.created_at > newest) newest = event.created_at;
    return { stored: true, replaced: Boolean(key) };
  }

  return {
    ingest,
    /** How far back the cache reaches — the cursor for paging further. */
    get oldest() { return oldest; },
    /** Most recent event held — where a live subscription should resume. */
    get newest() { return newest; },
    get size() { return byId.size; },
    has: (id) => byId.has(id),
    all: () => [...byId.values()],
    /** Events of the given kinds, newest first. */
    byKinds(kinds) {
      const want = new Set(kinds);
      return [...byId.values()]
        .filter((e) => want.has(e.kind))
        .sort((a, b) => b.created_at - a.created_at);
    },
    /** The live event filling a replaceable slot, or null. */
    slot(kind, pubkey, d = "") {
      const id = slots.get(ADDRESSABLE.has(kind) ? `${kind}:${pubkey}:${d}` : `${kind}:${pubkey}`);
      return id ? byId.get(id) || null : null;
    },
  };
}
