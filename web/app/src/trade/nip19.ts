/**
 * npub encode/decode (bech32, BIP-173 — not bech32m). Pure, no DOM.
 *
 * `decodeNpub` mirrors `decode_npub` in
 * crates/buzz/crates/maxplayer-private-protocol/src/mint.rs, the canonical
 * check behind `nostr://npub1…` mint addresses: lowercase only, exactly 63
 * characters, valid checksum, 32-byte payload with zero padding bits.
 */
const CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const GEN = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];

function polymod(values: number[]): number {
  let chk = 1;
  for (const v of values) {
    const top = chk >>> 25;
    chk = (((chk & 0x1ffffff) << 5) ^ v) >>> 0;
    for (let i = 0; i < 5; i++) if ((top >>> i) & 1) chk = (chk ^ (GEN[i] as number)) >>> 0;
  }
  return chk;
}

const hrpExpand = (hrp: string): number[] => [
  ...[...hrp].map((c) => c.charCodeAt(0) >> 5),
  0,
  ...[...hrp].map((c) => c.charCodeAt(0) & 31),
];

/** 32-byte x-only key from a canonical npub, or null. */
export function decodeNpub(npub: string): Uint8Array | null {
  if (npub.length !== 63 || !npub.startsWith("npub1")) return null;
  const data: number[] = [];
  for (const c of npub.slice(5)) {
    const p = CHARSET.indexOf(c);
    if (p < 0) return null;
    data.push(p);
  }
  if (polymod([...hrpExpand("npub"), ...data]) !== 1) return null;
  const out = new Uint8Array(32);
  let acc = 0, bits = 0, index = 0;
  for (const v of data.slice(0, -6)) {
    acc = (acc << 5) | v;
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      if (index >= 32) return null;
      out[index++] = (acc >> bits) & 0xff;
      acc &= (1 << bits) - 1;
    }
  }
  return index === 32 && bits < 5 && acc === 0 ? out : null;
}

/** npub for a 64-char lowercase hex key. */
export function npubEncode(hex: string): string {
  if (!/^[0-9a-f]{64}$/.test(hex)) throw new Error("expected 32-byte hex key");
  const data: number[] = [];
  let acc = 0, bits = 0;
  for (let i = 0; i < 64; i += 2) {
    acc = (acc << 8) | parseInt(hex.slice(i, i + 2), 16);
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      data.push((acc >> bits) & 31);
    }
    acc &= (1 << bits) - 1;
  }
  if (bits > 0) data.push((acc << (5 - bits)) & 31);
  const mod = polymod([...hrpExpand("npub"), ...data, 0, 0, 0, 0, 0, 0]) ^ 1;
  const checksum = Array.from({ length: 6 }, (_, i) => (mod >>> (5 * (5 - i))) & 31);
  return "npub1" + [...data, ...checksum].map((v) => CHARSET[v]).join("");
}
