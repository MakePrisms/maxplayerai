import {
  RECEIPT,
  OFFER,
  CLAIM,
  RESULT,
  AWARD,
  ACCEPT,
  PROFILE,
} from "../src/model/kinds.js";
/** Offline integration: real loopback Nostr relay, stub free inline seller, browser controller,
 * API verification and injected HTTP->WS bridge. NOT deployed Buzz HTTP/NIP-98 admission proof.
 * Run: node --import tsx scripts/try-local.ts ws://127.0.0.1:<port>
 */
import assert from "node:assert/strict";
import { IDBFactory } from "fake-indexeddb";
import { schnorr } from "@noble/curves/secp256k1";
import { bytesToHex } from "@noble/hashes/utils";
import {
  auth,
  claim,
  Event,
  hash,
  namespace,
  now,
  offerTags,
  one,
  outgoing,
  preimage,
  sign,
  verify,
} from "../src/try/wire.js";
import { createRecord, openStore } from "../src/try/store.js";
import { step, Transport } from "../src/try/controller.js";
import { forward } from "../api/try.js";
const url = new URL(process.argv[2] ?? "");
if (
  url.protocol !== "ws:" ||
  !["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)
)
  throw Error("Loopback relay required");
const sellerKey = "02".repeat(32),
  seller = bytesToHex(schnorr.getPublicKey(sellerKey));
const socket = new WebSocket(url),
  pending = new Map<string, (m: any) => void>();
await new Promise<void>((resolve, reject) => {
  socket.onopen = () => resolve();
  socket.onerror = reject;
});
socket.onmessage = ({ data }) => {
  const m = JSON.parse(String(data));
  pending.get(m[1])?.(m);
};
const send = (e: Event) =>
  new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(Error("publish timeout")), 5000);
    pending.set(e.id, (m) => {
      clearTimeout(timer);
      pending.delete(e.id);
      if (m[0] === "OK" && m[2]) resolve();
      else reject(Error("rejected"));
    });
    socket.send(JSON.stringify(["EVENT", e]));
  });
const read = (filter: object) =>
  new Promise<Event[]>((resolve, reject) => {
    const id = crypto.randomUUID(),
      events: Event[] = [];
    const timer = setTimeout(() => reject(Error("query timeout")), 5000);
    pending.set(id, (m) => {
      if (m[0] === "EVENT") events.push(m[2]);
      if (m[0] === "EOSE") {
        clearTimeout(timer);
        pending.delete(id);
        socket.send(JSON.stringify(["CLOSE", id]));
        resolve(events);
      }
    });
    socket.send(JSON.stringify(["REQ", id, filter]));
  });
const db = await openStore(new IDBFactory()),
  record = createRecord("Explain why slick tyres work.", now());
record.offer = sign(
  record.secret,
  OFFER,
  offerTags(one(record.offer, "i")[1]!, record.offer.created_at, seller),
  "",
  record.offer.created_at,
);
await db.reserve(record);
let offer: Event | undefined, selected: Event | undefined;
const kinds: number[] = [];
const bridge: typeof fetch = async (_url, init) => {
  const event = JSON.parse(init!.body as string) as Event;
  verify(event);
  await send(event);
  kinds.push(event.kind);
  if (event.kind === OFFER) {
    offer = event;
    selected = sign(sellerKey, CLAIM, [
      ["status", "processing"],
      ["e", event.id, "", "root"],
      ["p", event.pubkey],
      ["p", seller],
      ["payment", "none"],
      ...namespace,
    ]);
    claim(selected, event, seller);
    await send(selected);
  }
  if (event.kind === AWARD) {
    assert.ok(offer && selected);
    outgoing(event, { offer, claim: selected }, seller);
    const content =
      "Slick tyres maximize contact on dry tarmac. They are unsuitable for standing water.";
    const shell = sign(sellerKey, RESULT, [], content);
    const cosig = bytesToHex(
      schnorr.sign(hash(preimage(offer, shell, seller)), sellerKey),
    );
    const result = sign(
      sellerKey,
      RESULT,
      [
        ["e", offer.id, "", "root"],
        ["p", offer.pubkey],
        ["delivery", "inline"],
        ["output", "text/plain"],
        ["amount", "0", "sat"],
        ["job-hash", hash(`${offer.id}|${one(offer, "i")[1]}|0`)],
        ["sig", "seller", cosig],
        ...namespace,
      ],
      content,
    );
    await send(result);
  }
  return Response.json({ accepted: true, event_id: event.id });
};
const transport: Transport = {
  read,
  async publish(event, secret, evidence) {
    const eventBody = JSON.stringify(event),
      res = await forward(
        JSON.stringify({
          eventBody,
          relayAuth: auth(secret, eventBody),
          evidence,
        }),
        true,
        bridge,
        now(),
        seller,
      );
    assert.equal(res.status, 200, await res.text());
  },
};
try {
  for (let i = 0; i < 4; i++) {
    const s = await step(db, transport, now(), seller);
    if (s?.phase === "done") break;
  }
  const done = await db.read();
  assert.equal(done?.phase, "done");
  assert.deepEqual(kinds, [PROFILE, OFFER, AWARD, ACCEPT]);
  assert.equal((await read({ kinds: [RECEIPT], limit: 10 })).length, 0);
  console.log(
    JSON.stringify({
      passed: true,
      buyerWrites: kinds,
      phase: done.phase,
      answerVerified: !!done.binding,
      noPaymentReceipt: true,
    }),
  );
} finally {
  db.close();
  socket.close();
}
