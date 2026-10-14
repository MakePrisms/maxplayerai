//! NIP-44/NIP-59 transport with relay-compatible outer timestamps. Content policy is
//! deliberately outside `wrap`: payments reuse the primitive, never content recipients.
use super::{Error, PreparedContent, Result};
use nostr_sdk::{nostr::nips::nip44, prelude::*};

pub const TIMESTAMP_TWEAK_SECS: u64 = 180;
pub const MAX_WRAPPER_CONTENT: usize = 128 * 1024;

// Pinned to nostr 0.44's NIP-44 implementation; the boundary test deliberately
// fails on a library upgrade so its transport/peer compatibility is reconsidered.
pub(crate) const NIP44_MAX: usize = 65_408;

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
