/**
 * Typed model — raw relay events in, typed records out.
 *
 * Parsing happens once, here, at the edge. No view, metric or store may reach
 * into a raw tag array: if a tag shape changes, this file is the only casualty.
 * A malformed event yields `null` rather than throwing — one bad event from an
 * open relay must never take the page down.
 */
import {
  ACCEPT, AWARD, CLAIM, FEEDBACK, HEARTBEAT, OFFER, PROFILE, RECEIPT,
  RESULT, SELF_TRADE_TAG, TRADE_STAGES, type Stage,
} from "./kinds.js";

/** A raw NIP-01 event as the relay sends it. Untrusted input. */
export interface RawEvent {
  id: string;
  kind: number;
  pubkey: string;
  created_at: number;
  tags?: string[][];
  content?: string;
  sig?: string;
}

export interface AdvertisementTag { name: string; values: string[] }

/**
 * One parsed record. A single shape with optional fields, not a union: nearly
 * every consumer branches on `stage`/`kind` at runtime and reads a handful of
 * fields — the optional shape keeps that direct, and `parseEvent` is the only
 * writer so the invariants live in one place.
 */
export interface ParsedEvent {
  id: string;
  kind: number;
  pubkey: string;
  created_at: number;
  /** Present exactly when the event belongs to a trade. */
  stage: Stage | null;
  offerId: string | null;

  buyer?: string;
  seller?: string;
  selfTrade?: boolean;
  amount?: number | null;
  targetSeller?: string | null;
  description?: string;
  outputType?: string | null;
  deadline?: number | null;
  status?: string | null;
  hasPaymentRequest?: boolean;
  agents?: string[];
  claimId?: string | null;
  awardedSeller?: string | null;
  harness?: string | null;
  model?: string | null;
  deliveryVia?: string | null;
  commit?: string | null;
  wallTimeSeconds?: number | null;
  reason?: string;
  reasonCode?: string | null;
  feedbackClass?: string | null;
  terminal?: boolean;
  d?: string;
  version?: string | null;
  accepting?: string | null;
  queueDepth?: number | null;
  rateSats?: number | null;
  acceptedMints?: string[];
  advertisementTags?: AdvertisementTag[];
  advertisementContent?: Record<string, unknown> | null;
  name?: string | null;
  about?: string | null;
  profile?: Record<string, unknown>;
}

const tagsNamed = (event: RawEvent, name: string): string[][] =>
  (event.tags || []).filter((t) => t[0] === name);

const firstTag = (event: RawEvent, ...names: string[]): string | null => {
  for (const name of names) {
    const t = tagsNamed(event, name)[0];
    if (t && t[1]) return t[1];
  }
  return null;
};

/** Every value on the first multi-value tag with this name. */
const tagValues = (event: RawEvent, name: string): string[] => {
  const t = tagsNamed(event, name)[0];
  return t ? t.slice(1).filter((v) => typeof v === "string" && v.length > 0) : [];
};

/**
 * Preserve the complete public advertisement, including fields introduced by
 * a newer seller than this reader knows about. The renderer escapes every
 * value; this layer only normalises tag cells to strings.
 */
function advertisementTags(event: RawEvent): AdvertisementTag[] {
  return (event.tags || [])
    .filter((t) => Array.isArray(t) && typeof t[0] === "string" && t[0].length > 0)
    .map((t) => ({ name: t[0] as string, values: t.slice(1).map((v) => String(v)) }));
}

/** First finite number found under any of the given tag names. */
function firstNumber(event: RawEvent, ...names: string[]): number | null {
  for (const name of names) {
    for (const t of tagsNamed(event, name)) {
      const n = Number.parseFloat(t[1] ?? "");
      if (Number.isFinite(n)) return n;
    }
  }
  return null;
}

/**
 * Nostr ids and pubkeys are 32 bytes of lowercase hex. Nothing else is one.
 *
 * Enforced at the boundary because a relay is untrusted input: these values
 * end up in markup and `data-` attributes, so a non-hex "pubkey" would be an
 * injection path. Rejecting the event here means nothing downstream has to
 * remember to escape them.
 */
const HEX32 = /^[0-9a-f]{64}$/;
export const isHex32 = (s: unknown): s is string => typeof s === "string" && HEX32.test(s);

/**
 * The offer a trade event belongs to.
 *
 * Prefer an `e` tag explicitly marked `root`: an award also e-tags the winning
 * claim, so taking the first `e` blindly can key a trade off a claim id and
 * split one trade into two.
 */
export function rootOfferId(event: RawEvent): string | null {
  const es = tagsNamed(event, "e");
  for (const t of es) if (t[3] === "root" && isHex32(t[1])) return t[1] as string;
  for (const t of es) if (isHex32(t[1])) return t[1] as string;
  // An offer id becomes a DOM key and a rendered label, so a tag whose value
  // is not an event id is not one, whatever it claims.
  const named = firstTag(event, "E", "offer", "root");
  return isHex32(named) ? named : null;
}

/** The non-root event selected by an award (the winning claim). */
function awardClaimId(event: RawEvent, offerId: string | null): string | null {
  for (const t of tagsNamed(event, "e")) {
    if (isHex32(t[1]) && t[1] !== offerId) return t[1] as string;
  }
  return null;
}

/** The other participant on a buyer-authored award is the selected seller. */
function awardSeller(event: RawEvent): string | null {
  for (const t of tagsNamed(event, "p")) {
    if (isHex32(t[1]) && t[1] !== event.pubkey) return t[1] as string;
  }
  return null;
}

function parseJsonContent(event: RawEvent): Record<string, unknown> {
  try {
    const value = JSON.parse(event.content || "{}");
    return value && typeof value === "object" ? (value as Record<string, unknown>) : {};
  } catch { return {}; }
}

/** A `param` tag is a named value: ["param", "deadline", "1785184881"]. */
export function param(event: RawEvent, name: string): string | null {
  for (const t of event.tags || []) if (t[0] === "param" && t[1] === name) return t[2] ?? null;
  return null;
}

/**
 * protocol-v1 §7.1: `reason_code` names the class. The vocabulary is
 * extensible, so a reader that meets an unknown code MUST fall back to
 * `status` and MUST NOT treat the event as malformed.
 *
 * A Map, not an object literal — a correctness requirement, not style:
 * `reason_code` is an arbitrary relay string, and an object lookup answers for
 * inherited names too (`obj["constructor"]` is truthy), so an unknown code
 * would be misread as a class and never fall back to `status`. A Map has no
 * prototype chain to consult; the safety is structural.
 */
const REASON_CODE_CLASS = new Map<string, string>([
  ["below_rate", "refusal"],
  ["unsupported_version", "refusal"],
  ["mint_incompatible", "refusal"],
  ["at_capacity", "refusal"],
  ["execution_failed", "error"],
  ["delivery_failed", "error"],
  ["no_sentinel", "refusal"],
]);

/** protocol-v1 §7.2. `progress` is explicitly non-terminal; the rest end an attempt. */
const TERMINAL_FEEDBACK_CLASSES: ReadonlySet<string> = new Set(["claim_released", "refusal", "error"]);

/**
 * The protocol class of a feedback event, read from tags only.
 *
 * protocol-v1 §7.1 is explicit: "A reader MUST NOT parse `content` to
 * determine the class." `feedbackReason` below reads content on purpose, but
 * it is a DISPLAY helper and must never decide whether work ended.
 */
export function feedbackClass(event: RawEvent): string | null {
  const code = firstTag(event, "reason_code");
  const known = code ? REASON_CODE_CLASS.get(code) : null;
  if (known) return known;
  return firstTag(event, "status") || null;
}

/**
 * Whether a feedback event ends the awarded seller's attempt.
 *
 * Unclassified feedback is NOT terminal: terminalizing on an event we cannot
 * classify is what let a routine `progress` note clear the work lamp before
 * any result. The deadline clock is what stops an unclassified job running
 * forever.
 */
export function feedbackIsTerminal(event: RawEvent): boolean {
  const cls = feedbackClass(event);
  return cls != null && TERMINAL_FEEDBACK_CLASSES.has(cls);
}

/**
 * A feedback event's reason is the code before the first colon
 * ("claim_released: ..."), not free text. DISPLAY ONLY — it reads `content`,
 * so §7.1 forbids using it to decide the event's class; use `feedbackClass`.
 */
export function feedbackReason(event: RawEvent): string {
  const head = String(event.content || "").trim().split(":")[0]?.trim() ?? "";
  if (head && head.length <= 40 && /^[a-z0-9_\- ]+$/i.test(head)) return head;
  return firstTag(event, "reason", "code", "status") || "unspecified";
}

/**
 * Parse one event into a typed record, or null if it is not something we
 * model. `stage` is present exactly when the event belongs to a trade, so
 * callers can branch on it without knowing kind numbers.
 */
/**
 * Parse-once cache. Events are immutable and id-addressed, and the market
 * derivations re-walk the full event set many times per recompute — without
 * this, one board refresh at ~2,700 events re-parses tens of thousands of
 * times and visibly stalls the main thread (the ticker froze on every poll).
 */
const PARSE_CACHE = new Map<string, ParsedEvent | null>();

export function parseEvent(event: RawEvent | null | undefined): ParsedEvent | null {
  if (!event || typeof event.kind !== "number" || typeof event.created_at !== "number") return null;
  if (!isHex32(event.id) || !isHex32(event.pubkey)) return null;
  const hit = PARSE_CACHE.get(event.id);
  if (hit !== undefined) return hit;
  const parsed = parseEventUncached(event);
  PARSE_CACHE.set(event.id, parsed);
  return parsed;
}

function parseEventUncached(event: RawEvent): ParsedEvent | null {

  const base: ParsedEvent = {
    id: event.id,
    kind: event.kind,
    pubkey: event.pubkey,
    created_at: event.created_at,
    stage: TRADE_STAGES[event.kind] ?? null,
    offerId: null,
  };

  switch (event.kind) {
    case OFFER:
      return { ...base, offerId: event.id, buyer: event.pubkey,
               // A buyer commissioning its own seller marks the offer
               // ["t","self-trade"]. A structured predicate, not prose.
               selfTrade: tagsNamed(event, "t").some((t) => t[1] === SELF_TRADE_TAG),
               amount: firstNumber(event, "amount", "rate", "price", "sats"),
               targetSeller: firstTag(event, "p"),
               // The job itself is the `i` (input) tag. Offer content is empty
               // in practice, so reading it yields a field that is never set.
               description: firstTag(event, "i") || "",
               outputType: firstTag(event, "output"),
               deadline: Number.parseInt(param(event, "deadline") ?? "", 10) || null };
    case CLAIM:
      return { ...base, offerId: rootOfferId(event), seller: event.pubkey,
               status: firstTag(event, "status"), hasPaymentRequest: Boolean(firstTag(event, "creq")),
               agents: tagValues(event, "agents") };
    // An award is the moment work starts. Preserve the exact winning claim and
    // seller: multiple runners may claim one offer, so the first claim on the
    // trade is not necessarily the runner the racer selected.
    case AWARD: {
      const offerId = rootOfferId(event);
      return { ...base, offerId, buyer: event.pubkey,
               claimId: awardClaimId(event, offerId),
               awardedSeller: awardSeller(event),
               status: firstTag(event, "status") };
    }
    case ACCEPT:
      return { ...base, offerId: rootOfferId(event), buyer: event.pubkey,
               status: firstTag(event, "status") };
    case RESULT:
      return { ...base, offerId: rootOfferId(event), seller: event.pubkey,
               amount: firstNumber(event, "amount", "amt", "sats"),
               // What actually did the work, and how it was handed over.
               harness: firstTag(event, "harness"),
               model: firstTag(event, "model"),
               deliveryVia: firstTag(event, "delivery"),
               commit: firstTag(event, "commit"),
               wallTimeSeconds: firstNumber(event, "wall_time") };
    case FEEDBACK:
      return { ...base, offerId: rootOfferId(event), seller: event.pubkey,
               // `reason` is for display. `feedbackClass`/`terminal` are the
               // structural read the protocol requires — see §7.1.
               reason: feedbackReason(event),
               status: firstTag(event, "status"),
               reasonCode: firstTag(event, "reason_code"),
               feedbackClass: feedbackClass(event),
               terminal: feedbackIsTerminal(event) };
    case RECEIPT:
      return { ...base, offerId: rootOfferId(event),
               amount: firstNumber(event, "amount", "amt", "sats") };
    case HEARTBEAT:
      return { ...base,
               d: firstTag(event, "d") || "",
               version: firstTag(event, "v"),
               accepting: firstTag(event, "accepting"),
               queueDepth: firstNumber(event, "queue_depth"),
               rateSats: firstNumber(event, "rate"),
               acceptedMints: tagValues(event, "accepted_mints"),
               agents: tagValues(event, "agents"),
               advertisementTags: advertisementTags(event),
               advertisementContent: String(event.content || "").trim() ? parseJsonContent(event) : null };
    case PROFILE: {
      const p = parseJsonContent(event);
      return { ...base,
               name: (p.name as string) || (p.display_name as string) || null,
               about: (p.about as string) || null,
               profile: p };
    }
    default:
      return null;
  }
}
