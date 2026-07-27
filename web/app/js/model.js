/**
 * Typed model — raw relay events in, typed records out.
 *
 * Parsing happens once, here, at the edge. No view, metric or store may reach
 * into a raw tag array: if a tag shape changes, this file is the only casualty.
 * A malformed event yields `null` rather than throwing — one bad event from an
 * open relay must never take the page down.
 */
import {
  AWARD, CLAIM, FEEDBACK, HANDLER, HEARTBEAT, OFFER, PROFILE, RECEIPT, RESULT,
  TRADE_STAGES,
} from "./kinds.js";

const tagsNamed = (event, name) => (event.tags || []).filter((t) => t[0] === name);
const firstTag = (event, ...names) => {
  for (const name of names) {
    const t = tagsNamed(event, name)[0];
    if (t && t[1]) return t[1];
  }
  return null;
};

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
  for (const t of es) if (t[3] === "root") return t[1];
  if (es.length && es[0][1]) return es[0][1];
  return firstTag(event, "E", "offer", "root");
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
 * A feedback event's reason is the code before the first colon
 * ("claim_released: ..."), not free text. Anything unlike a code is unspecified.
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
export function parseEvent(event) {
  if (!event || typeof event.kind !== "number" || typeof event.id !== "string") return null;
  if (typeof event.pubkey !== "string" || typeof event.created_at !== "number") return null;

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
               amount: firstNumber(event, "amount", "rate", "price", "sats"),
               targetSeller: firstTag(event, "p"),
               // The job itself is the `i` (input) tag. Offer content is empty
               // in practice, so reading it yields a field that is never set.
               description: firstTag(event, "i") || "",
               outputType: firstTag(event, "output"),
               deadline: Number.parseInt(param(event, "deadline"), 10) || null };
    case CLAIM:
      return { ...base, offerId: rootOfferId(event), seller: event.pubkey,
               status: firstTag(event, "status"), hasPaymentRequest: Boolean(firstTag(event, "creq")) };
    case AWARD:
      return { ...base, offerId: rootOfferId(event), buyer: event.pubkey,
               status: firstTag(event, "status") };
    case RESULT:
      return { ...base, offerId: rootOfferId(event), seller: event.pubkey,
               amount: firstNumber(event, "amount", "amt", "sats"),
               // What actually did the work, and how it was handed over.
               harness: firstTag(event, "harness"),
               deliveryVia: firstTag(event, "delivery"),
               commit: firstTag(event, "commit"),
               wallTimeSeconds: firstNumber(event, "wall_time") };
    case FEEDBACK:
      return { ...base, offerId: rootOfferId(event), seller: event.pubkey,
               reason: feedbackReason(event) };
    case RECEIPT:
      return { ...base, offerId: rootOfferId(event),
               amount: firstNumber(event, "amount", "amt", "sats") };
    case HEARTBEAT:
      return { ...base, d: firstTag(event, "d") || "", status: firstTag(event, "status") };
    case HANDLER:
      return { ...base, d: firstTag(event, "d") || "", handler: parseJsonContent(event) };
    case PROFILE: {
      const p = parseJsonContent(event);
      return { ...base, name: p.name || p.display_name || null, about: p.about || null };
    }
    default:
      return null;
  }
}
