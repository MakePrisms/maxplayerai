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
  RESULT, SELF_TRADE_TAG, TRADE_STAGES,
} from "./kinds.js";

const tagsNamed = (event, name) => (event.tags || []).filter((t) => t[0] === name);
const firstTag = (event, ...names) => {
  for (const name of names) {
    const t = tagsNamed(event, name)[0];
    if (t && t[1]) return t[1];
  }
  return null;
};

/** Every value on the first multi-value tag with this name. */
const tagValues = (event, name) => {
  const t = tagsNamed(event, name)[0];
  return t ? t.slice(1).filter((v) => typeof v === "string" && v.length > 0) : [];
};

/**
 * Preserve the complete public advertisement, including fields introduced by
 * a newer seller than this reader knows about. The renderer escapes every
 * value; this layer only normalises tag cells to strings.
 */
function advertisementTags(event) {
  return (event.tags || [])
    .filter((t) => Array.isArray(t) && typeof t[0] === "string" && t[0].length > 0)
    .map((t) => ({ name: t[0], values: t.slice(1).map((v) => String(v)) }));
}

/** First finite number found under any of the given tag names. */
function firstNumber(event, ...names) {
  for (const name of names) {
    for (const t of tagsNamed(event, name)) {
      const n = Number.parseFloat(t[1]);
      if (Number.isFinite(n)) return n;
    }
  }
  return null;
}

/**
 * The offer a trade event belongs to.
 *
 * Prefer an `e` tag explicitly marked `root`: an award also e-tags the winning
 * claim, so taking the first `e` blindly can key a trade off a claim id and
 * split one trade into two.
 */
export function rootOfferId(event) {
  const es = tagsNamed(event, "e");
  for (const t of es) if (t[3] === "root" && isHex32(t[1])) return t[1];
  for (const t of es) if (isHex32(t[1])) return t[1];
  // An offer id becomes a DOM key and a rendered label, so a tag whose value is
  // not an event id is not one, whatever it claims.
  const named = firstTag(event, "E", "offer", "root");
  return isHex32(named) ? named : null;
}

/** The non-root event selected by an award (the winning claim). */
function awardClaimId(event, offerId) {
  for (const t of tagsNamed(event, "e")) {
    if (isHex32(t[1]) && t[1] !== offerId) return t[1];
  }
  return null;
}

/** The other participant on a buyer-authored award is the selected seller. */
function awardSeller(event) {
  for (const t of tagsNamed(event, "p")) {
    if (isHex32(t[1]) && t[1] !== event.pubkey) return t[1];
  }
  return null;
}

function parseJsonContent(event) {
  try {
    const value = JSON.parse(event.content || "{}");
    return value && typeof value === "object" ? value : {};
  } catch { return {}; }
}

/**
 * A `param` tag is a named value: ["param", "deadline", "1785184881"].
 */
export function param(event, name) {
  for (const t of event.tags || []) if (t[0] === "param" && t[1] === name) return t[2];
  return null;
}

/**
 * protocol-v1 §7.1: `reason_code` names the class. The vocabulary is extensible,
 * so a reader that meets an unknown code MUST fall back to `status` (§7.1) and
 * MUST NOT treat the event as malformed.
 */
/**
 * A Map, not an object literal, and that is a correctness requirement rather
 * than a style choice. `reason_code` is an arbitrary string from the relay, so
 * an object lookup answers for inherited names too: `obj["constructor"]`
 * returns a function, which is truthy, so an UNKNOWN code would be reported as
 * a class and would never fall back to `status` as §7.1 requires. A seller
 * could then publish a terminal `status=error` alongside `reason_code=toString`
 * and keep the job showing as active. A Map has no prototype chain to consult,
 * so the safety is structural — the next person to add a code cannot forget a
 * guard that does not exist.
 */
const REASON_CODE_CLASS = new Map([
  ["below_rate", "refusal"],
  ["unsupported_version", "refusal"],
  ["mint_incompatible", "refusal"],
  ["at_capacity", "refusal"],
  ["execution_failed", "error"],
  ["delivery_failed", "error"],
  ["no_sentinel", "refusal"],
]);

/** protocol-v1 §7.2. `progress` is explicitly non-terminal; the rest end an attempt. */
const TERMINAL_FEEDBACK_CLASSES = Object.freeze(new Set(["claim_released", "refusal", "error"]));

/**
 * The protocol class of a feedback event, read from tags only.
 *
 * protocol-v1 §7.1 is explicit: "A reader MUST NOT parse `content` to determine
 * the class." `feedbackReason` below reads content on purpose, but it is a
 * DISPLAY helper and must never decide whether work ended. Returns null when
 * the event carries neither a known code nor a status.
 */
export function feedbackClass(event) {
  const code = firstTag(event, "reason_code");
  const known = code ? REASON_CODE_CLASS.get(code) : null;
  if (known) return known;
  return firstTag(event, "status") || null;
}

/**
 * Whether a feedback event ends the awarded seller's attempt.
 *
 * Unclassified feedback is NOT terminal. Terminalizing on an event we cannot
 * classify is what let a routine `progress` note clear the work lamp before any
 * result. An unclassified event now leaves the job running, and the #681
 * deadline clock is what stops it running forever — so the conservative choice
 * here no longer costs us the indefinite-working bug it used to.
 */
export function feedbackIsTerminal(event) {
  return TERMINAL_FEEDBACK_CLASSES.has(feedbackClass(event));
}

/**
 * A feedback event's reason is the code before the first colon
 * ("claim_released: ..."), not free text. Anything unlike a code is unspecified.
 *
 * DISPLAY ONLY. It reads `content`, so protocol-v1 §7.1 forbids using it to
 * decide a feedback event's class — use `feedbackClass` for that.
 */
export function feedbackReason(event) {
  const head = String(event.content || "").trim().split(":")[0].trim();
  if (head && head.length <= 40 && /^[a-z0-9_\- ]+$/i.test(head)) return head;
  return firstTag(event, "reason", "code", "status") || "unspecified";
}

/**
 * Parse one event into a typed record, or null if it is not something we model.
 *
 * `stage` is present exactly when the event belongs to a trade, so callers can
 * branch on it without knowing kind numbers.
 */
/**
 * Nostr ids and pubkeys are 32 bytes of lowercase hex. Nothing else is one.
 *
 * This is enforced at the boundary because a relay is untrusted input: these
 * values end up in markup and in `data-` attributes, so a non-hex "pubkey"
 * would be an injection path. Rejecting the event here means nothing
 * downstream has to remember to escape them.
 */
const HEX32 = /^[0-9a-f]{64}$/;
export const isHex32 = (s) => typeof s === "string" && HEX32.test(s);

export function parseEvent(event) {
  if (!event || typeof event.kind !== "number" || typeof event.created_at !== "number") return null;
  if (!isHex32(event.id) || !isHex32(event.pubkey)) return null;

  const base = {
    id: event.id,
    kind: event.kind,
    pubkey: event.pubkey,
    created_at: event.created_at,
    stage: TRADE_STAGES[event.kind] || null,
    offerId: null,
  };

  switch (event.kind) {
    case OFFER:
      return { ...base, offerId: event.id, buyer: event.pubkey,
               // A buyer commissioning its own seller marks the offer
               // ["t","self-trade"]. A structured predicate, not prose: the
               // disclosure in the job text is for humans, this is for counting.
               selfTrade: tagsNamed(event, "t").some((t) => t[1] === SELF_TRADE_TAG),
               amount: firstNumber(event, "amount", "rate", "price", "sats"),
               targetSeller: firstTag(event, "p"),
               // The job itself is the `i` (input) tag. Offer content is empty
               // in practice, so reading it yields a field that is never set.
               description: firstTag(event, "i") || "",
               outputType: firstTag(event, "output"),
               deadline: Number.parseInt(param(event, "deadline"), 10) || null };
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
      return { ...base, name: p.name || p.display_name || null, about: p.about || null,
               profile: p };
    }
    default:
      return null;
  }
}
