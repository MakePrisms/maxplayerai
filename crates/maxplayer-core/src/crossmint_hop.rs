//! Carrying out a cross-mint hop, and resuming one that was interrupted mid-flight.
//!
//! [`crate::crossmint`] decides *whether* to hop; this module performs one. The two legs are a NUT-05
//! melt at the buyer's funded mint, paying a NUT-04 mint quote raised at the seller's, after which the
//! buyer holds fresh ecash at the target and the ordinary send path takes over unchanged.
//!
//! Everything here answers one question: after a crash, did the buyer pay twice? cdk persists each leg
//! on its own — a melt quote's state is recoverable from a cold process by quote id, and a mint quote
//! that was paid but never issued can still be issued later — but nothing in cdk knows that the two
//! quotes are ONE hop. That pairing is what this module journals, before the melt, and it is what makes
//! the melt leg safe to re-enter: on a resumed attempt the persisted quote ids WIN over anything freshly
//! planned, because raising a second melt for one attempt id is exactly the double-pay the journal
//! exists to prevent.
//!
//! The resume decision is taken from what the MINTS say, never from what we infer:
//!
//! | melt at source | mint at target | action |
//! |---|---|---|
//! | `Unpaid`  | —          | nothing left the source; melt (the source mint's own answer, not a guess) |
//! | `Pending` | —          | money in flight; refuse, stay retryable, never melt again |
//! | `Failed`  | unpaid     | raise one bounded replacement against the same invoice; the plan-time authorization covers it |
//! | `Failed`  | paid       | the strand: issue the ecash at the target, and say so LOUDLY |
//! | `Failed`  | issued     | both legs already landed; complete without touching either mint |
//! | `Failed`  | pending / unknown | ambiguous target truth; refuse without another melt; the record stays live |
//! | `Paid`    | not issued | the strand: issue the ecash at the target, and say so LOUDLY |
//! | `Paid`    | issued     | both legs already landed; complete without touching either mint |
//!
//! The strand row is the one that must never pass in silence. A buyer whose sats left the source mint
//! but whose ecash never appeared at the target has money that is neither spent nor held, and the only
//! thing standing between that and a silent loss is an operator who can see it.

use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cdk::nuts::{MeltQuoteState, MintQuoteState, PaymentMethod};
use cdk::wallet::Wallet;

use crate::buyer_fund;
use crate::crossmint::{HopCost, HopJournal};
use crate::home::MaxplayerHome;
use crate::payment_wallet::{MINT_TOUCH_TIMEOUT, is_mint_unreachable};

/// What the source mint says about the melt leg.
///
/// A narrowing of cdk's melt quote state to the four answers the hop acts on. A state that is not a
/// statement about where the money is — cdk's `Unknown` — maps to [`Self::Pending`], the row that
/// refuses rather than re-melting, because "I don't know" and "it might be in flight" call for the
/// same caution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeltLeg {
    /// The source mint has not paid the invoice. Nothing has left the wallet.
    Unpaid,
    /// The source mint is paying, or will not say. Money may be in flight.
    Pending,
    /// The source mint paid the invoice. The sats have left.
    Paid,
    /// The source mint tried and failed. Nothing left the wallet and nothing will through this
    /// quote.
    Failed,
}

/// What the target mint says about the mint leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MintLeg {
    /// The invoice raised at the target has not been paid yet.
    Unpaid,
    /// The invoice is paid but the ecash has not been issued to the buyer.
    Paid,
    /// The ecash has been issued; the buyer holds it.
    Issued,
    /// The target mint is paying, or will not say. Completing or superseding would act on an
    /// invoice whose truth we do not hold, so recovery refuses and the record stays live.
    Pending,
}

/// Both legs of one hop, as the two mints report them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopSettled {
    /// Ecash issued at the target mint. Equals the pinned delivery amount, or the hop refuses.
    pub minted_sats: u64,
    /// Whether this run found the melt already paid and the ecash not yet issued — a hop that an
    /// earlier run left stranded and this one recovered.
    pub recovered_strand: bool,
    /// Unused Lightning fee reserve to credit back to the budget: the reserved fee ceiling minus the
    /// fee ACTUALLY paid by the melt this run (MakePrisms/maxplayerai#186). Zero when this run did not
    /// perform the melt itself — a melt that landed on an earlier run cannot be reconciled here, so
    /// the spend stays at the safe over-counted value rather than crediting a fee we never observed.
    pub unused_fee_reserve_sats: u64,
}

/// A hop failure. Every variant refuses fail-closed; none of them can be reached with the seller's
/// delivered amount already reduced, because the delivery amount is pinned before any of this runs.
#[derive(Debug)]
pub enum HopError {
    /// The journal could not be read, or could not be durably appended. Fail-closed on purpose:
    /// without a durable pairing there is no way to tell a crashed hop from a fresh one, so no melt
    /// may fire.
    Journal(String),
    /// A mint refused, or could not be reached, while the hop was under way.
    Mint(String),
    /// A mint returned no response, timed out, or answered with a 5xx server error.
    MintUnreachable { label: String, detail: String },
    /// Two different pairings claim one attempt id. Refusing is the whole point: acting on either
    /// pairing risks a second melt against an attempt that already has one.
    PairingConflict {
        /// The attempt both pairings claim.
        attempt_id: String,
        /// The pairing already on disk.
        persisted: Box<HopJournal>,
        /// The pairing this run arrived with.
        planned: Box<HopJournal>,
    },
    /// A process tried to append a second pairing while the first pairing was still active.
    DuplicatePlanned {
        /// The attempt both pairings claim.
        attempt_id: String,
        /// Melt quote belonging to the active pairing already on disk.
        active_melt_quote_id: String,
    },
    /// The melt is settling at the source mint. Money is in flight, so this attempt must not melt
    /// again; it refuses and stays retryable once the source mint reaches a terminal answer.
    MeltInFlight {
        /// The attempt whose melt is settling.
        attempt_id: String,
        /// Melt quote to re-ask about on the next run.
        melt_quote_id: String,
    },
    /// The source mint reports the melt as failed: the Lightning payment did not go through and the
    /// proofs are back in the wallet. Kept as a typed refusal for callers that still match it;
    /// `run_hop` recovers a failed melt by superseding against the same unpaid invoice rather than
    /// returning this.
    MeltFailed {
        /// The attempt whose melt failed.
        attempt_id: String,
        /// Melt quote the source mint reports as failed.
        melt_quote_id: String,
    },
    /// The source mint has failed and this attempt has already consumed its bounded number of
    /// superseding melt quotes. The target invoice remains payable, so only an operator may decide
    /// what happens next.
    MeltSupersessionExhausted {
        /// The attempt whose recovery ceiling was reached.
        attempt_id: String,
        /// Number of superseding melt quotes already journalled.
        attempts: usize,
    },
    /// The target invoice is pending or unknown. Completing would mint against an invoice whose
    /// truth we do not hold; superseding would raise a second melt against that same ambiguity.
    /// The record stays live so the next sweep can retry.
    AmbiguousTarget {
        /// The attempt left retryable.
        attempt_id: String,
        /// Target mint quote whose state is not terminal.
        mint_quote_id: String,
    },
    /// A replacement melt quote costs more than the hop record's plan-time authorization. The
    /// record is the payment: its original charge covers every retry, and a dearer quote is refused
    /// rather than growing exposure.
    ReplacementCostExceedsAuthorization {
        /// The attempt whose replacement was refused.
        attempt_id: String,
        /// Cost authorized when the pairing was planned.
        authorized_cost: u64,
        /// Total cost of the replacement quote.
        replacement_cost: u64,
    },
    /// The source wallet cannot fund the hop. Raised while planning, so the cap is never charged
    /// for a melt that could not have run.
    InsufficientSource {
        /// The buyer's funded mint.
        mint: String,
        /// What the buyer holds there.
        balance: u64,
        /// What the hop would cost.
        planned_cost: u64,
    },
    /// The target mint issued an amount other than the pinned delivery amount. Refused rather than
    /// carried forward: the send that follows must hand the seller exactly the offer amount.
    MintedAmountMismatch {
        /// Amount pinned by the buyer-signed offer.
        expected: u64,
        /// Amount the target mint actually issued.
        minted: u64,
    },
    /// The target mint (seller-chosen) returned a bolt11 whose amount is not the pinned delivery
    /// amount, no amount at all, or one that does not decode (REPORT1110 F1). Refused while
    /// planning, before any melt quote is raised, so the buyer never pays an invoice larger than
    /// the offer.
    InvoiceAmountMismatch {
        /// Target mint that raised the invoice.
        mint: String,
        /// Amount pinned by the buyer-signed offer.
        expected_sats: u64,
        /// Invoice amount in msat, `None` for an amountless or undecodable invoice.
        invoice_msat: Option<u64>,
        /// The string did not decode as a bolt11 (as opposed to decoding with no amount).
        undecodable: bool,
    },
    /// The target invoice has expired, or would within one money-moving leg
    /// ([`MIN_INVOICE_LIFETIME`]). Checked at plan and again before a replacement melt quote is
    /// raised: such an invoice cannot be paid, so quoting it only strands a quote.
    InvoiceExpired {
        /// Target mint that raised the invoice.
        mint: String,
        /// Lifetime it had left, by the buyer's clock (0 if expired or unreadable).
        remaining_secs: u64,
    },
    /// The source mint priced the hop invoice at an amount other than the pinned delivery.
    SourceQuoteMismatch {
        /// Source mint that raised the melt quote.
        mint: String,
        /// Amount pinned by the buyer-signed offer.
        expected_sats: u64,
        /// Amount the source melt quote would melt.
        quoted_sats: u64,
    },
    /// The invoice a melt is about to pay is not the one the hop planned: wrong amount, a different
    /// invoice string, or undecodable (REPORT1110 F1, review r1). Checked before EVERY melt,
    /// including a recovered or superseding one and a pairing journalled before the pin existed.
    UnpinnedInvoice {
        /// Attempt the refused melt belongs to.
        attempt_id: String,
        /// Amount pinned by the buyer-signed offer.
        expected_sats: u64,
        /// Amount of the invoice about to be paid in msat, `None` if amountless or undecodable.
        invoice_msat: Option<u64>,
        /// The string did not decode as a bolt11.
        undecodable: bool,
        /// Whether it is the planned invoice (digest match); `None` when the plan predates the pin.
        invoice_matches: Option<bool>,
    },
    /// The melt cdk prepared is not the one the hop authorized: wrong source mint, or it would take
    /// more than the planned cost (#1292 review L1). Checked between `prepare_melt` and `confirm`;
    /// the prepared melt is cancelled, so no melt request reaches the mint.
    PreparedMeltRefused {
        /// Attempt the refused melt belongs to.
        attempt_id: String,
        /// What did not match.
        reason: String,
    },
    /// This attempt's hop was refused on an earlier run with nothing melted, and is not retried
    /// (#1292 review L2). Every pin refusal is deterministic, so a retry could only refuse again.
    HopRefused {
        /// Attempt that was refused.
        attempt_id: String,
        /// The refusal recorded when it happened.
        reason: String,
    },
    /// The seller's target mint answered a new quote request with a quote id the buyer's wallet
    /// already holds (#1292 fable r3 L4). cdk keys rows on id alone, so the earlier row has been
    /// overwritten; it is left in place (not deleted) and the hop refuses.
    TargetQuoteIdReused {
        /// Target mint that reused the id.
        mint: String,
        /// The reused quote id.
        quote_id: String,
    },
}

impl fmt::Display for HopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Journal(detail) => write!(formatter, "cross-mint hop journal: {detail}"),
            Self::Mint(detail) => write!(formatter, "cross-mint hop: {detail}"),
            Self::MintUnreachable { label, detail } => write!(
                formatter,
                "cross-mint hop: {label} mint unreachable or erroring ({detail})"
            ),
            Self::PairingConflict {
                attempt_id,
                persisted,
                planned,
            } => write!(
                formatter,
                "cross-mint hop: attempt {attempt_id} already journals melt quote {} at {} paired \
                 with mint quote {} at {}, but this run arrived with melt quote {} at {} paired \
                 with mint quote {} at {}; refusing rather than risking a second melt",
                persisted.melt_quote_id,
                persisted.source_mint,
                persisted.mint_quote_id,
                persisted.target_mint,
                planned.melt_quote_id,
                planned.source_mint,
                planned.mint_quote_id,
                planned.target_mint,
            ),
            Self::DuplicatePlanned {
                attempt_id,
                active_melt_quote_id,
            } => write!(
                formatter,
                "cross-mint hop journal: attempt {attempt_id} already has active Planned melt quote \
                 {active_melt_quote_id}; refusing a duplicate Planned record"
            ),
            Self::MeltInFlight {
                attempt_id,
                melt_quote_id,
            } => write!(
                formatter,
                "cross-mint hop: melt quote {melt_quote_id} for attempt {attempt_id} is still \
                 settling at the source mint; refusing to melt again while the payment is in \
                 flight (retry once the mint reports paid or unpaid)"
            ),
            Self::MeltFailed {
                attempt_id,
                melt_quote_id,
            } => write!(
                formatter,
                "cross-mint hop: the source mint reports melt quote {melt_quote_id} for attempt \
                 {attempt_id} as failed; no sats left the wallet, but this attempt cannot reach \
                 the seller's mint through a failed melt quote; see MakePrisms/maxplayerai#194 for \
                 the recovery path"
            ),
            Self::MeltSupersessionExhausted {
                attempt_id,
                attempts,
            } => write!(
                formatter,
                "cross-mint hop: attempt {attempt_id} exhausted its {attempts} superseding melt \
                 quotes while the target invoice remained unpaid; operator attention required"
            ),
            Self::AmbiguousTarget {
                attempt_id,
                mint_quote_id,
            } => write!(
                formatter,
                "cross-mint hop: refusing an ambiguous supersession of attempt {attempt_id}: \
                 target mint quote {mint_quote_id} is pending or unknown; the record stays live"
            ),
            Self::ReplacementCostExceedsAuthorization {
                attempt_id,
                authorized_cost,
                replacement_cost,
            } => write!(
                formatter,
                "cross-mint hop: replacement melt for attempt {attempt_id} costs {replacement_cost} \
                 sats, exceeding the record's authorized cost {authorized_cost}; refusing the \
                 supersession"
            ),
            Self::InsufficientSource {
                mint,
                balance,
                planned_cost,
            } => write!(
                formatter,
                "cross-mint hop: the buyer holds {balance} sats at {mint} but the hop costs \
                 {planned_cost} sats (delivery plus the source mint's fee reserve and input fee); \
                 refusing before any budget is charged"
            ),
            Self::MintedAmountMismatch { expected, minted } => write!(
                formatter,
                "cross-mint hop: target mint issued {minted} sats but the buyer-signed offer pins \
                 delivery at {expected} sats"
            ),
            Self::InvoiceAmountMismatch {
                mint,
                expected_sats,
                invoice_msat,
                undecodable,
            } => write!(
                formatter,
                "cross-mint hop: target mint {mint} returned {} but the buyer-signed offer pins \
                 delivery at {expected_sats} sats; refusing the hop with zero spend",
                describe_invoice(*invoice_msat, *undecodable)
            ),
            Self::InvoiceExpired {
                mint,
                remaining_secs,
            } => write!(
                formatter,
                "cross-mint hop: the invoice from target mint {mint} has {remaining_secs}s of \
                 lifetime left by this machine's clock, under the {MIN_INVOICE_LIFETIME:?} a payment \
                 needs; refusing before it is paid or re-quoted"
            ),
            Self::SourceQuoteMismatch {
                mint,
                expected_sats,
                quoted_sats,
            } => write!(
                formatter,
                "cross-mint hop: source mint {mint} priced the hop invoice at {quoted_sats} sats, not \
                 the pinned delivery of {expected_sats} sats; refusing before the melt"
            ),
            Self::UnpinnedInvoice {
                attempt_id,
                expected_sats,
                invoice_msat,
                undecodable,
                invoice_matches,
            } => write!(
                formatter,
                "cross-mint hop: attempt {attempt_id} would pay {} {} but the plan pins \
                 {expected_sats} sats to one invoice; refusing before the melt",
                describe_invoice(*invoice_msat, *undecodable),
                match invoice_matches {
                    Some(true) => "(the planned invoice)",
                    Some(false) => "(not the planned invoice)",
                    None => "(the plan predates the invoice pin)",
                }
            ),
            Self::PreparedMeltRefused { attempt_id, reason } => write!(
                formatter,
                "cross-mint hop: attempt {attempt_id}: {reason}; the prepared melt was cancelled \
                 before it reached the mint"
            ),
            Self::HopRefused { attempt_id, reason } => write!(
                formatter,
                "cross-mint hop: attempt {attempt_id} was refused on an earlier run with nothing \
                 melted and is not retried ({reason})"
            ),
            Self::TargetQuoteIdReused { mint, quote_id } => write!(
                formatter,
                "cross-mint hop: target mint {mint} returned quote id {quote_id}, which this wallet \
                 already held; refusing the hop and leaving that row in place"
            ),
        }
    }
}

/// How a refusal names the invoice it refused.
fn describe_invoice(invoice_msat: Option<u64>, undecodable: bool) -> String {
    match (undecodable, invoice_msat) {
        (true, _) => "an undecodable invoice".to_owned(),
        (false, None) => "an invoice with no amount".to_owned(),
        (false, Some(msat)) => format!("an invoice for {msat} msat"),
    }
}

impl std::error::Error for HopError {}

impl HopError {
    /// A refusal of the pinned invoice or melt that fires before anything is melted and will repeat
    /// on every run of the attempt (#1292 review L2).
    fn is_pin_refusal(&self) -> bool {
        matches!(
            self,
            Self::UnpinnedInvoice { .. }
                | Self::SourceQuoteMismatch { .. }
                | Self::InvoiceExpired { .. }
                | Self::PreparedMeltRefused { .. }
                | Self::HopRefused { .. }
        )
    }
}

/// The two effects a hop has on the world, plus the two questions it asks before applying them.
///
/// A trait rather than direct wallet calls so the crash windows can be toothed hermetically: a fake
/// that pays the melt and then dies before the mint reproduces the exact strand the journal exists to
/// survive, which no amount of testing against a live mint pair could produce on demand.
pub(crate) trait HopEffects {
    /// Ask the SOURCE mint what became of the melt. Asking is also cdk's recovery trigger — a melt
    /// interrupted mid-saga resumes on this call rather than needing a separate sweep.
    fn melt_leg(&mut self, melt_quote_id: &str) -> Result<MeltLeg, HopError>;

    /// Ask the TARGET mint whether the ecash has been issued.
    fn mint_leg(&mut self, mint_quote_id: &str) -> Result<MintLeg, HopError>;

    /// Ask the target mint for a mint quote's state and exact invoice in one response.
    fn target_invoice(&mut self, mint_quote_id: &str) -> Result<(MintLeg, String), HopError>;

    /// The bolt11 a source melt quote will pay and the amount it will melt, as the buyer's wallet
    /// stored them when the quote was raised. Read before every melt so the hop pays only the
    /// invoice it planned (REPORT1110 F1).
    fn melt_invoice(&mut self, melt_quote_id: &str) -> Result<(String, u64), HopError>;

    /// Raise, but do not pay, a fresh source-mint melt quote against `bolt11`.
    fn supersede_melt_quote(&mut self, bolt11: &str) -> Result<(String, u64), HopError>;

    /// Refuse before any journal write if the source no longer covers a replacement quote.
    fn require_source_covers(&mut self, planned_cost: u64) -> Result<(), HopError>;

    /// Melt at the source mint, paying the invoice the target raised. Returns the ACTUAL Lightning
    /// fee paid (`Melted::fee_paid`), so the caller can reconcile it against the reserved fee and
    /// credit the unused reserve back to the budget (MakePrisms/maxplayerai#186).
    ///
    /// `pin` is the hop's pairing: the effect re-checks the prepared melt against it immediately
    /// before `confirm`, so the check and the money movement read the same quote (#1292 review L1).
    fn melt(&mut self, melt_quote_id: &str, pin: &HopJournal) -> Result<u64, HopError>;

    /// Issue the ecash at the target mint. Returns what was actually issued, which the caller checks
    /// against the pinned delivery amount rather than trusting.
    fn mint(&mut self, mint_quote_id: &str, expected_sats: u64) -> Result<u64, HopError>;
}

/// One line of the hop journal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum HopRecord {
    /// The pairing, written and synced BEFORE the melt.
    Planned(HopJournal),
    /// A replacement source-mint quote for the same target invoice, written before it is melted.
    /// A state transition on the existing payment record — not a new payment and not a new
    /// authorization.
    MeltSuperseded {
        /// Attempt this supersession belongs to.
        attempt_id: String,
        /// Quote that was active and confirmed failed.
        superseded_melt_quote_id: String,
        /// Fresh quote that becomes active.
        melt_quote_id: String,
        /// Fresh quote's melt amount, fee reserve, and input fee. Compared to the Planned
        /// record's authorized cost; never charged separately.
        planned_cost: u64,
    },
    /// Both legs landed and the buyer holds the ecash at the target.
    Settled {
        /// Attempt this hop funded.
        attempt_id: String,
        /// Ecash issued at the target mint.
        minted_sats: u64,
    },
    /// A pin guard refused the hop before anything was melted (#1292 review L2). Terminal: later
    /// runs and the recovery sweep do not retry it, and no money left the source for it.
    Refused {
        /// Attempt that was refused.
        attempt_id: String,
        /// The refusal, as displayed.
        reason: String,
    },
}

/// Durable record of which two quotes form one hop.
pub(crate) trait HopJournalStore {
    /// Every record already written for one attempt, oldest first.
    fn replay(&self, attempt_id: &str) -> Result<Vec<HopRecord>, HopError>;

    /// Append one record and durably sync it before the caller applies any effect.
    fn append_sync(&self, record: &HopRecord) -> Result<(), HopError>;
}

/// Append-only JSONL hop journal, one file per attempt id under a single directory.
///
/// Shares the payment journal's durability discipline — exclusive file lock, `sync_all` on the file
/// and on its parent directory — because the guarantee is the same one: a record the caller acted on
/// must still be there after the power goes out.
#[derive(Clone, Debug)]
pub struct FsHopJournal {
    dir: PathBuf,
}

impl FsHopJournal {
    /// A journal rooted at `dir`. The directory is created on first append.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path_for(&self, attempt_id: &str) -> PathBuf {
        self.dir.join(format!("{attempt_id}.jsonl"))
    }

    /// Every attempt this journal holds records for. An absent directory means no hop has ever run
    /// here, which is not an error.
    pub fn attempt_ids(&self) -> Result<Vec<String>, HopError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(journal_error("read dir", error)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|error| journal_error("read dir entry", error))?
                .path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                ids.push(stem.to_owned());
            }
        }
        // Deterministic order so a sweep's output reads the same way twice.
        ids.sort();
        Ok(ids)
    }
}

fn journal_error(context: &str, error: impl fmt::Display) -> HopError {
    HopError::Journal(format!("{context}: {error}"))
}

fn sync_parent_directory(path: &Path) -> Result<(), HopError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| journal_error("parent directory sync", error))
}

fn decode_records(attempt_id: &str, bytes: &[u8]) -> Result<Vec<HopRecord>, HopError> {
    // A record without its commit newline is a torn write. Refuse it rather than parse around
    // it: a half-written pairing is exactly the state where a wrong answer melts twice.
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(HopError::Journal(format!(
            "attempt {attempt_id}: last record is missing its commit newline (torn write)"
        )));
    }
    let mut records = Vec::new();
    for (index, line) in BufReader::new(bytes).lines().enumerate() {
        let line = line.map_err(|error| journal_error("read line", error))?;
        let record = serde_json::from_str::<HopRecord>(&line).map_err(|error| {
            HopError::Journal(format!(
                "attempt {attempt_id} line {}: {error}",
                index.saturating_add(1)
            ))
        })?;
        records.push(record);
    }
    Ok(records)
}

fn require_current_superseded_melt(
    records: &[HopRecord],
    attempt_id: &str,
    superseded_melt_quote_id: &str,
) -> Result<(), HopError> {
    if settled_of(records).is_some() {
        return Err(HopError::Journal(format!(
            "attempt {attempt_id}: refusing a supersession after settlement"
        )));
    }
    if refused_of(records).is_some() {
        return Err(HopError::Journal(format!(
            "attempt {attempt_id}: refusing a supersession after a terminal refusal"
        )));
    }
    match active_melt(records) {
        Some((active, _)) if active == superseded_melt_quote_id => Ok(()),
        Some((active, _)) => Err(HopError::Journal(format!(
            "attempt {attempt_id}: supersession expected active melt quote \
             {superseded_melt_quote_id}, but the journal tail is {active}; refusing a concurrent \
             replacement"
        ))),
        None => Err(HopError::Journal(format!(
            "attempt {attempt_id}: supersession has no Planned record"
        ))),
    }
}

fn require_no_active_planned(records: &[HopRecord], attempt_id: &str) -> Result<(), HopError> {
    if let Some(HopRecord::Planned(journal)) = records.last() {
        return Err(HopError::DuplicatePlanned {
            attempt_id: attempt_id.to_owned(),
            active_melt_quote_id: journal.melt_quote_id.clone(),
        });
    }
    Ok(())
}

impl HopJournalStore for FsHopJournal {
    fn replay(&self, attempt_id: &str) -> Result<Vec<HopRecord>, HopError> {
        let path = self.path_for(attempt_id);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(journal_error("open", error)),
        };
        file.lock().map_err(|error| journal_error("lock", error))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| journal_error("seek", error))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| journal_error("read", error))?;
        decode_records(attempt_id, &bytes)
    }

    fn append_sync(&self, record: &HopRecord) -> Result<(), HopError> {
        let attempt_id = match record {
            HopRecord::Planned(journal) => journal.attempt_id.as_str(),
            HopRecord::MeltSuperseded { attempt_id, .. } => attempt_id.as_str(),
            HopRecord::Settled { attempt_id, .. } | HopRecord::Refused { attempt_id, .. } => {
                attempt_id.as_str()
            }
        };
        std::fs::create_dir_all(&self.dir).map_err(|error| journal_error("create dir", error))?;
        let path = self.path_for(attempt_id);
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|error| journal_error("open for append", error))?;
        file.lock().map_err(|error| journal_error("lock", error))?;
        if matches!(
            record,
            HopRecord::Planned(_) | HopRecord::MeltSuperseded { .. }
        ) {
            file.seek(SeekFrom::Start(0))
                .map_err(|error| journal_error("seek", error))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| journal_error("read", error))?;
            let records = decode_records(attempt_id, &bytes)?;
            match record {
                HopRecord::Planned(_) => require_no_active_planned(&records, attempt_id)?,
                HopRecord::MeltSuperseded {
                    superseded_melt_quote_id,
                    ..
                } => {
                    require_current_superseded_melt(
                        &records,
                        attempt_id,
                        superseded_melt_quote_id,
                    )?;
                }
                HopRecord::Settled { .. } | HopRecord::Refused { .. } => unreachable!(),
            }
        }
        let mut line =
            serde_json::to_vec(record).map_err(|error| journal_error("encode", error))?;
        line.push(b'\n');
        file.write_all(&line)
            .map_err(|error| journal_error("append", error))?;
        file.sync_all()
            .map_err(|error| journal_error("sync", error))?;
        sync_parent_directory(&path)
    }
}

/// The pairing already on disk for an attempt, if any.
fn planned_of(records: &[HopRecord]) -> Option<&HopJournal> {
    records.iter().find_map(|record| match record {
        HopRecord::Planned(journal) => Some(journal),
        HopRecord::MeltSuperseded { .. } | HopRecord::Settled { .. } | HopRecord::Refused { .. } => None,
    })
}

/// The pairing already journalled for an attempt, if this attempt has planned a hop before.
///
/// Read BEFORE raising fresh quotes: an attempt that already has a pairing must reuse it, and quotes
/// raised only to be discarded are quotes the mints have to expire.
pub(crate) fn journalled_pairing<S: HopJournalStore>(
    store: &S,
    attempt_id: &str,
) -> Result<Option<HopJournal>, HopError> {
    Ok(planned_of(&store.replay(attempt_id)?).cloned())
}

/// The completion record for an attempt, if the hop already finished.
fn settled_of(records: &[HopRecord]) -> Option<u64> {
    records.iter().find_map(|record| match record {
        HopRecord::Settled { minted_sats, .. } => Some(*minted_sats),
        HopRecord::Planned(_) | HopRecord::MeltSuperseded { .. } | HopRecord::Refused { .. } => None,
    })
}

/// The terminal refusal recorded for an attempt, if a pin guard refused it (#1292 review L2).
fn refused_of(records: &[HopRecord]) -> Option<&str> {
    records.iter().find_map(|record| match record {
        HopRecord::Refused { reason, .. } => Some(reason.as_str()),
        _ => None,
    })
}

/// The single currently-active melt quote and the number of quotes that superseded the original.
fn active_melt(records: &[HopRecord]) -> Option<(String, usize)> {
    let mut current = None;
    let mut supersessions = 0usize;
    for record in records {
        match record {
            HopRecord::Planned(journal) => current = Some(journal.melt_quote_id.clone()),
            HopRecord::MeltSuperseded { melt_quote_id, .. } => {
                current = Some(melt_quote_id.clone());
                supersessions += 1;
            }
            HopRecord::Settled { .. } | HopRecord::Refused { .. } => {}
        }
    }
    current.map(|id| (id, supersessions))
}

/// The operator-visible line for a hop whose sats left the source but whose ecash never arrived.
///
/// Written to stderr on its own line and prefixed with a fixed, greppable marker: the point is that
/// somebody watching the buyer's output sees it without having to know what a mint quote is.
fn strand_line(journal: &HopJournal, active_melt_quote_id: &str) -> String {
    format!(
        "CROSSMINT STRAND attempt={} melted at {} (melt quote {}) but the ecash at {} was never \
         issued (mint quote {}); {} sats are paid and unissued — completing the mint leg now",
        journal.attempt_id,
        journal.source_mint,
        active_melt_quote_id,
        journal.target_mint,
        journal.mint_quote_id,
        journal.delivered_sats,
    )
}

fn melt_exhausted_line(journal: &HopJournal, melt_quote_id: &str, attempts: usize) -> String {
    format!(
        "CROSSMINT MELT EXHAUSTED attempt={} melt quote {} failed after {} supersessions against \
         still-unpaid target mint quote {}; target invoice remains payable, needs an operator",
        journal.attempt_id, melt_quote_id, attempts, journal.mint_quote_id
    )
}

/// Perform the hop described by `journal`, resuming instead of repeating whatever already happened.
///
/// `journal` is the freshly planned pairing. If a pairing is already on disk for this attempt it WINS:
/// the fresh quote ids are discarded, because a second melt quote for one attempt id is the double-pay
/// this journal exists to prevent. A pairing that disagrees with the persisted one is refused outright
/// rather than reconciled.
///
/// Ordering is write-before-effect throughout: the pairing is durable before the melt, and the
/// completion record is durable before the caller is told the ecash is in hand. A superseding melt
/// is a state transition on that same record: the plan-time authorization covers it, so a replacement
/// whose cost is equal or lower proceeds with no budget interaction, and a dearer quote is refused.
pub(crate) fn run_hop<S: HopJournalStore, E: HopEffects>(
    store: &S,
    effects: &mut E,
    journal: &HopJournal,
) -> Result<HopSettled, HopError> {
    let records = store.replay(&journal.attempt_id)?;
    if let Some(minted_sats) = settled_of(&records) {
        // Already complete. Neither mint is touched — this is the pays-once row. No melt ran this
        // run, so there is no actual fee to reconcile: the credit is 0 (spend stays over-counted).
        return Ok(HopSettled {
            minted_sats,
            recovered_strand: false,
            unused_fee_reserve_sats: 0,
        });
    }
    if let Some(reason) = refused_of(&records) {
        // Refused earlier with nothing melted: neither mint is touched again (#1292 review L2).
        return Err(HopError::HopRefused {
            attempt_id: journal.attempt_id.clone(),
            reason: reason.to_owned(),
        });
    }

    let pairing = match planned_of(&records) {
        None => {
            store.append_sync(&HopRecord::Planned(journal.clone()))?;
            journal.clone()
        }
        Some(persisted) if persisted == journal => persisted.clone(),
        Some(persisted) => {
            return Err(HopError::PairingConflict {
                attempt_id: journal.attempt_id.clone(),
                persisted: Box::new(persisted.clone()),
                planned: Box::new(journal.clone()),
            });
        }
    };
    let records = store.replay(&journal.attempt_id)?;

    // Melt leg. The source mint's answer decides; we never infer from our own records whether money
    // moved, because the record was written before the melt precisely so it could not know.
    let (mut active_melt_quote_id, mut supersessions) = active_melt(&records).ok_or_else(|| {
        HopError::Journal(format!(
            "attempt {}: Planned record was established but active melt is missing",
            pairing.attempt_id
        ))
    })?;
    let mut melted_this_run = false;
    // Actual Lightning fee paid by the melt THIS run, set by whichever `melt` call fires. Stays
    // `None` when no melt runs this pass (a melt that landed earlier), which keeps the reconciliation
    // a no-op on the recovery path (#186).
    let mut actual_fee_this_run: Option<u64> = None;
    let melted_earlier = loop {
        break match effects.melt_leg(&active_melt_quote_id)? {
            MeltLeg::Unpaid => {
                // Also the gate for a pairing journalled before the plan-time check: it is reused
                // as-is (authorize_pay), so this is the first place its invoice is ever checked.
                refuse_terminally(
                    store,
                    &pairing,
                    require_melt_pinned(effects, &pairing, &active_melt_quote_id),
                )?;
                actual_fee_this_run = Some(refuse_terminally(
                    store,
                    &pairing,
                    effects.melt(&active_melt_quote_id, &pairing),
                )?);
                false
            }
            MeltLeg::Pending => {
                report_overpaid(effects, &pairing, &active_melt_quote_id);
                return Err(HopError::MeltInFlight {
                    attempt_id: pairing.attempt_id.clone(),
                    melt_quote_id: active_melt_quote_id.clone(),
                });
            }
            MeltLeg::Failed => {
                let (invoice_state, bolt11) = effects.target_invoice(&pairing.mint_quote_id)?;
                match invoice_state {
                    MintLeg::Paid | MintLeg::Issued => break true,
                    MintLeg::Pending => {
                        return Err(HopError::AmbiguousTarget {
                            attempt_id: pairing.attempt_id.clone(),
                            mint_quote_id: pairing.mint_quote_id.clone(),
                        });
                    }
                    MintLeg::Unpaid => {}
                }
                // Never raise a replacement quote against an invoice the plan did not pin, or one
                // too close to expiry to be paid.
                refuse_terminally(store, &pairing, require_requotable(&pairing, &bolt11))?;
                if supersessions >= MAX_MELT_SUPERSESSIONS {
                    eprintln!(
                        "{}",
                        melt_exhausted_line(&pairing, &active_melt_quote_id, supersessions)
                    );
                    return Err(HopError::MeltSupersessionExhausted {
                        attempt_id: pairing.attempt_id.clone(),
                        attempts: supersessions,
                    });
                }
                let (new_melt_quote_id, planned_cost) = effects.supersede_melt_quote(&bolt11)?;
                effects.require_source_covers(planned_cost)?;
                if planned_cost > pairing.planned_cost {
                    return Err(HopError::ReplacementCostExceedsAuthorization {
                        attempt_id: pairing.attempt_id.clone(),
                        authorized_cost: pairing.planned_cost,
                        replacement_cost: planned_cost,
                    });
                }
                // The replacement quote must melt exactly the delivery, against the same invoice:
                // `planned_cost` bounds fees, it is not an amount pin.
                refuse_terminally(
                    store,
                    &pairing,
                    require_melt_pinned(effects, &pairing, &new_melt_quote_id),
                )?;
                store.append_sync(&HopRecord::MeltSuperseded {
                    attempt_id: pairing.attempt_id.clone(),
                    superseded_melt_quote_id: active_melt_quote_id.clone(),
                    melt_quote_id: new_melt_quote_id.clone(),
                    planned_cost,
                })?;
                actual_fee_this_run = Some(refuse_terminally(
                    store,
                    &pairing,
                    effects.melt(&new_melt_quote_id, &pairing),
                )?);
                melted_this_run = true;
                active_melt_quote_id = new_melt_quote_id;
                supersessions += 1;
                continue;
            }
            MeltLeg::Paid => {
                if !melted_this_run {
                    // Paid on an earlier run, possibly by a buyer from before the pin existed.
                    report_overpaid(effects, &pairing, &active_melt_quote_id);
                }
                !melted_this_run
            }
        };
    };

    // Mint leg. A melt that landed on an EARLIER run with no ecash to show for it is the strand.
    let mint_leg = effects.mint_leg(&pairing.mint_quote_id)?;
    let recovered_strand = melted_earlier && mint_leg != MintLeg::Issued;
    if recovered_strand {
        eprintln!("{}", strand_line(&pairing, &active_melt_quote_id));
    }
    let minted_sats = match mint_leg {
        MintLeg::Issued => pairing.delivered_sats,
        MintLeg::Unpaid | MintLeg::Paid => {
            effects.mint(&pairing.mint_quote_id, pairing.delivered_sats)?
        }
        MintLeg::Pending => {
            return Err(HopError::AmbiguousTarget {
                attempt_id: pairing.attempt_id.clone(),
                mint_quote_id: pairing.mint_quote_id.clone(),
            });
        }
    };
    if minted_sats != pairing.delivered_sats {
        return Err(HopError::MintedAmountMismatch {
            expected: pairing.delivered_sats,
            minted: minted_sats,
        });
    }

    store.append_sync(&HopRecord::Settled {
        attempt_id: pairing.attempt_id.clone(),
        minted_sats,
    })?;
    // Reconcile the reserved Lightning fee against what the melt actually paid THIS run, and report
    // the unused remainder so the caller can credit it back to the budget (#186). A run that did not
    // melt (recovered a prior melt) has no observed fee, so it credits nothing — the safe direction.
    let unused_fee_reserve_sats = actual_fee_this_run
        .map(|actual| pairing.reserved_ln_fee_sats().saturating_sub(actual))
        .unwrap_or(0);
    Ok(HopSettled {
        minted_sats,
        recovered_strand,
        unused_fee_reserve_sats,
    })
}

/// Plan-time hop authorization: quoted cost plus a fee-market buffer.
///
/// Buffer = `multiplier × fee_reserve` (the source mint's own melt fee reserve for this quote).
/// The ceiling is committed here, when the fee market is known; recovery compares replacement
/// quotes against this number **unchanged**. `multiplier == 0` restores exact quoted-cost
/// authorization.
fn authorized_hop_cost(
    quoted_cost: u64,
    fee_reserve: u64,
    multiplier: u64,
) -> Result<u64, HopError> {
    let buffer = fee_reserve.checked_mul(multiplier).ok_or_else(|| {
        HopError::Mint(format!(
            "cross-mint hop fee buffer overflows u64: fee_reserve={fee_reserve} multiplier={multiplier}"
        ))
    })?;
    quoted_cost.checked_add(buffer).ok_or_else(|| {
        HopError::Mint(format!(
            "cross-mint hop authorized cost overflows u64: quoted_cost={quoted_cost} fee_buffer={buffer}"
        ))
    })
}

/// What the hop reads from a bolt11: amount in msat (`None` if amountless) and the lifetime left.
/// `None` for an undecodable string. `Bolt11Invoice::from_str` rejects an amount that is not a
/// whole number of msat (`ImpreciseAmount`), so a sub-msat invoice is undecodable and refused:
/// there is no rounding floor.
#[derive(Debug)]
struct InvoiceFacts {
    msat: Option<u64>,
    remaining: Duration,
}

fn invoice_facts(bolt11: &str) -> Option<InvoiceFacts> {
    use std::str::FromStr as _;
    let invoice = cdk::Bolt11Invoice::from_str(bolt11.trim()).ok()?;
    Some(InvoiceFacts {
        msat: invoice.amount_milli_satoshis(),
        remaining: invoice.duration_until_expiry(),
    })
}

/// SHA-256 (hex) of a bolt11 as the hop pins it: trimmed and lowercased (bech32 is
/// case-insensitive), so only a different invoice changes it.
fn invoice_digest(bolt11: &str) -> String {
    use cdk::secp256k1::hashes::{Hash as _, sha256};
    sha256::Hash::hash(bolt11.trim().to_ascii_lowercase().as_bytes()).to_string()
}

/// Least lifetime a target invoice must have left to be planned, melted or re-quoted. Its own
/// value (#1292 fable r3 N6), held at least one money-moving leg by the assertion below: raising
/// `HOP_LEG_TIMEOUT` past it fails the build rather than silently shutting out mints whose
/// invoices are short-lived.
const MIN_INVOICE_LIFETIME: Duration = Duration::from_secs(90);
const _: () = assert!(MIN_INVOICE_LIFETIME.as_secs() >= HOP_LEG_TIMEOUT.as_secs());

fn require_lifetime(facts: &InvoiceFacts, mint: &str) -> Result<(), HopError> {
    if facts.remaining < MIN_INVOICE_LIFETIME {
        return Err(HopError::InvoiceExpired {
            mint: mint.to_owned(),
            remaining_secs: facts.remaining.as_secs(),
        });
    }
    Ok(())
}

/// Plan-time half of the F1 pin: refuse a target-mint bolt11 whose amount is not exactly
/// `delivered_sats`, and one with less than [`MIN_INVOICE_LIFETIME`] left. Undecodable and
/// amountless invoices are refused too: either would let the payer side decide the amount, and
/// the payer side here is the seller's own mint. Returns the digest the hop journals.
fn require_invoice_amount(
    bolt11: &str,
    mint: &str,
    delivered_sats: u64,
) -> Result<String, HopError> {
    // cdk parses the request untrimmed: refuse padding here so the hop and cdk agree on what
    // decodes (#1292 fable r3 L1), instead of passing the pin and failing at the melt quote.
    let facts = if bolt11 == bolt11.trim() {
        invoice_facts(bolt11)
    } else {
        None
    };
    let undecodable = facts.is_none();
    let invoice_msat = facts.as_ref().and_then(|facts| facts.msat);
    let expected_msat = delivered_sats.checked_mul(1000);
    let Some(facts) = facts.filter(|_| invoice_msat.is_some() && invoice_msat == expected_msat)
    else {
        return Err(HopError::InvoiceAmountMismatch {
            mint: mint.to_owned(),
            expected_sats: delivered_sats,
            invoice_msat,
            undecodable,
        });
    };
    require_lifetime(&facts, mint)?;
    Ok(invoice_digest(bolt11))
}

/// Run-time half of the F1 pin: the invoice a melt is about to pay must be for exactly the pinned
/// delivery and, when the plan journalled one, be the planned invoice. A pre-pin pairing (no
/// digest) is held to the amount alone, which is what closes F1 for hops an rc7 buyer planned.
fn require_pinned_invoice(pairing: &HopJournal, bolt11: &str) -> Result<InvoiceFacts, HopError> {
    let facts = invoice_facts(bolt11);
    let invoice_msat = facts.as_ref().and_then(|facts| facts.msat);
    let undecodable = facts.is_none();
    let invoice_matches = pairing
        .invoice_sha256
        .as_ref()
        .map(|pinned| pinned.eq_ignore_ascii_case(&invoice_digest(bolt11)));
    let amount_ok =
        invoice_msat.is_some() && invoice_msat == pairing.delivered_sats.checked_mul(1000);
    match facts {
        Some(facts) if amount_ok && invoice_matches != Some(false) => Ok(facts),
        _ => Err(HopError::UnpinnedInvoice {
            attempt_id: pairing.attempt_id.clone(),
            expected_sats: pairing.delivered_sats,
            invoice_msat,
            undecodable,
            invoice_matches,
        }),
    }
}

/// Before raising a replacement melt quote: the target invoice must still be the pinned one and
/// have [`MIN_INVOICE_LIFETIME`] left, or the re-quote can only fail again (up to
/// `MAX_MELT_SUPERSESSIONS` times) without saying why.
fn require_requotable(pairing: &HopJournal, bolt11: &str) -> Result<(), HopError> {
    let facts = require_pinned_invoice(pairing, bolt11)?;
    require_lifetime(&facts, &pairing.target_mint)
}

/// Gate one melt: the source quote must pay the pinned invoice and melt exactly the delivery.
fn require_melt_pinned<E: HopEffects>(
    effects: &mut E,
    pairing: &HopJournal,
    melt_quote_id: &str,
) -> Result<(), HopError> {
    let (bolt11, melt_sats) = effects.melt_invoice(melt_quote_id)?;
    let facts = require_pinned_invoice(pairing, &bolt11)?;
    if melt_sats != pairing.delivered_sats {
        return Err(HopError::SourceQuoteMismatch {
            mint: pairing.source_mint.clone(),
            expected_sats: pairing.delivered_sats,
            quoted_sats: melt_sats,
        });
    }
    // A resumed pairing whose invoice has run out gets no futile melt (#1292 fable r3 L3); the
    // caller records the refusal as terminal, so the sweep stops reporting it as a strand.
    require_lifetime(&facts, &pairing.target_mint)
}

/// Record a pin refusal as terminal before returning it (#1292 review L2). Every caller runs while
/// the source reports the active quote Unpaid or Failed and before (or instead of) the melt, so
/// nothing has been melted for the attempt. A failed append is reported, not substituted for the
/// refusal: the refusal still stands, the next run just evaluates it again.
fn refuse_terminally<S: HopJournalStore, T>(
    store: &S,
    pairing: &HopJournal,
    result: Result<T, HopError>,
) -> Result<T, HopError> {
    if let Err(refusal) = &result
        && refusal.is_pin_refusal()
        && let Err(error) = store.append_sync(&HopRecord::Refused {
            attempt_id: pairing.attempt_id.clone(),
            reason: refusal.to_string(),
        })
    {
        eprintln!(
            "cross-mint hop: attempt {} refused but the refusal could not be journalled: {error}",
            pairing.attempt_id
        );
    }
    result
}

/// The melt cdk prepared, checked against the pairing right before `confirm` (#1292 review L1):
/// the invoice and amount pins, the source mint, and the total it takes from the wallet
/// (`amount + fee_reserve + input fee`) against the authorized `planned_cost`. The swap fee of a
/// pre-melt swap is not part of the hop's planned cost today and is not bound here.
fn require_prepared_within_pin(
    pin: &HopJournal,
    source_mint: &str,
    quote_mint: Option<&str>,
    request: &str,
    amount: u64,
    fee_reserve: u64,
    input_fee: u64,
) -> Result<(), HopError> {
    require_pinned_invoice(pin, request)?;
    if amount != pin.delivered_sats {
        return Err(HopError::SourceQuoteMismatch {
            mint: source_mint.to_owned(),
            expected_sats: pin.delivered_sats,
            quoted_sats: amount,
        });
    }
    if quote_mint != Some(source_mint) {
        return Err(HopError::PreparedMeltRefused {
            attempt_id: pin.attempt_id.clone(),
            reason: format!(
                "the melt quote belongs to {}, not the hop's source mint {source_mint}",
                quote_mint.unwrap_or("no mint")
            ),
        });
    }
    let cost = amount.saturating_add(fee_reserve).saturating_add(input_fee);
    if cost > pin.planned_cost {
        return Err(HopError::PreparedMeltRefused {
            attempt_id: pin.attempt_id.clone(),
            reason: format!(
                "the prepared melt would take {cost} sats (melt {amount} + fee reserve \
                 {fee_reserve} + input fee {input_fee}), above the authorized {}",
                pin.planned_cost
            ),
        });
    }
    Ok(())
}

/// Report-only check of a melt that already paid or is in flight (#1292 review L4). The money
/// cannot be recalled; this is the one moment a buyer upgraded from before the pin can learn that
/// an earlier hop paid an inflated invoice. Never changes control flow.
fn report_overpaid<E: HopEffects>(effects: &mut E, pairing: &HopJournal, melt_quote_id: &str) {
    if let Ok((bolt11, melt_sats)) = effects.melt_invoice(melt_quote_id)
        && let Some(line) = overpaid_line(pairing, melt_quote_id, &bolt11, melt_sats)
    {
        eprintln!("{line}");
    }
}

fn overpaid_line(
    pairing: &HopJournal,
    melt_quote_id: &str,
    bolt11: &str,
    melt_sats: u64,
) -> Option<String> {
    if require_pinned_invoice(pairing, bolt11).is_ok() && melt_sats == pairing.delivered_sats {
        return None;
    }
    let paid = invoice_facts(bolt11)
        .and_then(|facts| facts.msat)
        .map_or_else(|| "an unreadable amount".to_owned(), |msat| format!("{msat} msat"));
    Some(format!(
        "CROSSMINT OVERPAID attempt={} melt quote {} at {} paid an invoice for {paid} (melting \
         {melt_sats} sats) for a {} sat delivery to {}; this cannot be recalled — report the \
         target mint",
        pairing.attempt_id,
        melt_quote_id,
        pairing.source_mint,
        pairing.delivered_sats,
        pairing.target_mint,
    ))
}

/// Bound on one hop leg that moves money.
///
/// Longer than [`MINT_TOUCH_TIMEOUT`], which bounds mint *reads*: a melt is a Lightning payment
/// between two mints, not a keyset lookup. Shorter than forever, because a leg that never returns is
/// invariant 5's failure — an await with no timer is a park nothing can end.
const HOP_LEG_TIMEOUT: Duration = Duration::from_secs(90);

/// Maximum replacement quotes for one attempt. Hygiene, not a financial bound: the invoice can
/// settle only once, and the plan-time authorization already caps exposure.
const MAX_MELT_SUPERSESSIONS: usize = 3;

/// Run one async hop step on its own thread and its own runtime, returning synchronously.
///
/// The budget gate's effect boundary is synchronous — `authorize_then_attempt` takes an `FnOnce` —
/// and the hop is async, so the two need a bridge. Blocking the caller's runtime thread is not on the
/// table (`block_on` inside a runtime panics), and the crate already answers this exact question for
/// the wallet effects the same way: own thread, own current-thread runtime, synchronous handoff.
fn block_on_leg<T, F>(label: &str, future: F) -> Result<T, HopError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let label = label.to_owned();
    std::thread::Builder::new()
        .name("maxplayer-crossmint-hop".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|runtime| runtime.block_on(future))
        })
        .map_err(|error| HopError::Mint(format!("{label}: hop thread: {error}")))?
        .join()
        .map_err(|_| HopError::Mint(format!("{label}: hop thread panicked")))?
        .map_err(|error| HopError::Mint(format!("{label}: hop runtime: {error}")))
}

/// Bound an async mint touch, turning a hang into a refusal.
async fn bounded<T>(
    label: &str,
    timeout: Duration,
    future: impl Future<Output = Result<T, cdk::Error>>,
) -> Result<T, HopError> {
    match tokio::time::timeout(timeout, future).await {
        Err(_elapsed) => Err(HopError::MintUnreachable {
            label: label.to_owned(),
            detail: format!("request exceeded {timeout:?}"),
        }),
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) if is_mint_unreachable(&error) => Err(HopError::MintUnreachable {
            label: label.to_owned(),
            detail: error.to_string(),
        }),
        Ok(Err(error)) => Err(HopError::Mint(format!("{label}: {error}"))),
    }
}

/// The two mint wallets one hop runs against.
///
/// Both are opened through [`buyer_fund::open_wallet_at_mint_async`], which fences the mint it is
/// asked for. That is why the hop cannot become a back door around `allow_real_mints`: the fence is
/// inside the opener, so the TARGET is fenced by the same code that fences the source, without this
/// module having to remember to do it.
pub(crate) struct CdkHopEffects {
    source: Wallet,
    target: Wallet,
    /// `[buyer] hop_fee_buffer_multiplier`. Applied only when writing the Planned record.
    hop_fee_buffer_multiplier: u64,
    /// The home's `wallet/` dir: the melt's proof selection holds its selection lock
    /// ([`crate::shared_wallet`]). `None` only for in-memory test wallets.
    wallet_dir: Option<PathBuf>,
}

impl CdkHopEffects {
    /// Open the buyer's wallet at both mints. One sqlite store, two bound mints.
    pub(crate) async fn open(
        home: &MaxplayerHome,
        source_mint: &str,
        target_mint: &str,
    ) -> Result<Self, HopError> {
        // A hop melts at its source, and its recovery sweep asks the source melt quote's status —
        // both cdk calls that can release `nostr://` inputs early. `plan_payment` never plans such a
        // hop; this refuses a journaled one (or any other caller) before a wallet is even opened.
        if crate::mint_wire::is_nostr_scheme(source_mint) {
            return Err(HopError::Mint(format!(
                "source mint {source_mint}: a nostr:// mint cannot be a hop source (it never melts)"
            )));
        }
        if crate::mint_wire::is_nostr_scheme(target_mint) {
            return Err(HopError::Mint(format!(
                "target mint {target_mint}: a nostr:// mint cannot be a hop target (credits cannot receive a hop)"
            )));
        }
        let source = buyer_fund::open_wallet_at_mint_async(home, source_mint)
            .await
            .map_err(|error| HopError::Mint(format!("source mint {source_mint}: {error}")))?;
        let target = buyer_fund::open_wallet_at_mint_async(home, target_mint)
            .await
            .map_err(|error| HopError::Mint(format!("target mint {target_mint}: {error}")))?;
        Ok(Self {
            source,
            target,
            hop_fee_buffer_multiplier: home.config.buyer.hop_fee_buffer_multiplier,
            wallet_dir: Some(home.wallet_dir.clone()),
        })
    }

    /// Price the hop and raise both quotes, moving no money.
    ///
    /// Delivery is PINNED: the mint quote at the target is raised for exactly `delivered_sats`, the
    /// amount from the buyer-signed offer. The buyer's cost is what floats — melt amount, the source
    /// mint's Lightning fee reserve, and its input fee — so no fee reading can ever reduce what the
    /// seller receives.
    ///
    /// Runs before the budget gate, so every failure here refuses with zero spend.
    ///
    /// The journalled `planned_cost` is the quoted cost plus a fee buffer (`multiplier ×` the
    /// source mint's melt fee reserve). That buffered figure is the authorization the budget
    /// gate charges and the ceiling recovery compares against — the comparison itself is not
    /// loosened.
    pub(crate) async fn plan_quotes(
        &self,
        attempt_id: &str,
        delivered_sats: u64,
    ) -> Result<HopJournal, HopError> {
        if delivered_sats == 0 {
            return Err(HopError::Mint(
                "cross-mint hop: delivery amount is 0 (refusing to raise a quote for nothing)"
                    .into(),
            ));
        }
        // #1292 fable r3 L4: the quote id is the seller mint's choice and cdk keys rows on id alone.
        // Read what the wallet already holds, so a reused id is refused rather than deleted below.
        let known_ids: std::collections::HashSet<String> = tokio::time::timeout(
            MINT_TOUCH_TIMEOUT,
            self.target.localstore.get_mint_quotes(),
        )
        .await
        .map_err(|_elapsed| HopError::MintUnreachable {
            label: "target quote ids (wallet store)".to_owned(),
            detail: format!("store read exceeded {MINT_TOUCH_TIMEOUT:?}"),
        })?
        .map_err(|error| HopError::Mint(format!("target quote ids: {error}")))?
        .into_iter()
        .map(|quote| quote.id)
        .collect();
        let mint_quote = bounded(
            "target mint quote",
            MINT_TOUCH_TIMEOUT,
            self.target.mint_quote(
                PaymentMethod::BOLT11,
                Some(cdk::Amount::from(delivered_sats)),
                None,
                None,
            ),
        )
        .await?;
        let target_quote_id = mint_quote_id(&mint_quote.id);
        if known_ids.contains(&target_quote_id) {
            eprintln!(
                "cross-mint hop: target mint {} reused quote id {target_quote_id}; the earlier row \
                 was overwritten by the wallet library and is left in place",
                self.target.mint_url
            );
            return Err(HopError::TargetQuoteIdReused {
                mint: self.target.mint_url.to_string(),
                quote_id: target_quote_id,
            });
        }
        // Any refusal from here on leaves neither the target quote nor a raised source quote in
        // the buyer's stores (#1292 fable r3 L1): nothing will ever pay them.
        let mut raised_melt: Option<String> = None;
        let planned = self
            .plan_against(
                attempt_id,
                delivered_sats,
                &target_quote_id,
                &mint_quote.request,
                &mut raised_melt,
            )
            .await;
        if planned.is_err() {
            self.discard_planning_quotes(&target_quote_id, raised_melt.as_deref())
                .await;
        }
        planned
    }

    /// The body of [`Self::plan_quotes`] once the target quote exists. `raised_melt` is set as soon
    /// as a source melt quote is stored, so the caller can drop it on refusal.
    async fn plan_against(
        &self,
        attempt_id: &str,
        delivered_sats: u64,
        target_quote_id: &str,
        request: &str,
        raised_melt: &mut Option<String>,
    ) -> Result<HopJournal, HopError> {
        if request.trim().is_empty() {
            return Err(HopError::Mint(format!(
                "target mint {} returned an empty bolt11 for quote {target_quote_id} (refusing a \
                 hop with nothing to pay)",
                self.target.mint_url
            )));
        }
        // REPORT1110 F1: the target mint is the SELLER's choice, so the invoice it returns is
        // untrusted. It must be for exactly the pinned delivery amount — checked BEFORE the source
        // is even asked to price it, so an inflated invoice can never reach the budget gate. Its
        // digest is journalled so every later melt can prove it pays this same invoice.
        let invoice_sha256 =
            require_invoice_amount(request, &self.target.mint_url.to_string(), delivered_sats)?;
        let (melt_quote_id, quoted_cost, cost) = self.raise_melt_quote(request).await?;
        *raised_melt = Some(melt_quote_id.clone());
        // cdk 0.17.2 compares a melt quote to its invoice only for sat/msat wallets, only after the
        // quote is raised, and truncates msat to sat; the decode above is the control that binds
        // the offer. For the sat wallet the hop uses, cdk already refuses a mismatched quote with
        // `IncorrectQuoteAmount`, so this branch is unreachable through cdk today: it is defence in
        // depth for a non-sat unit or a cdk that drops that check.
        if cost.melt_amount != delivered_sats {
            return Err(HopError::SourceQuoteMismatch {
                mint: self.source.mint_url.to_string(),
                expected_sats: delivered_sats,
                quoted_sats: cost.melt_amount,
            });
        }
        self.require_source_covers(quoted_cost).await?;
        let planned_cost = authorized_hop_cost(
            quoted_cost,
            cost.fee_reserve,
            self.hop_fee_buffer_multiplier,
        )?;
        Ok(HopJournal {
            attempt_id: attempt_id.to_owned(),
            source_mint: self.source.mint_url.to_string(),
            melt_quote_id,
            target_mint: self.target.mint_url.to_string(),
            mint_quote_id: target_quote_id.to_owned(),
            delivered_sats,
            planned_cost,
            fee_reserve: cost.fee_reserve,
            input_fee: cost.input_fee,
            invoice_sha256: Some(invoice_sha256),
        })
    }

    /// Drop the quotes a refused plan raised. Best effort: the refusal stands either way, but a
    /// failed delete is said on stderr so accumulating rows are visible (#1292 fable r3 N5).
    async fn discard_planning_quotes(&self, target_quote_id: &str, melt_quote_id: Option<&str>) {
        match tokio::time::timeout(
            MINT_TOUCH_TIMEOUT,
            self.target.localstore.remove_mint_quote(target_quote_id),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!(
                "cross-mint hop: could not drop refused target quote {target_quote_id}: {error}"
            ),
            Err(_elapsed) => eprintln!(
                "cross-mint hop: dropping refused target quote {target_quote_id} timed out"
            ),
        }
        if let Some(id) = melt_quote_id {
            self.discard_melt_quote(id).await;
        }
    }

    async fn discard_melt_quote(&self, melt_quote_id: &str) {
        match tokio::time::timeout(
            MINT_TOUCH_TIMEOUT,
            self.source.localstore.remove_melt_quote(melt_quote_id),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!(
                "cross-mint hop: could not drop unused source quote {melt_quote_id}: {error}"
            ),
            Err(_elapsed) => eprintln!(
                "cross-mint hop: dropping unused source quote {melt_quote_id} timed out"
            ),
        }
    }

    /// Raise and price one source-mint melt quote without moving money.
    ///
    /// Returns `(quote id, quoted cost, HopCost)`. Quoted cost is melt amount + fee reserve
    /// + input fee — the actual quote, with no buffer. The buffer is applied only in
    /// [`Self::plan_quotes`], so a superseding quote is compared against the already-buffered
    /// record rather than growing the ceiling. The full [`HopCost`] is returned so the caller can
    /// journal `fee_reserve` and `input_fee` for the post-melt reconciliation (#186).
    async fn raise_melt_quote(&self, bolt11: &str) -> Result<(String, u64, HopCost), HopError> {
        let melt_quote = bounded(
            "source melt quote",
            MINT_TOUCH_TIMEOUT,
            self.source
                .melt_quote(PaymentMethod::BOLT11, bolt11.to_owned(), None, None),
        )
        .await?;
        let input_fee = match self.source_input_fee_ceiling().await {
            Ok(fee) => fee,
            Err(error) => {
                self.discard_melt_quote(&mint_quote_id(&melt_quote.id)).await;
                return Err(error);
            }
        };
        let cost = HopCost {
            melt_amount: melt_quote.amount.to_u64(),
            fee_reserve: melt_quote.fee_reserve.to_u64(),
            input_fee,
        };
        let quoted_cost = cost
            .planned_cost()
            .map_err(|error| HopError::Mint(error.to_string()))?;
        Ok((mint_quote_id(&melt_quote.id), quoted_cost, cost))
    }

    /// Refuse a hop the source wallet cannot fund, BEFORE the cap is charged.
    ///
    /// Without this the buyer would charge its budget the full planned cost and only then discover
    /// it has nothing to melt — spending cap on a payment that never happens. Before this slice a
    /// buyer with no route to the seller's mint simply declined at zero cost; a hop must not turn
    /// that into a charge.
    async fn require_source_covers(&self, planned_cost: u64) -> Result<(), HopError> {
        let balance = bounded(
            "source balance",
            MINT_TOUCH_TIMEOUT,
            self.source.total_balance(),
        )
        .await?
        .to_u64();
        if balance < planned_cost {
            return Err(HopError::InsufficientSource {
                mint: self.source.mint_url.to_string(),
                balance,
                planned_cost,
            });
        }
        Ok(())
    }

    /// Upper bound on the source mint's input fee for this melt.
    ///
    /// The melt selects its inputs when it runs, so the exact input count is not knowable before the
    /// cap check. Priced at the most inputs the melt could possibly select — every unspent proof at
    /// the source — which over-states the fee rather than under-stating it. Under-stating would put
    /// a fee on the wire that the cap never saw, which is the defect class the cap exists to stop.
    ///
    /// interim: the reserve-versus-actual reconciliation that would make this exact reshapes the
    /// spend ledger, which is money-gate machinery and its own slice — see MakePrisms/maxplayerai#186.
    async fn source_input_fee_ceiling(&self) -> Result<u64, HopError> {
        let proofs = bounded(
            "source proof count",
            MINT_TOUCH_TIMEOUT,
            self.source.get_unspent_proofs(),
        )
        .await?;
        let inputs = u64::try_from(proofs.len()).unwrap_or(u64::MAX).max(1);
        crate::payment_wallet::bounded_input_fee(&self.source, inputs)
            .await
            .map(|fee| fee.to_u64())
            .map_err(|error| HopError::Mint(format!("source input fee: {error}")))
    }
}

/// cdk quote ids render as their string form; the journal stores that, since it is what
/// `check_melt_quote_status` / `check_mint_quote` are asked for on recovery.
fn mint_quote_id(id: &impl fmt::Display) -> String {
    id.to_string()
}

impl HopEffects for CdkHopEffects {
    fn melt_leg(&mut self, melt_quote_id: &str) -> Result<MeltLeg, HopError> {
        let wallet = self.source.clone();
        let quote_id = melt_quote_id.to_owned();
        // Asking is also cdk's own recovery trigger: a melt interrupted mid-saga resumes on this
        // call, so the answer reflects a settled world rather than the moment we crashed.
        let state = block_on_leg("melt state", async move {
            bounded(
                "melt quote status",
                HOP_LEG_TIMEOUT,
                wallet.check_melt_quote_status(&quote_id),
            )
            .await
            .map(|quote| quote.state)
        })??;
        Ok(match state {
            MeltQuoteState::Unpaid => MeltLeg::Unpaid,
            MeltQuoteState::Paid => MeltLeg::Paid,
            MeltQuoteState::Failed => MeltLeg::Failed,
            // Pending is money in flight; Unknown is the mint declining to say. Neither is a
            // statement that the sats are still ours, so neither may lead to a second melt.
            MeltQuoteState::Pending | MeltQuoteState::Unknown => MeltLeg::Pending,
        })
    }

    fn mint_leg(&mut self, mint_quote_id: &str) -> Result<MintLeg, HopError> {
        let wallet = self.target.clone();
        let quote_id = mint_quote_id.to_owned();
        let state = block_on_leg("mint state", async move {
            bounded(
                "mint quote status",
                HOP_LEG_TIMEOUT,
                wallet.check_mint_quote(&quote_id),
            )
            .await
            .map(|quote| quote.state)
        })??;
        Ok(match state {
            MintQuoteState::Unpaid => MintLeg::Unpaid,
            MintQuoteState::Paid => MintLeg::Paid,
            MintQuoteState::Issued => MintLeg::Issued,
        })
    }

    fn target_invoice(&mut self, mint_quote_id: &str) -> Result<(MintLeg, String), HopError> {
        let wallet = self.target.clone();
        let quote_id = mint_quote_id.to_owned();
        let quote = block_on_leg("target invoice", async move {
            bounded(
                "target invoice",
                HOP_LEG_TIMEOUT,
                wallet.check_mint_quote(&quote_id),
            )
            .await
        })??;
        let state = match quote.state {
            MintQuoteState::Unpaid => MintLeg::Unpaid,
            MintQuoteState::Paid => MintLeg::Paid,
            MintQuoteState::Issued => MintLeg::Issued,
        };
        Ok((state, quote.request))
    }

    fn melt_invoice(&mut self, melt_quote_id: &str) -> Result<(String, u64), HopError> {
        let wallet = self.source.clone();
        let quote_id = melt_quote_id.to_owned();
        block_on_leg("melt invoice", async move {
            // A local store read, but bounded like every other leg: it runs inside the budget
            // gate, where an await with no timer parks the pay path (#1292 review L3).
            let quote = tokio::time::timeout(
                MINT_TOUCH_TIMEOUT,
                wallet.localstore.get_melt_quote(&quote_id),
            )
            .await
            .map_err(|_elapsed| HopError::MintUnreachable {
                label: "melt invoice (wallet store)".to_owned(),
                detail: format!("store read exceeded {MINT_TOUCH_TIMEOUT:?}"),
            })?
            .map_err(|error| HopError::Mint(format!("melt invoice: {error}")))?
            .ok_or_else(|| {
                HopError::Mint(format!(
                    "melt invoice: source melt quote {quote_id} is not in the wallet store"
                ))
            })?;
            // The store is shared by every mint wallet in the home and keyed on quote id alone.
            if quote.mint_url.as_ref() != Some(&wallet.mint_url) {
                return Err(HopError::Mint(format!(
                    "melt invoice: quote {quote_id} belongs to {:?}, not the source mint {}",
                    quote.mint_url.as_ref().map(ToString::to_string),
                    wallet.mint_url
                )));
            }
            Ok::<_, HopError>((quote.request, quote.amount.to_u64()))
        })?
    }

    fn supersede_melt_quote(&mut self, bolt11: &str) -> Result<(String, u64), HopError> {
        let effects = Self {
            source: self.source.clone(),
            target: self.target.clone(),
            hop_fee_buffer_multiplier: self.hop_fee_buffer_multiplier,
            wallet_dir: self.wallet_dir.clone(),
        };
        let bolt11 = bolt11.to_owned();
        block_on_leg("source melt quote", async move {
            effects
                .raise_melt_quote(&bolt11)
                .await
                .map(|(id, quoted_cost, _cost)| (id, quoted_cost))
        })?
    }

    fn require_source_covers(&mut self, planned_cost: u64) -> Result<(), HopError> {
        let effects = Self {
            source: self.source.clone(),
            target: self.target.clone(),
            hop_fee_buffer_multiplier: self.hop_fee_buffer_multiplier,
            wallet_dir: self.wallet_dir.clone(),
        };
        block_on_leg("source coverage", async move {
            effects.require_source_covers(planned_cost).await
        })?
    }

    fn melt(&mut self, melt_quote_id: &str, pin: &HopJournal) -> Result<u64, HopError> {
        let wallet = self.source.clone();
        let quote_id = melt_quote_id.to_owned();
        let wallet_dir = self.wallet_dir.clone();
        let pin = pin.clone();
        block_on_leg("melt", async move {
            // Review r6 H1: the one mint payment gate, before this melt's own operation lock.
            crate::payment_wallet::payment_gate(&wallet, wallet_dir.as_deref())
                .await
                .map_err(|error| HopError::Mint(format!("melt: {error}")))?;
            // Mint metadata first, outside the lock; the lock is then held only across prepare_melt's
            // local selection + reservation (no mint request inside it), outside the mint bound.
            let metadata = tokio::time::timeout(
                MINT_TOUCH_TIMEOUT,
                crate::shared_wallet::warm_metadata(&wallet),
            )
            .await
            .unwrap_or_default();
            // Review r3 H1: in flight from before the melt saga exists until `confirm` returns.
            let _operation = crate::shared_wallet::operation_guard(wallet_dir.as_deref())
                .await
                .map_err(|error| HopError::Mint(format!("melt: {error}")))?;
            let select = crate::shared_wallet::select_guard(wallet_dir.as_deref())
                .await
                .map_err(|error| HopError::Mint(format!("melt: {error}")))?;
            let prepared = bounded(
                "prepare melt",
                MINT_TOUCH_TIMEOUT,
                crate::shared_wallet::local_only(
                    metadata,
                    wallet.prepare_melt(&quote_id, HashMap::new()),
                ),
            )
            .await?;
            // #1292 review L1: the pin is checked on the quote THIS melt will confirm, not on an
            // earlier read of the store. A refusal cancels the prepared melt (releasing the
            // reserved proofs) before any melt request is sent.
            let checked = {
                let quote = prepared.quote();
                let quote_mint = quote.mint_url.as_ref().map(ToString::to_string);
                require_prepared_within_pin(
                    &pin,
                    &wallet.mint_url.to_string(),
                    quote_mint.as_deref(),
                    &quote.request,
                    quote.amount.to_u64(),
                    quote.fee_reserve.to_u64(),
                    prepared.input_fee().to_u64(),
                )
            };
            if let Err(refusal) = checked {
                if let Err(error) =
                    bounded("cancel melt", MINT_TOUCH_TIMEOUT, prepared.cancel()).await
                {
                    eprintln!(
                        "cross-mint hop: attempt {}: refused melt could not be cancelled \
                         ({error}); saga recovery releases its proofs",
                        pin.attempt_id
                    );
                }
                return Err(refusal);
            }
            drop(select);
            let melted = bounded("melt confirm", HOP_LEG_TIMEOUT, prepared.confirm()).await?;
            // `confirm` is the effect boundary — the melt has settled and the fee actually paid is
            // known here. The mint returned the unused reserve as change; reporting the real fee lets
            // the caller credit that difference back to the cap (#186).
            Ok::<u64, HopError>(melted.fee_paid().to_u64())
        })?
    }

    fn mint(&mut self, mint_quote_id: &str, expected_sats: u64) -> Result<u64, HopError> {
        let wallet = self.target.clone();
        let quote_id = mint_quote_id.to_owned();
        block_on_leg("mint", async move {
            // The shared poll-then-issue path, which already refuses a phantom credit and an
            // under- or over-funded issue. Nothing about a hop makes those checks weaker.
            crate::wallet_ops::poll_and_mint(&wallet, &quote_id, expected_sats)
                .await
                .map_err(|error| HopError::Mint(format!("target mint issue: {error}")))
        })?
    }
}

/// One hop the sweep found unfinished and tried to complete.
#[derive(Debug)]
pub struct SweptHop {
    /// The attempt whose hop was resumed.
    pub attempt_id: String,
    /// What resuming it did, or why it could not be finished.
    pub result: Result<HopSettled, HopError>,
}

/// The directory a home keeps its hop pairings in.
pub fn hop_journal_dir(home: &MaxplayerHome) -> PathBuf {
    home.root.join("crossmint-journal")
}

/// Finish every hop this home left in flight.
///
/// A hop interrupted by a crash is not something the next pay attempt necessarily re-drives — that
/// attempt may never be retried — so without a sweep the sats could sit melted at the source with no
/// ecash anywhere and nothing looking for them.
///
/// Both wallets are opened and both are recovered before any pairing is resumed, and that is not
/// belt-and-braces: cdk's `recover_incomplete_sagas` filters to its own wallet's mint, so a two-mint
/// operation recovered on one wallet SILENTLY SKIPS the other mint's saga. One wallet is not half a
/// recovery here; it is a recovery that reports success having looked at half the problem.
///
/// Every hop is attempted independently, and a hop that cannot be finished says so on stderr rather
/// than being dropped from the report — including a hop whose mints the fence no longer admits,
/// which is stuck by design but must not be stuck in silence.
pub async fn sweep_hops(home: &MaxplayerHome) -> Result<Vec<SweptHop>, HopError> {
    let store = FsHopJournal::new(hop_journal_dir(home));
    let mut swept = Vec::new();
    for attempt_id in store.attempt_ids()? {
        let records = store.replay(&attempt_id)?;
        if settled_of(&records).is_some() || refused_of(&records).is_some() {
            continue;
        }
        let Some(pairing) = planned_of(&records).cloned() else {
            continue;
        };
        let active_melt_quote_id = active_melt(&records)
            .map(|(id, _)| id)
            .unwrap_or_else(|| pairing.melt_quote_id.clone());
        let result = sweep_one(home, &store, pairing.clone()).await;
        if let Err(error) = &result
            && error.is_pin_refusal()
        {
            // Refused before anything was melted: no sats are stranded (#1292 review L2).
            eprintln!(
                "CROSSMINT REFUSED attempt={} nothing was melted (source {}, target {}): {error}",
                pairing.attempt_id, pairing.source_mint, pairing.target_mint,
            );
        } else if let Err(error) = &result {
            eprintln!(
                "CROSSMINT STRAND attempt={} could not be completed by the recovery sweep \
                 (source {} melt quote {}, target {} mint quote {}): {error}",
                pairing.attempt_id,
                pairing.source_mint,
                active_melt_quote_id,
                pairing.target_mint,
                pairing.mint_quote_id,
            );
        }
        swept.push(SweptHop { attempt_id, result });
    }
    Ok(swept)
}

/// Refuse a recovery that did not cover both of the hop's mints.
///
/// cdk's saga recovery reports success after looking only at its own wallet's mint, so "recovery
/// ran" and "the hop was recovered" are different claims. This turns the difference into a refusal:
/// a sweep that somehow touched one mint stops here instead of reporting a hop as swept.
fn require_both_mints_recovered(
    recovered: &[String],
    pairing: &HopJournal,
) -> Result<(), HopError> {
    for mint in [&pairing.source_mint, &pairing.target_mint] {
        if !recovered.iter().any(|seen| seen == mint) {
            return Err(HopError::Journal(format!(
                "attempt {}: saga recovery covered {recovered:?}, which does not include {mint}; \
                 refusing to report a hop as swept on half its mints",
                pairing.attempt_id
            )));
        }
    }
    Ok(())
}

async fn sweep_one(
    home: &MaxplayerHome,
    store: &FsHopJournal,
    pairing: HopJournal,
) -> Result<HopSettled, HopError> {
    let mut effects = CdkHopEffects::open(home, &pairing.source_mint, &pairing.target_mint).await?;
    let mut recovered = Vec::new();
    for (label, wallet) in [("source", &effects.source), ("target", &effects.target)] {
        // Never plain cdk recovery: on a nostr:// mint it would unspend inputs and drop swap rows
        // while a signed request can still land. A held mint leaves the hop unfinished (fail-closed,
        // printed as a strand) for the next sweep.
        let outcome = bounded(
            &format!("{label} saga recovery"),
            HOP_LEG_TIMEOUT,
            crate::payment_wallet::recover_incomplete_sagas_guarded(
                wallet,
                Some(home.wallet_dir.as_path()),
            ),
        )
        .await?;
        if let crate::payment_wallet::GuardedRecovery::Held(reason) = outcome {
            return Err(HopError::Mint(format!(
                "{label} saga recovery on {}: {reason}",
                wallet.mint_url
            )));
        }
        recovered.push(wallet.mint_url.to_string());
    }
    require_both_mints_recovered(&recovered, &pairing)?;
    // `run_hop` blocks (it bridges each leg onto its own runtime), so it may not run on the
    // caller's async thread.
    let store = store.clone();
    tokio::task::spawn_blocking(move || run_hop(&store, &mut effects, &pairing))
        .await
        .map_err(|error| HopError::Mint(format!("hop sweep task: {error}")))?
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::Arc;

    use super::*;
    use crate::budget::{BudgetGate, BudgetRefuse};
    use crate::home::default_hop_fee_buffer_multiplier;

    fn journal(attempt: &str) -> HopJournal {
        HopJournal {
            attempt_id: attempt.to_owned(),
            source_mint: "https://a.example".to_owned(),
            melt_quote_id: "melt-1".to_owned(),
            target_mint: "https://b.example".to_owned(),
            mint_quote_id: "mint-1".to_owned(),
            delivered_sats: 100,
            planned_cost: 109,
            // 109 = 100 delivered + 7 fee reserve + 2 input fee (multiplier 0), so
            // reserved_ln_fee_sats() == 7.
            fee_reserve: 7,
            input_fee: 2,
            // Pre-pin shape (as an rc7 buyer journalled it): held to the amount check only.
            invoice_sha256: None,
        }
    }

    /// The one invoice the fake hop is planned against: signed, 100 sats.
    /// Built once: a bolt11 carries a seconds timestamp, so a rebuilt one is a different invoice.
    fn honest_invoice() -> String {
        static INVOICE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        INVOICE
            .get_or_init(|| {
                crate::lnurl_pay::test_support::signed_bolt11(
                    Some(100_000),
                    b"fake-planned-target",
                    Duration::ZERO,
                )
            })
            .clone()
    }

    fn honest_hash() -> String {
        invoice_digest(&honest_invoice())
    }

    fn pinned_journal(attempt: &str) -> HopJournal {
        HopJournal {
            invoice_sha256: Some(honest_hash()),
            ..journal(attempt)
        }
    }

    fn inflated_invoice(seed: &[u8]) -> String {
        crate::lnurl_pay::test_support::signed_bolt11(Some(29_000_000), seed, Duration::ZERO)
    }

    // ── #1292 fable r3 ─────────────────────────────────────────────────────────────────────

    #[test]
    fn the_digest_pin_accepts_case_and_padding_and_refuses_other_forms() {
        let pin = pinned_journal("attempt-1");
        let honest = honest_invoice();
        require_pinned_invoice(&pin, &honest.to_ascii_uppercase()).expect("all-uppercase is the same invoice");
        require_pinned_invoice(&pin, &format!(" {honest}\n")).expect("padding is the same invoice at melt time");
        let mixed = format!("{}{}", honest[..1].to_ascii_uppercase(), &honest[1..]);
        for other in [mixed, format!("lightning:{honest}")] {
            let error = require_pinned_invoice(&pin, &other).expect_err("must refuse");
            assert!(matches!(error, HopError::UnpinnedInvoice { undecodable: true, .. }), "got: {error}");
        }
        // At plan the hop refuses padding outright: cdk would not decode it.
        let error = require_invoice_amount(&format!("{honest}\n"), "https://b.example", 100)
            .expect_err("a padded invoice must refuse at plan");
        assert!(matches!(error, HopError::InvoiceAmountMismatch { undecodable: true, .. }), "got: {error}");
    }

    #[test]
    fn a_pinned_pairing_whose_invoice_is_about_to_expire_is_refused_terminally() {
        // 3600 s expiry, issued 3570 s ago: 30 s left, inside the window the old is_expired() missed.
        let dying = crate::lnurl_pay::test_support::signed_bolt11(Some(100_000), b"dying-pinned", Duration::from_secs(3570));
        let pin = HopJournal { invoice_sha256: Some(invoice_digest(&dying)), ..journal("attempt-1") };
        for leg in [MeltLeg::Failed, MeltLeg::Unpaid] {
            let store = MemJournal::default();
            let world = MintWorld::shared();
            world.borrow_mut().melt_leg = Some(leg);
            world.borrow_mut().target_bolt11 = dying.clone();
            world.borrow_mut().planned_melt_invoice = (dying.clone(), 100);
            let mut effects = FakeMints { world: Rc::clone(&world) };
            let error = run_hop(&store, &mut effects, &pin).expect_err("must refuse");
            assert!(matches!(error, HopError::InvoiceExpired { remaining_secs, .. } if remaining_secs <= 30), "got: {error}");
            assert!(error.to_string().contains("s of lifetime left"), "{error}");
            assert!(world.borrow().superseding_quotes.is_empty(), "no re-quote");
            assert!(world.borrow().melts.is_empty(), "no futile melt");
            assert!(refused_of(&store.replay("attempt-1").unwrap()).is_some(), "recorded as terminal");
        }
    }

    // ── #1292 review r2 (fable L1–L5) ─────────────────────────────────────────────────────

    #[test]
    fn a_pin_refusal_is_terminal_and_never_touches_the_mints_again() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().planned_melt_invoice = (inflated_invoice(b"terminal"), 29_000);
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let first = run_hop(&store, &mut effects, &journal("attempt-1")).expect_err("refuses");
        assert!(matches!(first, HopError::UnpinnedInvoice { .. }), "got: {first}");
        assert!(matches!(
            store.replay("attempt-1").unwrap().as_slice(),
            [HopRecord::Planned(_), HopRecord::Refused { .. }]
        ));
        // A second run: the source is unreachable, so any mint touch would error differently.
        world.borrow_mut().melt_leg = None;
        let second = run_hop(&store, &mut effects, &journal("attempt-1")).expect_err("refuses");
        assert!(matches!(second, HopError::HopRefused { .. }), "got: {second}");
        assert!(second.is_pin_refusal(), "the sweep must not call this a strand");
        assert!(world.borrow().melts.is_empty());
    }

    #[test]
    fn a_restart_gates_the_journalled_replacement_not_the_original_quote() {
        let store = MemJournal::default();
        store.append_sync(&HopRecord::Planned(pinned_journal("attempt-1"))).unwrap();
        store
            .append_sync(&HopRecord::MeltSuperseded {
                attempt_id: "attempt-1".into(),
                superseded_melt_quote_id: "melt-1".into(),
                melt_quote_id: "melt-2".into(),
                planned_cost: 109,
            })
            .unwrap();
        let world = MintWorld::shared();
        world.borrow_mut().melt_invoices.insert("melt-1".into(), (honest_invoice(), 100));
        world
            .borrow_mut()
            .melt_invoices
            .insert("melt-2".into(), (inflated_invoice(b"restart-replacement"), 29_000));
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let error = run_hop(&store, &mut effects, &pinned_journal("attempt-1"))
            .expect_err("the active (replacement) quote is inflated");
        assert!(matches!(error, HopError::UnpinnedInvoice { .. }), "got: {error}");
        assert!(world.borrow().melts.is_empty());
    }

    #[test]
    fn a_replacement_whose_stored_invoice_differs_refuses_before_it_is_journalled() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().superseding_invoice = Some(crate::lnurl_pay::test_support::signed_bolt11(
            Some(100_000),
            b"swapped-replacement",
            Duration::ZERO,
        ));
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let error = run_hop(&store, &mut effects, &pinned_journal("attempt-1"))
            .expect_err("a replacement for another invoice must refuse");
        assert!(
            matches!(error, HopError::UnpinnedInvoice { invoice_matches: Some(false), .. }),
            "got: {error}"
        );
        assert!(world.borrow().melts.is_empty());
        assert!(matches!(
            store.replay("attempt-1").unwrap().as_slice(),
            [HopRecord::Planned(_), HopRecord::Refused { .. }]
        ));
    }

    #[test]
    fn a_pre_pin_pairing_on_the_failed_leg_does_not_requote_an_inflated_invoice() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().target_bolt11 = inflated_invoice(b"pre-pin-failed");
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let error = run_hop(&store, &mut effects, &journal("attempt-1")).expect_err("refuses");
        assert!(
            matches!(error, HopError::UnpinnedInvoice { invoice_matches: None, .. }),
            "got: {error}"
        );
        assert!(world.borrow().superseding_quotes.is_empty());
    }

    #[test]
    fn an_rc7_journal_on_disk_loads_and_its_inflated_melt_refuses() {
        let dir = scratch_dir("rc7-on-disk");
        std::fs::create_dir_all(&dir).unwrap();
        let line = serde_json::to_string(&HopRecord::Planned(journal("attempt-rc7"))).unwrap();
        assert!(!line.contains("invoice_sha256"), "rc7 rows carry no pin");
        std::fs::write(dir.join("attempt-rc7.jsonl"), format!("{line}\n")).unwrap();
        let store = FsHopJournal::new(&dir);
        let pairing = journalled_pairing(&store, "attempt-rc7").unwrap().expect("rc7 row loads");
        assert_eq!(pairing.invoice_sha256, None);
        let world = MintWorld::shared();
        world.borrow_mut().planned_melt_invoice = (inflated_invoice(b"rc7-on-disk"), 29_000);
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let error = run_hop(&store, &mut effects, &pairing).expect_err("refuses");
        assert!(matches!(error, HopError::UnpinnedInvoice { .. }), "got: {error}");
        assert!(world.borrow().melts.is_empty());
        assert!(refused_of(&store.replay("attempt-rc7").unwrap()).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_prepared_melt_check_binds_invoice_amount_mint_and_cost() {
        let pin = pinned_journal("attempt-1");
        let mint = "https://a.example";
        let ok = |request: &str, amount, reserve, input, quote_mint: Option<&str>| {
            require_prepared_within_pin(&pin, mint, quote_mint, request, amount, reserve, input)
        };
        ok(&honest_invoice(), 100, 7, 2, Some(mint)).expect("the planned melt passes");
        assert!(matches!(
            ok(&honest_invoice(), 100, 8, 2, Some(mint)),
            Err(HopError::PreparedMeltRefused { .. })
        ));
        assert!(matches!(
            ok(&honest_invoice(), 100, 7, 2, Some("https://other.example")),
            Err(HopError::PreparedMeltRefused { .. })
        ));
        assert!(matches!(
            ok(&honest_invoice(), 100, 7, 2, None),
            Err(HopError::PreparedMeltRefused { .. })
        ));
        assert!(matches!(
            ok(&honest_invoice(), 101, 6, 2, Some(mint)),
            Err(HopError::SourceQuoteMismatch { .. })
        ));
        assert!(matches!(
            ok(&inflated_invoice(b"prepared"), 100, 7, 2, Some(mint)),
            Err(HopError::UnpinnedInvoice { .. })
        ));
    }

    #[test]
    fn an_already_paid_inflated_melt_is_reported_without_changing_the_outcome() {
        let pairing = journal("attempt-1");
        assert!(overpaid_line(&pairing, "melt-1", &honest_invoice(), 100).is_none());
        let line = overpaid_line(&pairing, "melt-1", &inflated_invoice(b"paid"), 29_000)
            .expect("an inflated paid melt is reported");
        assert!(line.starts_with("CROSSMINT OVERPAID attempt=attempt-1"), "{line}");
        assert!(line.contains("29000000 msat"), "{line}");
        // Control flow: an earlier inflated melt that already landed still completes the mint leg.
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Paid);
        world.borrow_mut().mint_leg = Some(MintLeg::Paid);
        world.borrow_mut().planned_melt_invoice = (inflated_invoice(b"paid"), 29_000);
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let settled = run_hop(&store, &mut effects, &journal("attempt-1")).expect("completes");
        assert_eq!(settled.minted_sats, 100);
    }

    #[test]
    fn plan_time_amounts_below_or_between_whole_sats_refuse() {
        let mint = "https://seller-chosen-mint.test";
        for (msat, seed) in [(99_000u64, &b"under"[..]), (100_500, &b"half-sat"[..])] {
            let bolt11 = crate::lnurl_pay::test_support::signed_bolt11(Some(msat), seed, Duration::ZERO);
            let error = require_invoice_amount(&bolt11, mint, 100).expect_err("must refuse");
            assert!(
                matches!(error, HopError::InvoiceAmountMismatch { invoice_msat: Some(m), undecodable: false, .. } if m == msat),
                "got: {error}"
            );
        }
    }

    // ── REPORT1110 F1 review r1: the pin holds at EVERY melt, not only at plan time ──────────

    #[test]
    fn a_pre_pin_pairing_whose_melt_pays_an_inflated_invoice_refuses_before_the_melt() {
        // An rc7 buyer journalled this hop with no invoice check; authorize_pay reuses it as-is.
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().planned_melt_invoice = (inflated_invoice(b"pre-pin-inflated"), 29_000);
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let error = run_hop(&store, &mut effects, &journal("attempt-1"))
            .expect_err("an inflated pre-pin hop must refuse");
        assert!(
            matches!(
                error,
                HopError::UnpinnedInvoice {
                    expected_sats: 100,
                    invoice_msat: Some(29_000_000),
                    invoice_matches: None,
                    ..
                }
            ),
            "got: {error}"
        );
        assert!(world.borrow().melts.is_empty(), "nothing may be melted");
        assert!(world.borrow().mints.is_empty());
    }

    #[test]
    fn a_pinned_pairing_refuses_a_melt_for_a_different_invoice_of_the_same_amount() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        let swapped = crate::lnurl_pay::test_support::signed_bolt11(Some(100_000), b"swapped", Duration::ZERO);
        world.borrow_mut().planned_melt_invoice = (swapped, 100);
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let error = run_hop(&store, &mut effects, &pinned_journal("attempt-1"))
            .expect_err("a different payment hash must refuse");
        assert!(
            matches!(error, HopError::UnpinnedInvoice { invoice_matches: Some(false), .. }),
            "got: {error}"
        );
        assert!(world.borrow().melts.is_empty());
    }

    #[test]
    fn a_pinned_pairing_paying_its_own_invoice_settles() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let settled = run_hop(&store, &mut effects, &pinned_journal("attempt-1"))
            .expect("the planned invoice pays");
        assert_eq!(settled.minted_sats, 100);
        assert_eq!(world.borrow().melts, vec!["melt-1"]);
    }

    #[test]
    fn recovery_refuses_a_target_invoice_that_changed_since_the_plan() {
        // cdk 0.17.2 happens to keep the stored request on a status check; the hop must not rely
        // on that. A failed melt whose target now reports a larger invoice raises no new quote.
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().target_bolt11 = inflated_invoice(b"recovery-inflated");
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let error = run_hop(&store, &mut effects, &pinned_journal("attempt-1"))
            .expect_err("a changed invoice must refuse");
        assert!(
            matches!(error, HopError::UnpinnedInvoice { invoice_msat: Some(29_000_000), .. }),
            "got: {error}"
        );
        assert!(world.borrow().superseding_quotes.is_empty(), "no replacement quote may be raised");
        assert!(world.borrow().melts.is_empty());
    }

    #[test]
    fn recovery_does_not_requote_an_expired_target_invoice() {
        // Right amount (pre-pin row, so amount is the only pin), but expired since the plan: a
        // replacement quote could only fail again, so none is raised.
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().target_bolt11 = crate::lnurl_pay::test_support::signed_bolt11(
            Some(100_000),
            b"recovery-expired",
            Duration::from_secs(7200),
        );
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let error = run_hop(&store, &mut effects, &journal("attempt-1"))
            .expect_err("an expired invoice must not be re-quoted");
        assert!(matches!(error, HopError::InvoiceExpired { .. }), "got: {error}");
        assert!(world.borrow().superseding_quotes.is_empty(), "no replacement quote may be raised");
        assert!(world.borrow().melts.is_empty());
    }

    #[test]
    fn a_superseding_quote_that_melts_more_than_the_delivery_refuses_before_it_is_journalled() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().superseding_melt_sats = Some(105);
        let mut effects = FakeMints { world: Rc::clone(&world) };
        let error = run_hop(&store, &mut effects, &journal("attempt-1"))
            .expect_err("a replacement melting more than the delivery must refuse");
        assert!(
            matches!(
                error,
                HopError::SourceQuoteMismatch { expected_sats: 100, quoted_sats: 105, .. }
            ),
            "got: {error}"
        );
        assert!(world.borrow().melts.is_empty());
        assert!(
            matches!(
                store.replay("attempt-1").unwrap().as_slice(),
                [HopRecord::Planned(_), HopRecord::Refused { .. }]
            ),
            "the refused replacement must not be journalled as active; the refusal is terminal"
        );
    }

    /// What the fake mints did, shared with the test so a restart can inspect it.
    #[derive(Default)]
    struct MintWorld {
        melt_leg: Option<MeltLeg>,
        mint_leg: Option<MintLeg>,
        melts: Vec<String>,
        mints: Vec<String>,
        /// Melt succeeds, then the process dies before the mint leg — the crash window.
        die_after_melt: bool,
        /// The melt effect refuses before firing, after its caller has journalled it.
        fail_before_melt: bool,
        mint_fails: bool,
        target_invoice_fails: bool,
        supersede_quote_fails: bool,
        superseding_quotes: Vec<(String, String, u64)>,
        superseding_planned_cost: u64,
        source_coverage: u64,
        fail_all_melts: bool,
        /// Actual Lightning fee the melt effect reports as paid (#186 reconciliation input).
        melt_fee: u64,
        /// Bolt11 the target reports for the hop's mint quote (recovery reads it).
        target_bolt11: String,
        /// What the planned melt quote pays: (bolt11, melt amount).
        planned_melt_invoice: (String, u64),
        /// Override for the melt amount of a superseding quote (default: its invoice's amount).
        superseding_melt_sats: Option<u64>,
        /// Override for the invoice a superseding quote's store row holds (default: the one it was
        /// raised against).
        superseding_invoice: Option<String>,
        /// Per-quote-id store rows, consulted before everything else.
        melt_invoices: HashMap<String, (String, u64)>,
        /// Shared with the journal so a test can assert write-before-effect order.
        ops: Rc<RefCell<Vec<String>>>,
    }

    impl MintWorld {
        fn shared() -> Rc<RefCell<Self>> {
            Rc::new(RefCell::new(Self {
                melt_leg: Some(MeltLeg::Unpaid),
                mint_leg: Some(MintLeg::Unpaid),
                superseding_planned_cost: 109,
                source_coverage: u64::MAX,
                target_bolt11: honest_invoice(),
                planned_melt_invoice: (honest_invoice(), 100),
                ..Self::default()
            }))
        }
    }

    struct FakeMints {
        world: Rc<RefCell<MintWorld>>,
    }

    impl HopEffects for FakeMints {
        fn melt_leg(&mut self, _melt_quote_id: &str) -> Result<MeltLeg, HopError> {
            self.world
                .borrow()
                .melt_leg
                .ok_or_else(|| HopError::Mint("source mint unreachable".into()))
        }

        fn mint_leg(&mut self, _mint_quote_id: &str) -> Result<MintLeg, HopError> {
            self.world
                .borrow()
                .mint_leg
                .ok_or_else(|| HopError::Mint("target mint unreachable".into()))
        }

        fn target_invoice(&mut self, _mint_quote_id: &str) -> Result<(MintLeg, String), HopError> {
            let world = self.world.borrow();
            if world.target_invoice_fails {
                return Err(HopError::Mint(
                    "target mint unreachable during recovery".into(),
                ));
            }
            let state = world
                .mint_leg
                .ok_or_else(|| HopError::Mint("target mint unreachable".into()))?;
            Ok((state, world.target_bolt11.clone()))
        }

        fn supersede_melt_quote(&mut self, bolt11: &str) -> Result<(String, u64), HopError> {
            let mut world = self.world.borrow_mut();
            if world.supersede_quote_fails {
                return Err(HopError::Mint(
                    "source mint unreachable during recovery".into(),
                ));
            }
            let id = format!("melt-{}", world.superseding_quotes.len() + 2);
            let planned_cost = world.superseding_planned_cost;
            world
                .superseding_quotes
                .push((id.clone(), bolt11.to_owned(), planned_cost));
            Ok((id, planned_cost))
        }

        fn melt_invoice(&mut self, melt_quote_id: &str) -> Result<(String, u64), HopError> {
            let world = self.world.borrow();
            if let Some(row) = world.melt_invoices.get(melt_quote_id) {
                return Ok(row.clone());
            }
            if let Some((_, raised_against, _)) = world
                .superseding_quotes
                .iter()
                .find(|(id, _, _)| id == melt_quote_id)
            {
                let bolt11 = world.superseding_invoice.clone().unwrap_or_else(|| raised_against.clone());
                let sats = world.superseding_melt_sats.unwrap_or_else(|| {
                    invoice_facts(&bolt11)
                        .and_then(|facts| facts.msat)
                        .map_or(0, |msat| msat / 1000)
                });
                return Ok((bolt11, sats));
            }
            Ok(world.planned_melt_invoice.clone())
        }

        fn require_source_covers(&mut self, planned_cost: u64) -> Result<(), HopError> {
            let world = self.world.borrow();
            if world.source_coverage < planned_cost {
                return Err(HopError::InsufficientSource {
                    mint: "https://a.example".into(),
                    balance: world.source_coverage,
                    planned_cost,
                });
            }
            Ok(())
        }

        fn melt(&mut self, melt_quote_id: &str, _pin: &HopJournal) -> Result<u64, HopError> {
            let mut world = self.world.borrow_mut();
            world.ops.borrow_mut().push(format!("melt:{melt_quote_id}"));
            if world.fail_before_melt {
                return Err(HopError::Mint("melt effect failed before firing".into()));
            }
            world.melts.push(melt_quote_id.to_owned());
            world.melt_leg = Some(if world.fail_all_melts {
                MeltLeg::Failed
            } else {
                MeltLeg::Paid
            });
            // The target's invoice is paid by the melt, so its quote flips too — unless the target
            // is unreachable (`None`), which stays unreachable no matter what the source did.
            if !world.fail_all_melts && world.mint_leg.is_some() {
                world.mint_leg = Some(MintLeg::Paid);
            }
            if world.die_after_melt {
                return Err(HopError::Mint("process died after the melt".into()));
            }
            Ok(world.melt_fee)
        }

        fn mint(&mut self, mint_quote_id: &str, expected_sats: u64) -> Result<u64, HopError> {
            let mut world = self.world.borrow_mut();
            if world.mint_fails {
                return Err(HopError::Mint("target mint refused to issue".into()));
            }
            world.mints.push(mint_quote_id.to_owned());
            world.mint_leg = Some(MintLeg::Issued);
            Ok(expected_sats)
        }
    }

    /// An in-memory journal that survives a "restart" (the store outlives the effects).
    struct MemJournal {
        records: RefCell<HashMap<String, Vec<HopRecord>>>,
        ops: Rc<RefCell<Vec<String>>>,
    }

    impl Default for MemJournal {
        fn default() -> Self {
            Self {
                records: RefCell::new(HashMap::new()),
                ops: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl MemJournal {
        fn sharing_ops(ops: Rc<RefCell<Vec<String>>>) -> Self {
            Self {
                records: RefCell::new(HashMap::new()),
                ops,
            }
        }
    }

    impl HopJournalStore for MemJournal {
        fn replay(&self, attempt_id: &str) -> Result<Vec<HopRecord>, HopError> {
            Ok(self
                .records
                .borrow()
                .get(attempt_id)
                .cloned()
                .unwrap_or_default())
        }

        fn append_sync(&self, record: &HopRecord) -> Result<(), HopError> {
            let attempt_id = match record {
                HopRecord::Planned(journal) => journal.attempt_id.clone(),
                HopRecord::MeltSuperseded { attempt_id, .. } => attempt_id.clone(),
                HopRecord::Settled { attempt_id, .. } | HopRecord::Refused { attempt_id, .. } => {
                attempt_id.clone()
            }
            };
            let mut all_records = self.records.borrow_mut();
            let records = all_records.entry(attempt_id.clone()).or_default();
            match record {
                HopRecord::Planned(_) => require_no_active_planned(records, &attempt_id)?,
                HopRecord::MeltSuperseded {
                    superseded_melt_quote_id,
                    ..
                } => {
                    require_current_superseded_melt(
                        records,
                        &attempt_id,
                        superseded_melt_quote_id,
                    )?;
                }
                HopRecord::Settled { .. } | HopRecord::Refused { .. } => {}
            }
            let label = match record {
                HopRecord::Planned(_) => "journal:Planned",
                HopRecord::MeltSuperseded { .. } => "journal:MeltSuperseded",
                HopRecord::Settled { .. } => "journal:Settled",
                HopRecord::Refused { .. } => "journal:Refused",
            };
            self.ops.borrow_mut().push(label.to_owned());
            records.push(record.clone());
            Ok(())
        }
    }

    #[test]
    fn credits_first_https_hop_settles_once_with_credit_listed_first() {
        let credit = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
        let target = "https://seller.example";
        let plan = crate::crossmint::plan_payment(
            "https://buyer.example",
            &[credit.into(), target.into()],
            true,
        )
        .unwrap();
        assert_eq!(plan.realized_mint().to_string(), target);
        let mut pairing = journal("credits-first-hop");
        pairing.source_mint = plan.source_mint().to_string();
        pairing.target_mint = plan.realized_mint().to_string();
        let store = MemJournal::default();
        let world = MintWorld::shared();
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        assert_eq!(
            run_hop(&store, &mut effects, &pairing).unwrap().minted_sats,
            100
        );
        assert_eq!(
            run_hop(&store, &mut effects, &pairing).unwrap().minted_sats,
            100
        );
        assert_eq!(world.borrow().melts.len(), 1);
        assert_eq!(world.borrow().mints.len(), 1);
    }

    #[test]
    fn a_clean_hop_melts_once_mints_once_and_journals_the_pairing_before_the_melt() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let settled = run_hop(&store, &mut effects, &journal("attempt-1")).expect("hop completes");

        assert_eq!(settled.minted_sats, 100);
        assert!(!settled.recovered_strand);
        assert_eq!(world.borrow().melts.len(), 1);
        assert_eq!(world.borrow().mints.len(), 1);

        // The pairing is the FIRST record — written before the melt, which is what makes the melt
        // leg re-enterable after a crash.
        let records = store.replay("attempt-1").expect("replays");
        assert!(matches!(records.first(), Some(HopRecord::Planned(_))));
        assert!(matches!(
            records.last(),
            Some(HopRecord::Settled {
                minted_sats: 100,
                ..
            })
        ));
    }

    // #186 RED-PROVE: a hop whose melt pays LESS than the reserved Lightning fee must report the
    // unused reserve so the caller can credit it back. `journal("…")` reserves 7 fee sats
    // (planned 109 − delivered 100 − input 2); a melt that actually pays 2 leaves 5 to credit.
    // Before #186 there was nothing to carry the difference and remaining() stayed understated.
    #[test]
    fn hop_reconciles_the_unused_fee_reserve_when_the_actual_fee_is_lower() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_fee = 2; // actual fee < reserved 7
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let settled = run_hop(&store, &mut effects, &journal("attempt-1")).expect("hop completes");
        assert_eq!(
            settled.unused_fee_reserve_sats, 5,
            "reserved 7 − actual 2 = 5 must be credited back (#186)"
        );
    }

    // #186 boundary: a melt that consumes the ENTIRE reserve credits nothing back (no phantom credit).
    #[test]
    fn hop_credits_nothing_when_the_actual_fee_equals_the_reserve() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_fee = 7; // actual fee == reserved 7
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let settled = run_hop(&store, &mut effects, &journal("attempt-1")).expect("hop completes");
        assert_eq!(
            settled.unused_fee_reserve_sats, 0,
            "a fully-consumed reserve credits nothing"
        );
    }

    // #186 fail-safe: a run that RECOVERS a melt landed on an earlier run never observed a fee this
    // pass, so it must credit nothing rather than invent a reconciliation from a fee it did not see.
    #[test]
    fn recovered_melt_reconciles_nothing() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().die_after_melt = true;
        world.borrow_mut().melt_fee = 2;
        let mut dying = FakeMints {
            world: Rc::clone(&world),
        };
        let _ = run_hop(&store, &mut dying, &journal("attempt-1"));
        // Restart: the melt already landed, so this run does not melt again.
        world.borrow_mut().die_after_melt = false;
        let mut restarted = FakeMints {
            world: Rc::clone(&world),
        };
        let settled =
            run_hop(&store, &mut restarted, &journal("attempt-1")).expect("the restart recovers");
        assert_eq!(
            world.borrow().melts.len(),
            1,
            "the restart must not melt again"
        );
        assert_eq!(
            settled.unused_fee_reserve_sats, 0,
            "a recovered melt has no observed fee to reconcile (fail-safe)"
        );
    }

    // Invariant 4, strong form. Kill between the melt and the mint, restart, and the hop must pay
    // exactly once — the second run melts ZERO times — and must say out loud that it found a strand.
    #[test]
    fn kill_between_melt_and_mint_pays_once_on_restart_and_reports_the_strand() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().die_after_melt = true;

        let mut dying = FakeMints {
            world: Rc::clone(&world),
        };
        let error = run_hop(&store, &mut dying, &journal("attempt-1"))
            .expect_err("the run dies after the melt");
        assert!(matches!(error, HopError::Mint(_)), "got: {error}");
        assert_eq!(world.borrow().melts.len(), 1, "the melt did land");
        assert!(world.borrow().mints.is_empty(), "the ecash never issued");

        // Restart: same journal on disk, fresh effects over the same mints.
        world.borrow_mut().die_after_melt = false;
        let mut restarted = FakeMints {
            world: Rc::clone(&world),
        };
        let settled =
            run_hop(&store, &mut restarted, &journal("attempt-1")).expect("the restart recovers");

        assert_eq!(settled.minted_sats, 100);
        assert!(
            settled.recovered_strand,
            "a melted-but-unissued hop is a strand and must be reported as one"
        );
        assert_eq!(
            world.borrow().melts.len(),
            1,
            "exactly-once: the restart must not melt again"
        );
        assert_eq!(world.borrow().mints.len(), 1);

        // The loud line names both quote ids and both mints, so an operator reading stderr can act
        // on it without opening the journal.
        let line = strand_line(&journal("attempt-1"), "melt-1");
        for needle in [
            "CROSSMINT STRAND",
            "attempt-1",
            "melt-1",
            "mint-1",
            "https://a.example",
            "https://b.example",
        ] {
            assert!(
                line.contains(needle),
                "strand line missing {needle}: {line}"
            );
        }
        let superseded_pairing = journal("superseded-strand");
        store
            .append_sync(&HopRecord::Planned(superseded_pairing.clone()))
            .unwrap();
        store
            .append_sync(&HopRecord::MeltSuperseded {
                attempt_id: superseded_pairing.attempt_id.clone(),
                superseded_melt_quote_id: "melt-1".into(),
                melt_quote_id: "melt-2".into(),
                planned_cost: 109,
            })
            .unwrap();
        let superseded_world = MintWorld::shared();
        superseded_world.borrow_mut().melt_leg = Some(MeltLeg::Paid);
        superseded_world.borrow_mut().mint_leg = Some(MintLeg::Paid);
        let mut superseded_effects = FakeMints {
            world: Rc::clone(&superseded_world),
        };
        let superseded = run_hop(&store, &mut superseded_effects, &superseded_pairing)
            .expect("the superseded strand recovers");
        assert!(superseded.recovered_strand);
        let active = active_melt(&store.replay("superseded-strand").unwrap())
            .unwrap()
            .0;
        let superseded_line = strand_line(&superseded_pairing, &active);
        assert!(superseded_line.contains("melt quote melt-2"));
        assert!(!superseded_line.contains("melt quote melt-1"));
    }

    // A completed hop is never re-run: neither mint is touched again, no matter how many times the
    // attempt is retried.
    #[test]
    fn a_settled_hop_touches_neither_mint_again() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        run_hop(&store, &mut effects, &journal("attempt-1")).expect("hop completes");

        for _ in 0..3 {
            let again =
                run_hop(&store, &mut effects, &journal("attempt-1")).expect("replay is a no-op");
            assert_eq!(again.minted_sats, 100);
            assert!(!again.recovered_strand);
        }
        assert_eq!(world.borrow().melts.len(), 1);
        assert_eq!(world.borrow().mints.len(), 1);
    }

    // Money in flight is not a licence to melt again. The source mint saying "pending" refuses.
    #[test]
    fn a_pending_melt_refuses_instead_of_melting_again() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Pending);
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let error = run_hop(&store, &mut effects, &journal("attempt-1"))
            .expect_err("an in-flight melt must refuse");
        assert!(
            matches!(error, HopError::MeltInFlight { .. }),
            "got: {error}"
        );
        assert!(
            world.borrow().melts.is_empty(),
            "never melt while a melt is in flight"
        );
    }

    // Freshly planned quotes must NOT override a pairing already on disk — that is the double-melt.
    #[test]
    fn a_second_pairing_for_one_attempt_is_refused_rather_than_melted() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().die_after_melt = true;
        let mut dying = FakeMints {
            world: Rc::clone(&world),
        };
        let _ = run_hop(&store, &mut dying, &journal("attempt-1"));
        assert_eq!(world.borrow().melts.len(), 1);

        // The retry re-planned and arrived with brand new quote ids.
        let mut replanned = journal("attempt-1");
        replanned.melt_quote_id = "melt-2".to_owned();
        replanned.mint_quote_id = "mint-2".to_owned();
        world.borrow_mut().die_after_melt = false;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let error = run_hop(&store, &mut effects, &replanned)
            .expect_err("a conflicting pairing must refuse");
        assert!(
            matches!(error, HopError::PairingConflict { .. }),
            "got: {error}"
        );
        assert_eq!(
            world.borrow().melts.len(),
            1,
            "the conflicting pairing must not have melted"
        );
    }

    // The send that follows hands the seller exactly the offer amount, so a short issue is refused
    // here rather than carried into the send path.
    #[test]
    fn issuing_less_than_the_pinned_delivery_amount_refuses() {
        struct ShortMint;
        impl HopEffects for ShortMint {
            fn melt_leg(&mut self, _: &str) -> Result<MeltLeg, HopError> {
                Ok(MeltLeg::Unpaid)
            }
            fn mint_leg(&mut self, _: &str) -> Result<MintLeg, HopError> {
                Ok(MintLeg::Paid)
            }
            fn target_invoice(&mut self, _: &str) -> Result<(MintLeg, String), HopError> {
                unreachable!()
            }
            fn supersede_melt_quote(&mut self, _: &str) -> Result<(String, u64), HopError> {
                unreachable!()
            }
            fn require_source_covers(&mut self, _: u64) -> Result<(), HopError> {
                unreachable!()
            }
            fn melt_invoice(&mut self, _: &str) -> Result<(String, u64), HopError> {
                Ok((honest_invoice(), 100))
            }
            fn melt(&mut self, _: &str, _: &HopJournal) -> Result<u64, HopError> {
                Ok(0)
            }
            fn mint(&mut self, _: &str, expected_sats: u64) -> Result<u64, HopError> {
                Ok(expected_sats.saturating_sub(1))
            }
        }
        let store = MemJournal::default();
        let error = run_hop(&store, &mut ShortMint, &journal("attempt-1"))
            .expect_err("a short issue must refuse");
        assert!(
            matches!(
                error,
                HopError::MintedAmountMismatch {
                    expected: 100,
                    minted: 99
                }
            ),
            "got: {error}"
        );
        assert!(
            settled_of(&store.replay("attempt-1").expect("replays")).is_none(),
            "a refused hop must not journal a completion"
        );
    }

    /// A fresh journal directory for one test, named so concurrent test binaries cannot collide.
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "maxplayer-crossmint-journal-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    async fn cdk_hop_with_source(source_mint: &str) -> CdkHopEffects {
        let source_store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let target_store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        CdkHopEffects {
            source: Wallet::new(
                source_mint,
                cashu::CurrencyUnit::Sat,
                source_store,
                [9; 64],
                None,
            )
            .unwrap(),
            target: Wallet::new(
                "https://127.0.0.1:1",
                cashu::CurrencyUnit::Sat,
                target_store,
                [10; 64],
                None,
            )
            .unwrap(),
            hop_fee_buffer_multiplier: default_hop_fee_buffer_multiplier(),
            wallet_dir: None,
        }
    }

    /// Review r6 H1: the hop melt runs the mint payment gate before it selects anything.
    #[test]
    fn hop_melt_is_refused_while_a_crashed_send_waits_on_its_swap() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (mut effects, scene) = runtime.block_on(async {
            let mut effects = cdk_hop_with_source("https://127.0.0.1:1").await;
            effects.wallet_dir = Some(dir.path().to_path_buf());
            let scene =
                crate::payment_wallet::gate_scene::crashed_send(&effects.source, true).await;
            (effects, scene)
        });
        let error = effects
            .melt("r6-quote", &journal("attempt-r6"))
            .expect_err("SAFETY (r6 H1): the hop must not melt other proofs");
        crate::payment_wallet::gate_scene::assert_refusal(&error.to_string(), &scene);
        runtime.block_on(crate::payment_wallet::gate_scene::assert_nothing_selected(
            &effects.source,
            &scene,
        ));
    }
    async fn cdk_hop_with_target(target_mint: &str) -> CdkHopEffects {
        let source_store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let target_store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        CdkHopEffects {
            source: Wallet::new(
                "https://127.0.0.1:1",
                cashu::CurrencyUnit::Sat,
                source_store,
                [7; 64],
                None,
            )
            .unwrap(),
            target: Wallet::new(
                target_mint,
                cashu::CurrencyUnit::Sat,
                target_store,
                [8; 64],
                None,
            )
            .unwrap(),
            hop_fee_buffer_multiplier: default_hop_fee_buffer_multiplier(),
            wallet_dir: None,
        }
    }

    // ── REPORT1110 F1: the target mint is SELLER-chosen, so its invoice is untrusted ──────────────
    //
    // A scripted mint over cdk's own HTTP client: `/v1/info`, `/v1/keysets`, `/v1/keys`, the bolt11
    // mint quote (target) and melt quote (source). The target answers a quote request for `n` sats
    // with a REAL signed bolt11 for whatever the test chooses; the source prices whatever invoice it
    // is shown and counts how often it was asked.
    #[derive(Clone, Debug, Default)]
    struct ScriptedMint {
        keyset: Option<cashu::KeySet>,
        invoice: String,
        melt_amount: u64,
        fee_reserve: u64,
        melt_quotes: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl cdk::wallet::HttpTransport for ScriptedMint {
        fn with_proxy(&mut self, _: url::Url, _: Option<&str>, _: bool) -> Result<(), cdk::Error> {
            Ok(())
        }

        async fn http_get<R>(&self, url: url::Url, _: Option<cashu::nuts::AuthToken>) -> Result<R, cdk::Error>
        where
            R: serde::de::DeserializeOwned,
        {
            let keyset = self.keyset.clone().expect("scripted keyset");
            let path = url.path().to_owned();
            let value = if path.ends_with("/v1/keysets") {
                serde_json::to_value(cashu::KeysetResponse {
                    keysets: vec![cashu::KeySetInfo {
                        id: keyset.id,
                        unit: keyset.unit.clone(),
                        active: true,
                        input_fee_ppk: 0,
                        final_expiry: None,
                    }],
                })
            } else if path.contains("/v1/keys") {
                serde_json::to_value(cashu::KeysResponse { keysets: vec![keyset] })
            } else if path.ends_with("/v1/info") {
                serde_json::to_value(cashu::MintInfo::new())
            } else {
                return Err(cdk::Error::Custom(format!("unexpected GET {path}")));
            };
            serde_json::from_value(value.map_err(|e| cdk::Error::Custom(e.to_string()))?)
                .map_err(|e| cdk::Error::Custom(e.to_string()))
        }

        async fn http_post<P, R>(&self, url: url::Url, _: Option<cashu::nuts::AuthToken>, _: &P) -> Result<R, cdk::Error>
        where
            P: serde::Serialize + ?Sized + Send + Sync,
            R: serde::de::DeserializeOwned,
        {
            let path = url.path().to_owned();
            let value = if path.ends_with("/v1/mint/quote/bolt11") {
                serde_json::json!({
                    "quote": "target-quote-1",
                    "request": self.invoice,
                    "amount": null,
                    "unit": "sat",
                    "state": "UNPAID",
                    "expiry": 4_000_000_000u64,
                })
            } else if path.ends_with("/v1/melt/quote/bolt11") {
                self.melt_quotes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                serde_json::json!({
                    "quote": "source-melt-1",
                    "amount": self.melt_amount,
                    "fee_reserve": self.fee_reserve,
                    "state": "UNPAID",
                    "expiry": 4_000_000_000u64,
                })
            } else {
                return Err(cdk::Error::Custom(format!("unexpected POST {path}")));
            };
            serde_json::from_value(value).map_err(|e| cdk::Error::Custom(e.to_string()))
        }
    }

    fn scripted_keyset(seed: u8) -> cashu::KeySet {
        let keys = (0..16u32)
            .map(|bit| {
                let amount = cashu::Amount::from(1u64 << bit);
                let key = cashu::SecretKey::from_slice(&[seed.wrapping_add(bit as u8 + 1); 32]).unwrap();
                (amount, key.public_key())
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let keys = cashu::Keys::new(keys);
        cashu::KeySet {
            id: cashu::Id::v2_from_data(&keys, &cashu::CurrencyUnit::Sat, 0, None),
            unit: cashu::CurrencyUnit::Sat,
            active: Some(true),
            keys,
            input_fee_ppk: 0,
            final_expiry: None,
        }
    }

    /// A wallet at `mint_url` speaking to `transport`, holding `balance` (power-of-two proofs).
    async fn scripted_wallet(mint_url: &str, transport: ScriptedMint, balance: u64, seed: u8) -> Wallet {
        use cdk::cdk_database::WalletDatabase as _;
        let url: cdk::mint_url::MintUrl = mint_url.parse().unwrap();
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        store.add_mint(url.clone(), Some(cashu::MintInfo::new())).await.unwrap();
        let keyset_id = transport.keyset.as_ref().unwrap().id;
        let proofs = (0..16u32)
            .filter(|bit| balance & (1u64 << bit) != 0)
            .map(|bit| {
                let proof = cashu::Proof::new(
                    cashu::Amount::from(1u64 << bit),
                    keyset_id,
                    cashu::secret::Secret::generate(),
                    cashu::SecretKey::from_slice(&[9; 32]).unwrap().public_key(),
                );
                cdk::wallet::types::ProofInfo::new(proof, url.clone(), cashu::State::Unspent, cashu::CurrencyUnit::Sat).unwrap()
            })
            .collect::<Vec<_>>();
        store.update_proofs(proofs, vec![]).await.unwrap();
        let connector = Arc::new(cdk::wallet::BaseHttpClient::with_transport(url.clone(), transport, None));
        cdk::wallet::WalletBuilder::new()
            .mint_url(url)
            .unit(cashu::CurrencyUnit::Sat)
            .localstore(store)
            .seed([seed; 64])
            .shared_client(connector)
            .build()
            .unwrap()
    }

    /// Hop effects whose target mint answers every quote with `invoice`, and whose source (the
    /// buyer's own mint, funded with 30 000 sats) prices it at `melt_amount` + a 290-sat reserve.
    async fn scripted_hop(invoice: String, melt_amount: u64) -> (CdkHopEffects, Arc<std::sync::atomic::AtomicUsize>) {
        let melt_quotes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = ScriptedMint {
            keyset: Some(scripted_keyset(40)),
            melt_amount,
            fee_reserve: 290,
            melt_quotes: melt_quotes.clone(),
            ..Default::default()
        };
        let target = ScriptedMint { keyset: Some(scripted_keyset(80)), invoice, ..Default::default() };
        let effects = CdkHopEffects {
            source: scripted_wallet("https://buyer-mint.test", source, 30_000, 11).await,
            target: scripted_wallet("https://seller-chosen-mint.test", target, 0, 12).await,
            hop_fee_buffer_multiplier: default_hop_fee_buffer_multiplier(),
            wallet_dir: None,
        };
        (effects, melt_quotes)
    }

    #[tokio::test]
    async fn plan_quotes_refuses_a_target_invoice_above_the_offer_amount() {
        // Asked for 100 sats, the seller-chosen mint returns a signed invoice for 29 000.
        let inflated = crate::lnurl_pay::test_support::signed_bolt11(
            Some(29_000_000),
            b"f1-inflated",
            Duration::ZERO,
        );
        let (effects, melt_quotes) = scripted_hop(inflated, 29_000).await;
        let error = effects
            .plan_quotes("attempt-f1-inflated", 100)
            .await
            .expect_err("an invoice above the offer amount must refuse the hop");
        assert!(
            matches!(
                error,
                HopError::InvoiceAmountMismatch { expected_sats: 100, invoice_msat: Some(29_000_000), .. }
            ),
            "got: {error:?}"
        );
        assert_eq!(
            melt_quotes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the source mint must never be asked to price the inflated invoice"
        );
        assert!(
            effects.target.localstore.get_mint_quotes().await.unwrap().is_empty(),
            "the refused target quote must not stay in the buyer's wallet store"
        );
    }

    #[tokio::test]
    async fn plan_quotes_refuses_a_target_invoice_with_less_than_one_leg_of_lifetime_left() {
        // 3600 s expiry, issued 3570 s ago: 30 s left, under one hop leg.
        let dying = crate::lnurl_pay::test_support::signed_bolt11(
            Some(100_000),
            b"f1-dying",
            Duration::from_secs(3570),
        );
        let (effects, melt_quotes) = scripted_hop(dying, 100).await;
        let error = effects
            .plan_quotes("attempt-f1-dying", 100)
            .await
            .expect_err("an invoice about to expire must refuse");
        assert!(matches!(error, HopError::InvoiceExpired { .. }), "got: {error}");
        assert_eq!(melt_quotes.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_sub_millisatoshi_invoice_amount_is_undecodable_and_refused() {
        use cdk::lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret, SiPrefix};
        use cdk::secp256k1::hashes::{Hash as _, sha256};
        use cdk::secp256k1::{Secp256k1, SecretKey};
        let key = SecretKey::from_slice(&[0x42; 32]).expect("32 bytes is a key");
        let build = |pico: u64| {
            let mut raw = InvoiceBuilder::new(Currency::Bitcoin)
                .description("sub-msat".into())
                .payment_hash(sha256::Hash::hash(b"f1-sub-msat"))
                .payment_secret(PaymentSecret([7u8; 32]))
                .current_timestamp()
                .min_final_cltv_expiry_delta(144)
                .amount_milli_satoshis(100_000)
                .build_raw()
                .expect("raw invoice");
            raw.hrp.raw_amount = Some(pico);
            raw.hrp.si_prefix = Some(SiPrefix::Pico);
            raw.sign::<_, ()>(|hash| Ok(Secp256k1::new().sign_ecdsa_recoverable(hash, &key)))
                .expect("signs")
                .to_string()
        };
        // Control (#1292 fable r3 N3): the same construction with whole millisatoshis decodes.
        assert_eq!(invoice_facts(&build(1_000_000)).and_then(|facts| facts.msat), Some(100_000));
        assert_eq!(invoice_facts(&build(1_000_010)).and_then(|facts| facts.msat), Some(100_001));
        // 1 000 001 pico-BTC = 100 000.1 msat: not a whole millisatoshi.
        let bolt11 = build(1_000_001);
        assert!(invoice_facts(&bolt11).is_none(), "an imprecise amount must not decode");
        let error = require_invoice_amount(&bolt11, "https://seller-chosen-mint.test", 100)
            .expect_err("a sub-msat amount must refuse");
        assert!(
            matches!(
                error,
                HopError::InvoiceAmountMismatch { invoice_msat: None, undecodable: true, .. }
            ),
            "got: {error}"
        );
    }

    #[test]
    fn refusals_tell_an_undecodable_invoice_from_an_amountless_one() {
        let mint = "https://seller-chosen-mint.test";
        let garbage = require_invoice_amount("lnbc-not-an-invoice", mint, 100).unwrap_err();
        assert!(
            matches!(garbage, HopError::InvoiceAmountMismatch { undecodable: true, .. }),
            "got: {garbage}"
        );
        assert!(garbage.to_string().contains("an undecodable invoice"), "got: {garbage}");
        let amountless =
            crate::lnurl_pay::test_support::signed_bolt11(None, b"f1-no-amount", Duration::ZERO);
        let none = require_invoice_amount(&amountless, mint, 100).unwrap_err();
        assert!(
            matches!(none, HopError::InvoiceAmountMismatch { undecodable: false, invoice_msat: None, .. }),
            "got: {none}"
        );
        assert!(none.to_string().contains("an invoice with no amount"), "got: {none}");
    }

    #[tokio::test]
    async fn plan_quotes_refuses_an_amountless_target_invoice() {
        let amountless =
            crate::lnurl_pay::test_support::signed_bolt11(None, b"f1-amountless", Duration::ZERO);
        let (effects, melt_quotes) = scripted_hop(amountless, 100).await;
        let error = effects
            .plan_quotes("attempt-f1-amountless", 100)
            .await
            .expect_err("an amountless invoice must refuse the hop");
        assert!(
            matches!(error, HopError::InvoiceAmountMismatch { invoice_msat: None, .. }),
            "got: {error:?}"
        );
        assert_eq!(melt_quotes.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // Positive control: an honest target (invoice == offer amount) still plans, and the planned
    // cost stays offer + fee allowance, nowhere near the per-job cap.
    #[tokio::test]
    async fn plan_quotes_accepts_a_target_invoice_for_exactly_the_offer_amount() {
        let honest =
            crate::lnurl_pay::test_support::signed_bolt11(Some(100_000), b"f1-honest", Duration::ZERO);
        let (effects, melt_quotes) = scripted_hop(honest.clone(), 100).await;
        let pairing = effects.plan_quotes("attempt-f1-honest", 100).await.expect("honest hop plans");
        assert_eq!(pairing.delivered_sats, 100);
        assert_eq!(pairing.fee_reserve, 290);
        assert_eq!(
            pairing.invoice_sha256,
            Some(invoice_digest(&honest)),
            "the planned invoice's digest is journalled"
        );
        let multiplier = default_hop_fee_buffer_multiplier();
        assert_eq!(pairing.planned_cost, 100 + 290 + 290 * multiplier, "melt + reserve + buffer");
        assert_eq!(melt_quotes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn plan_quotes_refuses_an_expired_target_invoice() {
        let expired = crate::lnurl_pay::test_support::signed_bolt11(
            Some(100_000),
            b"f1-expired",
            Duration::from_secs(7200),
        );
        let (effects, melt_quotes) = scripted_hop(expired, 100).await;
        let error = effects
            .plan_quotes("attempt-f1-expired", 100)
            .await
            .expect_err("an expired invoice must refuse");
        assert!(matches!(error, HopError::InvoiceExpired { .. }), "got: {error}");
        assert_eq!(melt_quotes.load(std::sync::atomic::Ordering::SeqCst), 0, "source never quoted");
    }

    #[tokio::test]
    async fn cdk_melt_invoice_reads_the_planned_quote_from_the_wallet_store() {
        let honest =
            crate::lnurl_pay::test_support::signed_bolt11(Some(100_000), b"f1-store", Duration::ZERO);
        let (effects, _) = scripted_hop(honest.clone(), 100).await;
        let pairing = effects.plan_quotes("attempt-f1-store", 100).await.expect("honest hop plans");
        let quote_id = pairing.melt_quote_id.clone();
        let (bolt11, melt_sats) = tokio::task::spawn_blocking(move || {
            let mut effects = effects;
            effects.melt_invoice(&quote_id)
        })
        .await
        .expect("join")
        .expect("the planned melt quote is in the store");
        assert_eq!(bolt11, honest);
        assert_eq!(melt_sats, 100);
        require_pinned_invoice(&pairing, &bolt11).expect("the planned quote pays the pinned invoice");
    }

    #[tokio::test]
    async fn every_plan_refusal_leaves_no_quote_in_the_buyer_stores() {
        let honest = crate::lnurl_pay::test_support::signed_bolt11(Some(100_000), b"f1-cleanup", Duration::ZERO);
        // Padded invoice: refused at the pin, before the source is asked.
        let (effects, melt_quotes) = scripted_hop(format!("{honest}\n"), 100).await;
        effects.plan_quotes("attempt-padded", 100).await.expect_err("padding refuses");
        assert_eq!(melt_quotes.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(effects.target.localstore.get_mint_quotes().await.unwrap().is_empty());
        // Source prices the hop wrong: refused after the target quote exists.
        let (effects, _) = scripted_hop(honest, 101).await;
        effects.plan_quotes("attempt-mismatch", 100).await.expect_err("mismatch refuses");
        assert!(effects.target.localstore.get_mint_quotes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_reused_target_quote_id_is_refused_and_the_earlier_row_kept() {
        let honest = crate::lnurl_pay::test_support::signed_bolt11(Some(100_000), b"f1-reuse", Duration::ZERO);
        // The scripted mint always answers with id `target-quote-1`.
        let (effects, _) = scripted_hop(honest, 100).await;
        effects.plan_quotes("attempt-first", 100).await.expect("first plan");
        let error = effects.plan_quotes("attempt-second", 100).await.expect_err("reused id refuses");
        assert!(matches!(error, HopError::TargetQuoteIdReused { .. }), "got: {error}");
        assert_eq!(effects.target.localstore.get_mint_quotes().await.unwrap().len(), 1, "earlier row kept");
    }

    #[tokio::test]
    async fn cdk_melt_invoice_refuses_a_quote_missing_from_the_store() {
        let honest =
            crate::lnurl_pay::test_support::signed_bolt11(Some(100_000), b"f1-missing", Duration::ZERO);
        let (effects, _) = scripted_hop(honest, 100).await;
        let error = tokio::task::spawn_blocking(move || {
            let mut effects = effects;
            effects.melt_invoice("no-such-quote")
        })
        .await
        .expect("join")
        .expect_err("an unknown quote must refuse");
        assert!(error.to_string().contains("not in the wallet store"), "got: {error}");
    }

    // #1292 review L1/L5: the real effect re-checks the prepared quote before confirm. A store row
    // tampered after planning (fee reserve raised past the authorized cost) is cancelled before any
    // melt request: the scripted source would answer a POST /v1/melt/bolt11 with a different error.
    #[tokio::test]
    async fn cdk_melt_cancels_a_prepared_melt_outside_the_authorized_cost() {
        let honest =
            crate::lnurl_pay::test_support::signed_bolt11(Some(100_000), b"f1-prepared", Duration::ZERO);
        let (effects, _) = scripted_hop(honest, 100).await;
        let pairing = effects.plan_quotes("attempt-f1-prepared", 100).await.expect("plans");
        let mut row = effects
            .source
            .localstore
            .get_melt_quote(&pairing.melt_quote_id)
            .await
            .unwrap()
            .expect("planned quote stored");
        row.fee_reserve = cdk::Amount::from(pairing.planned_cost - 100 + 1);
        effects.source.localstore.add_melt_quote(row).await.unwrap();
        let before = effects.source.total_balance().await.unwrap();
        let (effects, result) = tokio::task::spawn_blocking(move || {
            let mut effects = effects;
            let result = effects.melt(&pairing.melt_quote_id, &pairing);
            (effects, result)
        })
        .await
        .expect("join");
        let error = result.expect_err("a melt above the authorized cost must refuse");
        assert!(matches!(error, HopError::PreparedMeltRefused { .. }), "got: {error}");
        assert_eq!(
            effects.source.total_balance().await.unwrap(),
            before,
            "the cancelled melt must release its proofs"
        );
    }

    #[tokio::test]
    async fn crossmint_hop_plan_quotes_classifies_502_as_mint_unreachable_without_journal() {
        let (target_mint, responder) = crate::payment_wallet::http_502_mint();
        let effects = cdk_hop_with_target(&target_mint).await;
        let journal_dir = scratch_dir("plan-502-no-journal");
        let store = FsHopJournal::new(&journal_dir);

        let error = effects
            .plan_quotes("attempt-502", 100)
            .await
            .expect_err("a target mint returning 502 must refuse quote planning");
        responder.join().unwrap();

        assert!(
            matches!(
                error,
                HopError::MintUnreachable { ref label, .. } if label == "target mint quote"
            ),
            "502 must be a typed mint-unreachable refusal, got: {error}"
        );
        assert!(
            !store.path_for("attempt-502").exists(),
            "quote-planning refusal must not create an attempt journal"
        );
        assert!(
            !journal_dir.exists(),
            "quote-planning refusal must not mutate the journal directory"
        );
    }

    #[tokio::test]
    async fn crossmint_hop_plan_quotes_classifies_connection_refused_without_journal() {
        let effects = cdk_hop_with_target("https://127.0.0.1:1").await;
        let journal_dir = scratch_dir("plan-refused-no-journal");
        let store = FsHopJournal::new(&journal_dir);

        let error = effects
            .plan_quotes("attempt-refused", 100)
            .await
            .expect_err("an unreachable target mint must refuse quote planning");

        assert!(
            matches!(
                error,
                HopError::MintUnreachable { ref label, .. } if label == "target mint quote"
            ),
            "transport failure must be a typed mint-unreachable refusal, got: {error}"
        );
        assert!(
            !store.path_for("attempt-refused").exists(),
            "quote-planning refusal must not create an attempt journal"
        );
        assert!(
            !journal_dir.exists(),
            "quote-planning refusal must not mutate the journal directory"
        );
    }

    #[tokio::test]
    async fn bounded_classifies_5xx_timeout_and_transport_as_mint_unreachable() {
        // cdk's check_mint_quote errors LOCALLY for a quote id absent from the wallet store, so
        // the target-invoice leg cannot be driven to the wire without a fuller mint mock (#91
        // territory). The wire-level 502 path is proven end-to-end once by
        // crossmint_hop_recovery_source_quote_502_refuses_without_supersession_record; here the
        // shared classification point — bounded() — is proven directly for every arm.
        let err = bounded::<()>("target invoice", Duration::from_secs(1), async {
            Err(cdk::Error::HttpError(Some(502), "bad gateway".to_owned()))
        })
        .await
        .unwrap_err();
        assert!(
            matches!(err, HopError::MintUnreachable { ref label, .. } if label == "target invoice")
        );

        let err = bounded::<()>("target invoice", Duration::from_secs(1), async {
            Err(cdk::Error::HttpError(None, "connection refused".to_owned()))
        })
        .await
        .unwrap_err();
        assert!(matches!(err, HopError::MintUnreachable { .. }));

        let err = bounded::<()>(
            "target invoice",
            Duration::from_millis(10),
            std::future::pending(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, HopError::MintUnreachable { .. }),
            "a timeout means the mint would not answer: {err:?}"
        );

        let err = bounded::<()>("target invoice", Duration::from_secs(1), async {
            Err(cdk::Error::HttpError(Some(400), "bad request".to_owned()))
        })
        .await
        .unwrap_err();
        assert!(
            matches!(err, HopError::Mint(_)),
            "a 4xx is a protocol bug, never clean-cancelled as downtime: {err:?}"
        );
    }

    // #1034 re-review: the opener is the guard for journal replay (`sweep_hops`) and authorize_pay's
    // hop path. A nostr:// source must be refused before either wallet is opened.
    #[tokio::test]
    async fn a_nostr_target_cannot_open_a_hop() {
        let root = scratch_dir("nostr-target-open");
        let home = crate::home::bootstrap(&root).expect("bootstrap");
        let nostr = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
        let Err(error) = CdkHopEffects::open(&home, "https://127.0.0.1:1", nostr).await else {
            panic!("a nostr:// hop target opened");
        };
        assert!(
            matches!(error, HopError::Mint(ref detail) if detail.contains("cannot be a hop target")),
            "{error:?}"
        );
        assert!(!root.join("crossmint-journal").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_nostr_source_cannot_open_a_hop() {
        let root = scratch_dir("nostr-source-open");
        let home = crate::home::bootstrap(&root).expect("bootstrap");
        let nostr = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
        let Err(error) = CdkHopEffects::open(&home, nostr, "https://seller.example").await else {
            panic!("a nostr:// hop source opened");
        };
        assert!(
            matches!(error, HopError::Mint(ref detail) if detail.contains("cannot be a hop source")),
            "unexpected refusal: {error:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn crossmint_hop_recovery_source_quote_502_refuses_without_supersession_record() {
        let (source_mint, responder) = crate::payment_wallet::http_502_mint();
        let mut effects = cdk_hop_with_source(&source_mint).await;
        let store = MemJournal::default();
        store
            .append_sync(&HopRecord::Planned(journal("source-quote-502")))
            .unwrap();
        // A REAL (BOLT11 spec test-vector) invoice: cdk parses the bolt11 locally before any
        // HTTP, so a placeholder string errors client-side, the one-shot responder never serves
        // its request, and responder.join() deadlocks the test. The request must reach the wire.
        let error = effects
            .supersede_melt_quote("lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqqsgq2a25dxl5hrntdtn6zvydt7d66hyzsyhqs4wdynavys42xgl6sgx9c4g7me86a27t07mdtfry458rtjr0v92cnmswpsjscgt2vcse3sgpz3uapa")
            .unwrap_err();
        assert!(
            matches!(error, HopError::MintUnreachable { ref label, .. } if label == "source melt quote"),
            "expected MintUnreachable(source melt quote), got: {error:?}"
        );
        responder.join().unwrap();
        assert!(
            !store
                .replay("source-quote-502")
                .unwrap()
                .iter()
                .any(|record| matches!(record, HopRecord::MeltSuperseded { .. }))
        );
    }

    #[test]
    fn the_file_journal_round_trips_a_pairing_supersession_and_completion() {
        let store = FsHopJournal::new(scratch_dir("round-trip"));
        assert!(store.replay("attempt-1").expect("empty replays").is_empty());

        let pairing = journal("attempt-1");
        store
            .append_sync(&HopRecord::Planned(pairing.clone()))
            .expect("pairing appends");
        store
            .append_sync(&HopRecord::MeltSuperseded {
                attempt_id: "attempt-1".to_owned(),
                superseded_melt_quote_id: "melt-1".to_owned(),
                melt_quote_id: "melt-2".to_owned(),
                planned_cost: 111,
            })
            .expect("supersession appends");
        store
            .append_sync(&HopRecord::Settled {
                attempt_id: "attempt-1".to_owned(),
                minted_sats: 100,
            })
            .expect("completion appends");

        let records = store.replay("attempt-1").expect("replays");
        assert_eq!(records.len(), 3);
        assert_eq!(planned_of(&records), Some(&pairing));
        assert_eq!(active_melt(&records), Some(("melt-2".to_owned(), 1)));
        assert_eq!(settled_of(&records), Some(100));
        // Attempts do not see each other's records.
        assert!(store.replay("attempt-2").expect("replays").is_empty());
    }

    // Invariant 3, strong form. A cap one sat under the hop's planned cost must stop the hop BEFORE
    // it melts — not fail it afterwards, by which point the sats have already left the buyer's
    // wallet. Driven through the real `BudgetGate`, because the property being tested is that the
    // gate never reaches the effect, and a fake gate would be testing the fake.
    #[test]
    fn a_cap_one_sat_under_the_planned_cost_means_no_melt_is_attempted_at_all() {
        let pairing = journal("attempt-1");
        let store = MemJournal::default();
        let world = MintWorld::shared();
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        // The per-job cap sits one sat under what the hop would cost.
        let under = pairing.planned_cost.saturating_sub(1);
        let mut gate = BudgetGate::new(under);
        let refusal = gate
            .authorize_then_attempt(&pairing.attempt_id, pairing.planned_cost, || {
                run_hop(&store, &mut effects, &pairing)
            })
            .expect_err("a cap under the planned cost must refuse");

        assert!(matches!(refusal, BudgetRefuse::PerJob { .. }), "{refusal}");
        assert!(
            world.borrow().melts.is_empty(),
            "the melt must never be attempted when the cap refuses"
        );
        assert!(world.borrow().mints.is_empty());
        assert_eq!(gate.spent(), 0, "a refused hop spends nothing");
        assert!(
            store.replay("attempt-1").expect("replays").is_empty(),
            "a refused hop leaves no pairing on disk"
        );

        // The same hop at a cap that covers it does melt — so the refusal above came from the cap
        // and not from something else refusing first.
        let mut gate = BudgetGate::new(pairing.planned_cost);
        gate.authorize_then_attempt(&pairing.attempt_id, pairing.planned_cost, || {
            run_hop(&store, &mut effects, &pairing)
        })
        .expect("the cap admits the hop")
        .expect("the hop completes");
        assert_eq!(world.borrow().melts.len(), 1);
        assert_eq!(gate.spent(), pairing.planned_cost);
    }

    // The cap must see the fee, not just the delivery. A cap sized to the amount the seller receives
    // is NOT enough to authorize the hop that delivers it — which is the whole reason the hop is
    // charged its planned cost rather than the offer amount.
    #[test]
    fn a_cap_sized_to_the_delivered_amount_does_not_authorize_the_hop_that_delivers_it() {
        let pairing = journal("attempt-1");
        assert!(
            pairing.planned_cost > pairing.delivered_sats,
            "the fixture must actually cost more than it delivers"
        );
        let store = MemJournal::default();
        let world = MintWorld::shared();
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        let mut gate = BudgetGate::new(pairing.delivered_sats);
        let refusal = gate
            .authorize_then_attempt(&pairing.attempt_id, pairing.planned_cost, || {
                run_hop(&store, &mut effects, &pairing)
            })
            .expect_err("the fee reserve and input fee must not slip past the cap");
        assert!(matches!(refusal, BudgetRefuse::PerJob { .. }), "{refusal}");
        assert!(world.borrow().melts.is_empty());
    }

    // Invariant 4's trap, made into a refusal. cdk's saga recovery filters to its own wallet's
    // mint, so a sweep that ran on one wallet would report success having examined half the hop.
    #[test]
    fn a_sweep_that_covered_one_mint_refuses_to_call_the_hop_swept() {
        let pairing = journal("attempt-1");
        require_both_mints_recovered(
            &[pairing.source_mint.clone(), pairing.target_mint.clone()],
            &pairing,
        )
        .expect("both mints covered");

        for half in [&pairing.source_mint, &pairing.target_mint] {
            let error = require_both_mints_recovered(std::slice::from_ref(half), &pairing)
                .expect_err("one mint is not a recovery");
            assert!(error.to_string().contains("half its mints"), "got: {error}");
        }
        // A recovery that touched two mints, but not the RIGHT two, is no better.
        let error = require_both_mints_recovered(
            &[
                "https://elsewhere.example".to_owned(),
                pairing.source_mint.clone(),
            ],
            &pairing,
        )
        .expect_err("the wrong second mint is not a recovery");
        assert!(
            error.to_string().contains(&pairing.target_mint),
            "got: {error}"
        );
    }

    #[test]
    fn a_failed_melt_with_unreachable_recovery_refuses_without_melting_again() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().target_invoice_fails = true;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let error = run_hop(&store, &mut effects, &journal("attempt-1"))
            .expect_err("unreachable recovery must refuse");
        assert!(matches!(error, HopError::Mint(_)), "got: {error}");
        assert!(world.borrow().melts.is_empty());
        assert!(world.borrow().mints.is_empty());
    }

    #[test]
    fn a_failed_melt_supersedes_the_same_invoice_and_settles() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let settled = run_hop(&store, &mut effects, &journal("attempt-1"))
            .expect("the superseding melt completes");
        assert_eq!(settled.minted_sats, 100);
        assert_eq!(world.borrow().melts, vec!["melt-2"]);
        assert_eq!(
            world.borrow().superseding_quotes[0].1,
            honest_invoice()
        );
        let records = store.replay("attempt-1").unwrap();
        assert!(matches!(
            records.as_slice(),
            [
                HopRecord::Planned(_),
                HopRecord::MeltSuperseded {
                    superseded_melt_quote_id,
                    melt_quote_id,
                    ..
                },
                HopRecord::Settled {
                    minted_sats: 100,
                    ..
                }
            ] if superseded_melt_quote_id == "melt-1" && melt_quote_id == "melt-2"
        ));

        let melts = world.borrow().melts.clone();
        let mints = world.borrow().mints.clone();
        let again = run_hop(&store, &mut effects, &journal("attempt-1")).unwrap();
        assert_eq!(again.minted_sats, settled.minted_sats);
        assert_eq!(again.recovered_strand, settled.recovered_strand);
        assert_eq!(world.borrow().melts, melts);
        assert_eq!(world.borrow().mints, mints);
    }

    #[test]
    fn a_failed_melt_with_issued_target_completes_without_touching_either_mint() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().mint_leg = Some(MintLeg::Issued);
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let settled = run_hop(&store, &mut effects, &journal("attempt-1")).unwrap();
        assert_eq!(settled.minted_sats, 100);
        assert!(!settled.recovered_strand);
        assert!(world.borrow().melts.is_empty());
        assert!(world.borrow().mints.is_empty());
        assert!(world.borrow().superseding_quotes.is_empty());
        assert!(matches!(
            store.replay("attempt-1").unwrap().as_slice(),
            [
                HopRecord::Planned(_),
                HopRecord::Settled {
                    minted_sats: 100,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn a_failed_melt_with_paid_target_uses_the_existing_strand_path() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().mint_leg = Some(MintLeg::Paid);
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let settled = run_hop(&store, &mut effects, &journal("attempt-1")).unwrap();
        assert!(
            settled.recovered_strand,
            "the loud strand path must be selected"
        );
        assert!(world.borrow().melts.is_empty());
        assert_eq!(world.borrow().mints, vec!["mint-1"]);
        assert!(world.borrow().superseding_quotes.is_empty());
        let line = strand_line(&journal("attempt-1"), "melt-1");
        assert!(line.contains("CROSSMINT STRAND"));
    }

    #[test]
    fn a_failed_melt_with_pending_target_refuses_without_raising_a_quote() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().mint_leg = Some(MintLeg::Pending);
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let error = run_hop(&store, &mut effects, &journal("attempt-1")).unwrap_err();
        assert!(
            matches!(error, HopError::AmbiguousTarget { .. }),
            "got: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("refusing an ambiguous supersession"),
            "got: {error}"
        );
        assert!(world.borrow().superseding_quotes.is_empty());
        assert!(world.borrow().melts.is_empty());
        assert!(world.borrow().mints.is_empty());
        assert!(matches!(
            store.replay("attempt-1").unwrap().as_slice(),
            [HopRecord::Planned(_)]
        ));
    }

    #[test]
    fn a_superseding_melt_journals_before_the_replacement_fires() {
        let ops = Rc::new(RefCell::new(Vec::new()));
        let store = MemJournal::sharing_ops(Rc::clone(&ops));
        let world = MintWorld::shared();
        world.borrow_mut().ops = Rc::clone(&ops);
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().fail_before_melt = true;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        run_hop(&store, &mut effects, &journal("attempt-1"))
            .expect_err("the injected melt failure stops the run");
        assert!(
            world.borrow().melts.is_empty(),
            "the melt effect never fired"
        );
        let journal_at = ops
            .borrow()
            .iter()
            .position(|event| event == "journal:MeltSuperseded")
            .expect("the supersession record must be written");
        let melt_at = ops
            .borrow()
            .iter()
            .position(|event| event == "melt:melt-2")
            .expect("the replacement melt must be invoked");
        assert!(
            journal_at < melt_at,
            "write-before-effect: journaled supersession must precede the replacement melt; got {:?}",
            ops.borrow()
        );
    }

    #[test]
    fn a_supersession_is_durable_when_the_melt_effect_fails_before_firing_and_recovery_uses_it() {
        let journal_dir = scratch_dir("supersession-crash-window");
        let store = FsHopJournal::new(&journal_dir);
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().fail_before_melt = true;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        run_hop(&store, &mut effects, &journal("attempt-1"))
            .expect_err("the injected melt failure stops the run");
        assert!(
            world.borrow().melts.is_empty(),
            "the melt effect never fired"
        );
        let restarted_store = FsHopJournal::new(&journal_dir);
        let fresh_records = restarted_store
            .replay("attempt-1")
            .expect("fresh journal read");
        assert!(matches!(
            fresh_records.as_slice(),
            [
                HopRecord::Planned(_),
                HopRecord::MeltSuperseded {
                    superseded_melt_quote_id,
                    melt_quote_id,
                    ..
                }
            ] if superseded_melt_quote_id == "melt-1" && melt_quote_id == "melt-2"
        ));

        world.borrow_mut().fail_before_melt = false;
        world.borrow_mut().melt_leg = Some(MeltLeg::Unpaid);
        let settled = run_hop(&restarted_store, &mut effects, &journal("attempt-1"))
            .expect("recovery resumes the journalled replacement");
        assert_eq!(settled.minted_sats, 100);
        assert_eq!(world.borrow().melts, vec!["melt-2"]);
        assert_eq!(world.borrow().superseding_quotes.len(), 1);
    }

    #[test]
    fn three_supersessions_mean_the_fourth_failed_active_quote_exhausts_without_settlement() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().fail_all_melts = true;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let error = run_hop(&store, &mut effects, &journal("attempt-1")).unwrap_err();
        assert!(matches!(
            error,
            HopError::MeltSupersessionExhausted {
                attempts: MAX_MELT_SUPERSESSIONS,
                ..
            }
        ));
        assert_eq!(
            world.borrow().melts.len(),
            MAX_MELT_SUPERSESSIONS,
            "the original was already Failed; exactly three superseding quotes are melted"
        );
        let records = store.replay("attempt-1").unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, HopRecord::MeltSuperseded { .. }))
                .count(),
            MAX_MELT_SUPERSESSIONS
        );
        assert!(settled_of(&records).is_none());
    }

    #[test]
    fn a_supersession_does_not_grow_budget_spent_or_mint_a_replacement_charge_key() {
        let pairing = journal("attempt-budget-recovery");
        let store = MemJournal::default();
        store
            .append_sync(&HopRecord::Planned(pairing.clone()))
            .unwrap();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().superseding_planned_cost = 100;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        let mut gate = BudgetGate::new(pairing.planned_cost);
        gate.authorize_then_attempt(&pairing.attempt_id, pairing.planned_cost, || ())
            .unwrap();
        let spent_before = gate.spent();
        run_hop(&store, &mut effects, &pairing).unwrap();
        let sub_key = format!("{}#melt1", pairing.attempt_id);
        assert!(gate.has_counted_attempt(&pairing.attempt_id));
        assert!(
            !gate.has_counted_attempt(&sub_key),
            "a supersession is a state transition on the record, not a new charge key"
        );
        assert_eq!(
            gate.spent(),
            spent_before,
            "exposure must not grow across supersessions"
        );
        assert_eq!(gate.spent(), pairing.planned_cost);
        assert!(matches!(
            store.replay(&pairing.attempt_id).unwrap().as_slice(),
            [
                HopRecord::Planned(_),
                HopRecord::MeltSuperseded {
                    planned_cost: 100,
                    ..
                },
                HopRecord::Settled { .. }
            ]
        ));
    }

    #[test]
    fn a_dearer_replacement_quote_refuses_by_cost_comparison_not_budget_cap() {
        let pairing = journal("attempt-dearer-replacement");
        let store = MemJournal::default();
        store
            .append_sync(&HopRecord::Planned(pairing.clone()))
            .unwrap();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().superseding_planned_cost = 200;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        let mut gate = BudgetGate::new(u64::MAX);
        gate.authorize_then_attempt(&pairing.attempt_id, pairing.planned_cost, || ())
            .unwrap();
        let error = run_hop(&store, &mut effects, &pairing).unwrap_err();
        assert!(
            matches!(
                error,
                HopError::ReplacementCostExceedsAuthorization {
                    authorized_cost: 109,
                    replacement_cost: 200,
                    ..
                }
            ),
            "got: {error}"
        );
        assert!(
            !error.to_string().contains("refused by budget cap"),
            "the refusal must be the cost comparison, not a budget-cap charge: {error}"
        );
        assert_eq!(gate.spent(), pairing.planned_cost);
        assert!(world.borrow().melts.is_empty());
        assert_eq!(world.borrow().superseding_quotes.len(), 1);
        assert!(matches!(
            store.replay(&pairing.attempt_id).unwrap().as_slice(),
            [HopRecord::Planned(_)]
        ));
    }

    /// Quoted hop used by [`journal`]: melt 100 + reserve 7 + input fee 2.
    const QUOTED_HOP_COST: u64 = 109;
    const QUOTED_FEE_RESERVE: u64 = 7;
    /// A fee-market bump inside the default 2×-reserve buffer (14) and above the quote.
    /// Without the buffer this replacement would refuse; with it, it must proceed.
    const WITHIN_BUFFER_REPLACEMENT: u64 = 120;

    fn journal_authorized(attempt: &str, multiplier: u64) -> HopJournal {
        let mut pairing = journal(attempt);
        pairing.planned_cost = authorized_hop_cost(QUOTED_HOP_COST, QUOTED_FEE_RESERVE, multiplier)
            .expect("quoted cost plus buffer fits in u64");
        pairing
    }

    #[test]
    fn authorized_hop_cost_adds_multiplier_times_the_mint_fee_reserve() {
        assert_eq!(
            authorized_hop_cost(QUOTED_HOP_COST, QUOTED_FEE_RESERVE, 2).unwrap(),
            123,
            "default buffer is 2× the mint's own fee reserve"
        );
        assert_eq!(
            authorized_hop_cost(QUOTED_HOP_COST, QUOTED_FEE_RESERVE, 0).unwrap(),
            QUOTED_HOP_COST,
            "multiplier 0 restores exact quoted-cost authorization"
        );
        assert!(
            authorized_hop_cost(u64::MAX, 1, 1).is_err(),
            "an unrepresentable authorized cost must refuse rather than wrap"
        );
    }

    #[test]
    fn a_replacement_quote_within_the_fee_buffer_proceeds() {
        let pairing =
            journal_authorized("attempt-within-buffer", default_hop_fee_buffer_multiplier());
        assert_eq!(pairing.planned_cost, 123);
        assert!(
            WITHIN_BUFFER_REPLACEMENT > QUOTED_HOP_COST,
            "the replacement must be dearer than the quote, or this does not prove a buffer"
        );
        assert!(WITHIN_BUFFER_REPLACEMENT <= pairing.planned_cost);

        let store = MemJournal::default();
        store
            .append_sync(&HopRecord::Planned(pairing.clone()))
            .unwrap();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().superseding_planned_cost = WITHIN_BUFFER_REPLACEMENT;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        let mut gate = BudgetGate::new(pairing.planned_cost);
        gate.authorize_then_attempt(&pairing.attempt_id, pairing.planned_cost, || ())
            .unwrap();
        let spent_before = gate.spent();
        run_hop(&store, &mut effects, &pairing).expect("within-buffer replacement must proceed");
        assert_eq!(
            gate.spent(),
            spent_before,
            "a within-buffer supersession is not a new budget interaction"
        );
        assert_eq!(world.borrow().melts, vec!["melt-2".to_owned()]);
        assert!(matches!(
            store.replay(&pairing.attempt_id).unwrap().as_slice(),
            [
                HopRecord::Planned(_),
                HopRecord::MeltSuperseded {
                    planned_cost: WITHIN_BUFFER_REPLACEMENT,
                    ..
                },
                HopRecord::Settled { .. }
            ]
        ));
    }

    #[test]
    fn a_replacement_quote_above_the_fee_buffer_refuses_naming_the_recorded_ceiling() {
        let pairing =
            journal_authorized("attempt-above-buffer", default_hop_fee_buffer_multiplier());
        let store = MemJournal::default();
        store
            .append_sync(&HopRecord::Planned(pairing.clone()))
            .unwrap();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().superseding_planned_cost = pairing.planned_cost + 1;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        let mut gate = BudgetGate::new(u64::MAX);
        gate.authorize_then_attempt(&pairing.attempt_id, pairing.planned_cost, || ())
            .unwrap();
        let error = run_hop(&store, &mut effects, &pairing).unwrap_err();
        assert!(
            matches!(
                error,
                HopError::ReplacementCostExceedsAuthorization {
                    authorized_cost: 123,
                    replacement_cost: 124,
                    ..
                }
            ),
            "got: {error}"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains("123"),
            "the refusal must name the recorded ceiling: {rendered}"
        );
        assert_eq!(gate.spent(), pairing.planned_cost);
        assert!(world.borrow().melts.is_empty());
        assert!(matches!(
            store.replay(&pairing.attempt_id).unwrap().as_slice(),
            [HopRecord::Planned(_)]
        ));
    }

    #[test]
    fn a_zero_fee_buffer_makes_the_within_buffer_replacement_refuse() {
        // Red-prove: the same replacement that proceeds under the default buffer must go RED
        // when the multiplier is 0. If it still proceeded, the tolerance would be in the
        // comparison (vacuous) rather than in the recorded authorization.
        let pairing = journal_authorized("attempt-zero-buffer", 0);
        assert_eq!(pairing.planned_cost, QUOTED_HOP_COST);
        let store = MemJournal::default();
        store
            .append_sync(&HopRecord::Planned(pairing.clone()))
            .unwrap();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().superseding_planned_cost = WITHIN_BUFFER_REPLACEMENT;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        let error = run_hop(&store, &mut effects, &pairing).unwrap_err();
        assert!(
            matches!(
                error,
                HopError::ReplacementCostExceedsAuthorization {
                    authorized_cost: QUOTED_HOP_COST,
                    replacement_cost: WITHIN_BUFFER_REPLACEMENT,
                    ..
                }
            ),
            "got: {error}"
        );
        assert!(world.borrow().melts.is_empty());
        assert!(matches!(
            store.replay(&pairing.attempt_id).unwrap().as_slice(),
            [HopRecord::Planned(_)]
        ));
    }

    #[test]
    fn replacement_cost_over_remaining_source_coverage_refuses_before_journal() {
        let pairing = journal("attempt-source-coverage");
        let store = MemJournal::default();
        store
            .append_sync(&HopRecord::Planned(pairing.clone()))
            .unwrap();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        world.borrow_mut().superseding_planned_cost = 100;
        world.borrow_mut().source_coverage = 99;
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };

        let error = run_hop(&store, &mut effects, &pairing).unwrap_err();
        assert!(matches!(
            error,
            HopError::InsufficientSource {
                balance: 99,
                planned_cost: 100,
                ..
            }
        ));
        assert_eq!(world.borrow().superseding_quotes.len(), 1);
        assert!(world.borrow().melts.is_empty());
        assert!(matches!(
            store.replay(&pairing.attempt_id).unwrap().as_slice(),
            [HopRecord::Planned(_)]
        ));
    }

    #[test]
    fn a_stale_concurrent_supersession_cannot_replace_the_journal_tail() {
        let store = FsHopJournal::new(scratch_dir("stale-supersession"));
        store
            .append_sync(&HopRecord::Planned(journal("attempt-1")))
            .unwrap();
        store
            .append_sync(&HopRecord::MeltSuperseded {
                attempt_id: "attempt-1".into(),
                superseded_melt_quote_id: "melt-1".into(),
                melt_quote_id: "melt-2".into(),
                planned_cost: 109,
            })
            .unwrap();
        let error = store
            .append_sync(&HopRecord::MeltSuperseded {
                attempt_id: "attempt-1".into(),
                superseded_melt_quote_id: "melt-1".into(),
                melt_quote_id: "melt-racing".into(),
                planned_cost: 109,
            })
            .unwrap_err();
        assert!(matches!(error, HopError::Journal(_)));
        assert_eq!(
            active_melt(&store.replay("attempt-1").unwrap()),
            Some(("melt-2".into(), 1))
        );
    }

    #[test]
    fn duplicate_planned_refuses_until_the_prior_planned_is_superseded() {
        let store = FsHopJournal::new(scratch_dir("duplicate-planned"));
        store
            .append_sync(&HopRecord::Planned(journal("attempt-1")))
            .unwrap();

        let mut duplicate = journal("attempt-1");
        duplicate.melt_quote_id = "melt-racing".into();
        let error = store
            .append_sync(&HopRecord::Planned(duplicate.clone()))
            .unwrap_err();
        assert!(matches!(
            error,
            HopError::DuplicatePlanned {
                ref attempt_id,
                ref active_melt_quote_id,
            } if attempt_id == "attempt-1" && active_melt_quote_id == "melt-1"
        ));

        store
            .append_sync(&HopRecord::MeltSuperseded {
                attempt_id: "attempt-1".into(),
                superseded_melt_quote_id: "melt-1".into(),
                melt_quote_id: "melt-2".into(),
                planned_cost: 109,
            })
            .unwrap();
        store.append_sync(&HopRecord::Planned(duplicate)).expect(
            "a supersession closes the prior Planned record before a later Planned is appended",
        );
    }

    // Every leg that can fail must leave the attempt un-completed: no completion record, so a later
    // run re-reads the pairing and asks the mints again rather than assuming anything landed. This
    // is the safety the old membership refusal used to provide, re-pinned at the layer that replaced
    // it — the hop is now what stands between "cannot settle at the seller's mint" and a wrong spend.
    #[test]
    fn no_failing_leg_leaves_a_completion_record_behind() {
        /// How one case breaks the world, paired with the name of the leg it breaks.
        type BrokenLeg = (&'static str, Box<dyn Fn(&mut MintWorld)>);

        let cases: Vec<BrokenLeg> = vec![
            (
                "source mint unreachable",
                Box::new(|world: &mut MintWorld| world.melt_leg = None),
            ),
            (
                "melt in flight",
                Box::new(|world: &mut MintWorld| world.melt_leg = Some(MeltLeg::Pending)),
            ),
            (
                "melt failed with unreachable target",
                Box::new(|world: &mut MintWorld| {
                    world.melt_leg = Some(MeltLeg::Failed);
                    world.target_invoice_fails = true;
                }),
            ),
            (
                "melt failed with pending target",
                Box::new(|world: &mut MintWorld| {
                    world.melt_leg = Some(MeltLeg::Failed);
                    world.mint_leg = Some(MintLeg::Pending);
                }),
            ),
            (
                "target mint unreachable",
                Box::new(|world: &mut MintWorld| world.mint_leg = None),
            ),
            (
                "target refuses to issue",
                Box::new(|world: &mut MintWorld| world.mint_fails = true),
            ),
        ];
        for (label, break_it) in cases {
            let store = MemJournal::default();
            let world = MintWorld::shared();
            break_it(&mut world.borrow_mut());
            let mut effects = FakeMints {
                world: Rc::clone(&world),
            };
            let error = run_hop(&store, &mut effects, &journal("attempt-1"))
                .expect_err(&format!("{label} must refuse"));
            assert!(
                settled_of(&store.replay("attempt-1").expect("replays")).is_none(),
                "{label}: refused with {error}, but left a completion record"
            );
            assert!(
                world.borrow().mints.is_empty(),
                "{label}: refused but claimed ecash anyway"
            );
        }
    }

    // A torn final record is refused, not parsed around: a half-written pairing is exactly the
    // state in which a wrong answer melts twice.
    #[test]
    fn a_torn_final_record_refuses_rather_than_being_parsed_around() {
        let journal_dir = scratch_dir("torn-tail");
        let store = FsHopJournal::new(&journal_dir);
        store
            .append_sync(&HopRecord::Planned(journal("attempt-1")))
            .expect("pairing appends");
        let path = journal_dir.join("attempt-1.jsonl");
        let mut bytes = std::fs::read(&path).expect("read");
        bytes.extend_from_slice(b"{\"record\":\"settled\"");
        std::fs::write(&path, bytes).expect("write");

        let error = store
            .replay("attempt-1")
            .expect_err("a torn record refuses");
        assert!(
            error.to_string().contains("torn write"),
            "expected a torn-write refusal, got: {error}"
        );
    }
}
