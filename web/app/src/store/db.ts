/**
 * IndexedDB persistence — why a returning visitor never sees a loading state.
 *
 * On boot the page paints from what this store already holds (milliseconds),
 * then the relay source resumes from the newest cached timestamp and fetches
 * only what was missed. Writes are batched and fire-and-forget: persistence
 * must never block a paint, and a lost batch costs one re-fetch, not data.
 *
 * Every browser API here is fallible (private windows, storage pressure), so
 * every failure path degrades to "works like the first visit" — never an error
 * the reader sees.
 */
import { DB_NAME } from "../config.js";
import type { RawEvent } from "../model/events.js";

const STORE = "events";
const META = "meta";
const COMPLETE_KEY = "historyComplete";
const VERSION = 2;

/**
 * Safety valve on the cache, not a routine working limit — roughly an order of
 * magnitude above the live market, so reaching it means something changed.
 *
 * The trade-off is deliberate and worth stating: pruning makes the store no
 * longer a COMPLETE history, so it also clears the completeness marker and the
 * relay walks the full history again next boot. That costs a slower boot. The
 * alternative — pruning quietly and leaving the marker set — would resume the
 * cheap forward read above the events just deleted, which is a permanent hole.
 * Slow is recoverable; a hole is not.
 */
const MAX_CACHED_EVENTS = 50_000;

export interface EventDb {
  /** Everything held, in no particular order. Empty on any failure. */
  loadAll(): Promise<RawEvent[]>;
  /** Queue events for persistence; flushed on a microtask batch. */
  save(events: RawEvent[]): void;
  /** Remove superseded replaceable versions so the store tracks the cache. */
  evict(ids: string[]): void;
  /**
   * Did a history walk ever finish? Without this the cache is just a bag of
   * events with no way to tell "everything" from "everything above a hole",
   * and the relay's cheap forward read would resume above the hole forever.
   * Unknown answers false: re-reading is cheap, a permanent gap is not.
   */
  historyComplete(): Promise<boolean>;
  /** Record that a walk reached genuine exhaustion. */
  markHistoryComplete(): void;
}

function openDb(): Promise<IDBDatabase | null> {
  return new Promise((resolve) => {
    let request: IDBOpenDBRequest;
    try {
      request = indexedDB.open(DB_NAME, VERSION);
    } catch (err) {
      console.warn("[db] IndexedDB is unavailable; every visit will be a first visit", err);
      resolve(null);
      return;
    }
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(STORE)) {
        db.createObjectStore(STORE, { keyPath: "id" });
      }
      // Kept out of the event store: a marker sharing that keyspace would come
      // back from loadAll() and be ingested as if it were market data.
      if (!db.objectStoreNames.contains(META)) {
        db.createObjectStore(META);
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => {
      console.warn("[db] could not open the event cache; every visit will be a first visit", request.error);
      resolve(null);
    };
    request.onblocked = () => {
      console.warn("[db] the event cache is blocked by another tab; this session will not persist");
      resolve(null);
    };
  });
}

export async function createEventDb(): Promise<EventDb> {
  const db = await openDb();
  let pending: RawEvent[] = [];
  let pendingEvictions: string[] = [];
  let flushQueued = false;

  function flush() {
    flushQueued = false;
    if (!db) { pending = []; pendingEvictions = []; return; }
    const toWrite = pending;
    const toEvict = pendingEvictions;
    pending = [];
    pendingEvictions = [];
    if (!toWrite.length && !toEvict.length) return;
    try {
      const tx = db.transaction(STORE, "readwrite");
      const store = tx.objectStore(STORE);
      for (const event of toWrite) store.put(event);
      for (const id of toEvict) store.delete(id);
      // Persistence is an optimization, never a dependency — a failed write
      // costs one re-fetch. But it is not allowed to be INVISIBLE: a store that
      // silently accepts nothing looks exactly like a working one.
      tx.onerror = () => {
        console.warn(`[db] a batch of ${toWrite.length} writes and ${toEvict.length} evictions failed`, tx.error);
      };
    } catch (err) {
      console.warn("[db] storage is unavailable; this session will not persist", err);
    }
  }

  function queueFlush() {
    if (flushQueued) return;
    flushQueued = true;
    queueMicrotask(flush);
  }

  function clearHistoryComplete(): void {
    if (!db) return;
    try {
      db.transaction(META, "readwrite").objectStore(META).delete(COMPLETE_KEY);
    } catch (err) {
      console.warn("[db] could not clear the completeness marker", err);
    }
  }

  /** Enforce the cap, newest kept. Returns what the caller should boot from. */
  function capped(all: RawEvent[]): RawEvent[] {
    if (all.length <= MAX_CACHED_EVENTS) return all;
    const byNewest = [...all].sort((a, b) => (b?.created_at ?? 0) - (a?.created_at ?? 0));
    const keep = byNewest.slice(0, MAX_CACHED_EVENTS);
    const drop = byNewest.slice(MAX_CACHED_EVENTS);
    console.warn(`[db] cache holds ${all.length} events, over the ${MAX_CACHED_EVENTS} cap — pruning ${drop.length} oldest`);
    pendingEvictions.push(...drop.map((e) => e.id));
    queueFlush();
    // The store is no longer a complete history, so it must stop claiming to be.
    clearHistoryComplete();
    return keep;
  }

  return {
    loadAll(): Promise<RawEvent[]> {
      if (!db) return Promise.resolve([]);
      return new Promise((resolve) => {
        try {
          const request = db.transaction(STORE, "readonly").objectStore(STORE).getAll();
          request.onsuccess = () => resolve(capped((request.result as RawEvent[]) || []));
          request.onerror = () => {
            console.warn("[db] could not read the cache; booting as a first visit", request.error);
            resolve([]);
          };
        } catch (err) {
          console.warn("[db] could not read the cache; booting as a first visit", err);
          resolve([]);
        }
      });
    },
    save(events) {
      if (!events.length) return;
      pending.push(...events);
      queueFlush();
    },
    evict(ids) {
      if (!ids.length) return;
      pendingEvictions.push(...ids);
      queueFlush();
    },
    historyComplete(): Promise<boolean> {
      if (!db) return Promise.resolve(false);
      return new Promise((resolve) => {
        try {
          const request = db.transaction(META, "readonly").objectStore(META).get(COMPLETE_KEY);
          request.onsuccess = () => resolve(request.result === true);
          request.onerror = () => resolve(false);
        } catch {
          resolve(false);
        }
      });
    },
    markHistoryComplete() {
      if (!db) return;
      try {
        db.transaction(META, "readwrite").objectStore(META).put(true, COMPLETE_KEY);
      } catch (err) {
        console.warn("[db] could not record history completeness; next visit re-walks", err);
      }
    },
  };
}
