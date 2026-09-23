//! Wire envelope for a Cashu mint reached over Nostr (`nostr://<npub>`), shared by the wallet-side
//! connector in [`crate::nostr_mint`] and the future `maxplayer-mint` sidecar.
//!
//! Maxplayer-specific and versioned; **not a NUT**. See `docs/specs/seller-credits.md` §3.
//!
//! - **Request**: kind [`REQUEST_KIND`] (23410), tag `["p", <mint hex>]`, NIP-44 v2 content to the
//!   mint key, signed by a per-session throwaway client key. Plaintext is a [`Request`]:
//!   `{"v":1,"id":…,"op":…,"body":<NUT JSON>,"exp":<unix>}`.
//! - **Response**: kind [`RESPONSE_KIND`] (23411), tags `["p", <client hex>]` and `["e", <request
//!   event id>]`, NIP-44 v2 content to the client, signed by the mint key. Plaintext is a
//!   [`Response`]: `{"v":1,"id":…,"ok":<NUT JSON>}` or `{"v":1,"id":…,"err":{"code":…,"detail":…}}`.
//!
//! Both kinds are in the ephemeral range, so a well-behaved relay forwards them and stores nothing.
//! No collision with [`crate::kinds`] (3400–3407, 30340).
//!
//! This module deliberately depends on `serde`/`serde_json` only — no cdk, no nostr — so the sidecar
//! can reuse it without pulling in the wallet, and core never pulls in a mint.

use serde::{Deserialize, Serialize};

/// Envelope version carried in every request and response plaintext (`"v"`).
pub const PROTOCOL_VERSION: u8 = 1;

/// Nostr event kind of a wallet → mint request (ephemeral range).
pub const REQUEST_KIND: u16 = 23410;

/// Nostr event kind of a mint → wallet response (ephemeral range).
pub const RESPONSE_KIND: u16 = 23411;

/// NIP-44 v2 caps the plaintext at 65,535 bytes. A request whose serialized [`Request`] is larger is
/// refused before anything is published.
pub const MAX_PLAINTEXT_BYTES: usize = 65_535;

/// URL scheme of a Nostr-transported mint: `nostr://<npub>`.
pub const NOSTR_MINT_SCHEME: &str = "nostr://";

/// Public relays a `nostr://` mint and its wallets use IN ADDITION to the home's own `relay_url`
/// (spec decision 15). Each was checked live on 23 Sep 2026 with fresh throwaway keys: it accepts
/// and delivers kinds 23410/23411 from a NIP-42-enabled client, carries a 51 KB plaintext, stores
/// neither kind afterwards, and re-delivers an identical re-sent event (the lost-reply path).
/// Refused for the record: relay.damus.io and relay.primal.net (stored both kinds), nos.lol and
/// offchain.pub (refused the 51 KB request, stored events), auth.nostr1.com (stored),
/// nostr.einundzwanzig.space (NIP-05 required to publish).
pub const FALLBACK_RELAYS: &[&str] = &["wss://relay.ditto.pub", "wss://nostr-pub.wellorder.net"];

/// Operation names (`"op"`). One per cdk `MintConnector` method that reaches the mint.
pub mod op {
    /// NUT-06 mint info. Body: `null`.
    pub const INFO: &str = "info";
    /// NUT-01 active keys. Body: `null`. Ok: `KeysResponse`.
    pub const KEYS: &str = "keys";
    /// NUT-01 one keyset's keys. Body: `{"id": <keyset id>}`. Ok: `KeysResponse`.
    pub const KEYSET: &str = "keyset";
    /// NUT-02 keysets. Body: `null`. Ok: `KeysetResponse`.
    pub const KEYSETS: &str = "keysets";
    /// NUT-03 swap. Body: `SwapRequest`. Ok: `SwapResponse`.
    pub const SWAP: &str = "swap";
    /// NUT-07 check state. Body: `CheckStateRequest`. Ok: `CheckStateResponse`.
    pub const CHECKSTATE: &str = "checkstate";
    /// NUT-09 restore. Body: `RestoreRequest`. Ok: `RestoreResponse`.
    pub const RESTORE: &str = "restore";
    /// NUT-04 mint quote. Body: `{"method", "request": <method's quote request>}`.
    pub const MINT_QUOTE: &str = "mint_quote";
    /// NUT-04 mint quote status. Body: `{"method", "quote": <quote id>}`.
    pub const MINT_QUOTE_STATUS: &str = "mint_quote_status";
    /// NUT-04 mint. Body: `{"method", "request": MintRequest}`.
    pub const MINT: &str = "mint";
    /// NUT-29 batch quote status. Body: `{"method", "request": BatchCheckMintQuoteRequest}`.
    pub const MINT_QUOTE_CHECK_BATCH: &str = "mint_quote_check_batch";
    /// NUT-29 batch mint. Body: `{"method", "request": BatchMintRequest}`.
    pub const MINT_BATCH: &str = "mint_batch";
    /// NUT-05 melt quote. Body: `{"method", "request": <method's quote request>}`.
    pub const MELT_QUOTE: &str = "melt_quote";
    /// NUT-05 melt quote status. Body: `{"method", "quote": <quote id>}`.
    pub const MELT_QUOTE_STATUS: &str = "melt_quote_status";
    /// NUT-05 melt. Body: `{"method", "request": <method's melt request>}`.
    pub const MELT: &str = "melt";
}

/// Transport-level error codes (`err.code` as a string). A NUT error from the mint itself travels
/// as its numeric NUT code instead (see [`ErrorCode::Nut`]).
pub mod code {
    /// The mint does not serve this op (e.g. mint/melt on a local-issue mint). Definitive.
    pub const UNSUPPORTED: &str = "unsupported";
    /// The mint is over its request rate. Definitive: the request was not executed.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// The request could not be parsed or is invalid. Definitive.
    pub const BAD_REQUEST: &str = "bad_request";
    /// The request's `exp` had passed when the mint saw it. Definitive: not executed.
    pub const EXPIRED: &str = "expired";
    /// The mint failed internally. Ambiguous: the mint may or may not have committed.
    pub const INTERNAL: &str = "internal";
}

/// Request plaintext (NIP-44-encrypted into a kind-23410 event).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// [`PROTOCOL_VERSION`].
    pub v: u8,
    /// Logical request id, unique per request. Echoed in the response; a re-send carries the same.
    pub id: String,
    /// One of [`op`].
    pub op: String,
    /// The NUT JSON body of the operation (`null` for bodiless ops).
    pub body: serde_json::Value,
    /// Unix seconds after which the mint must not execute this request.
    pub exp: u64,
}

/// Response plaintext (NIP-44-encrypted into a kind-23411 event).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// [`PROTOCOL_VERSION`].
    pub v: u8,
    /// The [`Request::id`] this answers.
    pub id: String,
    /// `"ok": <NUT JSON>` or `"err": {…}`.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// The two response shapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The NUT JSON the equivalent HTTP endpoint would have returned.
    Ok(serde_json::Value),
    /// A refusal or failure.
    Err(ErrorBody),
}

/// `err` object of a [`Response`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Numeric NUT error code or a transport [`code`] string.
    pub code: ErrorCode,
    /// Human-readable detail.
    #[serde(default)]
    pub detail: String,
}

/// An error code: either the mint's own NUT error (so a wallet reacts exactly as it would to the
/// same error over HTTPS, e.g. 11001 token already spent) or a transport [`code`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ErrorCode {
    /// NUT error code (NUT-00 error response `code`).
    Nut(u16),
    /// Transport code, one of [`code`] (unknown strings are treated as ambiguous).
    Named(String),
}

/// Whether `url` uses the `nostr://` scheme at all (case-insensitive). A `nostr://` URL is never
/// handed to an HTTP client, well-formed or not.
pub fn is_nostr_scheme(url: &str) -> bool {
    url.len() >= NOSTR_MINT_SCHEME.len()
        && url[..NOSTR_MINT_SCHEME.len()].eq_ignore_ascii_case(NOSTR_MINT_SCHEME)
}

/// The npub of a well-formed `nostr://<npub>` mint URL, else `None`.
///
/// Well-formed means: the lowercase `nostr://` scheme, then exactly one lowercase bech32 `npub1…`
/// (valid checksum, 32-byte payload), and nothing else except optional trailing `/` (which
/// `cashu::MintUrl` strips anyway). No path, query or userinfo.
pub fn nostr_mint_npub(url: &str) -> Option<&str> {
    let npub = url.strip_prefix(NOSTR_MINT_SCHEME)?.trim_end_matches('/');
    decode_npub(npub).map(|_| npub)
}

/// Whether `url` is a well-formed `nostr://<npub>` mint URL (see [`nostr_mint_npub`]).
pub fn is_nostr_mint_url(url: &str) -> bool {
    nostr_mint_npub(url).is_some()
}

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
/// Dependency-free on purpose: [`crate::home::mint_allowed`] is compiled in every feature set,
/// including ones without nostr-sdk. The checksum and payload length are checked; whether the 32
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

#[cfg(test)]
mod tests {
    use super::*;

    // x of the secp256k1 generator — a valid key. Encoded independently of nostr-sdk.
    const G_NPUB: &str = "npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";

    #[test]
    fn mint_wire_decodes_a_known_npub() {
        let key = decode_npub(G_NPUB).expect("valid npub");
        assert_eq!(
            hex::encode(key),
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        );
    }

    #[cfg(feature = "gateway")]
    #[test]
    fn mint_wire_decode_npub_agrees_with_nostr_sdk() {
        use nostr_sdk::prelude::{Keys, ToBech32};
        for _ in 0..32 {
            let keys = Keys::generate();
            let npub = keys.public_key().to_bech32().unwrap();
            assert_eq!(
                decode_npub(&npub),
                Some(keys.public_key().to_bytes()),
                "{npub}"
            );
        }
    }

    #[test]
    fn mint_wire_refuses_malformed_npubs() {
        let mut flipped = G_NPUB.to_owned();
        let last = flipped.pop().unwrap();
        flipped.push(if last == 'd' { 'q' } else { 'd' });
        for bad in [
            "",
            "npub1",
            &flipped,                            // checksum
            &G_NPUB.to_uppercase(),              // uppercase is refused (lowercase-only)
            &G_NPUB[..62],                       // short
            &format!("{G_NPUB}q"),               // long
            &G_NPUB.replacen("npub", "nsec", 1), // wrong hrp
            &G_NPUB.replacen('x', "b", 1),       // 'b' is not in the charset
        ] {
            assert_eq!(decode_npub(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn mint_wire_recognizes_nostr_mint_urls() {
        let url = format!("nostr://{G_NPUB}");
        assert_eq!(nostr_mint_npub(&url), Some(G_NPUB));
        assert!(is_nostr_mint_url(&format!("{url}/")));
        assert!(is_nostr_scheme(&url));
        assert!(is_nostr_scheme(&url.to_uppercase()));
        for bad in [
            format!("nostr://{G_NPUB}/v1"),
            format!("nostr://{G_NPUB}?x=1"),
            format!("NOSTR://{G_NPUB}"),
            format!("nostr://user@{G_NPUB}"),
            format!("https://{G_NPUB}"),
            "nostr://".to_owned(),
            "nostr://npub1notakey".to_owned(),
        ] {
            assert!(!is_nostr_mint_url(&bad), "{bad}");
        }
        assert!(!is_nostr_scheme("https://mint.example"));
        assert!(!is_nostr_scheme("nost"));
    }

    #[test]
    fn mint_wire_envelopes_have_the_documented_json_shape() {
        let request = Request {
            v: PROTOCOL_VERSION,
            id: "abc".into(),
            op: op::SWAP.into(),
            body: serde_json::json!({"inputs": [], "outputs": []}),
            exp: 42,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({"v":1,"id":"abc","op":"swap","body":{"inputs":[],"outputs":[]},"exp":42})
        );

        let ok = Response {
            v: 1,
            id: "abc".into(),
            outcome: Outcome::Ok(serde_json::json!({"a": 1})),
        };
        let ok_json = serde_json::json!({"v":1,"id":"abc","ok":{"a":1}});
        assert_eq!(serde_json::to_value(&ok).unwrap(), ok_json);
        assert_eq!(serde_json::from_value::<Response>(ok_json).unwrap(), ok);

        let named = serde_json::json!({"v":1,"id":"abc","err":{"code":"rate_limited","detail":"slow down"}});
        let parsed: Response = serde_json::from_value(named.clone()).unwrap();
        assert_eq!(
            parsed.outcome,
            Outcome::Err(ErrorBody {
                code: ErrorCode::Named(code::RATE_LIMITED.into()),
                detail: "slow down".into()
            })
        );
        assert_eq!(serde_json::to_value(&parsed).unwrap(), named);

        let nut: Response =
            serde_json::from_value(serde_json::json!({"v":1,"id":"abc","err":{"code":11001}}))
                .unwrap();
        assert_eq!(
            nut.outcome,
            Outcome::Err(ErrorBody {
                code: ErrorCode::Nut(11001),
                detail: String::new()
            })
        );

        assert!(serde_json::from_str::<Response>(r#"{"v":1,"id":"abc"}"#).is_err());
    }
}
