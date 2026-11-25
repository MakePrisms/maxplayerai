//! NIP-44/NIP-59 transport with relay-compatible outer timestamps. Content policy is
//! deliberately outside `wrap`: payments reuse the primitive, never content recipients.
use super::{Error, PreparedContent, Result};
use nostr_sdk::{nostr::nips::nip44, prelude::*};

pub const TIMESTAMP_TWEAK_SECS: u64 = 180;
pub const MAX_WRAPPER_CONTENT: usize = 128 * 1024;

// Pinned to nostr 0.44's NIP-44 implementation; the boundary test deliberately
// fails on a library upgrade so its transport/peer compatibility is reconsidered.
pub(crate) const NIP44_MAX: usize = 65_408;

/// Largest rumor (bytes of rumor JSON) whose NIP-59 seal still fits [`NIP44_MAX`]: 40,960.
/// NIP-44 pads to 1/8 of the next power of two, so above 32 KiB the seal grows in 8 KiB steps:
/// every rumor of 40,961..=49,152 bytes seals to the same 65,971 bytes (#1349). Size guards on
/// anything that rides in a rumor derive from this, never from a round 60 KiB.
// The size model's only non-test users are the review lane's (`wallet`) guards.
#[cfg_attr(not(feature = "wallet"), allow(dead_code))]
pub(crate) const MAX_SEALABLE_RUMOR: usize = max_sealable_rumor();

/// Seal JSON minus its content: fixed-width id, pubkey, sig, a 10-digit created_at, kind 13 and
/// empty tags. Pinned against a real seal by `seal_size_model_matches_wrap`.
#[cfg_attr(not(feature = "wallet"), allow(dead_code))]
const SEAL_JSON_OVERHEAD: usize = 343;

/// nostr 0.44 `nip44::v2::calc_padding`.
#[cfg_attr(not(feature = "wallet"), allow(dead_code))]
const fn nip44_padded_len(len: usize) -> usize {
    if len <= 32 {
        return 32;
    }
    let nextpower = 1usize << ((usize::BITS - 1 - (len - 1).leading_zeros()) + 1);
    let chunk = if nextpower <= 256 { 32 } else { nextpower / 8 };
    chunk * (((len - 1) / chunk) + 1)
}

/// Seal JSON bytes for a rumor of `rumor` bytes: base64(version + nonce + u16 length + padded
/// plaintext + MAC) plus the fixed fields. Deterministic: keys change bytes, never lengths.
#[cfg_attr(not(feature = "wallet"), allow(dead_code))]
pub(crate) const fn seal_json_len(rumor: usize) -> usize {
    SEAL_JSON_OVERHEAD + (1 + 32 + 2 + nip44_padded_len(rumor) + 32).div_ceil(3) * 4
}

#[cfg_attr(not(feature = "wallet"), allow(dead_code))]
const fn max_sealable_rumor() -> usize {
    let mut len = NIP44_MAX;
    while len > 0 && seal_json_len(len) > NIP44_MAX {
        len -= 1;
    }
    len
}

/// The checks [`wrap_checked`] applies to its two NIP-44 layers, without encrypting: builds the
/// same rumor (only its length matters) and predicts the seal from it. For trimming a message
/// to what will actually seal; [`wrap_checked`] stays the authority at send time. Tests only:
/// production trims on [`sealed_len`].
#[cfg(test)]
pub(crate) fn check_sealable(
    recipient: PublicKey,
    message: String,
) -> std::result::Result<(), WrapError> {
    let rumor = rumor_json_len(recipient, message);
    check_size("rumor", rumor, NIP44_MAX)?;
    check_size("seal", seal_json_len(rumor), NIP44_MAX)
}

/// Seal JSON bytes [`wrap_checked`] would produce for `message` (also past the limit).
#[cfg_attr(not(feature = "wallet"), allow(dead_code))]
pub(crate) fn sealed_len(recipient: PublicKey, message: String) -> usize {
    seal_json_len(rumor_json_len(recipient, message))
}

/// The rumor [`wrap_checked`] builds; the author is any fixed-width key, so the recipient serves.
#[cfg_attr(not(feature = "wallet"), allow(dead_code))]
fn rumor_json_len(recipient: PublicKey, message: String) -> usize {
    let mut rumor = EventBuilder::private_msg_rumor(recipient, message)
        .allow_self_tagging()
        .build(recipient);
    rumor.ensure_id();
    rumor.as_json().len()
}

#[derive(Debug)]
pub(crate) enum WrapError {
    Size {
        layer: &'static str,
        actual: usize,
        allowed: usize,
    },
    Other(&'static str),
}
fn check_size(
    layer: &'static str,
    actual: usize,
    allowed: usize,
) -> std::result::Result<(), WrapError> {
    if actual > allowed {
        return Err(WrapError::Size {
            layer,
            actual,
            allowed,
        });
    }
    Ok(())
}

pub async fn wrap(keys: &Keys, recipient: PublicKey, message: String) -> Result<Event> {
    wrap_checked(keys, recipient, message)
        .await
        .map_err(|error| match error {
            WrapError::Other(message) => Error(message),
            WrapError::Size { layer: "rumor", .. } => {
                Error("private rumor exceeds NIP-44 size limit")
            }
            WrapError::Size { layer: "seal", .. } => {
                Error("private seal exceeds NIP-44 size limit")
            }
            WrapError::Size { .. } => Error("encrypted wrapper too large"),
        })
}

/// One builder for content and review messages. Measure the actual plaintext at
/// each encryption layer, including padding, base64 and JSON escape expansion.
pub(crate) async fn wrap_checked(
    keys: &Keys,
    recipient: PublicKey,
    message: String,
) -> std::result::Result<Event, WrapError> {
    let mut rumor = EventBuilder::private_msg_rumor(recipient, message)
        .allow_self_tagging()
        .build(keys.public_key());
    rumor.ensure_id();
    check_size("rumor", rumor.as_json().len(), NIP44_MAX)?;
    let seal = EventBuilder::seal(keys, &recipient, rumor)
        .await
        .map_err(|_| WrapError::Other("could not construct private seal"))?
        .sign(keys)
        .await
        .map_err(|_| WrapError::Other("could not sign private seal"))?;
    check_size("seal", seal.as_json().len(), NIP44_MAX)?;
    let ephemeral = Keys::generate();
    let ciphertext = nip44::encrypt(
        ephemeral.secret_key(),
        &recipient,
        seal.as_json(),
        nip44::Version::default(),
    )
    .map_err(|_| WrapError::Other("could not encrypt private wrapper"))?;
    check_size("wrapper", ciphertext.len(), MAX_WRAPPER_CONTENT)?;
    EventBuilder::new(Kind::GiftWrap, ciphertext)
        .tags([Tag::public_key(recipient)])
        .custom_created_at(fresh_created_at())
        .sign_with_keys(&ephemeral)
        .map_err(|_| WrapError::Other("could not sign private wrapper"))
}

#[cfg(test)]
mod size_tests {
    use super::*;
    #[test]
    fn nip44_library_plaintext_limit_is_pinned() {
        let keys = Keys::generate();
        assert!(
            nip44::encrypt(
                keys.secret_key(),
                &keys.public_key(),
                "x".repeat(NIP44_MAX),
                nip44::Version::default()
            )
            .is_ok()
        );
        assert!(
            nip44::encrypt(
                keys.secret_key(),
                &keys.public_key(),
                "x".repeat(NIP44_MAX + 1),
                nip44::Version::default()
            )
            .is_err()
        );
    }

    /// #1349: the seal grows in NIP-44 padding steps, so the sealable rumor ceiling is 40,960
    /// bytes, well below `NIP44_MAX`. This pins our own const model only; a library change to
    /// padding or seal shape fails `seal_size_model_matches_wrap`, which seals at both ceilings.
    #[test]
    fn sealable_rumor_ceiling_is_pinned() {
        assert_eq!(MAX_SEALABLE_RUMOR, 40_960);
        assert_eq!(seal_json_len(40_960), 55_047);
        assert_eq!(seal_json_len(40_961), 65_971);
        assert_eq!(seal_json_len(49_152), 65_971);
    }

    /// The size model is exact against real seals, and `check_sealable` agrees with
    /// `wrap_checked` on both sides of the ceiling.
    #[tokio::test]
    async fn seal_size_model_matches_wrap() {
        let (keys, recipient) = (Keys::generate(), Keys::generate().public_key());
        let rumor_len = |message: &str| {
            let mut rumor = EventBuilder::private_msg_rumor(recipient, message)
                .allow_self_tagging()
                .build(keys.public_key());
            rumor.ensure_id();
            rumor.as_json().len()
        };
        let overhead = rumor_len("");
        let recipient_keys = Keys::generate();
        // The exact ceilings against real seals, unescaped so every message byte is a rumor byte:
        // the seal layer at MAX_SEALABLE_RUMOR and the rumor layer at NIP44_MAX, a byte each side.
        for (rumor, refused) in [
            (MAX_SEALABLE_RUMOR, None),
            (MAX_SEALABLE_RUMOR + 1, Some(("seal", 65_971))),
            (NIP44_MAX, Some(("seal", 87_815))),
            (NIP44_MAX + 1, Some(("rumor", NIP44_MAX + 1))),
        ] {
            let message = "x".repeat(rumor - overhead);
            assert_eq!(rumor_len(&message), rumor);
            let to = recipient_keys.public_key();
            let wrapped = wrap_checked(&keys, to, message.clone()).await;
            let predicted = check_sealable(to, message);
            match refused {
                None => {
                    predicted.unwrap();
                    let outer = wrapped.unwrap();
                    let seal =
                        nip44::decrypt(recipient_keys.secret_key(), &outer.pubkey, &outer.content)
                            .unwrap();
                    assert_eq!(seal.len(), 55_047, "{rumor}");
                    assert_eq!(seal.len(), seal_json_len(rumor), "{rumor}");
                }
                Some((layer, actual)) => {
                    for result in [wrapped.map(|_| ()), predicted] {
                        match result {
                            Err(WrapError::Size {
                                layer: l,
                                actual: a,
                                allowed,
                            }) => {
                                assert_eq!((l, a, allowed), (layer, actual, NIP44_MAX), "{rumor}")
                            }
                            other => panic!("{rumor}: {other:?}"),
                        }
                    }
                }
            }
        }
        for len in [
            1, 100, 255, 1_000, 30_000, 40_000, 40_681, 40_682, 40_683, 45_000, 60_000,
        ] {
            // Quotes and newlines escape, so the rumor is longer than the message.
            let message: String = "ab\"\n".repeat(len)[..len].to_owned();
            let wrapped = wrap_checked(&keys, recipient, message.clone()).await;
            let rumor = rumor_len(&message);
            assert!(rumor >= overhead + len, "{len}");
            match (&wrapped, check_sealable(recipient, message)) {
                (Ok(outer), Ok(())) => {
                    let seal = nip44::decrypt(
                        Keys::generate().secret_key(),
                        &outer.pubkey,
                        &outer.content,
                    );
                    assert!(seal.is_err(), "only the recipient decrypts");
                    assert!(rumor <= MAX_SEALABLE_RUMOR, "{len}");
                }
                (
                    Err(WrapError::Size { layer, actual, .. }),
                    Err(WrapError::Size {
                        layer: l,
                        actual: a,
                        ..
                    }),
                ) => {
                    assert_eq!((*layer, *actual), (l, a), "{len}");
                    assert!(rumor > MAX_SEALABLE_RUMOR, "{len}");
                }
                _ => panic!("check_sealable disagrees with wrap_checked at {len}"),
            }
        }
        // Exact seal length on a sealing size: decrypt the real wrapper as the recipient.
        for len in [500, 20_000, 40_000] {
            let message = "x".repeat(len);
            let outer = wrap_checked(&keys, recipient_keys.public_key(), message.clone())
                .await
                .unwrap();
            let seal =
                nip44::decrypt(recipient_keys.secret_key(), &outer.pubkey, &outer.content).unwrap();
            assert_eq!(seal.len(), seal_json_len(rumor_len(&message)), "{len}");
        }
    }
}

/// Authenticate the transport envelope before domain-specific decoding. Callers must
/// dispatch domains independently and never log failed-decode plaintext.
pub async fn unwrap_message(keys: &Keys, event: &Event) -> Result<(PublicKey, String)> {
    if event.kind != Kind::GiftWrap || event.content.len() > MAX_WRAPPER_CONTENT {
        return Err(Error("invalid private wrapper"));
    }
    event
        .verify()
        .map_err(|_| Error("invalid wrapper signature"))?;
    let expected = Tag::public_key(keys.public_key());
    if event.tags.len() != 1 || event.tags.iter().next() != Some(&expected) {
        return Err(Error("wrong outer recipient"));
    }
    let seal_json = nip44::decrypt(keys.secret_key(), &event.pubkey, &event.content)
        .map_err(|_| Error("invalid wrapper decryption"))?;
    super::strict_json::validate(seal_json.as_bytes())?;
    let seal = Event::from_json(&seal_json).map_err(|_| Error("invalid seal"))?;
    if seal.kind != Kind::Seal || !seal.tags.is_empty() {
        return Err(Error("invalid seal kind or tags"));
    }
    seal.verify().map_err(|_| Error("invalid seal signature"))?;
    let rumor_json = nip44::decrypt(keys.secret_key(), &seal.pubkey, &seal.content)
        .map_err(|_| Error("invalid rumor decryption"))?;
    super::strict_json::validate(rumor_json.as_bytes())?;
    let rumor = UnsignedEvent::from_json(&rumor_json).map_err(|_| Error("invalid rumor"))?;
    rumor.verify_id().map_err(|_| Error("invalid rumor id"))?;
    if rumor.kind != Kind::PrivateDirectMessage
        || seal.pubkey != rumor.pubkey
        || rumor.tags.len() != 1
        || rumor.tags.iter().next() != Some(&expected)
    {
        return Err(Error("wrong rumor author, kind or recipient"));
    }
    Ok((seal.pubkey, rumor.content))
}

pub async fn unwrap_content(keys: &Keys, event: &Event) -> Result<PreparedContent> {
    let (author, message) = unwrap_message(keys, event).await?;
    let content = PreparedContent::decode(&message)?;
    if content.body().author != author.to_hex()
        || !content
            .body()
            .recipients
            .contains(&keys.public_key().to_hex())
    {
        return Err(Error("content author or recipient mismatch"));
    }
    Ok(content)
}

/// Overlap outer timestamp randomization. Logical IDs, not wrapper IDs, deduplicate.
pub fn receive_since(last_received: u64) -> u64 {
    last_received.saturating_sub(TIMESTAMP_TWEAK_SECS + 60)
}

pub fn fresh_created_at() -> Timestamp {
    Timestamp::tweaked(0..TIMESTAMP_TWEAK_SECS)
}
