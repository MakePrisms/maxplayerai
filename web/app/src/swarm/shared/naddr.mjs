// NIP-19 `naddr` for swarm addresses (bech32, not bech32m). Pure, no DOM.
//
// Encoding matches the CLI's (`maxplayer swarm list/show/create`, nostr 0.44 coordinate_to_bech32):
// TLV identifier (0), author (2), kind (3, u32 big-endian), then each relay (1). A board link and the
// CLI's address for the same swarm are the same string when both name the same relay.
const CHARSET = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';
const GEN = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
// NIP-19 allows TLV entities longer than BIP-173's 90 characters; bound decoding anyway.
const MAX_LENGTH = 5000;
const SPECIAL = 0, RELAY = 1, AUTHOR = 2, KIND = 3;

function polymod(values) {
  let chk = 1;
  for (const v of values) {
    const top = chk >>> 25;
    chk = (((chk & 0x1ffffff) << 5) ^ v) >>> 0;
    for (let i = 0; i < 5; i++) if ((top >>> i) & 1) chk = (chk ^ GEN[i]) >>> 0;
  }
  return chk;
}
const expand = hrp => [...[...hrp].map(c => c.charCodeAt(0) >> 5), 0, ...[...hrp].map(c => c.charCodeAt(0) & 31)];
function convert(data, from, to, pad) {
  let acc = 0, bits = 0; const out = [], max = (1 << to) - 1;
  for (const v of data) {
    acc = (acc << from) | v; bits += from;
    while (bits >= to) { bits -= to; out.push((acc >> bits) & max); }
    acc &= (1 << bits) - 1;
  }
  if (pad) { if (bits) out.push((acc << (to - bits)) & max); }
  else if (bits >= from || acc) return null;
  return out;
}
function encode(hrp, bytes) {
  const data = convert(bytes, 8, 5, true);
  const chk = polymod([...expand(hrp), ...data, 0, 0, 0, 0, 0, 0]) ^ 1;
  const sum = Array.from({ length: 6 }, (_, i) => (chk >>> (5 * (5 - i))) & 31);
  return `${hrp}1${[...data, ...sum].map(v => CHARSET[v]).join('')}`;
}
function decode(text) {
  if (typeof text !== 'string' || text.length > MAX_LENGTH || text !== text.toLowerCase()) return null;
  const sep = text.lastIndexOf('1');
  if (sep < 1 || text.length - sep < 7) return null;
  const hrp = text.slice(0, sep), data = [];
  for (const c of text.slice(sep + 1)) { const v = CHARSET.indexOf(c); if (v < 0) return null; data.push(v); }
  if (polymod([...expand(hrp), ...data]) !== 1) return null;
  const bytes = convert(data.slice(0, -6), 5, 8, false);
  return bytes ? { hrp, bytes } : null;
}
const toHex = bytes => bytes.map(b => b.toString(16).padStart(2, '0')).join('');
const utf8 = new TextEncoder(), utf8Strict = new TextDecoder('utf-8', { fatal: true });

/**
 * naddr1… for (kind, author hex, d), with optional relay hints.
 * @param {{kind: number, pubkey: string, identifier: string, relays?: string[]}} address
 * @returns {string}
 */
export function naddrEncode({ kind, pubkey, identifier, relays = [] }) {
  if (!/^[0-9a-f]{64}$/.test(pubkey)) throw Error('expected 32-byte hex key');
  if (!Number.isSafeInteger(kind) || kind < 0 || kind > 0xffffffff) throw Error('invalid kind');
  const out = [], tlv = (type, value) => { if (value.length > 255) throw Error('TLV value too long'); out.push(type, value.length, ...value); };
  tlv(SPECIAL, [...utf8.encode(identifier)]);
  tlv(AUTHOR, pubkey.match(/../g).map(h => parseInt(h, 16)));
  tlv(KIND, [kind >>> 24 & 255, kind >>> 16 & 255, kind >>> 8 & 255, kind & 255]);
  for (const r of relays) tlv(RELAY, [...utf8.encode(r)]);
  return encode('naddr', out);
}

/**
 * { kind, pubkey, identifier, relays } from an naddr1…, or null. Unknown TLV types are skipped (NIP-19).
 * @param {string} text
 * @returns {{kind: number, pubkey: string, identifier: string, relays: string[]} | null}
 */
export function naddrDecode(text) {
  const d = decode(text);
  if (!d || d.hrp !== 'naddr') return null;
  let identifier = null, pubkey = null, kind = null; const relays = [];
  try {
    for (let i = 0; i < d.bytes.length;) {
      const type = d.bytes[i], len = d.bytes[i + 1];
      if (len === undefined || i + 2 + len > d.bytes.length) return null;
      const value = d.bytes.slice(i + 2, i + 2 + len); i += 2 + len;
      if (type === SPECIAL) { if (identifier !== null) return null; identifier = utf8Strict.decode(new Uint8Array(value)); }
      else if (type === AUTHOR) { if (pubkey !== null || len !== 32) return null; pubkey = toHex(value); }
      else if (type === KIND) { if (kind !== null || len !== 4) return null; kind = ((value[0] << 24) >>> 0) + (value[1] << 16) + (value[2] << 8) + value[3]; }
      else if (type === RELAY) relays.push(utf8Strict.decode(new Uint8Array(value)));
    }
  } catch { return null; }
  return identifier !== null && pubkey !== null && kind !== null ? { kind, pubkey, identifier, relays } : null;
}
