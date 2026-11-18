//! Cosigned mint link (`maxplayer account link --mint`): link a mint server's Nostr key to the
//! email account its operator already has, without a second email code, because BOTH keys sign.
//!
//! 1. `POST /account/link/mint/challenge`, NIP-98 signed by the **account key** (a key already
//!    linked by email code), body `{mint_pubkey}`. The service stores a fresh random challenge
//!    bound to (account key, mint key) and answers it with its lifetime (default 5 minutes).
//! 2. `POST /account/link/mint/confirm`, NIP-98 signed by the **account key** again (its `payload`
//!    tag covers the whole body), body `{challenge, mint_pubkey, label, mint_proof}`.
//!    `mint_proof` is a Nostr event signed by the **mint key** ([`build_proof`]): kind
//!    [`PROOF_KIND`], exactly one `u` tag = the confirm endpoint's public URL, one `challenge` tag
//!    = the challenge, one `p` tag = the account key, `created_at` within the NIP-98 skew.
//!
//! So the account key signs the endpoint, the challenge and the mint key (inside the body it
//! hashes), and the mint key signs the endpoint, the challenge and the account key. The challenge
//! row binds both keys and is consumed by the first successful confirm. The proof uses its own
//! kind, not 27235, so it can never pass as a NIP-98 authorization for any route.

use nostr::{
    Alphabet, Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, SingleLetterTag, Tag, TagKind,
    Timestamp,
};

/// Event kind of the mint key's proof. Ephemeral range (relays do not store it) and distinct
/// from NIP-98's 27235.
pub const PROOF_KIND: u16 = 27236;

/// Largest accepted `mint_proof` JSON. A real one is well under 1 KiB.
const MAX_PROOF_LEN: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofError {
    Malformed,
    WrongKind,
    BadSignature,
    /// Signed by a key other than the mint key named in the request.
    WrongKey,
    WrongUrl,
    WrongChallenge,
    /// The `p` tag names a key other than the account key that signed the request.
    WrongAccount,
    Stale,
}

impl ProofError {
    /// Stable `reason` returned next to `error: "mint_proof_invalid"`.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::WrongKind => "wrong_kind",
            Self::BadSignature => "bad_signature",
            Self::WrongKey => "wrong_key",
            Self::WrongUrl => "wrong_url",
            Self::WrongChallenge => "wrong_challenge",
            Self::WrongAccount => "wrong_account",
            Self::Stale => "stale",
        }
    }
}

fn single<'a>(event: &'a Event, kind: &TagKind<'_>) -> Option<&'a str> {
    let mut it = event.tags.iter().filter(|t| &t.kind() == kind);
    let first = it.next()?.content();
    if it.next().is_some() {
        return None;
    }
    first
}

fn u_kind() -> TagKind<'static> {
    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::U))
}

fn p_kind() -> TagKind<'static> {
    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::P))
}

fn challenge_kind() -> TagKind<'static> {
    TagKind::custom("challenge")
}

/// Check the mint key's proof for this confirm request.
pub fn verify_proof(
    proof_json: &str,
    mint: &PublicKey,
    account: &PublicKey,
    challenge: &str,
    expected_url: &str,
    now: u64,
    max_skew_secs: u64,
) -> Result<(), ProofError> {
    if proof_json.len() > MAX_PROOF_LEN {
        return Err(ProofError::Malformed);
    }
    let event = Event::from_json(proof_json).map_err(|_| ProofError::Malformed)?;
    if event.kind != Kind::from(PROOF_KIND) {
        return Err(ProofError::WrongKind);
    }
    event.verify().map_err(|_| ProofError::BadSignature)?;
    if &event.pubkey != mint {
        return Err(ProofError::WrongKey);
    }
    if !single(&event, &u_kind()).is_some_and(|u| crate::nip98::same_url(u, expected_url)) {
        return Err(ProofError::WrongUrl);
    }
    if single(&event, &challenge_kind()) != Some(challenge) {
        return Err(ProofError::WrongChallenge);
    }
    let account_hex = account.to_hex();
    if single(&event, &p_kind()) != Some(account_hex.as_str()) {
        return Err(ProofError::WrongAccount);
    }
    if event.created_at.as_secs().abs_diff(now) > max_skew_secs {
        return Err(ProofError::Stale);
    }
    Ok(())
}

/// The proof the mint key signs (what the CLI builds; used by the tests).
pub fn build_proof(
    mint: &Keys,
    url: &str,
    challenge: &str,
    account: &PublicKey,
    created_at: Timestamp,
) -> Event {
    EventBuilder::new(Kind::from(PROOF_KIND), "")
        .tags([
            Tag::custom(u_kind(), [url.to_string()]),
            Tag::custom(challenge_kind(), [challenge.to_string()]),
            Tag::custom(p_kind(), [account.to_hex()]),
        ])
        .custom_created_at(created_at)
        .sign_with_keys(mint)
        .expect("signing with local keys cannot fail")
}

/// 32 random bytes from the OS RNG, hex.
pub fn new_challenge() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("OS RNG");
    hex::encode(b)
}

/// A challenge as the API accepts it: 64 lowercase hex.
pub fn valid_challenge(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://relay.maxplayer.ai/account/link/mint/confirm";
    const NOW: u64 = 1_800_000_000;

    struct Case {
        mint: Keys,
        account: Keys,
        challenge: String,
    }

    fn case() -> Case {
        Case {
            mint: Keys::generate(),
            account: Keys::generate(),
            challenge: new_challenge(),
        }
    }

    fn check(c: &Case, proof: &Event) -> Result<(), ProofError> {
        verify_proof(
            &proof.as_json(),
            &c.mint.public_key(),
            &c.account.public_key(),
            &c.challenge,
            URL,
            NOW,
            60,
        )
    }

    fn proof(c: &Case) -> Event {
        build_proof(
            &c.mint,
            URL,
            &c.challenge,
            &c.account.public_key(),
            Timestamp::from(NOW),
        )
    }

    #[test]
    fn a_proof_built_for_the_request_verifies() {
        let c = case();
        assert_eq!(check(&c, &proof(&c)), Ok(()));
        // A trailing slash on the signed URL is not significant (as for NIP-98).
        let p = build_proof(
            &c.mint,
            &format!("{URL}/"),
            &c.challenge,
            &c.account.public_key(),
            Timestamp::from(NOW),
        );
        assert_eq!(check(&c, &p), Ok(()));
        assert!(valid_challenge(&c.challenge));
    }

    #[test]
    fn each_binding_is_checked() {
        let c = case();
        let account = c.account.public_key();
        let ts = Timestamp::from(NOW);
        // Another key signed it (the mint key was swapped).
        let other = build_proof(&Keys::generate(), URL, &c.challenge, &account, ts);
        assert_eq!(check(&c, &other), Err(ProofError::WrongKey));
        // Another endpoint.
        let p = build_proof(
            &c.mint,
            "https://relay.maxplayer.ai/account/link/confirm",
            &c.challenge,
            &account,
            ts,
        );
        assert_eq!(check(&c, &p), Err(ProofError::WrongUrl));
        // Another challenge.
        let p = build_proof(&c.mint, URL, &new_challenge(), &account, ts);
        assert_eq!(check(&c, &p), Err(ProofError::WrongChallenge));
        // Made out for another account key.
        let p = build_proof(
            &c.mint,
            URL,
            &c.challenge,
            &Keys::generate().public_key(),
            ts,
        );
        assert_eq!(check(&c, &p), Err(ProofError::WrongAccount));
        // Too old and too new.
        for t in [NOW - 61, NOW + 61] {
            let p = build_proof(&c.mint, URL, &c.challenge, &account, Timestamp::from(t));
            assert_eq!(check(&c, &p), Err(ProofError::Stale));
        }
    }

    #[test]
    fn wrong_kind_bad_signature_duplicates_and_garbage_are_refused() {
        let c = case();
        let account = c.account.public_key();
        // A NIP-98 event (27235) is never a proof, even with the same tags.
        let nip98 = EventBuilder::new(Kind::HttpAuth, "")
            .tags([
                Tag::custom(u_kind(), [URL.to_string()]),
                Tag::custom(challenge_kind(), [c.challenge.clone()]),
                Tag::custom(p_kind(), [account.to_hex()]),
            ])
            .custom_created_at(Timestamp::from(NOW))
            .sign_with_keys(&c.mint)
            .unwrap();
        assert_eq!(check(&c, &nip98), Err(ProofError::WrongKind));
        // Edited after signing.
        let mut v: serde_json::Value = serde_json::from_str(&proof(&c).as_json()).unwrap();
        v["created_at"] = serde_json::json!(NOW + 1);
        let r = verify_proof(
            &v.to_string(),
            &c.mint.public_key(),
            &account,
            &c.challenge,
            URL,
            NOW,
            60,
        );
        assert_eq!(r, Err(ProofError::BadSignature));
        // Two challenge tags: ambiguous, refused.
        let p = EventBuilder::new(Kind::from(PROOF_KIND), "")
            .tags([
                Tag::custom(u_kind(), [URL.to_string()]),
                Tag::custom(challenge_kind(), [c.challenge.clone()]),
                Tag::custom(challenge_kind(), [new_challenge()]),
                Tag::custom(p_kind(), [account.to_hex()]),
            ])
            .custom_created_at(Timestamp::from(NOW))
            .sign_with_keys(&c.mint)
            .unwrap();
        assert_eq!(check(&c, &p), Err(ProofError::WrongChallenge));
        let r = verify_proof("{}", &c.mint.public_key(), &account, "x", URL, NOW, 60);
        assert_eq!(r, Err(ProofError::Malformed));
    }
}
