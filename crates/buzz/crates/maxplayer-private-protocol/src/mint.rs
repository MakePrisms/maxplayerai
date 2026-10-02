//! Shared canonical npub decoding for mint addresses. No wallet dependency.
const BECH32_CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn bech32_polymod(values: impl Iterator<Item = u8>) -> u32 {
    const GENERATOR: [u32; 5] = [
        0x3b6a_57b2,
        0x2650_8e6d,
        0x1ea1_19fa,
        0x3d42_33dd,
        0x2a14_62b3,
    ];
    let mut checksum: u32 = 1;
    for value in values {
        let top = checksum >> 25;
        checksum = ((checksum & 0x01ff_ffff) << 5) ^ u32::from(value);
        for (bit, generator) in GENERATOR.iter().enumerate() {
            if (top >> bit) & 1 == 1 {
                checksum ^= generator;
            }
        }
    }
    checksum
}

/// Decode a lowercase bech32 (BIP-173, not bech32m) `npub1…` into its 32-byte key, or `None`.
///
/// Shared with core mint transport validation, including wallet-less builds.
/// The checksum and payload length are checked; whether the 32
/// bytes are an x-coordinate on the curve is left to the connector, which refuses it at construction.
pub fn decode_npub(npub: &str) -> Option<[u8; 32]> {
    const HRP: &str = "npub";
    // "npub" + "1" + 52 data chars (32 bytes) + 6 checksum chars.
    if npub.len() != 63 || !npub.starts_with("npub1") {
        return None;
    }
    let data: Vec<u8> = npub[HRP.len() + 1..]
        .bytes()
        .map(|byte| {
            BECH32_CHARSET
                .iter()
                .position(|c| *c == byte)
                .map(|p| p as u8)
        })
        .collect::<Option<_>>()?;
    let hrp_expanded = HRP
        .bytes()
        .map(|b| b >> 5)
        .chain(std::iter::once(0))
        .chain(HRP.bytes().map(|b| b & 31));
    if bech32_polymod(hrp_expanded.chain(data.iter().copied())) != 1 {
        return None;
    }
    let payload = &data[..data.len() - 6];
    let mut out = [0u8; 32];
    let (mut acc, mut bits, mut index) = (0u32, 0u32, 0usize);
    for value in payload {
        acc = (acc << 5) | u32::from(*value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            *out.get_mut(index)? = (acc >> bits) as u8;
            index += 1;
            acc &= (1 << bits) - 1;
        }
    }
    // 52 * 5 = 260 bits = 32 bytes + 4 padding bits, which must be zero.
    (index == 32 && bits < 5 && acc == 0).then_some(out)
}
