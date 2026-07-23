//! Seller daemon primitives: rate-gate, journal (pay-once), gift-wrap (kind-1059) unwrap, pay bind.
//!
//! Invariants:
//! - `rate_sats` is CLAIM-FLOOR only (`offer.amount ≥ rate_sats`)
//! - receive asserts `Amount == offer.amount` via `terms_for_offer`
//! - bind `job_id`(+`result_id`) before journaling; wrong-job → refuse
//! - gift-wrap (kind-1059): unwrap ONLY `p-tag==self`, exactly ONE decode site, never log plaintext

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::gateway::ParsedOffer;
use crate::home::{HomeError, MobeeHome, SellerConfig};

const JOURNAL_FILE: &str = "seller-journal.jsonl";
pub const DEFAULT_JOB_TIMEOUT_SECS: u64 = 600;

#[derive(Debug)]
pub enum SellerError {
    Config(String),
    Io(String),
    Journal(String),
    Policy(String),
    Payment(String),
    Home(HomeError),
}

impl std::fmt::Display for SellerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(message) => write!(f, "seller config error: {message}"),
            Self::Io(message) => write!(f, "seller io error: {message}"),
            Self::Journal(message) => write!(f, "seller journal error: {message}"),
            Self::Policy(message) => write!(f, "seller policy refused: {message}"),
            Self::Payment(message) => write!(f, "seller payment error: {message}"),
            Self::Home(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SellerError {}

impl From<HomeError> for SellerError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

/// Require the three locked `[seller]` fields (named fail-closed).
pub fn require_seller_config(home: &MobeeHome) -> Result<&SellerConfig, SellerError> {
    let seller = home.config.seller.as_ref().ok_or_else(|| {
        SellerError::Config("missing required [seller] section (agent_command, rate_sats, git_remote)".into())
    })?;
    if seller.agent_command.is_empty() {
        return Err(SellerError::Config(
            "missing required field agent_command (argv array)".into(),
        ));
    }
    if seller.agent_command.iter().any(|part| part.is_empty()) {
        return Err(SellerError::Config(
            "agent_command argv entries must be non-empty".into(),
        ));
    }
    if seller.git_remote.trim().is_empty() {
        return Err(SellerError::Config(
            "missing required field git_remote".into(),
        ));
    }
    // rate_sats: 0 is allowed (accept any amount ≥ 0) but field must be present (serde).
    Ok(seller)
}

/// Claim-floor: targeted-to-self AND `offer.amount ≥ rate_sats`.
///
/// Untargeted/open offers refuse by default. Pass `claim_open_pool = true` (explicit
/// seller opt-in) to allow untargeted offers that still clear the rate floor.
pub fn rate_gate_allows(
    offer: &ParsedOffer,
    seller_pubkey: &str,
    rate_sats: u64,
    claim_open_pool: bool,
) -> Result<(), SellerError> {
    match offer.seller_pubkey.as_deref() {
        None if claim_open_pool => {}
        None => {
            return Err(SellerError::Policy(
                "untargeted offer refused (seller claims only p-tag==self; set claim_open_pool=true to opt in)".into(),
            ));
        }
        Some(target) if target != seller_pubkey => {
            return Err(SellerError::Policy(format!(
                "offer targets seller {target}, not {seller_pubkey}"
            )));
        }
        Some(_) => {}
    }
    if offer.amount < rate_sats {
        return Err(SellerError::Policy(format!(
            "offer amount {} sat below seller rate_sats {rate_sats}",
            offer.amount
        )));
    }
    Ok(())
}

/// Job deadline: config override, else offer deadline, else now+DEFAULT.
pub fn job_deadline_unix(offer: &ParsedOffer, seller: &SellerConfig, now_unix: u64) -> u64 {
    if let Some(secs) = seller.job_timeout_secs {
        return now_unix.saturating_add(secs);
    }
    if offer.deadline_unix > now_unix {
        return offer.deadline_unix;
    }
    now_unix.saturating_add(DEFAULT_JOB_TIMEOUT_SECS)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEntry {
    Claim {
        job_id: String,
        ts: u64,
        /// Derived-expiry anchor: deadline this claim was journaled against.
        /// `#[serde(default)]` = older claim lines (no anchor) default to 0
        /// (treated as already-expired by [`plan_orphaned_claims`], the safe default —
        /// an old orphan is released, never left live).
        #[serde(default)]
        deadline_unix: u64,
        /// feedback-kind claim event id (so restart-reconcile can reference the dead claim).
        #[serde(default)]
        claim_id: String,
        /// Buyer pubkey (p-tag target for a reconcile feedback-kind release). Back-compat empty.
        #[serde(default)]
        buyer_pubkey: String,
    },
    Receipt {
        job_id: String,
        result_id: String,
        amount_received: u64,
        mint: String,
        buyer: String,
        swap_ok: bool,
        ts: u64,
    },
    /// Terminal release of an orphaned/undeliverable claim (restart-reconcile).
    /// Makes reconcile idempotent: a released job is terminal, never re-released.
    Release {
        job_id: String,
        reason: String,
        ts: u64,
    },
    /// Delivered-but-unpaid (#57): the seller published a result and is awaiting the buyer's payment
    /// wrap. Durable so a restart rebuilds the job into `awaiting_payment` — a backfilled/buffered
    /// wrap can then bind and redeem. A matching `Receipt` supersedes it (paid); a matching
    /// `Release` cancels it. Carries the money-critical fields the redeem path needs.
    Delivery {
        job_id: String,
        result_id: String,
        amount: u64,
        unit: String,
        buyer_pubkey: String,
        /// The mints this seller advertised for THIS job (the creq `m` list authored at claim,
        /// captured at delivery). The redeem guard for a RESTORED job settles against these — the
        /// ORIGINAL terms — not current config, so a config change across restart can neither
        /// strand an old payment (mint dropped) nor let a newly-added mint settle an old claim.
        /// `#[serde(default)]`: a delivery journaled before this field defaults to empty and
        /// restores against current config (prior behavior — back-compat).
        #[serde(default)]
        accepted_mints: Vec<String>,
        ts: u64,
    },
    /// Intent-to-receive breadcrumb written BEFORE the mint swap: a durable AUDIT record that this
    /// seller attempted to redeem this token for this job. If a crash lands the swap but not the
    /// `Receipt`, this breadcrumb (a pending receive with no matching `Receipt`) is the record an
    /// operator reconciles from. It is NOT auto-finalize proof of payment (finding S): a breadcrumb
    /// is appended immediately before every swap, so its presence proves only intent, never a prior
    /// redeem — an "already spent" receive is refused, never finalized off this record. Keyed by
    /// `token_hash` (SHA-256 of the token string — NEVER the token/proof plaintext). A matching
    /// `Receipt` supersedes it.
    PendingReceive {
        job_id: String,
        result_id: String,
        /// SHA-256 hex of the received token string. No proof/secret material.
        token_hash: String,
        buyer: String,
        mint: String,
        amount: u64,
        ts: u64,
    },
}

/// A delivered-but-unpaid job recovered from the journal at boot, enough to rebuild an
/// awaiting-payment binding so a stored/buffered wrap can redeem (#57).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredUnpaid {
    pub job_id: String,
    pub result_id: String,
    pub amount: u64,
    pub unit: String,
    pub buyer_pubkey: String,
    /// Original advertised mints journaled at delivery (empty for a pre-field delivery line);
    /// the restored job redeems against these, not current config (terms-drift fix).
    pub accepted_mints: Vec<String>,
}

/// Whether an orphaned claim is past its deadline at reconcile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimLiveness {
    /// `now > deadline_unix` — dead claim, buyer may re-post.
    Expired,
    /// `now <= deadline_unix` — deadline not yet reached.
    Live,
}

/// An in-flight claim discovered at daemon startup (journaled Claim, no matching
/// Receipt and no matching Release). Carries everything reconcile needs to publish a
/// feedback-kind release WITHOUT touching the relay or re-parsing the offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanClaim {
    pub job_id: String,
    pub claim_id: String,
    pub buyer_pubkey: String,
    pub deadline_unix: u64,
    pub liveness: ClaimLiveness,
}

/// Pure restart-reconcile plan over journal entries.
///
/// Returns every journaled `Claim` that has neither a `Receipt` (paid → CLOSED) nor a
/// `Release` (already terminal) for its `job_id`, classified `Expired`/`Live` by the
/// **injected** `now`. No wall-clock, no relay — expiry is DERIVED, never stored.
pub fn plan_orphaned_claims(entries: &[JournalEntry], now: u64) -> Vec<OrphanClaim> {
    let mut terminal: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for entry in entries {
        match entry {
            JournalEntry::Receipt { job_id, .. }
            | JournalEntry::Release { job_id, .. }
            // #57: a delivered job is NOT an orphaned in-flight claim — it published a result and is
            // awaiting payment (rebuilt into awaiting_payment on boot), so it must never be released.
            | JournalEntry::Delivery { job_id, .. }
            // An in-flight receive (swap initiated) is likewise not an orphaned claim.
            | JournalEntry::PendingReceive { job_id, .. } => {
                terminal.insert(job_id.as_str());
            }
            JournalEntry::Claim { .. } => {}
        }
    }
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for entry in entries {
        if let JournalEntry::Claim {
            job_id,
            deadline_unix,
            claim_id,
            buyer_pubkey,
            ..
        } = entry
        {
            if terminal.contains(job_id.as_str()) || !seen.insert(job_id.as_str()) {
                continue;
            }
            let liveness = if now > *deadline_unix {
                ClaimLiveness::Expired
            } else {
                ClaimLiveness::Live
            };
            out.push(OrphanClaim {
                job_id: job_id.clone(),
                claim_id: claim_id.clone(),
                buyer_pubkey: buyer_pubkey.clone(),
                deadline_unix: *deadline_unix,
                liveness,
            });
        }
    }
    out
}

/// Append-only seller journal — claim dedup + pay-once receipt evidence.
pub struct SellerJournal {
    path: PathBuf,
}

impl SellerJournal {
    pub fn open(home: &MobeeHome) -> Result<Self, SellerError> {
        let path = home.root.join(JOURNAL_FILE);
        let parent = path.parent();
        if let Some(parent) = parent {
            fs::create_dir_all(parent).map_err(|error| SellerError::Io(error.to_string()))?;
        }
        if !path.exists() {
            File::create(&path).map_err(|error| SellerError::Io(error.to_string()))?;
            // The append path's `sync_all` makes each record durable, but the file's directory
            // ENTRY must be fsync'd once at creation or a power-loss before the first append can
            // drop the whole journal — losing pay-once/receipt state on restart.
            if let Some(parent) = parent {
                crate::durable::sync_dir(parent).map_err(|error| SellerError::Io(error.to_string()))?;
            }
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> Result<Vec<JournalEntry>, SellerError> {
        // Fail-closed money-path read: a MISSING journal is the legitimate first-boot empty case
        // (Ok(empty)); every OTHER read failure — an unreadable/permission-denied file (Io) or a
        // corrupt line (Journal) — PROPAGATES so callers refuse/buffer/degrade rather than proceed
        // as if the journal were empty. This is the single seam the has_*/last_receipt_ts/
        // oldest_unsettled/pending_receive reads all funnel through, so none of them can silently
        // treat an unreadable journal as absent.
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(SellerError::Io(error.to_string())),
        };
        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|error| SellerError::Io(error.to_string()))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let entry: JournalEntry = serde_json::from_str(trimmed)
                .map_err(|error| SellerError::Journal(format!("corrupt journal line: {error}")))?;
            out.push(entry);
        }
        Ok(out)
    }

    pub fn has_claim(&self, job_id: &str) -> Result<bool, SellerError> {
        Ok(self.entries()?.into_iter().any(|entry| match entry {
            JournalEntry::Claim { job_id: id, .. } => id == job_id,
            JournalEntry::Receipt { job_id: id, .. } => id == job_id,
            JournalEntry::Release { job_id: id, .. } => id == job_id,
            JournalEntry::Delivery { job_id: id, .. } => id == job_id,
            JournalEntry::PendingReceive { job_id: id, .. } => id == job_id,
        }))
    }

    pub fn has_receipt(&self, job_id: &str) -> Result<bool, SellerError> {
        Ok(self.entries()?.into_iter().any(|entry| matches!(
            entry,
            JournalEntry::Receipt { job_id: id, .. } if id == job_id
        )))
    }

    pub fn has_release(&self, job_id: &str) -> Result<bool, SellerError> {
        Ok(self.entries()?.into_iter().any(|entry| matches!(
            entry,
            JournalEntry::Release { job_id: id, .. } if id == job_id
        )))
    }

    /// Newest `Receipt` timestamp in the journal — the `since` anchor for the boot wrap-backfill
    /// (#57), so a restart re-fetches stored 1059 payments the live subscription did not replay.
    /// `Ok(None)` when there are no receipts yet (or no journal file exists).
    pub fn last_receipt_ts(&self) -> Result<Option<u64>, SellerError> {
        // Fail-closed: any read error propagates (see `entries`). A genuinely-absent journal folds to
        // an empty list ⇒ Ok(None) ⇒ legitimate first boot; an unreadable one aborts the backfill.
        Ok(self
            .entries()?
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::Receipt { ts, .. } => Some(*ts),
                _ => None,
            })
            .max())
    }

    /// Oldest `Delivery` timestamp among jobs that are still UNSETTLED (no matching `Receipt` or
    /// `Release`). This is the safe lower bound for the wrap-backfill `since` cursor: a receipt for
    /// a NEWER job must never advance the cursor past an OLDER delivered-but-unpaid job whose
    /// payment wrap has not yet been collected. `Ok(None)` when nothing is unsettled.
    pub fn oldest_unsettled_delivery_ts(&self) -> Result<Option<u64>, SellerError> {
        // Fail-closed: any read error propagates (see `entries`); absent journal ⇒ empty ⇒ Ok(None).
        let entries = self.entries()?;
        let mut settled: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for entry in &entries {
            match entry {
                JournalEntry::Receipt { job_id, .. } | JournalEntry::Release { job_id, .. } => {
                    settled.insert(job_id.as_str());
                }
                _ => {}
            }
        }
        Ok(entries
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::Delivery { job_id, ts, .. }
                    if !settled.contains(job_id.as_str()) =>
                {
                    Some(*ts)
                }
                _ => None,
            })
            .min())
    }

    /// Journal a CLAIMED transition. `claim_id`/`buyer_pubkey`/`deadline_unix` are the
    /// anchors restart-reconcile needs to release an orphan without the relay.
    pub fn append_claim(
        &self,
        job_id: &str,
        claim_id: &str,
        buyer_pubkey: &str,
        deadline_unix: u64,
    ) -> Result<(), SellerError> {
        if self.has_claim(job_id)? {
            return Err(SellerError::Journal(format!(
                "job {job_id} already claimed (dedup)"
            )));
        }
        self.append(JournalEntry::Claim {
            job_id: job_id.to_owned(),
            ts: now_unix(),
            deadline_unix,
            claim_id: claim_id.to_owned(),
            buyer_pubkey: buyer_pubkey.to_owned(),
        })
    }

    /// Journal a terminal RELEASE marker (restart-reconcile). Idempotent — a second
    /// release for the same `job_id` is a no-op so repeated restarts do not re-fire.
    pub fn append_release(&self, job_id: &str, reason: &str) -> Result<(), SellerError> {
        if self.has_release(job_id)? {
            return Ok(());
        }
        self.append(JournalEntry::Release {
            job_id: job_id.to_owned(),
            reason: reason.to_owned(),
            ts: now_unix(),
        })
    }

    /// Bind payload job_id(+result_id) to the local claim/result BEFORE journaling.
    pub fn append_receipt(
        &self,
        local_job_id: &str,
        local_result_id: &str,
        payload_job_id: &str,
        payload_result_id: &str,
        amount_received: u64,
        expected_amount: u64,
        mint: &str,
        buyer: &str,
        swap_ok: bool,
    ) -> Result<(), SellerError> {
        if payload_job_id != local_job_id || payload_result_id != local_result_id {
            return Err(SellerError::Policy(format!(
                "payment bind refused: payload job/result ({payload_job_id}/{payload_result_id}) != local ({local_job_id}/{local_result_id})"
            )));
        }
        if amount_received != expected_amount {
            return Err(SellerError::Policy(format!(
                "receipt amount_received {amount_received} != offer.amount {expected_amount}"
            )));
        }
        if self.has_receipt(local_job_id)? {
            return Err(SellerError::Journal(format!(
                "job {local_job_id} already receipted (pay-once)"
            )));
        }
        self.append(JournalEntry::Receipt {
            job_id: local_job_id.to_owned(),
            result_id: local_result_id.to_owned(),
            amount_received,
            mint: mint.to_owned(),
            buyer: buyer.to_owned(),
            swap_ok,
            ts: now_unix(),
        })
    }

    /// Journal an intent-to-receive breadcrumb BEFORE the mint swap (see
    /// [`JournalEntry::PendingReceive`]). `token_hash` is the SHA-256 hex of the token string; no
    /// proof/secret material is stored. No-op if a receipt already exists for the job (pay-once).
    pub fn append_pending_receive(
        &self,
        job_id: &str,
        result_id: &str,
        token_hash: &str,
        buyer: &str,
        mint: &str,
        amount: u64,
    ) -> Result<(), SellerError> {
        self.append(JournalEntry::PendingReceive {
            job_id: job_id.to_owned(),
            result_id: result_id.to_owned(),
            token_hash: token_hash.to_owned(),
            buyer: buyer.to_owned(),
            mint: mint.to_owned(),
            amount,
            ts: now_unix(),
        })
    }

    /// Look up an un-finalized intent-to-receive for `token_hash`: a [`JournalEntry::PendingReceive`]
    /// whose job has no matching [`JournalEntry::Receipt`] yet. Returns the carried
    /// `(job_id, result_id, buyer, mint, amount)` for operator reconcile of an interrupted redeem.
    /// This is NOT used to auto-finalize a receipt (finding S — a breadcrumb is not proof of a prior
    /// redeem); `Ok(None)` when none.
    #[allow(clippy::type_complexity)]
    pub fn pending_receive_for_token(
        &self,
        token_hash: &str,
    ) -> Result<Option<(String, String, String, String, u64)>, SellerError> {
        // Fail-closed: any read error propagates (see `entries`); absent journal ⇒ empty ⇒ Ok(None).
        let entries = self.entries()?;
        let mut receipted: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for entry in &entries {
            if let JournalEntry::Receipt { job_id, .. } = entry {
                receipted.insert(job_id.as_str());
            }
        }
        Ok(entries.iter().rev().find_map(|entry| match entry {
            JournalEntry::PendingReceive {
                job_id,
                result_id,
                token_hash: hash,
                buyer,
                mint,
                amount,
                ..
            } if hash == token_hash && !receipted.contains(job_id.as_str()) => Some((
                job_id.clone(),
                result_id.clone(),
                buyer.clone(),
                mint.clone(),
                *amount,
            )),
            _ => None,
        }))
    }

    /// Journal a delivered-but-unpaid transition (#57) so a restart can rebuild the awaiting-payment
    /// binding and a stored/buffered wrap can redeem. Idempotent-safe to call more than once.
    pub fn append_delivery(
        &self,
        job_id: &str,
        result_id: &str,
        amount: u64,
        unit: &str,
        buyer_pubkey: &str,
        accepted_mints: &[String],
    ) -> Result<(), SellerError> {
        self.append(JournalEntry::Delivery {
            job_id: job_id.to_owned(),
            result_id: result_id.to_owned(),
            amount,
            unit: unit.to_owned(),
            buyer_pubkey: buyer_pubkey.to_owned(),
            accepted_mints: accepted_mints.to_vec(),
            ts: now_unix(),
        })
    }

    /// Delivered-but-unpaid jobs to rebuild into `awaiting_payment` at boot (#57): every `Delivery`
    /// whose `job_id` has no `Receipt` (paid) and no `Release` (cancelled). Deduped by `job_id`
    /// (latest Delivery wins).
    pub fn deliveries_awaiting_receipt(&self) -> Result<Vec<DeliveredUnpaid>, SellerError> {
        let entries = match self.entries() {
            Ok(entries) => entries,
            Err(SellerError::Io(_)) => return Ok(Vec::new()),
            Err(other) => return Err(other),
        };
        let mut settled: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for entry in &entries {
            match entry {
                JournalEntry::Receipt { job_id, .. } | JournalEntry::Release { job_id, .. } => {
                    settled.insert(job_id.as_str());
                }
                _ => {}
            }
        }
        let mut by_job: std::collections::BTreeMap<String, DeliveredUnpaid> =
            std::collections::BTreeMap::new();
        for entry in &entries {
            if let JournalEntry::Delivery {
                job_id,
                result_id,
                amount,
                unit,
                buyer_pubkey,
                accepted_mints,
                ..
            } = entry
            {
                if settled.contains(job_id.as_str()) {
                    continue;
                }
                by_job.insert(
                    job_id.clone(),
                    DeliveredUnpaid {
                        job_id: job_id.clone(),
                        result_id: result_id.clone(),
                        amount: *amount,
                        unit: unit.clone(),
                        buyer_pubkey: buyer_pubkey.clone(),
                        accepted_mints: accepted_mints.clone(),
                    },
                );
            }
        }
        Ok(by_job.into_values().collect())
    }

    fn append(&self, entry: JournalEntry) -> Result<(), SellerError> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| SellerError::Io(error.to_string()))?;
        let line = serde_json::to_string(&entry)
            .map_err(|error| SellerError::Journal(error.to_string()))?;
        // Journal lines carry ids/amounts/mint/buyer/swap_ok/ts only — never token material.
        writeln!(file, "{line}").map_err(|error| SellerError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| SellerError::Io(error.to_string()))?;
        Ok(())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Sign a receipt/job hash with the seller nostr key (schnorr hex).
#[cfg(feature = "gateway")]
pub fn sign_receipt_hash(
    keys: &nostr_sdk::prelude::Keys,
    job_hash: &str,
) -> Result<String, SellerError> {
    use nostr_sdk::secp256k1::Message;

    let bytes: [u8; 32] = hex::decode(job_hash)
        .map_err(|error| SellerError::Payment(format!("invalid receipt hash bytes: {error}")))?
        .try_into()
        .map_err(|_| SellerError::Payment("invalid receipt hash length".into()))?;
    Ok(keys
        .sign_schnorr(&Message::from_digest(bytes))
        .to_string())
}

/// Kind-1059 gift-wrap: the ONE decode site — unwrap ONLY when `p-tag==self`.
///
/// Returns `Ok(None)` when the wrap is not addressed to us or fails decrypt (not an error —
/// other parties' envelopes stay dark). Decrypted contents are NEVER logged.
#[cfg(all(feature = "gateway", feature = "wallet"))]
pub async fn unwrap_own_payment_gift_wrap(
    keys: &nostr_sdk::prelude::Keys,
    event: &nostr_sdk::prelude::Event,
) -> Result<Option<crate::payment_send::ReceivedPayment>, SellerError> {
    use nostr_sdk::prelude::{Kind, UnwrappedGift};

    if event.kind != Kind::GiftWrap {
        return Ok(None);
    }
    let self_pk = keys.public_key();
    // Kind-1059 condition: p-tag == self only (exact ONE decode site below).
    let addressed_to_self = event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.first().map(String::as_str) == Some("p")
            && slice
                .get(1)
                .map(|value| value == &self_pk.to_hex() || value == &self_pk.to_string())
                .unwrap_or(false)
    });
    if !addressed_to_self {
        return Ok(None);
    }

    let unwrapped = match UnwrappedGift::from_gift_wrap(keys, event).await {
        Ok(unwrapped) => unwrapped,
        Err(_) => return Ok(None),
    };
    // Do not log unwrapped.rumor.content — token material lives there.
    match crate::payment_send::parse_nip17_payment_payload(&unwrapped.rumor.content, unwrapped.sender)
    {
        Ok(received) => Ok(Some(received)),
        Err(_) => Ok(None),
    }
}

/// Derive the cashu P2PK signing key from the packaged nostr secret hex.
///
/// Buyer locks to BIP-340 even-y (`02||xonly`) because nostr only carries the
/// 32-byte x-only coordinate. A raw secp secret may derive an odd-y (`03`)
/// compressed pubkey — that key cannot witness a `02`-locked P2PK proof
/// (Inputs:0). Normalize here: if the derived pubkey is odd-parity, negate the
/// scalar (`d' = n − d`) so the signing keypair is even-y and matches the lock.
#[cfg(feature = "wallet")]
pub fn cashu_secret_from_nostr_hex(secret_hex: &str) -> Result<cashu::SecretKey, SellerError> {
    use std::ops::Deref;

    let bytes = hex::decode(secret_hex.trim())
        .map_err(|error| SellerError::Payment(format!("secret hex decode: {error}")))?;
    let key = cashu::SecretKey::from_slice(&bytes)
        .map_err(|error| SellerError::Payment(format!("cashu secret key: {error}")))?;
    // Compressed form: 0x02 = even-y, 0x03 = odd-y (BIP-340 / BIP-340-compatible).
    if key.public_key().to_bytes()[0] == 0x03 {
        Ok(cashu::SecretKey::from((*key.deref()).negate()))
    } else {
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ParsedOffer;
    use crate::home::{self, MobeeConfig, SellerConfig};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn offer(amount: u64, seller: Option<&str>) -> ParsedOffer {
        ParsedOffer {
            task: "task".into(),
            output: "text/plain".into(),
            amount,
            unit: "sat".into(),
            deadline_unix: 2_000_000_000,
            seller_pubkey: seller.map(str::to_owned),
        }
    }

    // Z3 (fail-closed journal reads) — the whole read class funnels through `entries`: a MISSING
    // journal is the only "empty" case (first boot); a CORRUPT/unreadable journal PROPAGATES an
    // error so no read silently proceeds as if the journal were empty. Covers entries + the derived
    // reads (has_receipt / last_receipt_ts / oldest_unsettled_delivery_ts / pending_receive).
    #[test]
    fn journal_reads_fail_closed_on_corruption_and_treat_missing_as_empty() {
        let root = temp_home("z3-failclosed");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");

        // Missing journal ⇒ empty, not an error (legitimate first boot).
        let journal_path = home.root.join(JOURNAL_FILE);
        let _ = std::fs::remove_file(&journal_path);
        let journal = SellerJournal::open(&home).expect("open");
        // open() touches the file; remove it again to exercise the NotFound⇒empty path directly.
        let _ = std::fs::remove_file(&journal_path);
        assert_eq!(journal.entries().expect("missing⇒empty"), Vec::new());
        assert_eq!(journal.last_receipt_ts().expect("missing⇒None"), None);
        assert_eq!(
            journal.oldest_unsettled_delivery_ts().expect("missing⇒None"),
            None
        );
        assert!(!journal.has_receipt("job").expect("missing⇒false"));

        // Corrupt line ⇒ every read fails closed (Err), never Ok(empty)/Ok(None)/Ok(false).
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&journal_path)
                .expect("open journal");
            f.write_all(b"{not valid json\n").expect("corrupt");
        }
        assert!(journal.entries().is_err(), "corrupt entries must Err");
        assert!(journal.last_receipt_ts().is_err(), "corrupt last_receipt must Err");
        assert!(
            journal.oldest_unsettled_delivery_ts().is_err(),
            "corrupt oldest_unsettled must Err"
        );
        assert!(journal.has_receipt("job").is_err(), "corrupt has_receipt must Err");
        assert!(
            journal.pending_receive_for_token("hash").is_err(),
            "corrupt pending_receive must Err"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rate_gate_floor_allows_above_rate_refuses_below_and_untargeted() {
        let seller = "aa".repeat(32);
        rate_gate_allows(&offer(5, Some(&seller)), &seller, 3, false).expect("above floor");
        rate_gate_allows(&offer(3, Some(&seller)), &seller, 3, false).expect("equal floor");
        assert!(rate_gate_allows(&offer(2, Some(&seller)), &seller, 3, false).is_err());
        assert!(rate_gate_allows(&offer(9, None), &seller, 1, false).is_err());
        rate_gate_allows(&offer(9, None), &seller, 1, true).expect("open-pool opt-in");
        assert!(rate_gate_allows(&offer(0, None), &seller, 1, true).is_err());
    }

    #[test]
    fn agent_command_string_refused_at_config_parse() {
        let raw = r#"
relay_url = "wss://example.invalid"
accepted_mints = ["https://testnut.cashudevkit.org"]
per_job_budget_sats = 21
total_budget_sats = 100

[seller]
agent_command = "claude --print"
rate_sats = 1
git_remote = "https://example.invalid/repo.git"
"#;
        let err = toml::from_str::<MobeeConfig>(raw).expect_err("string argv must refuse");
        assert!(
            err.to_string().contains("argv array") || err.to_string().contains("agent_command"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn agent_command_argv_array_parses() {
        let raw = r#"
relay_url = "wss://example.invalid"
accepted_mints = ["https://testnut.cashudevkit.org"]
per_job_budget_sats = 21
total_budget_sats = 100

[seller]
agent_command = ["claude", "--print"]
rate_sats = 7
git_remote = "https://example.invalid/repo.git"
"#;
        let config: MobeeConfig = toml::from_str(raw).expect("parse");
        let seller = config.seller.expect("seller");
        assert_eq!(seller.agent_command, vec!["claude", "--print"]);
        assert_eq!(seller.rate_sats, 7);
        // offer_backfill_secs ABSENT ⇒ serde default of 1200s (20 min).
        assert_eq!(
            seller.offer_backfill_secs, 1200,
            "absent offer_backfill_secs must default to 1200"
        );
    }

    #[test]
    fn offer_backfill_secs_parses_custom_and_zero() {
        let base = |backfill: &str| {
            format!(
                r#"
relay_url = "wss://example.invalid"
accepted_mints = ["https://testnut.cashudevkit.org"]
per_job_budget_sats = 21
total_budget_sats = 100

[seller]
agent_command = ["claude", "--print"]
rate_sats = 7
git_remote = "https://example.invalid/repo.git"
offer_backfill_secs = {backfill}
"#
            )
        };
        // Custom window.
        let custom: MobeeConfig = toml::from_str(&base("300")).expect("parse custom");
        assert_eq!(custom.seller.expect("seller").offer_backfill_secs, 300);
        // Explicit 0 = live-only (must NOT fall back to the default).
        let zero: MobeeConfig = toml::from_str(&base("0")).expect("parse zero");
        assert_eq!(
            zero.seller.expect("seller").offer_backfill_secs,
            0,
            "explicit 0 must parse as live-only, not the default"
        );
    }

    #[test]
    fn journal_pay_once_and_bind_refuse_wrong_job() {
        let root = temp_home("journal");
        let _ = fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home::save_config(&mut home, |config| {
            config.seller = Some(SellerConfig {
                agent_command: vec!["echo".into()],
                rate_sats: 1,
                git_remote: "https://example.invalid/repo.git".into(),
                job_timeout_secs: None,
                agent: None,
                claim_open_pool: false,
                offer_backfill_secs: 0,
                contribution_enabled: true,
            });
        })
        .expect("save");

        let journal = SellerJournal::open(&home).expect("journal");
        journal
            .append_claim("job-a", "claim-a", "buyer-a", 2_000_000_000)
            .expect("claim");
        assert!(
            journal
                .append_claim("job-a", "claim-a", "buyer-a", 2_000_000_000)
                .is_err()
        );

        let refused = journal.append_receipt(
            "job-a",
            "result-a",
            "job-OTHER",
            "result-a",
            7,
            7,
            "https://testnut.cashudevkit.org",
            "buyer",
            true,
        );
        assert!(refused.is_err(), "wrong-job must refuse");

        journal
            .append_receipt(
                "job-a",
                "result-a",
                "job-a",
                "result-a",
                7,
                7,
                "https://testnut.cashudevkit.org",
                "buyer",
                true,
            )
            .expect("receipt");
        assert!(
            journal
                .append_receipt(
                    "job-a",
                    "result-a",
                    "job-a",
                    "result-a",
                    7,
                    7,
                    "https://testnut.cashudevkit.org",
                    "buyer",
                    true,
                )
                .is_err(),
            "pay-once"
        );

        let raw = fs::read_to_string(journal.path()).expect("read");
        // Mint host may contain "cashu" (testnut.cashudevkit.org); refuse token/payload fields.
        assert!(
            !raw.contains("cashuA") && !raw.contains("\"token\"") && !raw.contains("nsec"),
            "journal must never hold token/key material: {raw}"
        );
        assert!(raw.contains("amount_received"));
    }

    // #57: last_receipt_ts anchors the boot wrap-backfill `since`. None before any receipt (so
    // backfill fetches all history), Some(ts) after — never a silent stranding.
    #[test]
    fn last_receipt_ts_none_then_some_after_receipt() {
        let root = temp_home("last-receipt-ts");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let journal = SellerJournal::open(&home).expect("journal");
        assert_eq!(journal.last_receipt_ts().expect("ts"), None);
        // A claim (not a receipt) must not set the anchor.
        journal
            .append_claim("job-t", "claim-t", "buyer-t", 2_000_000_000)
            .expect("claim");
        assert_eq!(journal.last_receipt_ts().expect("ts"), None);
        let before = now_unix();
        journal
            .append_receipt(
                "job-t",
                "result-t",
                "job-t",
                "result-t",
                7,
                7,
                "https://testnut.cashudevkit.org",
                "buyer",
                true,
            )
            .expect("receipt");
        let ts = journal.last_receipt_ts().expect("ts").expect("some after receipt");
        assert!(ts >= before, "receipt ts {ts} must be >= {before}");
    }

    // Finding F: a receipt for a NEWER job must not let the wrap-backfill cursor skip an OLDER
    // delivered-but-unpaid job. `last_receipt_ts` advances to the newer receipt, but
    // `oldest_unsettled_delivery_ts` (the clamp for the cursor) still points at the older wrap.
    #[test]
    fn oldest_unsettled_delivery_ts_survives_newer_receipt() {
        let root = temp_home("oldest-unsettled");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let journal = SellerJournal::open(&home).expect("journal");

        let delivery = |job: &str, ts: u64| JournalEntry::Delivery {
            job_id: job.into(),
            result_id: format!("r-{job}"),
            amount: 5,
            unit: "sat".into(),
            buyer_pubkey: "buyer".into(),
            accepted_mints: Vec::new(),
            ts,
        };
        let receipt = |job: &str, ts: u64| JournalEntry::Receipt {
            job_id: job.into(),
            result_id: format!("r-{job}"),
            amount_received: 5,
            mint: "https://testnut.cashudevkit.org".into(),
            buyer: "buyer".into(),
            swap_ok: true,
            ts,
        };

        // Two deliveries: older (ts=100), newer (ts=200); both unpaid.
        journal.append(delivery("old", 100)).expect("old delivery");
        journal.append(delivery("new", 200)).expect("new delivery");
        // A receipt arrives for the NEWER job only.
        journal.append(receipt("new", 250)).expect("newer receipt");

        assert_eq!(journal.last_receipt_ts().expect("ts"), Some(250));
        assert_eq!(
            journal.oldest_unsettled_delivery_ts().expect("ts"),
            Some(100),
            "cursor clamp must still reach the older unpaid wrap"
        );

        // Settling the older job too leaves nothing unsettled.
        journal.append(receipt("old", 300)).expect("older receipt");
        assert_eq!(journal.oldest_unsettled_delivery_ts().expect("ts"), None);
        let _ = fs::remove_dir_all(&root);
    }

    // Finding K: an intent-to-receive breadcrumb is written BEFORE the mint swap so a crash after
    // the swap but before the Receipt is recoverable. The lookup finds an un-finalized pending
    // receive by token hash, ignores other tokens, and stops matching once the Receipt supersedes.
    #[test]
    fn pending_receive_breadcrumb_found_then_superseded_by_receipt() {
        let root = temp_home("pending-receive");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let journal = SellerJournal::open(&home).expect("journal");
        const MINT: &str = "https://testnut.cashudevkit.org";
        let token_hash = "a".repeat(64);

        // Before the swap: breadcrumb written; recovery lookup finds it with carried fields.
        journal
            .append_pending_receive("job-k", "result-k", &token_hash, "buyer-k", MINT, 7)
            .expect("pending");
        assert_eq!(
            journal
                .pending_receive_for_token(&token_hash)
                .expect("lookup")
                .expect("present"),
            (
                "job-k".to_string(),
                "result-k".to_string(),
                "buyer-k".to_string(),
                MINT.to_string(),
                7
            )
        );
        // A different token hash is never matched.
        assert!(journal
            .pending_receive_for_token(&"b".repeat(64))
            .expect("lookup")
            .is_none());

        // Once the receipt is journaled, the pending receive is superseded (finalized).
        journal
            .append_receipt("job-k", "result-k", "job-k", "result-k", 7, 7, MINT, "buyer-k", true)
            .expect("receipt");
        assert!(journal
            .pending_receive_for_token(&token_hash)
            .expect("lookup")
            .is_none());
        let _ = fs::remove_dir_all(&root);
    }

    // #57: a delivered-but-unpaid job (Claim + Delivery, no Receipt) is NOT an orphaned in-flight
    // claim (never released, even past deadline) AND is recoverable for awaiting_payment rebuild.
    #[test]
    fn delivered_unpaid_is_recoverable_and_not_orphaned() {
        let root = temp_home("delivered-unpaid");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let journal = SellerJournal::open(&home).expect("journal");
        journal
            .append_claim("job-d", "claim-d", "buyer-d", 10)
            .expect("claim");
        journal
            .append_delivery(
                "job-d",
                "result-d",
                15,
                "sat",
                "buyer-d",
                &["https://mint-orig.testnut.example".to_string()],
            )
            .expect("delivery");

        // Recoverable: one delivered-but-unpaid job with the money-critical fields, INCLUDING the
        // original advertised mints (Fix Q — restored job redeems against these, not current config).
        let unpaid = journal.deliveries_awaiting_receipt().expect("unpaid");
        assert_eq!(unpaid.len(), 1);
        assert_eq!(unpaid[0].job_id, "job-d");
        assert_eq!(unpaid[0].result_id, "result-d");
        assert_eq!(unpaid[0].amount, 15);
        assert_eq!(unpaid[0].buyer_pubkey, "buyer-d");
        assert_eq!(
            unpaid[0].accepted_mints,
            vec!["https://mint-orig.testnut.example".to_string()],
            "original advertised mints must round-trip through the delivery journal"
        );

        // Not orphaned even long past the claim deadline (delivery resolves the claim).
        assert!(plan_orphaned_claims(&journal.entries().unwrap(), 9_999_999_999).is_empty());

        // Once receipted, it is no longer awaiting payment.
        journal
            .append_receipt("job-d", "result-d", "job-d", "result-d", 15, 15, "m", "buyer-d", true)
            .expect("receipt");
        assert!(journal.deliveries_awaiting_receipt().expect("unpaid2").is_empty());
    }

    #[test]
    fn receipt_amount_must_equal_offer_amount_not_rate() {
        let root = temp_home("amount");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let journal = SellerJournal::open(&home).expect("journal");
        journal
            .append_claim("job-b", "claim-b", "buyer-b", 2_000_000_000)
            .expect("claim");
        // offer.amount=21, rate would have been 1 — journal expects offer.amount
        let err = journal
            .append_receipt(
                "job-b",
                "result-b",
                "job-b",
                "result-b",
                1,
                21,
                "https://testnut.cashudevkit.org",
                "buyer",
                true,
            )
            .expect_err("amount_received must == offer.amount");
        assert!(err.to_string().contains("offer.amount"));
    }

    #[test]
    fn plan_orphaned_claims_from_real_journal_marks_past_deadline_expired() {
        // REAL orphaned-claim fixture: a journaled in-flight claim + a PAST deadline,
        // written to a real journal file (no relay). Restart-reconcile core.
        let root = temp_home("reconcile-plan");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let journal = SellerJournal::open(&home).expect("journal");

        // Orphaned claim: deadline in the deep past, no receipt, no release.
        let past_deadline = 1_000_000_000u64; // 2001-09-09
        journal
            .append_claim("job-orphan", "claim-orphan", "buyer-orphan", past_deadline)
            .expect("append orphan claim");
        // A paid job (Claim + Receipt) must NOT show up as an orphan.
        journal
            .append_claim("job-paid", "claim-paid", "buyer-paid", past_deadline)
            .expect("append paid claim");
        journal
            .append_receipt(
                "job-paid",
                "result-paid",
                "job-paid",
                "result-paid",
                7,
                7,
                "https://testnut.cashudevkit.org",
                "buyer-paid",
                true,
            )
            .expect("receipt");

        let now = 2_000_000_000u64; // well past the deadline
        let plan = plan_orphaned_claims(&journal.entries().expect("entries"), now);
        assert_eq!(plan.len(), 1, "only the unpaid orphan is in-flight: {plan:?}");
        let orphan = &plan[0];
        assert_eq!(orphan.job_id, "job-orphan");
        assert_eq!(orphan.claim_id, "claim-orphan");
        assert_eq!(orphan.buyer_pubkey, "buyer-orphan");
        assert_eq!(
            orphan.liveness,
            ClaimLiveness::Expired,
            "now > deadline_unix ⇒ EXPIRED (derived, never stored)"
        );

        // Release makes it terminal — reconcile is idempotent across restarts.
        journal
            .append_release("job-orphan", "deadline exceeded before restart")
            .expect("release");
        let plan2 = plan_orphaned_claims(&journal.entries().expect("entries"), now);
        assert!(plan2.is_empty(), "released orphan is terminal: {plan2:?}");
        // Second release is a no-op (idempotent).
        journal.append_release("job-orphan", "again").expect("idempotent release");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_orphaned_claims_live_before_deadline_and_backcompat_old_line() {
        let root = temp_home("reconcile-live");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let journal = SellerJournal::open(&home).expect("journal");

        // Within-deadline orphan ⇒ Live (a fixed `now` before the deadline).
        journal
            .append_claim("job-live", "claim-live", "buyer-live", 2_000_000_000)
            .expect("claim");
        let plan = plan_orphaned_claims(&journal.entries().expect("entries"), 1_500_000_000);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].liveness, ClaimLiveness::Live);

        // An older claim line has no deadline_unix field. It must parse
        // (serde default = 0) and classify Expired for any now>0 (safe: old orphan released).
        let raw_old = r#"{"kind":"claim","job_id":"job-old","ts":1}"#;
        {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(journal.path())
                .expect("open");
            writeln!(file, "{raw_old}").expect("write old line");
        }
        let entries = journal.entries().expect("entries parse old line");
        let plan = plan_orphaned_claims(&entries, 1_500_000_000);
        let old = plan
            .iter()
            .find(|c| c.job_id == "job-old")
            .expect("old claim planned");
        assert_eq!(old.deadline_unix, 0, "missing field defaults to 0");
        assert_eq!(old.liveness, ClaimLiveness::Expired);

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn cashu_secret_from_nostr_hex_normalizes_odd_y_to_even() {
        // Find a raw secret whose uncompressed/compressed pubkey is odd-y (03…).
        let odd_raw = (1u8..=u8::MAX)
            .map(|byte| [byte; 32])
            .find(|bytes| {
                cashu::SecretKey::from_slice(bytes)
                    .map(|key| key.public_key().to_bytes()[0] == 0x03)
                    .unwrap_or(false)
            })
            .expect("an odd-parity secp secret exists");
        let raw_key = cashu::SecretKey::from_slice(&odd_raw).expect("parse");
        assert_eq!(raw_key.public_key().to_bytes()[0], 0x03, "precondition: odd-y");
        let xonly_before = raw_key.public_key().x_only_public_key().to_string();

        let normalized =
            cashu_secret_from_nostr_hex(&hex::encode(odd_raw)).expect("normalize");
        assert_eq!(
            normalized.public_key().to_bytes()[0],
            0x02,
            "must force even-y compressed pubkey"
        );
        assert_eq!(
            normalized.public_key().x_only_public_key().to_string(),
            xonly_before,
            "x-only coordinate must be unchanged by BIP-340 negation"
        );
        // Matches buyer lock shape: 02||xonly.
        assert_eq!(
            normalized.public_key().to_hex(),
            format!("02{xonly_before}")
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn cashu_secret_from_nostr_hex_leaves_even_y_unchanged() {
        let even_raw = (1u8..=u8::MAX)
            .map(|byte| [byte; 32])
            .find(|bytes| {
                cashu::SecretKey::from_slice(bytes)
                    .map(|key| key.public_key().to_bytes()[0] == 0x02)
                    .unwrap_or(false)
            })
            .expect("an even-parity secp secret exists");
        let raw_key = cashu::SecretKey::from_slice(&even_raw).expect("parse");
        let before = raw_key.public_key().to_hex();
        let normalized =
            cashu_secret_from_nostr_hex(&hex::encode(even_raw)).expect("normalize");
        assert_eq!(normalized.public_key().to_hex(), before);
        assert_eq!(normalized.public_key().to_bytes()[0], 0x02);
    }
}
