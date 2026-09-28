import { OFFER, PROFILE } from "../model/kinds.js";
import { Event, identity, offerTags, promptText, sign } from "./wire.js";
export interface RecordState {
  version: 1;
  secret: string;
  name: string;
  profile: Event;
  offer: Event;
  profileAck?: boolean;
  offerAck?: boolean;
  claim?: Event;
  award?: Event;
  awardAck?: boolean;
  result?: Event;
  binding?: { resultId: string; integrityHash: string; answer: string };
  accept?: Event;
  acceptAck?: boolean;
  phase: string;
}
export function createRecord(input: string, time: number, visitor = identity()): RecordState {
  const prompt = promptText(input),
    { secret, name } = visitor;
  return {
    version: 1,
    secret,
    name,
    profile: sign(
      secret,
      PROFILE,
      [],
      JSON.stringify({ name, display_name: name }),
      time,
    ),
    offer: sign(secret, OFFER, offerTags(prompt, time), "", time),
    phase: "publishing",
  };
}
export async function openStore(factory: IDBFactory = indexedDB) {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const r = factory.open("maxplayer-try", 1);
    r.onupgradeneeded = () => r.result.createObjectStore("buyer");
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
  const transact = (
    update?: (old: RecordState | undefined) => RecordState | undefined,
  ) =>
    new Promise<RecordState | undefined>((resolve, reject) => {
      const tx = db.transaction("buyer", update ? "readwrite" : "readonly"),
        store = tx.objectStore("buyer"),
        r = store.get("one");
      let value: RecordState | undefined;
      r.onsuccess = () => {
        try {
          value = update ? update(r.result) : r.result;
          if (update && value) store.put(value, "one");
        } catch (e) {
          tx.abort();
          reject(e);
        }
      };
      tx.oncomplete = () => resolve(value);
      tx.onabort = () => reject(tx.error ?? Error("Storage unavailable"));
      tx.onerror = () => reject(tx.error);
    });
  return {
    read: () => transact(),
    reserve: (candidate: RecordState) => transact((old) => old ?? candidate),
    update: (fn: (old: RecordState) => RecordState) =>
      transact((old) => {
        if (!old) throw Error("Browser identity was removed");
        return fn(old);
      }),
    clear: () =>
      new Promise<void>((resolve, reject) => {
        const tx = db.transaction("buyer", "readwrite");
        tx.objectStore("buyer").delete("one");
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
      }),
    close: () => db.close(),
  };
}
export type Store = Awaited<ReturnType<typeof openStore>>;
