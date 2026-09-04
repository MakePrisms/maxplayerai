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
 * One serving harness and the model it reported, from a `["harness_model",
 * family, model]` tag. Paired on the wire, so paired here: a flat list of
 * models could not say which harness runs which.
 */
export interface HarnessModel { family: string; model: string }

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
  /** Offer only: it settles with no payment — see `settlesWithoutPayment`. */
  free?: boolean;
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
  acceptedSeller?: string | null;
  receiptSeller?: string | null;
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
  harnessFamilies?: string[];
  harnessModels?: HarnessModel[];
  capabilities?: string[];
  harnessVariant?: string | null;
  hardware?: string | null;
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
 * One wire value under the "stated or absent" contract of `docs/protocol-v1.md`
 * §4.5.2: trimmed, and null when nothing survives. Blank and all-whitespace are
 * ABSENT, and a reader must treat them identically to a missing tag.
 *
 * Trimming rather than merely rejecting is what makes this reader agree with the
 * emitter. A padded `" claude-code "` kept as-is would never equal the
 * `claude-code` a buyer names, so a seat would advertise a family it could never
 * be matched on. Interior whitespace is CONTENT and survives — only the edges
 * are noise.
 *
 * ⚠ APPLIED AT THE FIVE #784 READERS INDIVIDUALLY, NEVER INSIDE `tagValues` OR
 * `firstTag`. Those two are shared with `agents` and `accepted_mints`, and
 * changing how a mint list parses is not something to do as a side effect of a
 * capability change. Unifying at the helper is a coherent proposal; it is a
 * separate one. This mirrors `stated` in `crates/maxplayer-core/src/heartbeat.rs`,
 * which carries the same restriction for the same reason.
 */
const stated = (value: string | null | undefined): string | null => {
  const trimmed = (value ?? "").trim();
  return trimmed.length > 0 ? trimmed : null;
};

/** `stated` across a list, dropping the values that state nothing. */
const statedValues = (values: string[]): string[] =>
  values.map(stated).filter((v): v is string => v !== null);

/**
 * Every `["harness_model", family, model]` tag on the event.
 *
 * Deliberately NOT `tagValues`, which returns the cells of the FIRST tag with a
 * name. A seat serving two harnesses emits two `harness_model` tags, so reading
 * only the first would show one model and silently drop the rest — an absence
 * indistinguishable from a seat that runs a single harness.
 *
 * A pair missing either cell is dropped: a family with no model states nothing,
 * and a model with no family cannot say which harness reported it.
 *
 * Both halves go through `stated`, so an all-whitespace half is as unpairable as
 * an empty one. The pair is the unit — a stated model under a blank family is
 * not a partial answer worth salvaging.
 */
function harnessModels(event: RawEvent): HarnessModel[] {
  const pairs: HarnessModel[] = [];
  for (const t of tagsNamed(event, "harness_model")) {
    const family = stated(t[1]);
    const model = stated(t[2]);
    if (!family || !model) continue;
    if (!pairs.some((p) => p.family === family && p.model === model)) pairs.push({ family, model });
  }
  return pairs;
}

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

/**
 * The seller a buyer-authored event binds to. AWARD, ACCEPT and RECEIPT are all
 * authored by the buyer and carry two `p` tags — the buyer (the author) and the
 * seller — so the seller is the `p` that is not the author. This is the
 * buyer-authenticated name of the runner, independent of who claimed first.
 */
function boundSeller(event: RawEvent): string | null {
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
 * THE free-lane predicate: this offer settles with no payment.
 *
 * Read from the offer's own tags — `["param","payment","none"]` — because that
 * is the buyer's structured statement of it. A zero amount corroborates but
 * never decides: an offer with NO amount tag is a different thing (the price
 * was not stated), and one such offer exists on the live market, so a reader
 * that inferred "free" from a missing or zero price would misfile it.
 *
 * What it changes downstream: a free job's ACCEPT is its last event — no 3400
 * can ever follow because there is nothing to settle — so a delivered-and-
 * accepted free job is COMPLETE, not a delivery awaiting money. It must not be
 * counted as unpaid, and it must not be counted as paid either: it paid
 * nothing and counted nothing.
 */
export function settlesWithoutPayment(event: RawEvent): boolean {
  return param(event, "payment") === "none";
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
               free: settlesWithoutPayment(event),
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
               awardedSeller: boundSeller(event),
               status: firstTag(event, "status") };
    }
    // The accept is the buyer's pay-authorisation (§6.5). Like the award it is
    // buyer-authored and p-tags the bound seller — the authenticated winner,
    // never the first claimant to arrive.
    case ACCEPT:
      return { ...base, offerId: rootOfferId(event), buyer: event.pubkey,
               acceptedSeller: boundSeller(event),
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
    // The co-signed settlement (§6.8). Buyer-authored, and its `p` tags name
    // buyer and seller with the seller's own co-signature — the strongest
    // public statement of who was actually paid. Preserve that binding; the
    // amount alone throws the authoritative counterparty away.
    case RECEIPT:
      return { ...base, offerId: rootOfferId(event),
               receiptSeller: boundSeller(event),
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
               // The seat's advertised capability. `harness_family`,
               // `harness_model` and `capabilities` are machine-sourced at the
               // seat — the roster, the harness handshake, and a probe of the
               // job execution environment. `harness_variant` and `hardware`
               // are operator-typed and nothing verifies them; the renderer
               // keeps that split visible rather than showing five equal rows.
               // Each of the five normalized at its OWN reader (§4.5.2), never
               // in the shared helpers those two lines call — `agents` and
               // `accepted_mints` ride the same helpers and must not shift.
               harnessFamilies: statedValues(tagValues(event, "harness_family")),
               harnessModels: harnessModels(event),
               capabilities: statedValues(tagValues(event, "capabilities")),
               harnessVariant: stated(firstTag(event, "harness_variant")),
               hardware: stated(firstTag(event, "hardware")),
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
