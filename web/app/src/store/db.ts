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
const VERSION = 1;

export interface EventDb {
  /** Everything held, in no particular order. Empty on any failure. */
  loadAll(): Promise<RawEvent[]>;
  /** Queue events for persistence; flushed on a microtask batch. */
  save(events: RawEvent[]): void;
  /** Remove superseded replaceable versions so the store tracks the cache. */
  evict(ids: string[]): void;
}

function openDb(): Promise<IDBDatabase | null> {
  return new Promise((resolve) => {
    let request: IDBOpenDBRequest;
    try {
      request = indexedDB.open(DB_NAME, VERSION);
    } catch {
      resolve(null);
      return;
    }
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(STORE)) {
        db.createObjectStore(STORE, { keyPath: "id" });
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => resolve(null);
    request.onblocked = () => resolve(null);
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
      // Errors are absorbed: persistence is an optimization, never a dependency.
      tx.onerror = () => {};
    } catch { /* storage gone mid-session; next visit is a first visit */ }
  }

  function queueFlush() {
    if (flushQueued) return;
    flushQueued = true;
    queueMicrotask(flush);
  }

  return {
    loadAll(): Promise<RawEvent[]> {
      if (!db) return Promise.resolve([]);
      return new Promise((resolve) => {
        try {
          const request = db.transaction(STORE, "readonly").objectStore(STORE).getAll();
          request.onsuccess = () => resolve((request.result as RawEvent[]) || []);
          request.onerror = () => resolve([]);
        } catch {
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
  };
}
