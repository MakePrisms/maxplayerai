use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub fn result_content_hash_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

/// Domain separator for the co-signed receipt preimage (distinct from the receipt H-tuple
/// domain above so the two hashes can never collide).
pub const RECEIPT_PREIMAGE_DOMAIN: &str = "maxplayer/v1/receipt-preimage";

/// Marker committed in [`ReceiptPreimage::exec_metadata_commitment`] when no
/// exec-metadata is folded into the co-signature (the default today — see the type doc).
pub const EXEC_METADATA_COMMITMENT_EMPTY: &str = "none";

/// Delivered object kind bound (non-forgeably) into the co-signed receipt preimage.
///
/// In the preimage the kind is a signed field, so an unsigned path cannot be flipped to
/// reinterpret the same integrity hash as a different object kind. That property is the reason
/// [`Inline`](Self::Inline) is safe to add beside [`Fork`](Self::Fork): both slots carry a 64-hex
/// digest, and only the signed kind says which preimage it is a digest OF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryKind {
    /// Fork-tip `commit_oid` (git delivery).
    Fork,
    /// Inline answer carried in the result event's own content, with no git object anywhere.
    /// `delivery_integrity_hash` is then [`result_content_hash_hex`] over that content.
    Inline,
}

impl DeliveryKind {
    /// Wire label used in the preimage and the kind-3400 `delivery_kind` tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::Inline => "inline",
        }
    }
}

/// The message both parties schnorr-sign for a kind-3400 receipt.
///
/// Binds the trade **and** the delivered git object (`delivery_integrity_hash` +
/// `delivery_kind`).
///
/// Three deliberate properties of this preimage (money-semantics — do not silently "fix"):
/// - **`result_id` is EXCLUDED.** It is the seller's own result-kind event id, unknowable
///   when the seller signs at delivery (the signature is a tag *inside* that very event,
///   so including its id is circular). The result is still bound to the receipt by the
///   `["e", result_id, "", "reply"]` tag under the buyer's event-level nostr signature.
/// - **The realized `mint` is EXCLUDED.** The seller co-signs at delivery, BEFORE the buyer
///   picks which of the seller's `accepted_mints` it pays from, so a single co-signed mint would
///   make buyer/seller cosigs disagree on any non-default accepted mint (multi-mint claims). The
///   accepted-mint SET is still co-signed via `creq_hash`; the SPECIFIC realized mint is enforced
///   operationally on both ends (buyer pays only a mint ∈ accepted_mints; the seller redeem guard
///   `assert_redeem_mint` refuses a token from a mint outside its accepted set) and is proven by
///   the cashu token itself — it does not need to be in the signed preimage.
/// - **`exec_metadata_commitment` carries [`EXEC_METADATA_COMMITMENT_EMPTY`] today.** The
///   co-signature does not cover exec-metadata: `sig/seller` does not cover it
///   (seller-claimed, result-authoritative), and the buyer filters the echo — folding a
///   filtered set into the digest would break signature matching. Exec-metadata rides the
///   events as unsigned tags; the commitment is walk-forward (populating it later is
///   additive, never a retraction).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptPreimage {
    pub job_hash: String,
    pub offer_id: String,
    pub amount: u64,
    pub unit: String,
    /// Buyer nostr x-only pubkey hex (== offer author; the external anchor).
    pub buyer_pubkey: String,
    /// Seller nostr x-only pubkey hex (== accepted-claim seller; the external anchor).
    pub seller_pubkey: String,
    pub delivery_integrity_hash: String,
    /// Wire label for the delivered object kind — see [`DeliveryKind`] (live: `fork`).
    pub delivery_kind: String,
    /// Commitment over the echoed exec-metadata tag set, or [`EXEC_METADATA_COMMITMENT_EMPTY`].
    pub exec_metadata_commitment: String,
    /// SHA-256 hex of the seller-authored NUT-18 payment request (the `creqA…` string), so both
    /// co-signatures commit to the request the seller quoted. `None` for a claim that carries no
    /// `creq` — the slot is then omitted (not null), so the preimage hashes byte-identically to
    /// one built without a creq. `Some` once the seller authors a `creq`; the buyer sources it
    /// from the accepted claim's `creq` tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creq_hash: Option<String>,
}

impl ReceiptPreimage {
    /// Canonical JSON array (domain-prefixed, fixed field order) — the signed bytes.
    pub fn canonical_json(&self) -> String {
        let mut fields = vec![
            serde_json::json!(RECEIPT_PREIMAGE_DOMAIN),
            serde_json::json!(self.job_hash),
            serde_json::json!(self.offer_id),
            serde_json::json!(self.amount),
            serde_json::json!(self.unit),
            serde_json::json!(self.buyer_pubkey),
            serde_json::json!(self.seller_pubkey),
            serde_json::json!(self.delivery_integrity_hash),
            serde_json::json!(self.delivery_kind),
            serde_json::json!(self.exec_metadata_commitment),
        ];
        // Additive slot at a FIXED final position: folded ONLY when a creq_hash is present, so a
        // receipt with no creq hashes byte-identically to one built without a creq, while a bound
        // creq changes the co-signed digest.
        if let Some(creq_hash) = &self.creq_hash {
            fields.push(serde_json::json!(creq_hash));
        }
        serde_json::to_string(&serde_json::Value::Array(fields))
            .expect("receipt preimage is JSON-serializable")
    }

    /// SHA-256 digest both parties sign (schnorr `Message::from_digest`).
    pub fn digest_bytes(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_json().as_bytes());
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&hasher.finalize());
        bytes
    }

    /// Lowercase-hex form of [`Self::digest_bytes`].
    pub fn digest_hex(&self) -> String {
        hex::encode(self.digest_bytes())
    }
}

/// Canonical commitment over an echoed exec-metadata tag set.
///
/// Empty set → [`EXEC_METADATA_COMMITMENT_EMPTY`]. Otherwise a SHA-256 over the canonical
/// JSON of the tag rows (the caller passes the already-filtered canonical set, in order).
pub fn exec_metadata_commitment(tags: &[Vec<String>]) -> String {
    if tags.is_empty() {
        return EXEC_METADATA_COMMITMENT_EMPTY.to_owned();
    }
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(tags)
            .expect("exec-metadata tags are JSON-serializable")
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §6.4 — the two ends of an INLINE trade must co-sign the same bytes. The seller builds its
    /// preimage at delivery (`seller_node::run::delivery_receipt_preimage`) and the buyer rebuilds
    /// it at the pre-pay seam (`authorize_pay::receipt_preimage_for`); if one field maps
    /// differently the co-signature check refuses and the job cannot settle. This asserts the
    /// mapping agrees field for field, for the inline shape specifically.
    #[test]
    fn an_inline_trade_builds_the_same_preimage_on_both_sides() {
        let answer = "Europe/Zagreb";
        let job_hash = "ab".repeat(32);
        let job_id = "cd".repeat(32);
        let buyer = "11".repeat(32);
        let seller = "22".repeat(32);

        // Seller side: the digest is of the answer it is about to publish.
        let seller_side = ReceiptPreimage {
            job_hash: job_hash.clone(),
            offer_id: job_id.clone(),
            amount: 2,
            unit: "sat".to_owned(),
            buyer_pubkey: buyer.clone(),
            seller_pubkey: seller.clone(),
            delivery_integrity_hash: result_content_hash_hex(answer),
            delivery_kind: DeliveryKind::Inline.as_str().to_owned(),
            exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
            creq_hash: None,
        };

        // Buyer side: the digest is RE-DERIVED from the answer it received, never copied from the
        // seller's testimony. Equal digests are the whole point of the check.
        let buyer_side = ReceiptPreimage {
            job_hash,
            offer_id: job_id,
            amount: 2,
            unit: "sat".to_owned(),
            buyer_pubkey: buyer,
            seller_pubkey: seller,
            delivery_integrity_hash: result_content_hash_hex(answer),
            delivery_kind: DeliveryKind::Inline.as_str().to_owned(),
            exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
            creq_hash: None,
        };

        assert_eq!(seller_side.digest_hex(), buyer_side.digest_hex());

        // And a tampered answer does NOT produce the co-signed digest, which is what makes the
        // buyer's re-derivation a check rather than a formality.
        let tampered = ReceiptPreimage {
            delivery_integrity_hash: result_content_hash_hex("America/New_York"),
            ..buyer_side.clone()
        };
        assert_ne!(tampered.digest_hex(), seller_side.digest_hex());

        // The signed KIND is load-bearing: the same digest under `fork` is a different preimage,
        // so an answer digest can never be replayed as a commit oid.
        let as_fork = ReceiptPreimage {
            delivery_kind: DeliveryKind::Fork.as_str().to_owned(),
            ..buyer_side
        };
        assert_ne!(as_fork.digest_hex(), seller_side.digest_hex());
    }

    #[test]
    fn result_content_hash_is_sha256_hex_of_result_content() {
        assert_eq!(
            result_content_hash_hex("done"),
            "a4c3ed04a95a3da14a9d235c83d868bed7c0f45cf7f3faa751ee8f50598d2211"
        );
    }

    fn preimage() -> ReceiptPreimage {
        ReceiptPreimage {
            job_hash: "aa".repeat(32),
            offer_id: "offer".into(),
            amount: 7,
            unit: "sat".into(),
            buyer_pubkey: "bb".repeat(32),
            seller_pubkey: "cc".repeat(32),
            delivery_integrity_hash: "dd".repeat(20),
            delivery_kind: DeliveryKind::Fork.as_str().into(),
            exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.into(),
            creq_hash: None,
        }
    }

    #[test]
    fn preimage_digest_binds_the_delivered_object_and_kind() {
        let base = preimage();
        let mut other_hash = base.clone();
        other_hash.delivery_integrity_hash = "ee".repeat(20);
        // Live delivery is fork-only; the preimage still binds the kind wire label as a string,
        // so a different label must change the signed digest (non-forgeable kind binding).
        let mut other_kind = base.clone();
        other_kind.delivery_kind = "other".into();

        assert_eq!(base.digest_hex().len(), 64);
        assert_ne!(base.digest_hex(), other_hash.digest_hex());
        // Same 40-hex, different kind ⇒ different signed digest (kind is bound in, so non-forgeable).
        assert_ne!(base.digest_hex(), other_kind.digest_hex());
    }

    #[test]
    fn preimage_canonical_json_is_domain_prefixed() {
        assert!(preimage().canonical_json().starts_with(&format!(
            "[\"{RECEIPT_PREIMAGE_DOMAIN}\""
        )));
    }

    // A None creq_hash omits the additive slot entirely, so the co-signed digest is byte-identical
    // to a preimage built without a creq. Some(creq_hash) folds the slot, changing the digest, and
    // different creq_hashes yield different digests.
    #[test]
    fn receipt_preimage_binds_creq_hash_additively() {
        let none = preimage();
        let mut some = preimage();
        some.creq_hash = Some("11".repeat(32));
        let mut other = preimage();
        other.creq_hash = Some("22".repeat(32));

        // None must NOT append a slot: its canonical JSON stops at the exec-metadata field.
        assert!(none
            .canonical_json()
            .ends_with(&format!("\"{EXEC_METADATA_COMMITMENT_EMPTY}\"]")));
        // Some appends the creq_hash as the final array element.
        assert!(some
            .canonical_json()
            .ends_with(&format!("\"{}\"]", "11".repeat(32))));

        assert_ne!(none.digest_hex(), some.digest_hex());
        assert_ne!(some.digest_hex(), other.digest_hex());
        assert_eq!(none.digest_hex().len(), 64);
    }

    #[test]
    fn exec_metadata_commitment_empty_marker_and_stability() {
        assert_eq!(exec_metadata_commitment(&[]), EXEC_METADATA_COMMITMENT_EMPTY);
        let tags = vec![
            vec!["harness".into(), "claude-agent-acp".into()],
            vec!["tokens".into(), "3172".into(), "total".into()],
        ];
        let commit = exec_metadata_commitment(&tags);
        assert_eq!(commit.len(), 64);
        assert_eq!(commit, exec_metadata_commitment(&tags));
        assert_ne!(commit, exec_metadata_commitment(&tags[..1]));
    }
}
