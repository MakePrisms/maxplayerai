/**
 * The trade CLI's listing rules, ported 1:1 for the read-only /trades page.
 *
 * Source of truth: crates/maxplayer-trade/src/lib.rs (PR #1107) — `common`,
 * `parse_lot`, `Asset::validate` and `lifecycle`. Every rule below names the
 * Rust it mirrors. test/trade-validate.test.ts replays golden cases signed and
 * judged by that crate (test/fixtures/trade-rust.json) and requires the same
 * verdict on each, so a drift on either side goes red.
 *
 * One deliberate extension, named and tested: `nostr://npub1…` mint addresses
 * (canonical per decode_npub, the rule main already uses for private-job
 * mints) are accepted, because nostr mint support is being added to the CLI in
 * parallel. Everything else the CLI refuses, this refuses.
 *
 * Pure: no DOM, no network, no keys. Never throws on untrusted input.
 */
import { schnorr } from "@noble/curves/secp256k1";
import { sha256 } from "@noble/hashes/sha256";
import { bytesToHex } from "@noble/hashes/utils";
import { TRADE_LOT, TRADE_STATUS } from "../model/kinds.js";
import { JsonError, parseJson, string, struct, u8, uint, unitEnum, type Json } from "./json.js";
import { decodeNpub } from "./nip19.js";

/** A raw NIP-01 event from a relay. Untrusted. */
export interface NostrEvent {
  id: string;
  pubkey: string;
  created_at: number;
  kind: number;
  tags: string[][];
  content: string;
  sig: string;
}

export interface Asset { mint_url: string; unit: string }
export interface Leg extends Asset { net: number }
export interface Lot {
  give: Leg;
  want: Leg;
  maker: string;
  created_at: number;
  expires_at: number;
}
export type Status = "available" | "sold" | "cancelled";
export const STATUSES: readonly Status[] = ["available", "sold", "cancelled"];

/** Listing lifetime: `expires_at == created_at + 86400` (lot_event / parse_lot). */
export const LOT_LIFETIME_SECONDS = 86400;
/** parse_lot: `created_at <= now + 60`. */
export const FUTURE_SKEW_SECONDS = 60;
/** lifecycle: `events.len() <= 256`. */
export const MAX_STATUS_EVENTS = 256;
/** parse_lot: `net <= 1_000_000` on both legs. */
export const MAX_NET = 1_000_000n;
const DEADLINE_POLICY = { long_seconds: 3600n, short_seconds: 900n, min_gap_seconds: 2700n };
const FEE_POLICY = "sender-funds-net-v1";

export class TradeError extends Error {}
const ensure = (cond: unknown, msg: string): void => { if (!cond) throw new TradeError(msg); };

const utf8 = new TextEncoder();
const byteLen = (s: string): number => utf8.encode(s).length;
const HEX64 = /^[0-9a-f]{64}$/;

/* ---------------- event integrity (nostr-sdk Event::verify) ---------------- */

/** True when the id is the NIP-01 hash of the event and the BIP-340 signature verifies. */
export function verifyEvent(e: unknown): e is NostrEvent {
  try {
    const ev = e as NostrEvent;
    if (!ev || typeof ev !== "object") return false;
    if (!HEX64.test(ev.id) || !HEX64.test(ev.pubkey) || !/^[0-9a-f]{128}$/.test(ev.sig)) return false;
    if (!Number.isSafeInteger(ev.created_at) || ev.created_at < 0) return false;
    if (!Number.isInteger(ev.kind) || ev.kind < 0 || ev.kind > 65535) return false;
    if (typeof ev.content !== "string" || !Array.isArray(ev.tags)) return false;
    for (const t of ev.tags) {
      if (!Array.isArray(t) || t.length === 0 || t.some((v) => typeof v !== "string")) return false;
    }
    const id = bytesToHex(sha256(utf8.encode(JSON.stringify([0, ev.pubkey, ev.created_at, ev.kind, ev.tags, ev.content]))));
    if (id !== ev.id) return false;
    return schnorr.verify(ev.sig, ev.id, ev.pubkey);
  } catch {
    return false;
  }
}

/** lib.rs `tag`: exactly one tag with this name, and exactly [name, value]. */
export function tag(e: NostrEvent, name: string): string {
  const found = e.tags.filter((t) => t[0] === name);
  ensure(found.length === 1 && found[0]!.length === 2, `missing or duplicate tag ${name}`);
  return found[0]![1] as string;
}

/** lib.rs `common`: signature, kind, size bounds and protocol tags. */
function common(e: NostrEvent, kind: number): void {
  ensure(verifyEvent(e), "invalid signed event");
  ensure(e.kind === kind, "wrong event kind");
  ensure(byteLen(e.content) <= 8192 && e.tags.length <= 16, "oversized event");
  ensure(e.tags.every((t) => t.every((s) => byteLen(s) <= 512)), "oversized tag");
  ensure(tag(e, "t") === "maxplayer" && tag(e, "v") === "1", "wrong protocol tags");
}

/* ---------------- assets (Asset::new / Asset::validate) ---------------- */

/**
 * Asset::validate: unit `sat`, and the mint URL already canonical — no
 * userinfo, query or fragment; http(s) with a host; byte-equal to its own
 * WHATWG serialization minus trailing slashes. The url crate and the browser's
 * URL both implement WHATWG, so the serialization agrees (default port dropped,
 * host lowercased and punycoded, dot segments resolved, spaces escaped).
 *
 * Extension: `nostr://` + a canonical npub and nothing else.
 */
export function validateAsset(a: Asset): void {
  ensure(a.unit === "sat", "unsupported unit");
  const mint = a.mint_url;
  if (mint.startsWith("nostr://")) {
    ensure(decodeNpub(mint.slice("nostr://".length)) !== null, "invalid nostr mint");
    return;
  }
  let u: URL;
  try { u = new URL(mint); } catch { throw new TradeError("invalid mint URL"); }
  // URL.search is "" for both "no query" and "empty query"; serde's
  // `query().is_none()` refuses the bare "?" too, so look at the serialization.
  ensure(
    u.username === "" && u.password === "" && !u.href.includes("?") && !u.href.includes("#"),
    "ambiguous mint URL",
  );
  ensure(u.protocol === "https:" || u.protocol === "http:", "unsupported mint scheme");
  ensure(u.hostname !== "", "missing mint host");
  const canonical = u.href.replace(/\/+$/, "");
  ensure(mint === canonical, "noncanonical asset");
}

const sameAsset = (a: Asset, b: Asset): boolean => a.mint_url === b.mint_url && a.unit === b.unit;

/* ---------------- listings (parse_lot) ---------------- */

const LOT_FIELDS = ["trade_v", "give", "want", "maker_trade_pubkey", "expires_at", "deadline_policy", "fee_policy"] as const;
// Leg is `#[serde(flatten)] asset` + net, both deny_unknown_fields: one flat map, no positional form.
const LEG_FIELDS = ["mint_url", "unit", "net"] as const;
const DEADLINE_FIELDS = ["long_seconds", "short_seconds", "min_gap_seconds"] as const;

function leg(j: Json | undefined): { asset: Asset; net: bigint } {
  const f = struct(j, LEG_FIELDS, false);
  return { asset: { mint_url: string(f.mint_url), unit: string(f.unit) }, net: uint(f.net) };
}

export type LotVerdict =
  | { ok: true; lot: Lot }
  /** `expired` is the one refusal that is history rather than invalidity: the listing was good and ran out. */
  | { ok: false; error: string; expired: boolean };

/**
 * Every parse_lot rule except the clock. A signed, well-formed listing stays
 * well-formed forever; whether it has expired is a separate question, so the
 * page can show "expired" instead of silently dropping a lot that was real.
 */
export function checkLot(e: NostrEvent): { ok: true; lot: Lot } | { ok: false; error: string } {
  try {
    common(e, TRADE_LOT);
    let f: Record<string, Json>;
    let give, want, tradeV, expiresAt;
    try {
      f = struct(parseJson(e.content), LOT_FIELDS, true);
      tradeV = u8(f.trade_v);
      give = leg(f.give);
      want = leg(f.want);
      string(f.maker_trade_pubkey);
      expiresAt = uint(f.expires_at);
      const d = struct(f.deadline_policy, DEADLINE_FIELDS, true);
      ensure(
        uint(d.long_seconds) === DEADLINE_POLICY.long_seconds
          && uint(d.short_seconds) === DEADLINE_POLICY.short_seconds
          && uint(d.min_gap_seconds) === DEADLINE_POLICY.min_gap_seconds,
        "unsupported deadline policy",
      );
      string(f.fee_policy);
    } catch (err) {
      if (err instanceof JsonError) throw new TradeError(`malformed listing: ${err.message}`);
      throw err;
    }
    ensure(tradeV === 1n, "unknown protocol version");
    validateAsset(give.asset);
    validateAsset(want.asset);
    ensure(!sameAsset(give.asset, want.asset), "same-asset swap");
    ensure(give.net > 0n && want.net > 0n && give.net <= MAX_NET && want.net <= MAX_NET, "invalid or oversized lot");
    ensure(expiresAt === BigInt(e.created_at) + BigInt(LOT_LIFETIME_SECONDS), "expiry is not created_at + 24h");
    ensure(string(f.fee_policy) === FEE_POLICY, "unsupported fee policy");
    ensure(string(f.maker_trade_pubkey) === e.pubkey, "maker identity mismatch");
    const pairs: [string, string][] = [
      ["g", give.asset.mint_url],
      ["w", want.asset.mint_url],
      ["u", give.asset.unit],
      ["x", want.asset.unit],
      ["expiration", expiresAt.toString()],
    ];
    for (const [name, value] of pairs) ensure(tag(e, name) === value, "content/tag disagreement");
    return {
      ok: true,
      lot: {
        give: { ...give.asset, net: Number(give.net) },
        want: { ...want.asset, net: Number(want.net) },
        maker: e.pubkey,
        created_at: e.created_at,
        expires_at: Number(expiresAt),
      },
    };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : String(err) };
  }
}

/** parse_lot(e, now), verdict for verdict. */
export function parseLot(e: NostrEvent, now: number): LotVerdict {
  const r = checkLot(e);
  if (!r.ok) return { ...r, expired: false };
  if (e.created_at > now + FUTURE_SKEW_SECONDS) return { ok: false, error: "future listing", expired: false };
  if (now >= r.lot.expires_at) return { ok: false, error: "expired listing", expired: true };
  return r;
}

/* ---------------- status chains (lifecycle) ---------------- */

const REVISION_FIELDS = ["trade_v", "lot_id", "seq", "prev", "status"] as const;

interface Revision { seq: bigint; prev: string; status: Status; event: NostrEvent }

export type ChainVerdict =
  | { ok: true; status: Status; events: NostrEvent[] }
  /** The chain is forked, gapped, foreign, malformed or missing: quarantined, never shown as real. */
  | { ok: false; error: string };

/**
 * lifecycle(lot, events). Callers pass only events AUTHORED BY THE MAKER, as
 * the CLI's discover() does by querying with `.author(lot.pubkey)`: an
 * outsider's status must not be able to quarantine somebody else's listing.
 * `events` is checked for the 256 bound before de-duplication, as in Rust.
 */
export function lifecycle(lot: NostrEvent, events: readonly NostrEvent[]): ChainVerdict {
  try {
    ensure(events.length <= MAX_STATUS_EVENTS, "status history exceeds bound");
    const revisions = new Map<bigint, Revision>();
    const seen = new Set<string>();
    for (const e of events) {
      // The id is checked by common() before anything trusts it; a forged
      // duplicate id fails there rather than being skipped here.
      if (seen.has(e?.id)) continue;
      common(e, TRADE_STATUS);
      seen.add(e.id);
      ensure(e.pubkey === lot.pubkey, "unauthorized status signer");
      let r: Revision;
      let lotId: string;
      let tradeV: bigint;
      try {
        const f = struct(parseJson(e.content), REVISION_FIELDS, true);
        tradeV = u8(f.trade_v);
        lotId = string(f.lot_id);
        r = { seq: uint(f.seq), prev: string(f.prev), status: unitEnum(f.status, STATUSES), event: e };
      } catch (err) {
        if (err instanceof JsonError) throw new TradeError(`malformed status: ${err.message}`);
        throw err;
      }
      ensure(tradeV === 1n && lotId === lot.id && tag(e, "e") === lotId, "wrong status binding");
      ensure(!revisions.has(r.seq), "forked status history");
      revisions.set(r.seq, r);
    }
    let prev = lot.id;
    let status: Status = "available";
    let seq = 0n;
    const chain: NostrEvent[] = [];
    for (const n of [...revisions.keys()].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0))) {
      const r = revisions.get(n) as Revision;
      ensure(n === seq + 1n && r.prev === prev, "incomplete status history");
      ensure(status === "available", "terminal status cannot reopen or revise");
      ensure(seq !== 0n || r.status === "available", "initial status must be available");
      prev = r.event.id;
      status = r.status;
      seq = n;
      chain.push(r.event);
    }
    ensure(seq > 0n, "initial available revision missing");
    return { ok: true, status, events: chain };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : String(err) };
  }
}
