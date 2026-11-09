/**
 * Strict JSON, decoded the way the trade CLI's serde types decode it.
 *
 * `JSON.parse` is too forgiving to stand in for serde: it keeps the last of
 * two duplicate keys, turns `64.0` and `6.4e1` into the integer 64, rounds
 * integers past 2^53, and accepts lone surrogate escapes. serde_json refuses
 * every one of those for the `#[serde(deny_unknown_fields)]` u64/u8 structs in
 * crates/maxplayer-trade/src/lib.rs, so a listing the CLI rejects would render
 * here as real. This parser keeps number lexemes and key order so the decoders
 * below can apply serde's rules exactly. Golden cases: test/fixtures/trade-rust.json.
 */

export type Json =
  | { t: "null" }
  | { t: "bool"; v: boolean }
  /** The lexeme, untouched: integer-ness and range are decided by the decoder. */
  | { t: "num"; raw: string }
  | { t: "str"; v: string }
  | { t: "arr"; v: Json[] }
  /** Entries in document order, duplicates kept so a decoder can refuse them. */
  | { t: "obj"; v: [string, Json][] };

export class JsonError extends Error {}

/** serde_json's default recursion limit. */
const MAX_DEPTH = 128;
const NUMBER = /-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/y;

export function parseJson(text: string): Json {
  let i = 0;
  const ws = () => {
    while (i < text.length) {
      const c = text.charCodeAt(i);
      if (c === 0x20 || c === 0x09 || c === 0x0a || c === 0x0d) i++;
      else break;
    }
  };
  const fail = (what: string): never => { throw new JsonError(`${what} at ${i}`); };

  function str(): string {
    i++; // opening quote
    let out = "";
    for (;;) {
      if (i >= text.length) fail("unterminated string");
      const c = text.charCodeAt(i);
      if (c === 0x22) { i++; return out; }
      if (c < 0x20) fail("control character in string");
      if (c >= 0xd800 && c <= 0xdfff) {
        // A raw surrogate is only legal as half of a well-formed pair.
        const d = text.charCodeAt(i + 1);
        if (c > 0xdbff || !(d >= 0xdc00 && d <= 0xdfff)) fail("lone surrogate");
        out += text.slice(i, i + 2);
        i += 2;
        continue;
      }
      if (c !== 0x5c) { out += text[i]; i++; continue; }
      const e = text[i + 1];
      i += 2;
      switch (e) {
        case '"': out += '"'; break;
        case "\\": out += "\\"; break;
        case "/": out += "/"; break;
        case "b": out += "\b"; break;
        case "f": out += "\f"; break;
        case "n": out += "\n"; break;
        case "r": out += "\r"; break;
        case "t": out += "\t"; break;
        case "u": {
          const hi = hex4();
          if (hi >= 0xdc00 && hi <= 0xdfff) fail("lone trailing surrogate escape");
          if (hi >= 0xd800 && hi <= 0xdbff) {
            if (text[i] !== "\\" || text[i + 1] !== "u") fail("unexpected end of hex escape");
            i += 2;
            const lo = hex4();
            if (!(lo >= 0xdc00 && lo <= 0xdfff)) fail("lone leading surrogate escape");
            out += String.fromCharCode(hi, lo);
          } else {
            out += String.fromCharCode(hi);
          }
          break;
        }
        default: fail("invalid escape");
      }
    }
  }

  function hex4(): number {
    const h = text.slice(i, i + 4);
    if (!/^[0-9a-fA-F]{4}$/.test(h)) fail("invalid hex escape");
    i += 4;
    return parseInt(h, 16);
  }

  function value(depth: number): Json {
    if (depth > MAX_DEPTH) fail("recursion limit exceeded");
    ws();
    const c = text[i];
    if (c === "{") {
      i++;
      const v: [string, Json][] = [];
      ws();
      if (text[i] === "}") { i++; return { t: "obj", v }; }
      for (;;) {
        ws();
        if (text[i] !== '"') fail("expected key");
        const k = str();
        ws();
        if (text[i] !== ":") fail("expected colon");
        i++;
        v.push([k, value(depth + 1)]);
        ws();
        if (text[i] === ",") { i++; continue; }
        if (text[i] === "}") { i++; return { t: "obj", v }; }
        fail("expected , or }");
      }
    }
    if (c === "[") {
      i++;
      const v: Json[] = [];
      ws();
      if (text[i] === "]") { i++; return { t: "arr", v }; }
      for (;;) {
        v.push(value(depth + 1));
        ws();
        if (text[i] === ",") { i++; continue; }
        if (text[i] === "]") { i++; return { t: "arr", v }; }
        fail("expected , or ]");
      }
    }
    if (c === '"') return { t: "str", v: str() };
    if (text.startsWith("null", i)) { i += 4; return { t: "null" }; }
    if (text.startsWith("true", i)) { i += 4; return { t: "bool", v: true }; }
    if (text.startsWith("false", i)) { i += 5; return { t: "bool", v: false }; }
    NUMBER.lastIndex = i;
    const m = NUMBER.exec(text);
    if (m) {
      i += m[0].length;
      // serde_json refuses `01` as "invalid number", not as 0 then 1.
      if (/[0-9]/.test(text[i] ?? "")) fail("invalid number");
      return { t: "num", raw: m[0] };
    }
    return fail("expected value");
  }

  const v = value(0);
  ws();
  if (i !== text.length) fail("trailing characters");
  return v;
}

/* ---------------- serde-style decoders ---------------- */

const U64_MAX = (1n << 64n) - 1n;

/** An unsigned integer of at most `max`. Floats, exponents, `-0` and negatives all refuse. */
export function uint(j: Json | undefined, max: bigint = U64_MAX): bigint {
  if (j?.t !== "num" || !/^(?:0|[1-9]\d*)$/.test(j.raw)) throw new JsonError("expected unsigned integer");
  const n = BigInt(j.raw);
  if (n > max) throw new JsonError("integer out of range");
  return n;
}
export const u8 = (j: Json | undefined): bigint => uint(j, 255n);

export function string(j: Json | undefined): string {
  if (j?.t !== "str") throw new JsonError("expected string");
  return j.v;
}

/**
 * A `#[serde(deny_unknown_fields)]` struct: every field present exactly once,
 * nothing else. `seq` admits serde's positional form — a derived struct also
 * deserializes from an array of exactly its field count — which the CLI
 * accepts, so refusing it here would hide a listing the CLI trades. A struct
 * holding a `#[serde(flatten)]` field (Leg) has no positional form.
 * `optional` names trailing `#[serde(default)]` fields (Revision.lot): absent
 * from the map, or left off the end of the positional form, they are simply missing.
 */
export function struct(j: Json | undefined, fields: readonly string[], seq: boolean, optional: readonly string[] = []): Record<string, Json> {
  const out: Record<string, Json> = Object.create(null);
  // `optional` = trailing `#[serde(default)]` fields: they may be absent from the map, and the
  // positional form may stop before them.
  const required = fields.length - optional.length;
  if (j?.t === "arr" && seq) {
    if (j.v.length < required || j.v.length > fields.length) throw new JsonError("invalid length");
    j.v.forEach((v, n) => { out[fields[n] as string] = v; });
    return out;
  }
  if (j?.t !== "obj") throw new JsonError("expected struct");
  for (const [k, v] of j.v) {
    if (!fields.includes(k)) throw new JsonError(`unknown field ${k}`);
    if (k in out) throw new JsonError(`duplicate field ${k}`);
    out[k] = v;
  }
  for (const f of fields.slice(0, required)) if (!(f in out)) throw new JsonError(`missing field ${f}`);
  return out;
}

/**
 * A unit-variant enum: `"name"`, or serde_json's externally tagged map form
 * `{"name": null}` — which the CLI accepts too.
 */
export function unitEnum<T extends string>(j: Json | undefined, variants: readonly T[]): T {
  if (j?.t === "str" && (variants as readonly string[]).includes(j.v)) return j.v as T;
  if (j?.t === "obj" && j.v.length === 1) {
    const [k, v] = j.v[0] as [string, Json];
    if ((variants as readonly string[]).includes(k) && v.t === "null") return k as T;
  }
  throw new JsonError("unknown variant");
}

/* ---------------- structural equality with serde_json::Value ---------------- */

/** serde_json's map: duplicate keys collapse, the last one wins. */
function lastWins(entries: [string, Json][]): Map<string, Json> {
  const m = new Map<string, Json>();
  for (const [k, v] of entries) m.set(k, v);
  return m;
}

/**
 * `serde_json::Value == serde_json::to_value(v)` for a plain value built from
 * strings, safe non-negative integers and arrays/objects of those (a signed
 * Nostr event): key order is irrelevant, numbers must be integer lexemes
 * (`1.0` and `1e0` are floats to serde and never equal an integer).
 */
export function sameJson(j: Json, v: unknown): boolean {
  if (typeof v === "string") return j.t === "str" && j.v === v;
  if (typeof v === "number") {
    // serde_json reads `-0` as the float -0.0, which never equals an integer.
    if (j.t !== "num" || !Number.isSafeInteger(v) || v < 0) return false;
    return /^(?:0|[1-9]\d*)$/.test(j.raw) && BigInt(j.raw) === BigInt(v);
  }
  if (Array.isArray(v)) return j.t === "arr" && j.v.length === v.length && v.every((x, i) => sameJson(j.v[i] as Json, x));
  if (v && typeof v === "object") {
    if (j.t !== "obj") return false;
    const m = lastWins(j.v);
    const keys = Object.keys(v);
    return m.size === keys.length && keys.every((k) => m.has(k) && sameJson(m.get(k) as Json, (v as Record<string, unknown>)[k]));
  }
  return false;
}

/** A plain JS value for strict JSON (last duplicate key wins); null when a number is not a safe integer. */
export function toPlain(j: Json): unknown {
  switch (j.t) {
    case "null": return null;
    case "bool": return j.v;
    case "str": return j.v;
    case "num": return /^(?:0|[1-9]\d*)$/.test(j.raw) && Number.isSafeInteger(Number(j.raw)) ? Number(j.raw) : null;
    case "arr": return j.v.map(toPlain);
    case "obj": {
      const out: Record<string, unknown> = Object.create(null);
      for (const [k, v] of lastWins(j.v)) out[k] = toPlain(v);
      return out;
    }
  }
}
