/**
 * Defensive Nostr event parsing for the network observatory.
 * One malformed / hostile event must never throw into the page.
 */

import { ACCEPT, AWARD, CLAIM, FEEDBACK, HANDLER, HEARTBEAT, OFFER, PROFILE, RECEIPT, RESULT, HARNESS_FAMILIES } from "./kinds.js";

/**
 * @param {unknown} raw
 * @returns {object | null}
 */
export function parseEvent(raw) {
  try {
    if (!raw || typeof raw !== "object") return null;
    const ev = /** @type {Record<string, unknown>} */ (raw);

    const id = asString(ev.id);
    const pubkey = asString(ev.pubkey);
    const kind = asInt(ev.kind);
    const created_at = asInt(ev.created_at);
    if (!id || !pubkey || kind == null || created_at == null) return null;

    const tags = normalizeTags(ev.tags);
    const content = typeof ev.content === "string" ? ev.content : "";

    const base = {
      id,
      pubkey,
      kind,
      created_at,
      tags,
      content,
      contentJson: tryParseJson(content),
    };

    if (kind === PROFILE) return { ...base, role: "profile", profile: parseProfile(base) };
    if (kind === OFFER) return { ...base, role: "offer", offer: parseOffer(base) };
    if (kind === CLAIM) return { ...base, role: "claim", claim: parseClaim(base) };
    if (kind === AWARD) return { ...base, role: "award", award: parseAward(base) };
    // A distinct role, deliberately. Accept carries the same tags as award, so routing
    // it to role "award" would parse cleanly and re-create the double-count the 3405/3406
    // split removed — anything counting awards by role would count every job twice.
    if (kind === ACCEPT) return { ...base, role: "accept", accept: parseAccept(base) };
    if (kind === FEEDBACK) return { ...base, role: "feedback", feedback: parseFeedback(base) };
    if (kind === RESULT) return { ...base, role: "result", result: parseResult(base) };
    if (kind === RECEIPT) return { ...base, role: "receipt", receipt: parseReceipt(base) };
    if (kind === HANDLER) return { ...base, role: "handler", handler: parseHandler(base) };
    if (kind === HEARTBEAT) return { ...base, role: "heartbeat", heartbeat: parseHeartbeat(base) };
    return { ...base, role: "other" };
  } catch {
    return null;
  }
}

/** Max kind-0 content bytes we will attempt to parse (hostile 10MB picture must not blank). */
export const PROFILE_CONTENT_MAX = 64 * 1024;
/** Max picture URL length retained for rendering. */
export const PROFILE_PICTURE_MAX = 2048;

/**
 * Defensive NIP-01 kind-0 metadata parse.
 * @param {{ content: string, created_at: number }} base
 */
export function parseProfile(base) {
  const empty = {
    name: null,
    display_name: null,
    picture: null,
    about: null,
  };
  try {
    let raw = typeof base.content === "string" ? base.content : "";
    if (raw.length > PROFILE_CONTENT_MAX) {
      raw = raw.slice(0, PROFILE_CONTENT_MAX);
    }
    const obj = tryParseJson(raw);
    if (!obj || typeof obj !== "object") return empty;

    const name = clampStr(obj.name, 128);
    const display_name = clampStr(obj.display_name ?? obj.displayName, 128);
    let picture = clampStr(obj.picture, PROFILE_PICTURE_MAX);
    if (picture && !isSafePictureUrl(picture)) picture = null;
    const about = clampStr(obj.about, 512);

    return { name, display_name, picture, about };
  } catch {
    return empty;
  }
}

function clampStr(v, max) {
  if (typeof v !== "string") return null;
  const t = v.trim();
  if (!t) return null;
  return t.length > max ? t.slice(0, max) : t;
}

function isSafePictureUrl(url) {
  try {
    const u = new URL(url);
    return u.protocol === "https:" || u.protocol === "http:";
  } catch {
    return false;
  }
}

/**
 * Extract the usage adjunct.
 *
 * SPEC WINS: the seller emits exec-metadata as TAGS on the kind-3403 result (its content is a
 * non-JSON string like "delivery commit <oid>", so contentJson is null). We read tags first,
 * per the usage schema, and fall back to the legacy JSON vocabulary only for fields that
 * never had a tag form (measured_cost_tokens / paid_price_tokens). `harness` is carried verbatim
 * as `harness_id` — the seat's own claim — and separately READ for a family (claude-agent-acp →
 * claude) into `harness_family`, which stays null when the id names no family we recognize.
 *
 * degrades-never-blanks: a missing field is `null` (renders a dash) — NEVER a fabricated
 * value, and totals are never invented by summing siblings.
 * @param {unknown} contentJson
 * @param {string[][]} tags
 */
export function extractUsageAdjunct(contentJson, tags = []) {
  try {
    const root = asObject(contentJson) || {};
    const adjunct =
      asObject(root.usage_adjunct) ||
      asObject(root.completion_usage_adjunct) ||
      asObject(root.usage) ||
      root;

    const measure =
      asObject(adjunct.usage_measure) ||
      asObject(root.usage_measure) ||
      null;

    const legacyTotal = measure
      ? asNumberOrNull(measure.total_tokens)
      : asNumberOrNull(adjunct.total_tokens);
    const cost = costFromTags(tags);

    return {
      // tags win; legacy JSON is a fallback for total only.
      total_tokens: tokensTagValue(tags, "total") ?? legacyTotal,
      input_tokens: tokensTagValue(tags, "input"),
      output_tokens: tokensTagValue(tags, "output"),
      reasoning_tokens: tokensTagValue(tags, "reasoning"),
      cache_read_tokens: tokensTagValue(tags, "cache_read"),
      cache_write_tokens: tokensTagValue(tags, "cache_write"),
      model: firstTagValue(tags, "model"),
      cost_usd: cost.usd,
      cost_basis: cost.basis,
      // Legacy-only (never emitted as tags) — stay null on tagged results → dash.
      measured_cost_tokens: asNumberOrNull(
        adjunct.measured_cost_tokens ?? root.measured_cost_tokens,
      ),
      paid_price_tokens: asNumberOrNull(
        adjunct.paid_price_tokens ?? root.paid_price_tokens,
      ),
      usage_transport:
        firstTagValue(tags, "usage_transport") ??
        asEnumString(adjunct.usage_transport ?? root.usage_transport, [
          "acp-native",
          "side-channel",
        ]),
      // The seat's own claim, verbatim. Kept beside the family because the family is OUR reading
      // of this string, and a reading and a claim are not the same fact.
      harness_id: firstTagValue(tags, "harness"),
      harness_family:
        harnessFamilyFromId(firstTagValue(tags, "harness")) ??
        asEnumString(adjunct.harness_family ?? root.harness_family, HARNESS_FAMILIES),
      paid_price_sats: amountSatsFromTags(tags),
    };
  } catch {
    return emptyUsage();
  }
}

/** Value of a `["tokens","<n>","<qualifier>"]` tag (total/input/output/reasoning/cache_*). */
function tokensTagValue(tags, qualifier) {
  for (const tag of tags) {
    if (tag[0] === "tokens" && tag[2] === qualifier && tag[1] != null) {
      const n = Number(tag[1]);
      if (Number.isFinite(n)) return n;
    }
  }
  return null;
}

/** Reported USD cost from `["cost","<n>","usd","<basis>"]`; absent → both null. */
function costFromTags(tags) {
  for (const tag of tags) {
    if (tag[0] === "cost" && tag[2] === "usd" && tag[1] != null) {
      const n = Number(tag[1]);
      if (Number.isFinite(n)) return { usd: n, basis: tag[3] || null };
    }
  }
  return { usd: null, basis: null };
}

/**
 * Read a family off a seller's `harness` id. A recognized adapter identity → the family; absent
 * or UNRECOGNIZED → null.
 *
 * Unrecognized must not become `"other"`. The seller emits this id from one of two paths it does
 * not distinguish for us (`harness_and_transport`, `seller_exec.rs`): a config-defined preset
 * name, where the name IS the harness identity and a family outside our enum is the truth; or the
 * argv0 BASENAME fallback, which fires when nothing in the launch argv named a family — so the id
 * is the program that STARTED a harness (`sh`; and `npx`, before the emitter preferred the preset
 * label over argv0), not the harness itself. Both arrive here as the same unrecognized string and
 * nothing else on the receipt separates them, so the family is unavailable and `"other"` would
 * assert one we do not have. The caller renders `harness_id` verbatim, marked unidentified.
 *
 * `"other"` survives only where a seller states it OUTRIGHT in the legacy JSON field. There it is
 * the seller's own claim rather than our inference.
 *
 * Exported because the advertised-versus-delivered pairing needs the same reading. `agents`
 * advertises a PRESET LABEL (`claude`) while a receipt carries the ADAPTER IDENTITY
 * (`claude-agent-acp`) — two namespaces, so the pairing compares families and would report a
 * mismatch on every well-behaved seat if it compared strings. Two family readers would be free to
 * drift; there is one.
 */
export function harnessFamilyFromId(id) {
  if (!id) return null;
  const s = String(id).toLowerCase();
  // An id that IS a family — a seat's own `harness_family` value — passes through unchanged.
  for (const family of HARNESS_FAMILIES) if (s === family) return family;
  if (s.includes("claude")) return "claude-code";
  if (s.includes("cursor")) return "cursor";
  if (s.includes("codex")) return "codex";
  if (s.includes("goose")) return "goose";
  return null;
}

/** The absent-usage shape. One definition: every consumer reads the same keys, all null. */
export function emptyUsage() {
  return {
    total_tokens: null,
    input_tokens: null,
    output_tokens: null,
    reasoning_tokens: null,
    cache_read_tokens: null,
    cache_write_tokens: null,
    model: null,
    cost_usd: null,
    cost_basis: null,
    measured_cost_tokens: null,
    paid_price_tokens: null,
    usage_transport: null,
    harness_id: null,
    harness_family: null,
    paid_price_sats: null,
  };
}

function parseOffer(base) {
  return {
    task: firstTagValue(base.tags, "i"),
    amount_sats: amountSatsFromTags(base.tags),
    // A `p` tag on an offer = a targeted seller; absent = open-pool offer.
    seller: firstTagValue(base.tags, "p"),
    // The buyer binds the deadline as ["param","deadline","<unix-seconds>"] (not NIP-40).
    deadline: deadlineFromTags(base.tags),
    job_class: firstTagValue(base.tags, "job-class"),
  };
}

/** A seller's claim: it bids on the offer and carries the seller-authored `creq` invoice. */
function parseClaim(base) {
  return {
    offerId: firstETag(base.tags, "root") || firstETag(base.tags, null),
    creq: firstTagValue(base.tags, "creq"),
  };
}

/** A buyer's award: it selects a claim, e-tagging the offer (root) and the winning claim. */
function parseAward(base) {
  const offerId = firstETag(base.tags, "root") || firstETag(base.tags, null);
  let claimId = null;
  for (const tag of base.tags) {
    if (tag[0] === "e" && tag[1] && tag[3] !== "root" && tag[1] !== offerId) {
      claimId = tag[1];
      break;
    }
  }
  return { offerId, claimId };
}

/**
 * A buyer's accept: it binds payment to one verified result, e-tagging the offer (root)
 * and the claim. The tags match an award's, so the reader is shared; the KIND is what
 * says whether the buyer was choosing a seller or authorising payment for finished work.
 */
function parseAccept(base) {
  return parseAward(base);
}

/** A seller's feedback: a progress note, or an error/refusal that fails the job. */
function parseFeedback(base) {
  const status = firstTagValue(base.tags, "status");
  const offerId = firstETag(base.tags, "root") || firstETag(base.tags, null);
  return {
    status,
    // An "error"/"refusal" status means the job failed; the feed shows it as refused.
    isError: status === "error" || status === "refusal",
    offerId,
    message: typeof base.content === "string" && base.content ? base.content.slice(0, 280) : null,
  };
}

/** Deadline unix-seconds from ["param","deadline","<n>"]; absent → null. */
function deadlineFromTags(tags) {
  for (const tag of tags) {
    if (tag[0] === "param" && tag[1] === "deadline" && tag[2] != null) {
      const n = Number(tag[2]);
      if (Number.isFinite(n)) return n;
    }
  }
  return null;
}

function parseResult(base) {
  return {
    offerId: firstETag(base.tags, "root") || firstETag(base.tags, null),
    amount_sats: amountSatsFromTags(base.tags),
    // The seller's kind-3403 result is the AUTHORITATIVE usage source (the receipt
    // echo is a convenience copy). Read it from the result-event TAGS.
    usage: extractUsageAdjunct(base.contentJson, base.tags),
  };
}

function parseReceipt(base) {
  const usage = extractUsageAdjunct(base.contentJson, base.tags);
  return {
    offerId: firstETag(base.tags, "root") || firstETag(base.tags, null),
    resultId: firstETag(base.tags, "reply"),
    amount_sats: amountSatsFromTags(base.tags),
    mint: firstTagValue(base.tags, "mint"),
    usage,
  };
}

function parseHandler(base) {
  const j = asObject(base.contentJson) || {};
  const harness_name =
    asString(j.harness_name) ||
    asString(j.name) ||
    asString(j.display_name) ||
    null;
  const version =
    asString(j.version) ||
    asString(j.harness_version) ||
    firstTagValue(base.tags, "version") ||
    null;
  return {
    harness_name,
    version,
    d: firstTagValue(base.tags, "d"),
    k: allTagValues(base.tags, "k"),
  };
}

/**
 * Seller seat announcement (kind 30340). Addressable — the `d` tag scopes it within the author.
 * Freshness is the event's own created_at (the caller resolves the newest per author+d, and bounds
 * it: see `resolveSeatDirectory`). `status` is an optional self-reported state; content is a free
 * message.
 *
 * `agents` is the seat's ADVERTISED harness roster and is a CLAIM about itself — discovery
 * metadata, never proof that a harness will serve. Verification comes from delivery, and the
 * falsifier is the `harness` tag on that same seat's own receipts. Rendered as a claim.
 *
 * `version` is the announcement's `v`. It is absent on pre-`v` announcements, which are still on
 * the relay, so absent here means "an older shape" and not "version zero". It does not follow from
 * the `d`: a current-address announcement carrying the pre-rename tag set was measured on
 * 2026-08-21, so shape is read from the tags and never inferred from the address.
 */
function parseHeartbeat(base) {
  return {
    d: firstTagValue(base.tags, "d"),
    version: firstTagValue(base.tags, "v"),
    status: firstTagValue(base.tags, "status"),
    agents: firstTagRest(base.tags, "agents"),
    capability: parseSeatCapability(base.tags),
    message: typeof base.content === "string" && base.content ? base.content.slice(0, 280) : null,
  };
}

/**
 * The #784 seat capability advertisement, split exactly as the emitter splits it.
 *
 * `filterable` holds what the award decision can read; `displayOnly` holds what it structurally
 * cannot. The split is a shape, not a comment: nothing in `filterable` can reach `hardware`, so a
 * predicate written against it later cannot filter on hardware even by mistake.
 *
 * `harnesses` is the joined view a reader wants — one row per advertised family with its models.
 * A family that advertises NO model gets an empty `models` array, which a view must render as an
 * explicit absence. An empty array and a missing key look identical once formatted, so the
 * distinction has to survive into the render rather than being resolved here.
 *
 * `orphanModels` carries any `harness_model` naming a family absent from `harness_family`. Dropping
 * those would make the reader disagree with the wire while looking complete.
 */
export function parseSeatCapability(tags) {
  const families = firstTagRest(tags, "harness_family");
  const models = pairTagValues(tags, "harness_model");
  const byFamily = new Map(families.map((f) => [f, []]));
  const orphanModels = [];
  for (const { family, model } of models) {
    const bucket = byFamily.get(family);
    if (bucket) bucket.push(model);
    else orphanModels.push({ family, model });
  }
  return {
    filterable: {
      harness_family: families,
      capabilities: firstTagRest(tags, "capabilities"),
      harness_model: models,
    },
    displayOnly: {
      harness_variant: firstTagValue(tags, "harness_variant"),
      hardware: firstTagValue(tags, "hardware"),
    },
    harnesses: families.map((family) => ({ family, models: byFamily.get(family) })),
    orphanModels,
  };
}

/**
 * Every `[name, a, b]` tag as `{family: a, model: b}` — for a tag that REPEATS and carries a pair,
 * which is the opposite shape to `firstTagRest`'s one-tag list. `harness_model` is emitted once per
 * model, so taking only the first tag loses every model after it.
 *
 * A tag missing its second value is skipped rather than admitted with `model: null`: a pair with
 * one half absent states nothing, and a null model here would be indistinguishable in the joined
 * view from a family that advertises no model at all.
 */
function pairTagValues(tags, name) {
  const out = [];
  for (const tag of tags) {
    if (tag[0] === name && tag[1] && tag[2]) out.push({ family: tag[1], model: tag[2] });
  }
  return out;
}

/**
 * Every value of the first `name` tag past its key — for tags that carry a LIST in one tag
 * (`["agents","claude","codex"]`), which `allTagValues` cannot read because it takes index 1 of
 * each matching tag. Absent tag → empty array, never null: an unstated roster and an empty one are
 * both "advertises nothing", and the emitter omits the tag rather than sending it empty.
 */
function firstTagRest(tags, name) {
  for (const tag of tags) {
    if (tag[0] === name) return tag.slice(1).filter((v) => typeof v === "string" && v);
  }
  return [];
}

function normalizeTags(tags) {
  if (!Array.isArray(tags)) return [];
  const out = [];
  for (const tag of tags) {
    if (!Array.isArray(tag) || tag.length === 0) continue;
    const row = [];
    let ok = true;
    for (const cell of tag) {
      if (typeof cell !== "string") {
        ok = false;
        break;
      }
      row.push(cell);
    }
    if (ok && row.length) out.push(row);
  }
  return out;
}

export function firstTagValue(tags, name) {
  for (const tag of tags) {
    if (tag[0] === name && tag[1]) return tag[1];
  }
  return null;
}

export function allTagValues(tags, name) {
  const vals = [];
  for (const tag of tags) {
    if (tag[0] === name && tag[1]) vals.push(tag[1]);
  }
  return vals;
}

/** Prefer marker (root/reply); else first e tag. */
export function firstETag(tags, marker) {
  if (marker) {
    for (const tag of tags) {
      if (tag[0] === "e" && tag[1] && tag[3] === marker) return tag[1];
    }
  }
  for (const tag of tags) {
    if (tag[0] === "e" && tag[1]) return tag[1];
  }
  return null;
}

export function amountSatsFromTags(tags) {
  for (const tag of tags) {
    if (tag[0] === "amount" && tag[1]) {
      const n = Number(tag[1]);
      if (Number.isFinite(n)) return n;
    }
  }
  return null;
}

function tryParseJson(text) {
  if (!text || typeof text !== "string") return null;
  const t = text.trim();
  if (!t || (t[0] !== "{" && t[0] !== "[")) return null;
  try {
    return JSON.parse(t);
  } catch {
    return null;
  }
}

function asObject(v) {
  return v && typeof v === "object" && !Array.isArray(v) ? v : null;
}

function asString(v) {
  return typeof v === "string" && v.length ? v : null;
}

function asInt(v) {
  if (typeof v === "number" && Number.isFinite(v)) return Math.trunc(v);
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    if (Number.isFinite(n)) return Math.trunc(n);
  }
  return null;
}

function asNumberOrNull(v) {
  if (v == null) return null;
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

function asEnumString(v, allowed) {
  const s = asString(v);
  if (!s) return null;
  return allowed.includes(s) ? s : s; // keep unknown strings visible; don't blank
}

/**
 * Percentile of a numeric array (sorted copy). Empty → null.
 * @param {number[]} values
 * @param {number} p 0..100
 */
export function percentile(values, p) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  if (sorted.length === 1) return sorted[0];
  const rank = (p / 100) * (sorted.length - 1);
  const lo = Math.floor(rank);
  const hi = Math.ceil(rank);
  if (lo === hi) return sorted[lo];
  const w = rank - lo;
  return sorted[lo] * (1 - w) + sorted[hi] * w;
}
