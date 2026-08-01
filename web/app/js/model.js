/**
 * Typed model — raw relay events in, typed records out.
 *
 * Parsing happens once, here, at the edge. No view, metric or store may reach
 * into a raw tag array: if a tag shape changes, this file is the only casualty.
 * A malformed event yields `null` rather than throwing — one bad event from an
 * open relay must never take the page down.
 */
import {
  ACCEPT, AWARD, CLAIM, FEEDBACK, HANDLER, HEARTBEAT, OFFER, PROFILE, RECEIPT,
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
               status: firstTag(event, "status"), hasPaymentRequest: Boolean(firstTag(event, "creq")) };
    // AWARD and ACCEPT are both buyer-authored and carry the same tags, so they
    // parse identically. The kind is what says whether it selects a claim or
    // binds payment to a result, and `base.stage` already carries that.
    case AWARD:
    case ACCEPT:
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
    case HANDLER: {
      // A seller's advert: who they are, what they charge, whether they will
      // take work nobody offered them directly.
      const h = parseJsonContent(event);
      const rate = Number.parseFloat(h.rate_sats);
      return { ...base, d: firstTag(event, "d") || "", handler: h,
               name: h.name || h.display_name || null,
               about: h.about || null,
               askSats: Number.isFinite(rate) ? rate : null,
               openPool: h.claim_open_pool === true,
               mint: h.mint || null,
               runtime: h.agent || null };
    }
    case PROFILE: {
      const p = parseJsonContent(event);
      return { ...base, name: p.name || p.display_name || null, about: p.about || null };
    }
    default:
      return null;
  }
}
