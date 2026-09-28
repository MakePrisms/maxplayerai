import {
  OFFER,
  CLAIM,
  RESULT,
  AWARD,
  ACCEPT,
  HTTP_AUTH,
  PROFILE,
} from "../model/kinds.js";
import { schnorr } from "@noble/curves/secp256k1";
import { sha256 } from "@noble/hashes/sha256";
import { bytesToHex, hexToBytes } from "@noble/hashes/utils";
export const NEMO =
  "f0a77fbdcd2a2dc944310fcb1e5cc03a0120087fb81290822d2420084fa6d1ce";
export const RELAY_HTTP = "https://relay.maxplayer.ai/events";
export const RELAY_WS = "wss://relay.maxplayer.ai";
export interface Event {
  id: string;
  pubkey: string;
  created_at: number;
  kind: number;
  tags: string[][];
  content: string;
  sig: string;
}
export const hash = (s: string) =>
  bytesToHex(sha256(new TextEncoder().encode(s)));
export const bytes = (s: string) => new TextEncoder().encode(s).length;
export const now = () => Math.floor(Date.now() / 1000);
export const namespace = [
  ["t", "maxplayer"],
  ["v", "1"],
];
export function sign(
  secret: string,
  kind: number,
  tags: string[][],
  content = "",
  created_at = now(),
): Event {
  const pubkey = bytesToHex(schnorr.getPublicKey(secret));
  const id = hash(JSON.stringify([0, pubkey, created_at, kind, tags, content]));
  return {
    id,
    pubkey,
    created_at,
    kind,
    tags,
    content,
    sig: bytesToHex(schnorr.sign(id, secret)),
  };
}
export function verify(e: Event): void {
  if (
    !e ||
    !/^[a-f0-9]{64}$/.test(e.id) ||
    !/^[a-f0-9]{64}$/.test(e.pubkey) ||
    !/^[a-f0-9]{128}$/.test(e.sig) ||
    !Number.isSafeInteger(e.created_at) ||
    !Number.isSafeInteger(e.kind) ||
    typeof e.content !== "string" ||
    !Array.isArray(e.tags) ||
    e.tags.some(
      (t) =>
        !Array.isArray(t) || !t.length || t.some((v) => typeof v !== "string"),
    ) ||
    hash(
      JSON.stringify([0, e.pubkey, e.created_at, e.kind, e.tags, e.content]),
    ) !== e.id ||
    !schnorr.verify(e.sig, e.id, e.pubkey)
  )
    throw Error("Invalid signed event");
}
export function one(e: Event, key: string, sub?: string): string[] {
  const found = e.tags.filter(
    (t) => t[0] === key && (sub === undefined || t[1] === sub),
  );
  if (found.length !== 1) throw Error(`Missing or duplicate ${key}`);
  return found[0]!;
}
function equal(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b))
    throw Error("Unexpected event shape");
}
export function promptText(input: string) {
  const text = input.trim();
  if (
    !text ||
    [...text].length > 1000 ||
    bytes(text) > 4000 ||
    /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(
      text,
    )
  )
    throw Error("Use 1–1,000 characters (up to 4,000 bytes).");
  return text;
}
export const models =
  "vantor stradale apex aerion velora radian corsair solaro torven caldera virelli ignis chicane slipstream redline camber aero apexion valtor revora ventaro torqen astera voltane cravon serran veyra falcar valden aleron zenora meridan orvex savaro draeven varion thalor kestrel pinion kerbline".split(" ");
export const characters =
  "nero veloce corsa comet sprint rosso tempest vector falcon spectre foudre strada fulmine rapida nocturne ember thunder lightning carbon titanium silver crimson scarlet obsidian onyx cobalt graphite jet sonic swift rapid fierce blazing charged nimble dusk dawn stealth storm fire frost".split(" ");
export const designations = "gt gtr rs gts rr".split(" ");
const pick = (a: string[]) =>
  a[crypto.getRandomValues(new Uint32Array(1))[0]! % a.length]!;
export function generatedName() {
  const form = pick(["mc", "md", "mcd"]);
  return [
    pick(models),
    ...(form.includes("c") ? [pick(characters)] : []),
    ...(form.includes("d") ? [pick(designations)] : []),
  ].join("-");
}
export function validName(name: string) {
  const p = name.split("-");
  return (
    models.includes(p[0]!) &&
    ((p.length === 2 &&
      (characters.includes(p[1]!) || designations.includes(p[1]!))) ||
      (p.length === 3 &&
        characters.includes(p[1]!) &&
        designations.includes(p[2]!)))
  );
}
export function identity() {
  const secret = bytesToHex(schnorr.utils.randomPrivateKey());
  return { secret, name: generatedName() };
}
export function offerTags(prompt: string, time: number, seller = NEMO) {
  return [
    ["i", prompt],
    ["output", "text/plain"],
    ["amount", "0", "sat"],
    ["param", "deadline", String(time + 300)],
    ["param", "accepts-delivery", "inline"],
    ["param", "payment", "none"],
    ["p", seller],
    ...namespace,
  ];
}
export function selectionTags(o: Event, c: Event) {
  return [
    ["status", "accepted"],
    ["e", o.id, "", "root"],
    ["e", c.id],
    ["p", o.pubkey],
    ["p", c.pubkey],
    ...namespace,
  ];
}
export function validateOffer(o: Event, seller = NEMO) {
  verify(o);
  if (o.kind !== OFFER || o.content !== "") throw Error("Invalid offer");
  const task = one(o, "i")[1]!;
  if (promptText(task) !== task) throw Error("Untrimmed prompt");
  equal(o.tags, offerTags(task, o.created_at, seller));
}
export function trade(e: Event) {
  verify(e);
  equal(one(e, "t"), ["t", "maxplayer"]);
  equal(one(e, "v"), ["v", "1"]);
}
export function claim(c: Event, o: Event, seller = NEMO) {
  trade(c);
  if (
    c.kind !== CLAIM ||
    c.pubkey !== seller ||
    c.content !== "" ||
    c.created_at < o.created_at - 60 ||
    c.created_at > o.created_at + 300
  )
    throw Error("Invalid claim");
  equal(one(c, "status"), ["status", "processing"]);
  equal(one(c, "e"), ["e", o.id, "", "root"]);
  equal(
    c.tags.filter((t) => t[0] === "p"),
    [
      ["p", o.pubkey],
      ["p", seller],
    ],
  );
  equal(one(c, "payment"), ["payment", "none"]);
  if (
    c.tags.some(
      (t) =>
        ![
          "status",
          "e",
          "p",
          "payment",
          "t",
          "v",
          "agents",
          "capabilities",
          "harness_family",
          "harness_model",
        ].includes(t[0]!),
    )
  )
    throw Error("Unsupported claim");
}
export function preimage(o: Event, result: Event, seller = NEMO) {
  return JSON.stringify([
    "maxplayer/v1/receipt-preimage",
    hash(`${o.id}|${one(o, "i")[1]}|0`),
    o.id,
    0,
    "sat",
    o.pubkey,
    seller,
    hash(result.content),
    "inline",
    "none",
  ]);
}
export function result(r: Event, o: Event, c: Event, seller = NEMO) {
  claim(c, o, seller);
  trade(r);
  if (
    r.kind !== RESULT ||
    r.pubkey !== seller ||
    !r.content ||
    bytes(r.content) > 16384 ||
    r.created_at < c.created_at
  )
    throw Error("Invalid answer");
  const expected = [
    ["e", o.id, "", "root"],
    ["p", o.pubkey],
    ["delivery", "inline"],
    ["output", "text/plain"],
    ["amount", "0", "sat"],
    ["job-hash", hash(`${o.id}|${one(o, "i")[1]}|0`)],
  ];
  for (const t of expected) equal(one(r, t[0]!), t);
  if (
    r.tags.some(
      (t) =>
        ![
          "e",
          "p",
          "delivery",
          "output",
          "amount",
          "job-hash",
          "sig",
          "t",
          "v",
          "harness",
          "usage_transport",
          "metadata_trust",
          "wall_time",
          "model",
          "tokens",
          "cost",
        ].includes(t[0]!),
    )
  )
    throw Error("Unsupported answer tags");
  const sig = one(r, "sig");
  if (
    sig.length !== 3 ||
    sig[1] !== "seller" ||
    !schnorr.verify(sig[2]!, hash(preimage(o, r, seller)), seller)
  )
    throw Error("Invalid answer co-signature");
  return { resultId: r.id, integrityHash: hash(r.content), answer: r.content };
}
export interface Evidence {
  offer?: Event;
  claim?: Event;
  result?: Event;
}
export function outgoing(e: Event, evidence: Evidence = {}, seller = NEMO) {
  verify(e);
  if (e.kind === PROFILE) {
    const p = JSON.parse(e.content);
    if (
      bytes(e.content) > 256 ||
      e.tags.length ||
      Object.keys(p).sort().join() !== "display_name,name" ||
      typeof p.name !== "string" ||
      !validName(p.name) ||
      p.display_name !== p.name
    )
      throw Error("Invalid profile");
    return;
  }
  if (e.kind === OFFER) return validateOffer(e, seller);
  if (![AWARD, ACCEPT].includes(e.kind)) throw Error("Forbidden kind");
  const o = evidence.offer!,
    c = evidence.claim!;
  validateOffer(o, seller);
  claim(c, o, seller);
  if (
    e.pubkey !== o.pubkey ||
    e.content !== "" ||
    e.created_at < c.created_at ||
    (e.kind === AWARD && e.created_at > o.created_at + 300)
  )
    throw Error("Invalid selection");
  equal(e.tags, selectionTags(o, c));
  if (e.kind === ACCEPT) {
    result(evidence.result!, o, c, seller);
    if (e.created_at < evidence.result!.created_at)
      throw Error("Acceptance predates answer");
  }
}
export function auth(
  secret: string,
  eventBody: string,
  time = now(),
  url = RELAY_HTTP,
) {
  return sign(
    secret,
    HTTP_AUTH,
    [
      ["u", url],
      ["method", "POST"],
      ["payload", hash(eventBody)],
      ["nonce", bytesToHex(crypto.getRandomValues(new Uint8Array(16)))],
    ],
    "",
    time,
  );
}
export { hexToBytes };
