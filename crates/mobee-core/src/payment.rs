//! Durable trade-payment state and orchestration contracts.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use cashu::{Amount, CurrencyUnit, MintUrl, PublicKey, Token};
use nostr_sdk::secp256k1::schnorr::Signature as SchnorrSignature;
use nostr_sdk::secp256k1::{Message, Secp256k1};
use nostr_sdk::PublicKey as NostrPublicKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::delivery::{DeliveryError, DeliveryVerifier, GitDelivery};
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::payment_send::PaymentSent;
use crate::receipt::ReceiptPreimage;
use crate::wallet::VerifiedPayment;

const ATTEMPT_DOMAIN: &[u8] = b"mobee/v1/payment-attempt";

/// Nostr event kind of a co-signed settlement receipt. Stamped on [`ReceiptRecord`] so a consumer
/// discriminates a co-signed receipt (kind-3400) from a record with no co-signed receipt with a
/// LOCAL check.
// The co-signed receipt kind — the single registry lives in `crate::kinds`.
use crate::gateway::JOB_RECEIPT_KIND as RECEIPT_EVENT_KIND;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Validated market job identifier.
pub struct JobId(String);

impl JobId {
    /// Creates a non-empty job identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, PaymentError> {
        nonempty(value.into(), "job id").map(Self)
    }

    /// Returns the identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Validated result identifier.
pub struct ResultId(String);

impl ResultId {
    /// Creates a non-empty result identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, PaymentError> {
        nonempty(value.into(), "result id").map(Self)
    }

    /// Returns the identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Validated integrity identifier for delivered work.
pub struct DeliveryIntegrityHash(String);

impl DeliveryIntegrityHash {
    /// Parses a full git commit oid or lowercase SHA-256 content digest.
    pub fn from_hex(value: impl Into<String>) -> Result<Self, PaymentError> {
        let value = value.into();
        let valid_length = value.len() == 40 || value.len() == 64;
        let lowercase_hex = value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid_length && lowercase_hex {
            Ok(Self(value))
        } else {
            Err(PaymentError::InvalidInput(
                "delivery integrity hash must be full lowercase git or SHA-256 hex".into(),
            ))
        }
    }

    /// Returns the digest hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Validated streamed job digest.
pub struct JobHash(String);

impl JobHash {
    /// Parses a lowercase SHA-256 digest.
    pub fn from_hex(value: impl Into<String>) -> Result<Self, PaymentError> {
        digest_hex(value.into(), "job hash").map(Self)
    }

    /// Returns the digest hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Stable wallet reconciliation identifier for one trade payment.
pub struct AttemptId(String);

impl AttemptId {
    /// Returns the attempt identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn for_key(key: &PaymentKey) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(ATTEMPT_DOMAIN);
        hash_field(&mut hasher, key.job_id.as_str());
        hash_field(&mut hasher, key.result_id.as_str());
        hash_field(&mut hasher, key.delivery_integrity_hash.as_str());
        hash_field(&mut hasher, key.job_hash.as_str());
        hash_field(&mut hasher, &key.seller_pubkey.to_string());
        hash_field(&mut hasher, &key.amount.to_string());
        hash_field(&mut hasher, &key.unit.to_string());
        hash_field(&mut hasher, &key.mint.to_string());
        // Fold the seller-authored creq hash ONLY when present: a claim with no creq (`None`)
        // folds nothing extra, so its AttemptId is byte-identical to a no-creq attempt and
        // existing in-flight journals keep resolving. A claim with a creq (`Some`) makes the
        // attempt distinct.
        if let Some(creq_hash) = &key.creq_hash {
            hash_field(&mut hasher, creq_hash);
        }
        Self(hex::encode(hasher.finalize()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Typed payment facts shared by lock, verify, send, and journal policy.
pub struct PaymentTerms {
    pub mint: MintUrl,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub seller_nostr_pubkey: NostrPublicKey,
    pub seller_p2pk_lock: PublicKey,
}

impl PaymentTerms {
    /// Constructs typed payment terms.
    pub fn new(
        mint: MintUrl,
        amount: Amount,
        unit: CurrencyUnit,
        seller_nostr_pubkey: NostrPublicKey,
        seller_p2pk_lock: PublicKey,
    ) -> Self {
        Self {
            mint,
            amount,
            unit,
            seller_nostr_pubkey,
            seller_p2pk_lock,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Canonical eight-field trade-payment identity.
pub struct PaymentKey {
    pub job_id: JobId,
    pub result_id: ResultId,
    pub delivery_integrity_hash: DeliveryIntegrityHash,
    pub job_hash: JobHash,
    pub seller_pubkey: NostrPublicKey,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub mint: MintUrl,
    /// SHA-256 hex of the seller-authored NUT-18 payment request (`creqA…`), folded
    /// into the [`AttemptId`] so two claims for the same offer with different creqs reconcile as
    /// distinct attempts. `None` for a claim with no `creq` (byte-identical attempt id to a
    /// no-creq claim); `Some` once the seller authors a creq. The `mint`/`amount`/`unit`
    /// fields denote the realized terms, with `mint` re-pointed to the payload's mint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creq_hash: Option<String>,
}

impl PaymentKey {
    /// Builds a key from trade identity and typed payment terms.
    ///
    /// `creq_hash` is the seller-authored request hash bound into the attempt; pass
    /// `None` for a claim that carries no `creq` (behaves byte-identically to a no-creq claim).
    pub fn new(
        job_id: JobId,
        result_id: ResultId,
        delivery_integrity_hash: DeliveryIntegrityHash,
        job_hash: JobHash,
        terms: &PaymentTerms,
        creq_hash: Option<String>,
    ) -> Self {
        Self {
            job_id,
            result_id,
            delivery_integrity_hash,
            job_hash,
            seller_pubkey: terms.seller_nostr_pubkey,
            amount: terms.amount,
            unit: terms.unit.clone(),
            mint: terms.mint.clone(),
            creq_hash,
        }
    }

    /// Derives the stable reconciliation identifier.
    pub fn attempt_id(&self) -> AttemptId {
        AttemptId::for_key(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Durable metadata for a published receipt.
pub struct ReceiptRecord {
    /// The published kind-3400 co-signed receipt event id.
    pub receipt_id: String,
    /// Nostr event kind of the co-signed settlement receipt (`3400`). A LOCAL discriminator: `0`
    /// (the serde default) reads as no co-signed receipt, so `Sent`-with-no-receipt stays
    /// new-and-incomplete without a relay fetch.
    #[serde(default)]
    pub receipt_kind: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
/// Durable five-state trade-payment spine.
pub enum PaymentState {
    Intent {
        attempt_id: AttemptId,
    },
    Locked {
        attempt_id: AttemptId,
    },
    Sent {
        attempt_id: AttemptId,
        payment: PaymentSent,
    },
    ReceiptPublished {
        attempt_id: AttemptId,
        receipt: ReceiptRecord,
    },
    Closed {
        attempt_id: AttemptId,
        receipt: ReceiptRecord,
    },
}

impl PaymentState {
    /// Returns the reconciliation identifier carried by this state.
    pub fn attempt_id(&self) -> &AttemptId {
        match self {
            Self::Intent { attempt_id }
            | Self::Locked { attempt_id }
            | Self::Sent { attempt_id, .. }
            | Self::ReceiptPublished { attempt_id, .. }
            | Self::Closed { attempt_id, .. } => attempt_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// One append-only state record for a payment key.
pub struct PaymentRecord {
    pub key: PaymentKey,
    pub value: PaymentState,
}

/// Pure payment transition reducer.
pub struct PaymentMachine;

impl PaymentMachine {
    /// Folds ordered records into the current state.
    pub fn fold(
        key: &PaymentKey,
        records: &[PaymentRecord],
    ) -> Result<Option<PaymentState>, PaymentError> {
        let mut current = None;
        for record in records {
            if &record.key != key {
                return Err(PaymentError::Refused(
                    "journal guard returned a record for another payment key".into(),
                ));
            }
            current = Some(Self::decide(current.as_ref(), &record.value)?);
        }
        Ok(current)
    }

    /// Validates and applies one state transition.
    pub fn decide(
        current: Option<&PaymentState>,
        next: &PaymentState,
    ) -> Result<PaymentState, PaymentError> {
        let legal = match (current, next) {
            (None, PaymentState::Intent { .. }) => true,
            (
                Some(PaymentState::Intent { attempt_id }),
                PaymentState::Locked { attempt_id: next },
            )
            | (
                Some(PaymentState::Locked { attempt_id }),
                PaymentState::Sent {
                    attempt_id: next, ..
                },
            )
            | (
                Some(PaymentState::Sent { attempt_id, .. }),
                PaymentState::ReceiptPublished {
                    attempt_id: next, ..
                },
            )
            | (
                Some(PaymentState::ReceiptPublished { attempt_id, .. }),
                PaymentState::Closed {
                    attempt_id: next, ..
                },
            ) => attempt_id == next,
            _ => false,
        };
        if !legal {
            return Err(PaymentError::IllegalTransition {
                from: current.map(state_name).unwrap_or("none"),
                to: state_name(next),
            });
        }
        Ok(next.clone())
    }
}

/// Exclusive durable journal access for one payment key.
pub trait PaymentJournalGuard {
    /// Replays all records for the locked key.
    fn replay(&mut self) -> Result<Vec<PaymentRecord>, JournalError>;
    /// Durably syncs replayed records before an effect trusts them.
    fn sync_replay(&mut self) -> Result<(), JournalError>;
    /// Appends and durably syncs one record.
    fn append_sync(&mut self, record: &PaymentRecord) -> Result<(), JournalError>;

    /// Folds the locked key's records into its current state.
    fn current(&mut self) -> Result<Option<PaymentState>, PaymentError> {
        let records = self.replay()?;
        let key = records.first().map(|record| &record.key);
        match key {
            Some(key) => PaymentMachine::fold(key, &records),
            None => Ok(None),
        }
    }
}

/// Journal capable of locking one payment key across an effect.
pub trait PaymentJournal {
    /// Exclusive guard returned for the journal's lifetime.
    type Guard<'a>: PaymentJournalGuard
    where
        Self: 'a;

    /// Acquires exclusive journal access for a payment key.
    fn lock<'a>(&'a self, key: &PaymentKey) -> Result<Self::Guard<'a>, JournalError>;
}

#[derive(Clone, Debug)]
/// Append-only JSONL payment journal.
pub struct FsPaymentJournal {
    path: PathBuf,
    #[cfg(test)]
    parent_sync_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FsPaymentJournal {
    /// Opens a journal at the given path when first locked.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            #[cfg(test)]
            parent_sync_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Returns the journal path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    fn parent_sync_count(&self) -> usize {
        self.parent_sync_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Exclusive file-backed payment journal guard.
pub struct FsPaymentJournalGuard {
    file: File,
    key: PaymentKey,
}

impl PaymentJournal for FsPaymentJournal {
    type Guard<'a> = FsPaymentJournalGuard;

    fn lock<'a>(&'a self, key: &PaymentKey) -> Result<Self::Guard<'a>, JournalError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)?;
        file.lock()?;
        sync_parent_directory(&self.path)?;
        #[cfg(test)]
        self.parent_sync_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(FsPaymentJournalGuard {
            file,
            key: key.clone(),
        })
    }
}

fn sync_parent_directory(path: &Path) -> Result<(), JournalError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()?;
    Ok(())
}

impl PaymentJournalGuard for FsPaymentJournalGuard {
    fn replay(&mut self) -> Result<Vec<PaymentRecord>, JournalError> {
        let mut input = self.file.try_clone()?;
        input.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes)?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(JournalError::Corrupt {
                line: bytes.iter().filter(|byte| **byte == b'\n').count() + 1,
                detail: "record is missing its commit newline".into(),
            });
        }
        let reader = BufReader::new(bytes.as_slice());
        let mut records = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            let record = serde_json::from_str::<PaymentRecord>(&line).map_err(|error| {
                JournalError::Corrupt {
                    line: index + 1,
                    detail: error.to_string(),
                }
            })?;
            // Fail closed on a foreign PaymentKey: this journal is keyed to ONE attempt
            // (`append_sync` refuses to write a record for a different key), so a record whose key
            // does not match is corruption — a misplaced or tampered journal — NOT absence. Silently
            // skipping it would let a semantically-corrupt journal replay as EMPTY, which the caller
            // would read as "no prior send" and could send AGAIN under an already-counted budget
            // attempt. Reject instead so the corruption surfaces rather than double-spending.
            if record.key != self.key {
                return Err(JournalError::Corrupt {
                    line: index + 1,
                    detail: "record PaymentKey does not match this journal's attempt key".into(),
                });
            }
            records.push(record);
        }
        Ok(records)
    }

    fn sync_replay(&mut self) -> Result<(), JournalError> {
        self.file.sync_all()?;
        Ok(())
    }

    fn append_sync(&mut self, record: &PaymentRecord) -> Result<(), JournalError> {
        if record.key != self.key {
            return Err(JournalError::KeyMismatch);
        }
        serde_json::to_writer(&mut self.file, record)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A published kind-3400 receipt plus the co-signature material [`ReceiptAuthority`]
/// verifies. Signatures are real schnorr over [`ReceiptPreimage::digest_bytes`] — not a
/// caller-asserted signer list.
pub struct ReceiptEvidence {
    /// The published kind-3400 event id (deterministic ⇒ idempotent republish).
    pub receipt_id: String,
    /// The receipt event author — MUST equal the externally-anchored buyer (offer author).
    pub author: NostrPublicKey,
    /// The co-signed preimage; `verify` recomputes its digest (never trusts a caller digest).
    pub preimage: ReceiptPreimage,
    /// Seller schnorr signature (hex) over the preimage digest.
    pub seller_signature: String,
    /// Buyer (counter-)schnorr signature (hex) over the same preimage digest.
    pub buyer_signature: String,
    /// Relays that accepted the receipt publish. EMPTY ⇒ fail closed before evidence.
    pub relay_success: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// The **externally-anchored** receipt identities: buyer == the offer's author, seller ==
/// the accepted-claim seller. NEVER derived from the receipt's own `p`-tags (a
/// self-anchored check is circular — an attacker could name itself and lift the seller's
/// public signature). These are nostr identities; the co-signatures verify against them.
pub struct ReceiptAuthority {
    pub buyer: NostrPublicKey,
    pub seller: NostrPublicKey,
}

impl ReceiptAuthority {
    fn verify(
        &self,
        evidence: ReceiptEvidence,
        key: &PaymentKey,
    ) -> Result<ReceiptRecord, PaymentError> {
        // Money-path publish rule: empty relay_success ⇒ fail closed BEFORE returning
        // evidence (mirrors `send_payment`'s empty-relay gate). Recovery retries only the
        // idempotent receipt publish.
        if evidence.relay_success.is_empty() {
            return Err(PaymentError::NoRelayAccepted);
        }
        // Author must be the externally-anchored buyer (offer author).
        if evidence.author != self.buyer {
            return Err(PaymentError::ForgedReceipt);
        }
        // The signed preimage's embedded identities must equal the external anchors, so the
        // co-signature commits to the anchored parties — not the receipt's self-declared
        // `p`-tags.
        if evidence.preimage.buyer_pubkey != self.buyer.to_hex()
            || evidence.preimage.seller_pubkey != self.seller.to_hex()
        {
            return Err(PaymentError::ForgedReceipt);
        }
        // Real schnorr verification of BOTH co-signatures over the preimage digest, each
        // against its EXTERNAL anchor. Runs BEFORE the terms-binding check so a preimage field
        // tampered after signing still fails as a forged (digest-mismatched) signature.
        let message = Message::from_digest(evidence.preimage.digest_bytes());
        verify_schnorr_hex(&evidence.seller_signature, &message, &self.seller)?;
        verify_schnorr_hex(&evidence.buyer_signature, &message, &self.buyer)?;
        // Bind the receipt to THIS payment's terms. Anchor + signature checks alone would close
        // on ANY correctly co-signed receipt from the right parties — including one for an
        // unrelated payment. Compare every co-signed preimage field against the payment key (the
        // realized terms: amount/unit/mint are re-pointed onto the key at pay time), refusing on
        // any drift before the state can advance to Closed.
        require_receipt_binds_key(&evidence.preimage, key)?;
        Ok(ReceiptRecord {
            receipt_id: evidence.receipt_id,
            receipt_kind: RECEIPT_EVENT_KIND,
        })
    }

    /// THE load-bearing PRE-SPEND tooth (cross-bind / forged-cosig).
    ///
    /// Verifies the seller's schnorr co-signature over the canonical receipt preimage
    /// (`receipt.rs` `ReceiptPreimage::canonical_json` → `digest_bytes`) against the
    /// **external claim-seller anchor** ([`Self::seller`]) — never the receipt's own p-tags.
    /// The caller passes the EXACT preimage the pay path will co-sign and publish, so the
    /// bytes verified here are byte-identical to the bytes published later.
    ///
    /// This runs BEFORE any spend (before `authorize_pay` commits budget / opens the wallet /
    /// enters the payment SM). A missing / malformed / cross-authored / tampered signature
    /// fails CLOSED, so the buyer refuses with **zero spend** rather than spending and only
    /// detecting the bad receipt afterwards — which is what the post-spend [`Self::verify`]
    /// does at the `Sent → ReceiptPublished` transition (detection, not prevention).
    ///
    /// SHARED SEAM — do NOT inline this at call sites. It is the single pre-pay point at which
    /// every seller bind is checked. The receipt (job-hash) preimage signature is checked always;
    /// for a `contribution` result, an ADDITIONAL seller signature over its signed-result
    /// tuple bind `{job_id, seller_pubkey, target_repo, base_oid, fork_ref, commit_oid}` is verified
    /// against the SAME claim-seller anchor. One seam, more binds — never a parallel pre-pay gate.
    /// From-scratch trades pass `contribution = None` ⇒ byte-identical to the single-bind behavior.
    pub fn verify_seller_prepay_cosig(
        &self,
        preimage: &ReceiptPreimage,
        seller_signature_hex: &str,
        contribution: Option<ContributionCosig<'_>>,
    ) -> Result<(), PaymentError> {
        let message = Message::from_digest(preimage.digest_bytes());
        verify_schnorr_hex(seller_signature_hex, &message, &self.seller).map_err(|_| {
            PaymentError::Refused(format!(
                "pre-pay seller co-signature invalid: the accepted result's sig/seller does not \
                 verify over the receipt preimage against claim seller {} (zero spend; refused \
                 before payment)",
                self.seller.to_hex()
            ))
        })?;
        if let Some(contribution) = contribution {
            // The seller's own schnorr signature ties `seller_pubkey → this job_id → this
            // exact commit_oid` (against the pinned target + base + fork). A commit signed over a
            // DIFFERENT tuple (any field tampered post-signing) fails here ⇒ zero-spend refusal.
            let tuple_message = Message::from_digest(contribution.tuple_digest);
            verify_schnorr_hex(contribution.tuple_signature_hex, &tuple_message, &self.seller)
                .map_err(|_| {
                    PaymentError::Refused(format!(
                        "pre-pay contribution authorship invalid: the accepted result's signed-result \
                         tuple sig does not verify over {{job_id, seller_pubkey, target_repo, \
                         base_oid, fork_ref, commit_oid}} against claim seller {} (zero spend; \
                         refused before payment)",
                        self.seller.to_hex()
                    ))
                })?;
        }
        Ok(())
    }
}

/// Additional pre-pay seller bind for a contribution: the seller's schnorr signature over
/// the authorship tuple digest (`contribution::AuthorshipTuple::digest_bytes`). Verified at the ONE
/// pre-pay seam [`ReceiptAuthority::verify_seller_prepay_cosig`] alongside the receipt cosig.
#[derive(Clone, Copy, Debug)]
pub struct ContributionCosig<'a> {
    /// SHA-256 digest of the seller's signed-result authorship tuple (buyer-reconstructed).
    pub tuple_digest: [u8; 32],
    /// The seller's schnorr signature (hex) over `tuple_digest`, read from the accepted result.
    pub tuple_signature_hex: &'a str,
}

/// Refuse a receipt whose co-signed preimage does not bind THIS payment's terms. Every preimage
/// field the buyer/seller co-signed is compared against the [`PaymentKey`] (its realized
/// amount/unit/mint), so a valid co-signed receipt for a DIFFERENT payment cannot close this one.
/// `result_id` is not a preimage field (the co-signature binds `offer_id`==`job_id` plus the
/// delivery/creq hashes); it is enforced separately at accept-bind time.
fn require_receipt_binds_key(
    preimage: &ReceiptPreimage,
    key: &PaymentKey,
) -> Result<(), PaymentError> {
    let mismatch = |field: &str, expected: String, actual: &str| {
        Err(PaymentError::Refused(format!(
            "receipt does not bind this payment: {field} {actual:?} != expected {expected:?}"
        )))
    };
    if preimage.offer_id != key.job_id.as_str() {
        return mismatch("offer_id", key.job_id.as_str().to_owned(), &preimage.offer_id);
    }
    if preimage.job_hash != key.job_hash.as_str() {
        return mismatch("job_hash", key.job_hash.as_str().to_owned(), &preimage.job_hash);
    }
    if preimage.amount != key.amount.to_u64() {
        return mismatch(
            "amount",
            key.amount.to_u64().to_string(),
            &preimage.amount.to_string(),
        );
    }
    if preimage.unit != key.unit.to_string() {
        return mismatch("unit", key.unit.to_string(), &preimage.unit);
    }
    // The realized `mint` is deliberately NOT in the co-signed preimage (see `ReceiptPreimage`):
    // the accepted-mint SET is bound via `creq_hash` and the specific mint is enforced by the
    // allowlist guards on both ends, so there is no mint field to bind here.
    if preimage.delivery_integrity_hash != key.delivery_integrity_hash.as_str() {
        return mismatch(
            "delivery_integrity_hash",
            key.delivery_integrity_hash.as_str().to_owned(),
            &preimage.delivery_integrity_hash,
        );
    }
    if preimage.creq_hash != key.creq_hash {
        return Err(PaymentError::Refused(format!(
            "receipt does not bind this payment: creq_hash {:?} != expected {:?}",
            preimage.creq_hash, key.creq_hash
        )));
    }
    Ok(())
}

/// Verify one schnorr signature (hex) over `message` against a nostr x-only anchor.
/// Any parse or verification failure is a [`PaymentError::ForgedReceipt`] (fail closed).
fn verify_schnorr_hex(
    signature_hex: &str,
    message: &Message,
    anchor: &NostrPublicKey,
) -> Result<(), PaymentError> {
    let signature =
        SchnorrSignature::from_str(signature_hex).map_err(|_| PaymentError::ForgedReceipt)?;
    let anchor = anchor.xonly().map_err(|_| PaymentError::ForgedReceipt)?;
    Secp256k1::verification_only()
        .verify_schnorr(&signature, message, &anchor)
        .map_err(|_| PaymentError::ForgedReceipt)
}

/// Ephemeral wallet lock result; never persisted by the trade journal.
pub struct LockedPayment {
    token: Token,
}

impl LockedPayment {
    /// Wraps the token returned by the wallet reconciliation edge.
    pub fn new(token: Token) -> Self {
        Self { token }
    }

    /// Returns the locked token for verify and send adapters.
    pub fn token(&self) -> &Token {
        &self.token
    }
}

/// Injected wallet, verifier, send, and receipt effects.
///
/// Crate-private: its methods (notably `send_payment`) move funds and take in-crate-only
/// inputs (`LockedPayment`/`VerifiedPayment`). Sealed so no out-of-crate caller can drive a
/// send through the trait, bypassing the budget gate.
pub(crate) trait PaymentEffects {
    /// Creates or reconciles the wallet lock for an attempt.
    fn lock_or_reconcile(
        &mut self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<LockedPayment, EffectError>;

    /// Produces a fresh typed payment verification.
    fn verify_payment(
        &mut self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
        locked: &LockedPayment,
    ) -> Result<VerifiedPayment, EffectError>;

    /// Sends one newly locked and verified payment.
    fn send_payment(
        &mut self,
        key: &PaymentKey,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
        locked: &LockedPayment,
        verified: &VerifiedPayment,
    ) -> Result<PaymentSent, EffectError>;

    /// Publishes or recovers the receipt for a sent payment.
    fn publish_receipt(
        &mut self,
        key: &PaymentKey,
        payment: &PaymentSent,
    ) -> Result<ReceiptEvidence, EffectError>;
}

/// Guarded payment workflow orchestrator.
pub struct PaymentService<'a, J> {
    journal: &'a J,
}

impl<'a, J: PaymentJournal> PaymentService<'a, J> {
    /// Creates a service over a trade journal.
    pub fn new(journal: &'a J) -> Self {
        Self { journal }
    }

    /// Advance a payment whose delivery has ALREADY been verified + bind-checked (via
    /// [`verify_pay_path_delivery`]) by the caller.
    ///
    /// The production pay entry. The caller runs delivery verification BEFORE committing any
    /// budget (so a failed/hung verify burns zero budget) and then commits budget before the
    /// wallet send inside this call (write-before-mint). By construction the pay path only ever
    /// verifies through [`PayPathDeliveryVerifier`] (allowlist-sealed); in-crate unit tests that
    /// inject fake verifiers use [`Self::run_with_verifier`] (`#[cfg(test)]` only).
    pub(crate) fn run_verified<E: PaymentEffects>(
        &self,
        key: &PaymentKey,
        terms: &PaymentTerms,
        authority: &ReceiptAuthority,
        effects: &mut E,
    ) -> Result<PaymentState, PaymentError> {
        self.advance(key, terms, authority, effects)
    }

    /// Delivery-gated pay entry for in-crate unit tests only (fake / bare verifiers).
    ///
    /// Compiler-dropped outside `cfg(test)` — zero production reach (no in-core production
    /// caller can hand a bare verifier through this escape hatch).
    #[cfg(test)]
    pub(crate) fn run_with_verifier<D: DeliveryVerifier, E: PaymentEffects>(
        &self,
        delivery: &GitDelivery,
        delivery_verifier: &mut D,
        key: &PaymentKey,
        terms: &PaymentTerms,
        authority: &ReceiptAuthority,
        effects: &mut E,
    ) -> Result<PaymentState, PaymentError> {
        self.run_delivery_gated(delivery, delivery_verifier, key, terms, authority, effects)
    }

    /// Shared delivery-verify → tip-bind → [`Self::advance`] impl.
    ///
    /// Private: not a production generic entry. Production callers verify via
    /// [`verify_pay_path_delivery`] (before budget) then call [`Self::run_verified`].
    fn run_delivery_gated<D: DeliveryVerifier, E: PaymentEffects>(
        &self,
        delivery: &GitDelivery,
        delivery_verifier: &mut D,
        key: &PaymentKey,
        terms: &PaymentTerms,
        authority: &ReceiptAuthority,
        effects: &mut E,
    ) -> Result<PaymentState, PaymentError> {
        // Verify the delivery (fetch the branch tip, peel `^{commit}`, tip-match).
        let verified = delivery_verifier
            .verify(delivery)
            .map_err(PaymentError::Delivery)?;
        if key.delivery_integrity_hash.as_str() != verified.commit_oid().as_str() {
            return Err(PaymentError::Refused(
                "payment key does not bind the verified delivery commit".into(),
            ));
        }
        self.advance(key, terms, authority, effects)
    }

    /// Advances an already delivery-gated payment inside this module only.
    ///
    /// Module-private on purpose: other in-core modules cannot skip delivery verify by
    /// calling `advance` directly. Production callers use [`Self::run`]; same-module
    /// unit tests may call `advance` or [`Self::run_with_verifier`].
    fn advance<E: PaymentEffects>(
        &self,
        key: &PaymentKey,
        terms: &PaymentTerms,
        authority: &ReceiptAuthority,
        effects: &mut E,
    ) -> Result<PaymentState, PaymentError> {
        require_key_matches_terms(key, terms)?;
        let mut guard = self.journal.lock(key)?;
        let records = guard.replay()?;
        guard.sync_replay()?;
        let mut state = PaymentMachine::fold(key, &records)?;
        let recovered_locked = matches!(state, Some(PaymentState::Locked { .. }));
        let mut locked_payment = None;

        if state.is_none() {
            let intent = PaymentState::Intent {
                attempt_id: key.attempt_id(),
            };
            append_transition(&mut guard, key, state.as_ref(), &intent)?;
            state = Some(intent);
        }

        if let Some(PaymentState::Intent { attempt_id }) = state.clone() {
            locked_payment = Some(effects.lock_or_reconcile(&attempt_id, terms)?);
            require_locked_matches_terms(
                locked_payment
                    .as_ref()
                    .expect("wallet lock was stored for this transition"),
                terms,
            )?;
            let locked = PaymentState::Locked { attempt_id };
            append_transition(&mut guard, key, state.as_ref(), &locked)?;
            state = Some(locked);
        }

        if matches!(state, Some(PaymentState::Locked { .. })) {
            if recovered_locked {
                return Err(PaymentError::AmbiguousSendRefused);
            }
            let attempt_id = state
                .as_ref()
                .expect("locked state exists")
                .attempt_id()
                .clone();
            let locked = locked_payment.as_ref().ok_or_else(|| {
                PaymentError::Refused("locked token is unavailable after reconciliation".into())
            })?;
            let verified = effects.verify_payment(&attempt_id, terms, locked)?;
            require_verified_matches_terms(&verified, terms)?;
            let payment = effects.send_payment(key, &attempt_id, terms, locked, &verified)?;
            if payment.relay_success.is_empty() {
                return Err(PaymentError::NoRelayAccepted);
            }
            let sent = PaymentState::Sent {
                attempt_id,
                payment,
            };
            append_transition(&mut guard, key, state.as_ref(), &sent)?;
            state = Some(sent);
        }

        if let Some(PaymentState::Sent {
            attempt_id,
            payment,
        }) = state.clone()
        {
            let receipt = authority.verify(effects.publish_receipt(key, &payment)?, key)?;
            let published = PaymentState::ReceiptPublished {
                attempt_id,
                receipt,
            };
            append_transition(&mut guard, key, state.as_ref(), &published)?;
            state = Some(published);
        }

        if let Some(PaymentState::ReceiptPublished {
            attempt_id,
            receipt,
        }) = state.clone()
        {
            // Refuse to close on a non-receipt kind. A `verify`-built record always carries
            // RECEIPT_EVENT_KIND, but a record deserialized from an OLD journal defaults its
            // `receipt_kind` to 0 (kind-1059-aliased); such a record must NOT advance to Closed —
            // only a genuine kind-3400 receipt settles the trade.
            if receipt.receipt_kind != RECEIPT_EVENT_KIND {
                return Err(PaymentError::Refused(format!(
                    "refusing to close on non-receipt kind {} (expected {RECEIPT_EVENT_KIND})",
                    receipt.receipt_kind
                )));
            }
            let closed = PaymentState::Closed {
                attempt_id,
                receipt,
            };
            append_transition(&mut guard, key, state.as_ref(), &closed)?;
            state = Some(closed);
        }

        state.ok_or_else(|| PaymentError::Refused("payment state is absent".into()))
    }
}

/// Verify a pay-path delivery (allowlist + fetch + tip-match) and bind-check the verified commit
/// against the payment key. The production pay path calls this BEFORE committing any budget, so a
/// failed or hung verification burns ZERO budget (the invariant the budget-before-wallet-send
/// ordering must not violate). Mirrors the verify + bind step of [`PaymentService::run_delivery_gated`].
pub(crate) fn verify_pay_path_delivery(
    delivery_verifier: &mut PayPathDeliveryVerifier,
    delivery: &GitDelivery,
    key: &PaymentKey,
) -> Result<(), PaymentError> {
    let verified = delivery_verifier
        .verify(delivery)
        .map_err(PaymentError::Delivery)?;
    if key.delivery_integrity_hash.as_str() != verified.commit_oid().as_str() {
        return Err(PaymentError::Refused(
            "payment key does not bind the verified delivery commit".into(),
        ));
    }
    Ok(())
}

fn append_transition<G: PaymentJournalGuard>(
    guard: &mut G,
    key: &PaymentKey,
    current: Option<&PaymentState>,
    next: &PaymentState,
) -> Result<(), PaymentError> {
    PaymentMachine::decide(current, next)?;
    guard.append_sync(&PaymentRecord {
        key: key.clone(),
        value: next.clone(),
    })?;
    Ok(())
}

fn require_key_matches_terms(key: &PaymentKey, terms: &PaymentTerms) -> Result<(), PaymentError> {
    if key.mint != terms.mint
        || key.amount != terms.amount
        || key.unit != terms.unit
        || key.seller_pubkey != terms.seller_nostr_pubkey
    {
        return Err(PaymentError::Refused(
            "payment key does not match typed payment terms".into(),
        ));
    }
    Ok(())
}

fn require_verified_matches_terms(
    verified: &VerifiedPayment,
    terms: &PaymentTerms,
) -> Result<(), PaymentError> {
    if verified.mint != terms.mint || verified.amount != terms.amount || verified.unit != terms.unit
    {
        return Err(PaymentError::Refused(
            "verified payment does not match typed payment terms".into(),
        ));
    }
    Ok(())
}

fn require_locked_matches_terms(
    locked: &LockedPayment,
    terms: &PaymentTerms,
) -> Result<(), PaymentError> {
    let token = locked.token();
    let amount = token
        .value()
        .map_err(|error| PaymentError::Refused(format!("invalid locked token: {error}")))?;
    let mint = token
        .mint_url()
        .map_err(|error| PaymentError::Refused(format!("invalid locked token: {error}")))?;
    // Defense-in-depth: realized value must be non-zero and match terms before
    // any Sent / publish_receipt / Closed (buyer lock_or_reconcile is the primary gate).
    if amount == cashu::Amount::ZERO {
        return Err(PaymentError::Refused(
            "locked token realized value is zero".into(),
        ));
    }
    if amount != terms.amount || mint != terms.mint || token.unit().as_ref() != Some(&terms.unit) {
        return Err(PaymentError::Refused(
            "locked token does not match typed payment terms".into(),
        ));
    }
    Ok(())
}

fn state_name(state: &PaymentState) -> &'static str {
    match state {
        PaymentState::Intent { .. } => "Intent",
        PaymentState::Locked { .. } => "Locked",
        PaymentState::Sent { .. } => "Sent",
        PaymentState::ReceiptPublished { .. } => "ReceiptPublished",
        PaymentState::Closed { .. } => "Closed",
    }
}

fn nonempty(value: String, name: &str) -> Result<String, PaymentError> {
    if value.is_empty() {
        Err(PaymentError::InvalidInput(format!("{name} is empty")))
    } else {
        Ok(value)
    }
}

fn digest_hex(value: String, name: &str) -> Result<String, PaymentError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(value)
    } else {
        Err(PaymentError::InvalidInput(format!(
            "{name} must be 32-byte lowercase hex"
        )))
    }
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Failure returned by an injected payment effect.
pub struct EffectError(String);

impl EffectError {
    /// Creates an effect failure with a stable message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for EffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EffectError {}

#[derive(Debug)]
/// Durable journal failure.
pub enum JournalError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Corrupt { line: usize, detail: String },
    KeyMismatch,
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "payment journal I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "payment journal encoding failed: {error}"),
            Self::Corrupt { line, detail } => {
                write!(
                    formatter,
                    "payment journal line {line} is corrupt: {detail}"
                )
            }
            Self::KeyMismatch => formatter.write_str("payment journal guard key mismatch"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<std::io::Error> for JournalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for JournalError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug)]
/// Payment policy, transition, journal, or effect failure.
pub enum PaymentError {
    InvalidInput(String),
    IllegalTransition {
        from: &'static str,
        to: &'static str,
    },
    Journal(JournalError),
    Delivery(DeliveryError),
    Effect(EffectError),
    NoRelayAccepted,
    AmbiguousSendRefused,
    ForgedReceipt,
    Refused(String),
}

impl fmt::Display for PaymentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(formatter, "invalid payment input: {message}"),
            Self::IllegalTransition { from, to } => {
                write!(formatter, "illegal payment transition: {from} -> {to}")
            }
            Self::Journal(error) => error.fmt(formatter),
            Self::Delivery(error) => write!(formatter, "delivery verification refused: {error}"),
            Self::Effect(error) => write!(formatter, "payment effect failed: {error}"),
            Self::NoRelayAccepted => formatter.write_str("no relay accepted the payment"),
            Self::AmbiguousSendRefused => {
                formatter.write_str("payment send state is ambiguous; refusing automatic resend")
            }
            Self::ForgedReceipt => formatter.write_str("receipt author or signatures are invalid"),
            Self::Refused(message) => write!(formatter, "payment refused: {message}"),
        }
    }
}

impl std::error::Error for PaymentError {}

impl From<JournalError> for PaymentError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

impl From<EffectError> for PaymentError {
    fn from(error: EffectError) -> Self {
        Self::Effect(error)
    }
}

#[cfg(any(test, feature = "test-support"))]
mod memory {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};

    use super::*;

    #[derive(Clone, Default)]
    /// Mutex-backed journal for hermetic payment tests.
    pub struct MemoryPaymentJournal {
        records: Arc<Mutex<Vec<PaymentRecord>>>,
        sync_count: Arc<AtomicUsize>,
        replay_sync_count: Arc<AtomicUsize>,
    }

    impl MemoryPaymentJournal {
        /// Returns a snapshot of all recorded transitions.
        pub fn records(&self) -> Vec<PaymentRecord> {
            self.records
                .lock()
                .expect("memory journal poisoned")
                .clone()
        }

        /// Returns the number of durable append operations.
        pub fn sync_count(&self) -> usize {
            self.sync_count.load(Ordering::SeqCst)
        }

        /// Returns the number of replay durability syncs.
        pub fn replay_sync_count(&self) -> usize {
            self.replay_sync_count.load(Ordering::SeqCst)
        }
    }

    /// Exclusive mutex-backed journal guard.
    pub struct MemoryPaymentJournalGuard<'a> {
        records: MutexGuard<'a, Vec<PaymentRecord>>,
        key: PaymentKey,
        sync_count: &'a AtomicUsize,
        replay_sync_count: &'a AtomicUsize,
    }

    impl PaymentJournal for MemoryPaymentJournal {
        type Guard<'a> = MemoryPaymentJournalGuard<'a>;

        fn lock<'a>(&'a self, key: &PaymentKey) -> Result<Self::Guard<'a>, JournalError> {
            Ok(MemoryPaymentJournalGuard {
                records: self.records.lock().map_err(|_| {
                    JournalError::Io(std::io::Error::other("memory journal poisoned"))
                })?,
                key: key.clone(),
                sync_count: &self.sync_count,
                replay_sync_count: &self.replay_sync_count,
            })
        }
    }

    impl PaymentJournalGuard for MemoryPaymentJournalGuard<'_> {
        fn replay(&mut self) -> Result<Vec<PaymentRecord>, JournalError> {
            Ok(self
                .records
                .iter()
                .filter(|record| record.key == self.key)
                .cloned()
                .collect())
        }

        fn sync_replay(&mut self) -> Result<(), JournalError> {
            self.replay_sync_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn append_sync(&mut self, record: &PaymentRecord) -> Result<(), JournalError> {
            if record.key != self.key {
                return Err(JournalError::KeyMismatch);
            }
            self.records.push(record.clone());
            self.sync_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use memory::MemoryPaymentJournal;

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use cashu::secret::Secret;
    use cashu::{Id, Proof, SecretKey};

    use super::*;
    use crate::payment_send::PaymentRelayFailure;

    const MINT: &str = "https://testnut.cashu.space";

    #[test]
    fn machine_rejects_skipped_and_repeated_states() {
        let key = key();
        let attempt_id = key.attempt_id();
        assert!(matches!(
            PaymentMachine::decide(
                None,
                &PaymentState::Locked {
                    attempt_id: attempt_id.clone()
                }
            ),
            Err(PaymentError::IllegalTransition { .. })
        ));
        assert!(matches!(
            PaymentMachine::decide(
                Some(&PaymentState::Intent {
                    attempt_id: attempt_id.clone()
                }),
                &PaymentState::Intent { attempt_id }
            ),
            Err(PaymentError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn attempt_id_is_stable_and_unit_is_part_of_the_key() {
        let sat = key();
        let mut msat = sat.clone();
        msat.unit = CurrencyUnit::Msat;

        assert_eq!(sat.attempt_id(), sat.attempt_id());
        assert_ne!(sat.attempt_id(), msat.attempt_id());
    }

    // The seller-authored creq hash is part of the attempt identity. Two claims for the
    // same offer that quote different creqs reconcile as DISTINCT attempts; a claim with no creq
    // (`None`) keeps the no-creq AttemptId byte-for-byte (the regression guard — existing
    // journals still resolve).
    #[test]
    fn attempt_id_binds_creq_hash_with_none_byte_identical_to_no_creq() {
        let none = key();
        assert_eq!(none.creq_hash, None);

        let mut creq_a = key();
        creq_a.creq_hash = Some("aa".repeat(32));
        let mut creq_b = key();
        creq_b.creq_hash = Some("bb".repeat(32));

        // Same offer, different creq ⇒ different attempt ids.
        assert_ne!(creq_a.attempt_id(), creq_b.attempt_id());
        // A creq-bearing attempt is distinct from the no-creq attempt.
        assert_ne!(none.attempt_id(), creq_a.attempt_id());
        // Regression guard: `None` reproduces the exact no-creq attempt id (the no-creq path
        // folds nothing extra). This constant is the AttemptId of `key()` with no creq — if the
        // None fold ever changes the hash preimage, this pin breaks.
        assert_eq!(none.attempt_id().as_str(), KEY_ATTEMPT_ID);
    }

    // Frozen AttemptId of `key()` with no creq. Guards the None-creq regression path.
    const KEY_ATTEMPT_ID: &str =
        "99e8e7b4c53c7af9f2329e16a9625133e9f788d3ffe1257f0a5a121c549de3cd";

    #[test]
    fn content_hash_field_name_refuses_to_deserialize() {
        let mut value = serde_json::to_value(key()).expect("serialize payment key");
        let object = value.as_object_mut().expect("payment key object");
        let hash = object
            .remove("delivery_integrity_hash")
            .expect("delivery_integrity_hash present");
        object.insert("content_hash".into(), hash);

        assert!(serde_json::from_value::<PaymentKey>(value).is_err());
    }

    #[test]
    fn payment_key_delivery_integrity_hash_round_trips() {
        let original = key();
        let json = serde_json::to_value(&original).expect("serialize payment key");
        assert!(json.get("delivery_integrity_hash").is_some());
        assert!(json.get("content_hash").is_none());

        let parsed: PaymentKey = serde_json::from_value(json).expect("deserialize payment key");
        assert_eq!(parsed, original);
    }

    #[test]
    fn delivery_refusal_leaves_no_intent_and_fires_no_wallet_effect() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared.clone());
        let mut verifier = RejectDelivery;

        let result = PaymentService::new(&journal).run_with_verifier(
            &git_delivery(),
            &mut verifier,
            &git_key(),
            &terms(),
            &authority(),
            &mut effects,
        );

        assert!(matches!(result, Err(PaymentError::Delivery(_))));
        assert!(journal.records().is_empty());
        assert_eq!(shared.lock_calls.load(Ordering::SeqCst), 0);
        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn verified_delivery_commit_reaches_the_existing_payment_spine() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared.clone());
        let mut verifier = AcceptDelivery;

        let state = PaymentService::new(&journal)
            .run_with_verifier(
                &git_delivery(),
                &mut verifier,
                &git_key(),
                &terms(),
                &authority(),
                &mut effects,
            )
            .expect("verified delivery payment");

        assert!(matches!(state, PaymentState::Closed { .. }));
        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn payment_key_must_bind_the_verified_commit_before_intent() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared.clone());
        let mut verifier = AcceptDelivery;
        let wrong_key = PaymentKey::new(
            JobId::new("job").unwrap(),
            ResultId::new("result").unwrap(),
            DeliveryIntegrityHash::from_hex("44".repeat(20)).unwrap(),
            JobHash::from_hex("22".repeat(32)).unwrap(),
            &terms(),
            None,
        );

        let result = PaymentService::new(&journal).run_with_verifier(
            &git_delivery(),
            &mut verifier,
            &wrong_key,
            &terms(),
            &authority(),
            &mut effects,
        );

        assert!(matches!(result, Err(PaymentError::Refused(_))));
        assert!(journal.records().is_empty());
        assert_eq!(shared.lock_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn service_mints_at_most_once_across_retry_and_concurrency() {
        let journal = Arc::new(MemoryPaymentJournal::default());
        let shared = FakeShared::default();
        let key = Arc::new(key());
        let terms = Arc::new(terms());
        let authority = Arc::new(authority());

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let journal = Arc::clone(&journal);
                let shared = shared.clone();
                let key = Arc::clone(&key);
                let terms = Arc::clone(&terms);
                let authority = Arc::clone(&authority);
                thread::spawn(move || {
                    let mut effects = FakeEffects::new(shared);
                    PaymentService::new(journal.as_ref()).advance(
                        &key,
                        &terms,
                        &authority,
                        &mut effects,
                    )
                })
            })
            .collect();

        for handle in handles {
            let result = handle.join().unwrap();
            assert!(matches!(result, Ok(PaymentState::Closed { .. })));
        }
        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 1);
        assert_eq!(shared.send_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn crash_after_lock_effect_reconciles_without_reminting() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut crashing = FakeEffects::new(shared.clone());
        crashing.crash_after_lock = true;

        let first =
            PaymentService::new(&journal).advance(&key(), &terms(), &authority(), &mut crashing);
        assert!(matches!(first, Err(PaymentError::Effect(_))));
        assert!(matches!(
            journal.records().last().map(|record| &record.value),
            Some(PaymentState::Intent { .. })
        ));

        let mut recovered = FakeEffects::new(shared.clone());
        let result = PaymentService::new(&journal)
            .advance(&key(), &terms(), &authority(), &mut recovered)
            .unwrap();

        assert!(matches!(result, PaymentState::Closed { .. }));
        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 1);
        assert_eq!(shared.lock_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn blind_remint_negative_control_violates_the_pay_once_counter() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut crashing = FakeEffects::new(shared.clone());
        crashing.blind_lock = true;
        crashing.crash_after_lock = true;

        assert!(
            PaymentService::new(&journal)
                .advance(&key(), &terms(), &authority(), &mut crashing)
                .is_err()
        );

        let mut recovered = FakeEffects::new(shared.clone());
        recovered.blind_lock = true;
        assert!(
            PaymentService::new(&journal)
                .advance(&key(), &terms(), &authority(), &mut recovered)
                .is_ok()
        );

        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn every_effect_observes_its_write_ahead_sync() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared);
        effects.ordering_journal = Some(journal.clone());

        assert!(
            PaymentService::new(&journal)
                .advance(&key(), &terms(), &authority(), &mut effects)
                .is_ok()
        );
        assert_eq!(journal.sync_count(), 5);
    }

    #[test]
    fn replay_is_synced_before_the_first_effect() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared);
        effects.replay_sync_journal = Some(journal.clone());

        assert!(
            PaymentService::new(&journal)
                .advance(&key(), &terms(), &authority(), &mut effects)
                .is_ok()
        );
    }

    #[test]
    fn mismatched_locked_token_refuses_before_verify_or_send() {
        let expected = terms();
        let mut wrong_unit = expected.clone();
        wrong_unit.unit = CurrencyUnit::Msat;
        let mut wrong_amount = expected.clone();
        wrong_amount.amount = Amount::from(8);
        let mut wrong_mint = expected.clone();
        wrong_mint.mint = MintUrl::from_str("https://other-mint.example").unwrap();

        for locked_terms in [wrong_unit, wrong_amount, wrong_mint] {
            let journal = MemoryPaymentJournal::default();
            let shared = FakeShared::default();
            let mut effects = FakeEffects::new(shared.clone());
            effects.locked_terms = Some(locked_terms);

            let error = PaymentService::new(&journal)
                .advance(&key(), &expected, &authority(), &mut effects)
                .unwrap_err();

            assert!(
                matches!(error, PaymentError::Refused(message) if message.contains("locked token"))
            );
            assert!(matches!(
                journal.records().last().map(|record| &record.value),
                Some(PaymentState::Intent { .. })
            ));
            assert_eq!(shared.verify_count.load(Ordering::SeqCst), 0);
            assert_eq!(shared.send_count.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn empty_relay_success_stays_locked_and_never_auto_resends() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared.clone());
        effects.empty_send = true;

        let first =
            PaymentService::new(&journal).advance(&key(), &terms(), &authority(), &mut effects);
        assert!(matches!(first, Err(PaymentError::NoRelayAccepted)));
        assert!(matches!(
            journal.records().last().map(|record| &record.value),
            Some(PaymentState::Locked { .. })
        ));

        let mut retry = FakeEffects::new(shared.clone());
        assert!(matches!(
            PaymentService::new(&journal).advance(&key(), &terms(), &authority(), &mut retry),
            Err(PaymentError::AmbiguousSendRefused)
        ));
        assert_eq!(shared.send_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sent_recovery_retries_receipt_only() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared.clone());
        effects.fail_receipt = true;

        assert!(matches!(
            PaymentService::new(&journal).advance(&key(), &terms(), &authority(), &mut effects),
            Err(PaymentError::Effect(_))
        ));
        assert!(matches!(
            journal.records().last().map(|record| &record.value),
            Some(PaymentState::Sent { .. })
        ));

        let mut retry = FakeEffects::new(shared.clone());
        assert!(matches!(
            PaymentService::new(&journal)
                .advance(&key(), &terms(), &authority(), &mut retry)
                .unwrap(),
            PaymentState::Closed { .. }
        ));
        assert_eq!(shared.mint_count.load(Ordering::SeqCst), 1);
        assert_eq!(shared.send_count.load(Ordering::SeqCst), 1);
        assert_eq!(shared.receipt_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn receipt_published_recovery_closes_without_any_effect() {
        let journal = MemoryPaymentJournal::default();
        let payment_key = key();
        let attempt_id = payment_key.attempt_id();
        let states = [
            PaymentState::Intent {
                attempt_id: attempt_id.clone(),
            },
            PaymentState::Locked {
                attempt_id: attempt_id.clone(),
            },
            PaymentState::Sent {
                attempt_id: attempt_id.clone(),
                payment: sent(),
            },
            PaymentState::ReceiptPublished {
                attempt_id,
                receipt: ReceiptRecord {
                    receipt_id: "receipt".into(),
                    receipt_kind: RECEIPT_EVENT_KIND,
                },
            },
        ];
        {
            let mut guard = journal.lock(&payment_key).unwrap();
            for state in states {
                guard
                    .append_sync(&PaymentRecord {
                        key: payment_key.clone(),
                        value: state,
                    })
                    .unwrap();
            }
        }
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared.clone());

        assert!(matches!(
            PaymentService::new(&journal)
                .advance(&payment_key, &terms(), &authority(), &mut effects)
                .unwrap(),
            PaymentState::Closed { .. }
        ));
        assert_eq!(shared.lock_calls.load(Ordering::SeqCst), 0);
        assert_eq!(shared.send_count.load(Ordering::SeqCst), 0);
        assert_eq!(shared.receipt_count.load(Ordering::SeqCst), 0);
    }

    // Finding H: a receipt correctly co-signed by the right parties but bound to a DIFFERENT
    // payment's terms must not verify/close this payment. Same anchors + valid schnorr, but the
    // preimage's amount is another payment's ⇒ Refused at the terms-binding gate.
    #[test]
    fn receipt_authority_refuses_receipt_bound_to_other_payment_terms() {
        let other_terms = PaymentTerms::new(
            MintUrl::from_str(MINT).unwrap(),
            Amount::from(9), // this key pays 7
            CurrencyUnit::Sat,
            nostr_public_key(2),
            public_key(2),
        );
        let other_key = PaymentKey::new(
            JobId::new("job").unwrap(),
            ResultId::new("result").unwrap(),
            DeliveryIntegrityHash::from_hex("11".repeat(32)).unwrap(),
            JobHash::from_hex("22".repeat(32)).unwrap(),
            &other_terms,
            None,
        );
        // Evidence is correctly co-signed over the OTHER payment's preimage (amount 9).
        let evidence = valid_evidence(&other_key);
        let err = authority()
            .verify(evidence, &key())
            .expect_err("receipt for other terms must refuse");
        match err {
            PaymentError::Refused(message) => {
                assert!(
                    message.contains("does not bind this payment") && message.contains("amount"),
                    "unexpected refusal: {message}"
                );
            }
            other => panic!("expected terms-binding refusal, got {other:?}"),
        }
    }

    // Finding H: a ReceiptPublished state carrying a non-receipt kind (e.g. an OLD journal record
    // whose `receipt_kind` defaults to 0) must NOT advance to Closed.
    #[test]
    fn recovery_refuses_to_close_on_non_receipt_kind() {
        let journal = MemoryPaymentJournal::default();
        let payment_key = key();
        let attempt_id = payment_key.attempt_id();
        let states = [
            PaymentState::Intent {
                attempt_id: attempt_id.clone(),
            },
            PaymentState::Locked {
                attempt_id: attempt_id.clone(),
            },
            PaymentState::Sent {
                attempt_id: attempt_id.clone(),
                payment: sent(),
            },
            PaymentState::ReceiptPublished {
                attempt_id,
                receipt: ReceiptRecord {
                    receipt_id: "1059envelopeid".into(),
                    receipt_kind: 0, // old journal default — NOT a kind-3400 receipt
                },
            },
        ];
        {
            let mut guard = journal.lock(&payment_key).unwrap();
            for state in states {
                guard
                    .append_sync(&PaymentRecord {
                        key: payment_key.clone(),
                        value: state,
                    })
                    .unwrap();
            }
        }
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared);
        let err = PaymentService::new(&journal)
            .advance(&payment_key, &terms(), &authority(), &mut effects)
            .expect_err("must not close on kind 0");
        match err {
            PaymentError::Refused(message) => assert!(
                message.contains("non-receipt kind 0"),
                "unexpected refusal: {message}"
            ),
            other => panic!("expected non-receipt-kind refusal, got {other:?}"),
        }
    }

    #[test]
    fn forged_receipt_author_or_missing_signature_is_rejected() {
        let journal = MemoryPaymentJournal::default();
        let shared = FakeShared::default();
        let mut effects = FakeEffects::new(shared);
        effects.forged_receipt = true;

        assert!(matches!(
            PaymentService::new(&journal).advance(&key(), &terms(), &authority(), &mut effects),
            Err(PaymentError::ForgedReceipt)
        ));
        assert!(matches!(
            journal.records().last().map(|record| &record.value),
            Some(PaymentState::Sent { .. })
        ));
    }

    #[test]
    fn receipt_authority_accepts_real_cosigned_receipt_and_stamps_kind() {
        let record = authority()
            .verify(valid_evidence(&key()), &key())
            .expect("valid co-signed receipt verifies");
        assert_eq!(record.receipt_kind, RECEIPT_EVENT_KIND);
        assert_eq!(record.receipt_id, valid_evidence(&key()).receipt_id);
    }

    #[test]
    fn receipt_authority_rejects_forged_seller_signature() {
        let mut evidence = valid_evidence(&key());
        // A real schnorr signature, but by an attacker key — not the anchored seller.
        evidence.seller_signature = sign_hex(&attacker_keys(), evidence.preimage.digest_bytes());
        assert!(matches!(
            authority().verify(evidence, &key()),
            Err(PaymentError::ForgedReceipt)
        ));
    }

    #[test]
    fn receipt_authority_rejects_wrong_external_anchor() {
        // Verify against an authority whose seller anchor is NOT who signed. The
        // receipt's own preimage/p-tags cannot rescue it — anchors are external.
        let wrong_anchor = ReceiptAuthority {
            buyer: buyer_keys().public_key(),
            seller: attacker_keys().public_key(),
        };
        assert!(matches!(
            wrong_anchor.verify(valid_evidence(&key()), &key()),
            Err(PaymentError::ForgedReceipt)
        ));
    }

    #[test]
    fn receipt_authority_fails_closed_on_empty_relay_before_returning_evidence() {
        // Empty relay_success ⇒ Err even though both co-signatures are valid.
        let mut evidence = valid_evidence(&key());
        evidence.relay_success.clear();
        assert!(matches!(
            authority().verify(evidence, &key()),
            Err(PaymentError::NoRelayAccepted)
        ));
    }

    #[test]
    fn receipt_authority_rejects_tampered_delivery_binding() {
        // Flip the signed delivery oid AFTER signing — the co-signature no longer matches
        // the digest (the delivered object is really bound, not decorative).
        let mut evidence = valid_evidence(&key());
        evidence.preimage.delivery_integrity_hash = "ab".repeat(20);
        assert!(matches!(
            authority().verify(evidence, &key()),
            Err(PaymentError::ForgedReceipt)
        ));
    }

    #[test]
    fn receipt_backcompat_empty_exec_metadata_commitment_still_verifies() {
        // A receipt with no echoed exec-metadata (empty-marker commitment) is valid —
        // the exec-metadata tags are optional; their absence is never a verify failure.
        let evidence = valid_evidence(&key());
        assert_eq!(
            evidence.preimage.exec_metadata_commitment,
            crate::receipt::EXEC_METADATA_COMMITMENT_EMPTY
        );
        assert!(authority().verify(evidence, &key()).is_ok());
    }

    #[test]
    fn receipt_record_journal_without_kind_defaults_to_zero() {
        // A journal record with no `receipt_kind` field must still
        // deserialize (serde default 0 = kind-1059-aliased id) — never rejected.
        let record: ReceiptRecord =
            serde_json::from_str(r#"{"receipt_id":"1059envelopeid"}"#).expect("parse");
        assert_eq!(record.receipt_kind, 0);
        assert_eq!(record.receipt_id, "1059envelopeid");

        // A new record round-trips with the 3400 stamp.
        let new = ReceiptRecord {
            receipt_id: "3400id".into(),
            receipt_kind: RECEIPT_EVENT_KIND,
        };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<ReceiptRecord>(&json).unwrap(), new);
    }

    #[test]
    fn fs_journal_rejects_torn_tail() {
        let path = std::env::temp_dir().join(format!(
            "mobee-payment-journal-{}-{}.jsonl",
            std::process::id(),
            key().attempt_id().as_str()
        ));
        let _ = std::fs::remove_file(&path);
        let journal = FsPaymentJournal::new(&path);
        {
            let mut guard = journal.lock(&key()).unwrap();
            guard
                .append_sync(&PaymentRecord {
                    key: key(),
                    value: PaymentState::Intent {
                        attempt_id: key().attempt_id(),
                    },
                })
                .unwrap();
        }
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"key":"torn"#)
            .unwrap();

        let mut guard = journal.lock(&key()).unwrap();
        assert!(matches!(
            guard.replay(),
            Err(JournalError::Corrupt { line: 2, .. })
        ));
        drop(guard);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn fs_journal_rejects_complete_json_without_newline() {
        let path = std::env::temp_dir().join(format!(
            "mobee-payment-journal-complete-tail-{}-{}.jsonl",
            std::process::id(),
            key().attempt_id().as_str()
        ));
        let _ = std::fs::remove_file(&path);
        let record = PaymentRecord {
            key: key(),
            value: PaymentState::Intent {
                attempt_id: key().attempt_id(),
            },
        };
        serde_json::to_writer(File::create(&path).unwrap(), &record).unwrap();

        let journal = FsPaymentJournal::new(&path);
        let mut guard = journal.lock(&key()).unwrap();
        assert!(matches!(
            guard.replay(),
            Err(JournalError::Corrupt { line: 1, .. })
        ));
        drop(guard);
        std::fs::remove_file(path).unwrap();
    }

    // Finding EE: a record whose PaymentKey does NOT match this journal's attempt key is CORRUPTION
    // (a misplaced/tampered journal), not absence. Silently filtering it would let the journal replay
    // as EMPTY — the caller then reads "no prior send" and could send AGAIN under an already-counted
    // budget attempt (double-spend). Replay must fail closed (Err) on the foreign record. A
    // genuinely-empty/absent journal still reads as the legitimate empty case.
    #[test]
    fn fs_journal_rejects_foreign_payment_key_record() {
        let path = std::env::temp_dir().join(format!(
            "mobee-payment-journal-foreign-key-{}-{}.jsonl",
            std::process::id(),
            key().attempt_id().as_str()
        ));
        let _ = std::fs::remove_file(&path);

        // A record keyed to a DIFFERENT attempt (different job id ⇒ different PaymentKey), written
        // straight to disk — `append_sync` would refuse to write it through the guard (KeyMismatch),
        // which is exactly why a foreign record on disk can only be corruption.
        let foreign_terms = terms();
        let foreign_key = PaymentKey::new(
            JobId::new("other-job").unwrap(),
            ResultId::new("result").unwrap(),
            DeliveryIntegrityHash::from_hex("11".repeat(32)).unwrap(),
            JobHash::from_hex("22".repeat(32)).unwrap(),
            &foreign_terms,
            None,
        );
        assert_ne!(foreign_key, key(), "foreign key must differ from the journal key");
        let foreign = PaymentRecord {
            key: foreign_key.clone(),
            value: PaymentState::Intent {
                attempt_id: foreign_key.attempt_id(),
            },
        };
        let mut line = serde_json::to_string(&foreign).unwrap();
        line.push('\n');
        std::fs::write(&path, line.as_bytes()).unwrap();

        let journal = FsPaymentJournal::new(&path);
        let mut guard = journal.lock(&key()).unwrap();
        assert!(
            matches!(guard.replay(), Err(JournalError::Corrupt { line: 1, .. })),
            "a foreign-key record must fail closed on replay, not read as empty"
        );
        drop(guard);
        std::fs::remove_file(&path).unwrap();

        // Control: a genuinely-absent journal (no file) still replays as the legitimate empty case.
        let empty_path = std::env::temp_dir().join(format!(
            "mobee-payment-journal-absent-{}-{}.jsonl",
            std::process::id(),
            key().attempt_id().as_str()
        ));
        let _ = std::fs::remove_file(&empty_path);
        let empty_journal = FsPaymentJournal::new(&empty_path);
        let mut empty_guard = empty_journal.lock(&key()).unwrap();
        assert!(
            empty_guard.replay().unwrap().is_empty(),
            "an absent/empty journal must read as the legitimate empty case"
        );
        drop(empty_guard);
        std::fs::remove_file(&empty_path).unwrap();
    }

    #[test]
    fn fs_journal_syncs_parent_before_the_first_effect() {
        let directory = std::env::temp_dir().join(format!(
            "mobee-payment-journal-dir-{}-{}",
            std::process::id(),
            key().attempt_id().as_str()
        ));
        let path = directory.join("payments.jsonl");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let journal = FsPaymentJournal::new(&path);

        let guard = journal.lock(&key()).unwrap();
        assert_eq!(journal.parent_sync_count(), 1);
        drop(guard);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[derive(Clone, Default)]
    struct FakeShared {
        attempts: Arc<Mutex<Vec<AttemptId>>>,
        mint_count: Arc<AtomicUsize>,
        lock_calls: Arc<AtomicUsize>,
        verify_count: Arc<AtomicUsize>,
        send_count: Arc<AtomicUsize>,
        receipt_count: Arc<AtomicUsize>,
    }

    struct FakeEffects {
        shared: FakeShared,
        blind_lock: bool,
        crash_after_lock: bool,
        locked_terms: Option<PaymentTerms>,
        empty_send: bool,
        fail_receipt: bool,
        forged_receipt: bool,
        empty_receipt_relay: bool,
        ordering_journal: Option<MemoryPaymentJournal>,
        replay_sync_journal: Option<MemoryPaymentJournal>,
    }

    struct RejectDelivery;

    impl DeliveryVerifier for RejectDelivery {
        fn verify(
            &mut self,
            _delivery: &GitDelivery,
        ) -> Result<crate::delivery::VerifiedDelivery, DeliveryError> {
            Err(DeliveryError::GitCommandFailed("fetch"))
        }
    }

    struct AcceptDelivery;

    impl DeliveryVerifier for AcceptDelivery {
        fn verify(
            &mut self,
            delivery: &GitDelivery,
        ) -> Result<crate::delivery::VerifiedDelivery, DeliveryError> {
            crate::delivery::VerifiedDelivery::from_fetched_tip(
                delivery,
                delivery.commit_oid().clone(),
            )
        }
    }

    impl FakeEffects {
        fn new(shared: FakeShared) -> Self {
            Self {
                shared,
                blind_lock: false,
                crash_after_lock: false,
                locked_terms: None,
                empty_send: false,
                fail_receipt: false,
                forged_receipt: false,
                empty_receipt_relay: false,
                ordering_journal: None,
                replay_sync_journal: None,
            }
        }
    }

    impl PaymentEffects for FakeEffects {
        fn lock_or_reconcile(
            &mut self,
            attempt_id: &AttemptId,
            terms: &PaymentTerms,
        ) -> Result<LockedPayment, EffectError> {
            if let Some(journal) = &self.replay_sync_journal {
                assert_eq!(
                    journal.replay_sync_count(),
                    1,
                    "replay must sync before lock"
                );
            }
            if let Some(journal) = &self.ordering_journal {
                assert_eq!(journal.sync_count(), 1, "Intent must sync before lock");
            }
            self.shared.lock_calls.fetch_add(1, Ordering::SeqCst);
            let mut attempts = self.shared.attempts.lock().unwrap();
            if self.blind_lock || !attempts.contains(attempt_id) {
                attempts.push(attempt_id.clone());
                self.shared.mint_count.fetch_add(1, Ordering::SeqCst);
            }
            drop(attempts);
            if self.crash_after_lock {
                self.crash_after_lock = false;
                return Err(EffectError::new("simulated crash after lock effect"));
            }
            Ok(locked_payment(self.locked_terms.as_ref().unwrap_or(terms)))
        }

        fn verify_payment(
            &mut self,
            _attempt_id: &AttemptId,
            terms: &PaymentTerms,
            _locked: &LockedPayment,
        ) -> Result<VerifiedPayment, EffectError> {
            if let Some(journal) = &self.ordering_journal {
                assert_eq!(journal.sync_count(), 2, "Locked must sync before verify");
            }
            self.shared.verify_count.fetch_add(1, Ordering::SeqCst);
            Ok(VerifiedPayment {
                mint: terms.mint.clone(),
                amount: terms.amount,
                unit: terms.unit.clone(),
                proof_ys: Vec::new(),
            })
        }

        fn send_payment(
            &mut self,
            _key: &PaymentKey,
            _attempt_id: &AttemptId,
            _terms: &PaymentTerms,
            _locked: &LockedPayment,
            _verified: &VerifiedPayment,
        ) -> Result<PaymentSent, EffectError> {
            if let Some(journal) = &self.ordering_journal {
                assert_eq!(journal.sync_count(), 2, "Locked must sync before send");
            }
            self.shared.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(PaymentSent {
                payment_id: "payment".into(),
                relay_success: if self.empty_send {
                    Vec::new()
                } else {
                    vec!["memory://relay".into()]
                },
                relay_failed: if self.empty_send {
                    vec![PaymentRelayFailure {
                        relay: "memory://relay".into(),
                        error: "rejected".into(),
                    }]
                } else {
                    Vec::new()
                },
            })
        }

        fn publish_receipt(
            &mut self,
            key: &PaymentKey,
            _payment: &PaymentSent,
        ) -> Result<ReceiptEvidence, EffectError> {
            if let Some(journal) = &self.ordering_journal {
                assert_eq!(journal.sync_count(), 3, "Sent must sync before receipt");
            }
            self.shared.receipt_count.fetch_add(1, Ordering::SeqCst);
            if self.fail_receipt {
                return Err(EffectError::new("receipt relay unavailable"));
            }
            let mut evidence = valid_evidence(key);
            if self.forged_receipt {
                // Forge: seller co-signature by a non-anchor (attacker) key — a valid
                // schnorr signature, but not by the anchored seller ⇒ must fail verify.
                let digest = evidence.preimage.digest_bytes();
                evidence.seller_signature = sign_hex(&attacker_keys(), digest);
            }
            if self.empty_receipt_relay {
                evidence.relay_success.clear();
            }
            Ok(evidence)
        }
    }

    fn terms() -> PaymentTerms {
        PaymentTerms::new(
            MintUrl::from_str(MINT).unwrap(),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_public_key(2),
            public_key(2),
        )
    }

    fn key() -> PaymentKey {
        PaymentKey::new(
            JobId::new("job").unwrap(),
            ResultId::new("result").unwrap(),
            DeliveryIntegrityHash::from_hex("11".repeat(32)).unwrap(),
            JobHash::from_hex("22".repeat(32)).unwrap(),
            &terms(),
            None,
        )
    }

    fn git_delivery() -> GitDelivery {
        GitDelivery::new(
            "https://example.invalid/repo.git",
            "mobee/job",
            crate::delivery::CommitOid::parse("33".repeat(20)).unwrap(),
        )
        .unwrap()
    }

    fn git_key() -> PaymentKey {
        PaymentKey::new(
            JobId::new("job").unwrap(),
            ResultId::new("result").unwrap(),
            DeliveryIntegrityHash::from_hex("33".repeat(20)).unwrap(),
            JobHash::from_hex("22".repeat(32)).unwrap(),
            &terms(),
            None,
        )
    }

    fn authority() -> ReceiptAuthority {
        // External anchors: buyer == offer author (key 1), seller == accepted-claim
        // seller (key 2, == terms().seller_nostr_pubkey). Both are nostr identities.
        ReceiptAuthority {
            buyer: buyer_keys().public_key(),
            seller: seller_keys().public_key(),
        }
    }

    fn buyer_keys() -> nostr_sdk::Keys {
        nostr_sdk::Keys::parse(&"01".repeat(32)).unwrap()
    }

    fn seller_keys() -> nostr_sdk::Keys {
        // x-only pubkey == nostr_public_key(2) == terms().seller_nostr_pubkey.
        nostr_sdk::Keys::parse(&"02".repeat(32)).unwrap()
    }

    fn attacker_keys() -> nostr_sdk::Keys {
        nostr_sdk::Keys::parse(&"09".repeat(32)).unwrap()
    }

    /// The co-signed preimage a real buyer would reconstruct from the trade facts.
    fn receipt_preimage(key: &PaymentKey) -> ReceiptPreimage {
        ReceiptPreimage {
            job_hash: key.job_hash.as_str().to_owned(),
            offer_id: key.job_id.as_str().to_owned(),
            amount: key.amount.to_u64(),
            unit: key.unit.to_string(),
            buyer_pubkey: buyer_keys().public_key().to_hex(),
            seller_pubkey: seller_keys().public_key().to_hex(),
            delivery_integrity_hash: key.delivery_integrity_hash.as_str().to_owned(),
            delivery_kind: "fork".to_owned(),
            exec_metadata_commitment: crate::receipt::EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
            creq_hash: key.creq_hash.clone(),
        }
    }

    fn sign_hex(keys: &nostr_sdk::Keys, digest: [u8; 32]) -> String {
        keys.sign_schnorr(&Message::from_digest(digest)).to_string()
    }

    // ── The pre-pay seam ALSO binds the seller-signed authorship tuple ───────────────────────
    // The SAME seam that verifies the receipt cosig also verifies an additional seller signature
    // over {job_id, seller_pubkey, target_repo, base_oid, fork_ref, commit_oid} — one seam, more
    // binds. A valid pair passes; a sig over a DIFFERENT commit_oid or a tampered field refuses.
    fn authorship_tuple(commit_oid: &str) -> crate::contribution::AuthorshipTuple {
        crate::contribution::AuthorshipTuple {
            job_id: "job".into(),
            seller_pubkey: seller_keys().public_key().to_hex(),
            target: crate::contribution::TargetRepoPin::new(
                "aa".repeat(32),
                "https://mobee-relay.orveth.dev/git/owner/repo.git",
            )
            .unwrap(),
            base_oid: "77".repeat(20),
            fork: crate::contribution::ForkRef::new(
                "https://mobee-relay.orveth.dev/git/seller/fork.git",
                "mobee/contribution/job",
            )
            .unwrap(),
            commit_oid: commit_oid.to_owned(),
        }
    }

    #[test]
    fn prepay_seam_accepts_valid_receipt_and_authorship_tuple() {
        let key = git_key();
        let preimage = receipt_preimage(&key);
        let receipt_sig = sign_hex(&seller_keys(), preimage.digest_bytes());
        // Buyer reconstructs the tuple over the PAID commit; the seller signed exactly that tuple.
        let tuple = authorship_tuple(key.delivery_integrity_hash.as_str());
        let tuple_sig = sign_hex(&seller_keys(), tuple.digest_bytes());
        authority()
            .verify_seller_prepay_cosig(
                &preimage,
                &receipt_sig,
                Some(ContributionCosig {
                    tuple_digest: tuple.digest_bytes(),
                    tuple_signature_hex: &tuple_sig,
                }),
            )
            .expect("valid receipt + tuple cosig must pass");
    }

    #[test]
    fn prepay_seam_refuses_tuple_signed_over_a_different_commit_oid() {
        let key = git_key();
        let preimage = receipt_preimage(&key);
        let receipt_sig = sign_hex(&seller_keys(), preimage.digest_bytes());
        // Seller signed a tuple for a DIFFERENT commit; buyer reconstructs over the paid commit.
        let signed_tuple = authorship_tuple(&"ab".repeat(20));
        let tuple_sig = sign_hex(&seller_keys(), signed_tuple.digest_bytes());
        let buyer_tuple = authorship_tuple(key.delivery_integrity_hash.as_str());
        let err = authority()
            .verify_seller_prepay_cosig(
                &preimage,
                &receipt_sig,
                Some(ContributionCosig {
                    tuple_digest: buyer_tuple.digest_bytes(),
                    tuple_signature_hex: &tuple_sig,
                }),
            )
            .expect_err("a tuple sig over a different commit_oid must refuse");
        assert!(
            err.to_string().contains("contribution authorship invalid"),
            "must be the authorship refusal, got: {err}"
        );
    }

    #[test]
    fn prepay_seam_refuses_tampered_tuple_field() {
        let key = git_key();
        let preimage = receipt_preimage(&key);
        let receipt_sig = sign_hex(&seller_keys(), preimage.digest_bytes());
        // Seller signed the honest tuple; buyer reconstructs one with a TAMPERED base_oid.
        let honest = authorship_tuple(key.delivery_integrity_hash.as_str());
        let tuple_sig = sign_hex(&seller_keys(), honest.digest_bytes());
        let mut tampered = honest.clone();
        tampered.base_oid = "cd".repeat(20);
        assert!(authority()
            .verify_seller_prepay_cosig(
                &preimage,
                &receipt_sig,
                Some(ContributionCosig {
                    tuple_digest: tampered.digest_bytes(),
                    tuple_signature_hex: &tuple_sig,
                }),
            )
            .is_err());
    }

    #[test]
    fn prepay_seam_refuses_tuple_signed_by_a_non_seller_key() {
        // A tuple signed by an unrelated key (not the claim seller) must refuse — the authorship
        // anchor is the claim seller, so a third party cannot be paid for its own commit.
        let key = git_key();
        let preimage = receipt_preimage(&key);
        let receipt_sig = sign_hex(&seller_keys(), preimage.digest_bytes());
        let tuple = authorship_tuple(key.delivery_integrity_hash.as_str());
        let attacker_sig = sign_hex(&attacker_keys(), tuple.digest_bytes());
        assert!(authority()
            .verify_seller_prepay_cosig(
                &preimage,
                &receipt_sig,
                Some(ContributionCosig {
                    tuple_digest: tuple.digest_bytes(),
                    tuple_signature_hex: &attacker_sig,
                }),
            )
            .is_err());
    }

    /// A valid, real co-signed receipt over the trade's preimage.
    fn valid_evidence(key: &PaymentKey) -> ReceiptEvidence {
        let preimage = receipt_preimage(key);
        let digest = preimage.digest_bytes();
        ReceiptEvidence {
            receipt_id: "aa".repeat(32),
            author: buyer_keys().public_key(),
            seller_signature: sign_hex(&seller_keys(), digest),
            buyer_signature: sign_hex(&buyer_keys(), digest),
            preimage,
            relay_success: vec!["memory://relay".into()],
        }
    }

    fn sent() -> PaymentSent {
        PaymentSent {
            payment_id: "payment".into(),
            relay_success: vec!["memory://relay".into()],
            relay_failed: Vec::new(),
        }
    }

    fn public_key(byte: u8) -> PublicKey {
        SecretKey::from_slice(&[byte; 32]).unwrap().public_key()
    }

    fn nostr_public_key(byte: u8) -> NostrPublicKey {
        let compressed = public_key(byte).to_string();
        NostrPublicKey::from_hex(&compressed[2..]).unwrap()
    }

    fn locked_payment(terms: &PaymentTerms) -> LockedPayment {
        let proof = Proof::new(
            terms.amount,
            Id::from_str("010000000000000000000000000000000000000000000000000000000000000000")
                .unwrap(),
            Secret::new("payment-state-machine-fixture"),
            public_key(42),
        );
        LockedPayment::new(Token::new(
            terms.mint.clone(),
            vec![proof],
            None,
            terms.unit.clone(),
        ))
    }
}
