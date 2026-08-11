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
}

/// Both legs of one hop, as the two mints report them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopSettled {
    /// Ecash issued at the target mint. Equals the pinned delivery amount, or the hop refuses.
    pub minted_sats: u64,
    /// Whether this run found the melt already paid and the ecash not yet issued — a hop that an
    /// earlier run left stranded and this one recovered.
    pub recovered_strand: bool,
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
    /// The melt is settling at the source mint. Money is in flight, so this attempt must not melt
    /// again; it refuses and stays retryable once the source mint reaches a terminal answer.
    MeltInFlight {
        /// The attempt whose melt is settling.
        attempt_id: String,
        /// Melt quote to re-ask about on the next run.
        melt_quote_id: String,
    },
    /// The source mint reports the melt as failed: the Lightning payment did not go through and the
    /// proofs are back in the wallet. No money left, but this pairing is dead — the target's invoice
    /// can no longer be paid through this melt quote.
    ///
    /// interim: the attempt stops here rather than re-planning the melt leg against the (still
    /// unpaid) invoice at the target. Re-planning means a superseding-pairing record and a fresh
    /// argument about pays-once, which is its own reviewed change — see MakePrisms/maxplayerai#194.
    MeltFailed {
        /// The attempt whose melt failed.
        attempt_id: String,
        /// Melt quote the source mint reports as failed.
        melt_quote_id: String,
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
        }
    }
}

impl std::error::Error for HopError {}

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

    /// Melt at the source mint, paying the invoice the target raised.
    fn melt(&mut self, melt_quote_id: &str) -> Result<(), HopError>;

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
    /// Both legs landed and the buyer holds the ecash at the target.
    Settled {
        /// Attempt this hop funded.
        attempt_id: String,
        /// Ecash issued at the target mint.
        minted_sats: u64,
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
        // A record without its commit newline is a torn write. Refuse it rather than parse around
        // it: a half-written pairing is exactly the state where a wrong answer melts twice.
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(HopError::Journal(format!(
                "attempt {attempt_id}: last record is missing its commit newline (torn write)"
            )));
        }
        let mut records = Vec::new();
        for (index, line) in BufReader::new(bytes.as_slice()).lines().enumerate() {
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

    fn append_sync(&self, record: &HopRecord) -> Result<(), HopError> {
        let attempt_id = match record {
            HopRecord::Planned(journal) => journal.attempt_id.as_str(),
            HopRecord::Settled { attempt_id, .. } => attempt_id.as_str(),
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
        HopRecord::Settled { .. } => None,
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
        HopRecord::Planned(_) => None,
    })
}

/// The operator-visible line for a hop whose sats left the source but whose ecash never arrived.
///
/// Written to stderr on its own line and prefixed with a fixed, greppable marker: the point is that
/// somebody watching the buyer's output sees it without having to know what a mint quote is.
fn strand_line(journal: &HopJournal) -> String {
    format!(
        "CROSSMINT STRAND attempt={} melted at {} (melt quote {}) but the ecash at {} was never \
         issued (mint quote {}); {} sats are paid and unissued — completing the mint leg now",
        journal.attempt_id,
        journal.source_mint,
        journal.melt_quote_id,
        journal.target_mint,
        journal.mint_quote_id,
        journal.delivered_sats,
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
/// completion record is durable before the caller is told the ecash is in hand.
pub(crate) fn run_hop<S: HopJournalStore, E: HopEffects>(
    store: &S,
    effects: &mut E,
    journal: &HopJournal,
) -> Result<HopSettled, HopError> {
    let records = store.replay(&journal.attempt_id)?;
    if let Some(minted_sats) = settled_of(&records) {
        // Already complete. Neither mint is touched — this is the pays-once row.
        return Ok(HopSettled {
            minted_sats,
            recovered_strand: false,
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

    // Melt leg. The source mint's answer decides; we never infer from our own records whether money
    // moved, because the record was written before the melt precisely so it could not know.
    let melted_earlier = match effects.melt_leg(&pairing.melt_quote_id)? {
        MeltLeg::Unpaid => {
            effects.melt(&pairing.melt_quote_id)?;
            false
        }
        MeltLeg::Pending => {
            return Err(HopError::MeltInFlight {
                attempt_id: pairing.attempt_id.clone(),
                melt_quote_id: pairing.melt_quote_id.clone(),
            });
        }
        MeltLeg::Failed => {
            return Err(HopError::MeltFailed {
                attempt_id: pairing.attempt_id.clone(),
                melt_quote_id: pairing.melt_quote_id.clone(),
            });
        }
        MeltLeg::Paid => true,
    };

    // Mint leg. A melt that landed on an EARLIER run with no ecash to show for it is the strand.
    let mint_leg = effects.mint_leg(&pairing.mint_quote_id)?;
    let recovered_strand = melted_earlier && mint_leg != MintLeg::Issued;
    if recovered_strand {
        eprintln!("{}", strand_line(&pairing));
    }
    let minted_sats = match mint_leg {
        MintLeg::Issued => pairing.delivered_sats,
        MintLeg::Unpaid | MintLeg::Paid => {
            effects.mint(&pairing.mint_quote_id, pairing.delivered_sats)?
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
    Ok(HopSettled {
        minted_sats,
        recovered_strand,
    })
}

/// Bound on one hop leg that moves money.
///
/// Longer than [`MINT_TOUCH_TIMEOUT`], which bounds mint *reads*: a melt is a Lightning payment
/// between two mints, not a keyset lookup. Shorter than forever, because a leg that never returns is
/// invariant 5's failure — an await with no timer is a park nothing can end.
const HOP_LEG_TIMEOUT: Duration = Duration::from_secs(90);

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
}

impl CdkHopEffects {
    /// Open the buyer's wallet at both mints. One sqlite store, two bound mints.
    pub(crate) async fn open(
        home: &MaxplayerHome,
        source_mint: &str,
        target_mint: &str,
    ) -> Result<Self, HopError> {
        let source = buyer_fund::open_wallet_at_mint_async(home, source_mint)
            .await
            .map_err(|error| HopError::Mint(format!("source mint {source_mint}: {error}")))?;
        let target = buyer_fund::open_wallet_at_mint_async(home, target_mint)
            .await
            .map_err(|error| HopError::Mint(format!("target mint {target_mint}: {error}")))?;
        Ok(Self { source, target })
    }

    /// Price the hop and raise both quotes, moving no money.
    ///
    /// Delivery is PINNED: the mint quote at the target is raised for exactly `delivered_sats`, the
    /// amount from the buyer-signed offer. The buyer's cost is what floats — melt amount, the source
    /// mint's Lightning fee reserve, and its input fee — so no fee reading can ever reduce what the
    /// seller receives.
    ///
    /// Runs before the budget gate, so every failure here refuses with zero spend.
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
        if mint_quote.request.trim().is_empty() {
            return Err(HopError::Mint(format!(
                "target mint {} returned an empty bolt11 for quote {} (refusing a hop with nothing \
                 to pay)",
                self.target.mint_url, mint_quote.id
            )));
        }
        let melt_quote = bounded(
            "source melt quote",
            MINT_TOUCH_TIMEOUT,
            self.source.melt_quote(
                PaymentMethod::BOLT11,
                mint_quote.request.clone(),
                None,
                None,
            ),
        )
        .await?;
        let cost = HopCost {
            melt_amount: melt_quote.amount.to_u64(),
            fee_reserve: melt_quote.fee_reserve.to_u64(),
            input_fee: self.source_input_fee_ceiling().await?,
        };
        let planned_cost = cost.planned_cost().map_err(|error| {
            // `planned_cost` reports overflow through the pay error type; the hop states it in its
            // own words rather than smuggling a foreign error across the boundary.
            HopError::Mint(error.to_string())
        })?;
        self.require_source_covers(planned_cost).await?;
        Ok(HopJournal {
            attempt_id: attempt_id.to_owned(),
            source_mint: self.source.mint_url.to_string(),
            melt_quote_id: mint_quote_id(&melt_quote.id),
            target_mint: self.target.mint_url.to_string(),
            mint_quote_id: mint_quote_id(&mint_quote.id),
            delivered_sats,
            planned_cost,
        })
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

    fn melt(&mut self, melt_quote_id: &str) -> Result<(), HopError> {
        let wallet = self.source.clone();
        let quote_id = melt_quote_id.to_owned();
        block_on_leg("melt", async move {
            let prepared = bounded(
                "prepare melt",
                MINT_TOUCH_TIMEOUT,
                wallet.prepare_melt(&quote_id, HashMap::new()),
            )
            .await?;
            bounded("melt confirm", HOP_LEG_TIMEOUT, prepared.confirm()).await?;
            Ok::<(), HopError>(())
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
        if settled_of(&records).is_some() {
            continue;
        }
        let Some(pairing) = planned_of(&records).cloned() else {
            continue;
        };
        let result = sweep_one(home, &store, pairing.clone()).await;
        if let Err(error) = &result {
            eprintln!(
                "CROSSMINT STRAND attempt={} could not be completed by the recovery sweep \
                 (source {} melt quote {}, target {} mint quote {}): {error}",
                pairing.attempt_id,
                pairing.source_mint,
                pairing.melt_quote_id,
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
        bounded(
            &format!("{label} saga recovery"),
            HOP_LEG_TIMEOUT,
            wallet.recover_incomplete_sagas(),
        )
        .await?;
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

    fn journal(attempt: &str) -> HopJournal {
        HopJournal {
            attempt_id: attempt.to_owned(),
            source_mint: "https://a.example".to_owned(),
            melt_quote_id: "melt-1".to_owned(),
            target_mint: "https://b.example".to_owned(),
            mint_quote_id: "mint-1".to_owned(),
            delivered_sats: 100,
            planned_cost: 109,
        }
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
        mint_fails: bool,
    }

    impl MintWorld {
        fn shared() -> Rc<RefCell<Self>> {
            Rc::new(RefCell::new(Self {
                melt_leg: Some(MeltLeg::Unpaid),
                mint_leg: Some(MintLeg::Unpaid),
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

        fn melt(&mut self, melt_quote_id: &str) -> Result<(), HopError> {
            let mut world = self.world.borrow_mut();
            world.melts.push(melt_quote_id.to_owned());
            world.melt_leg = Some(MeltLeg::Paid);
            // The target's invoice is paid by the melt, so its quote flips too — unless the target
            // is unreachable (`None`), which stays unreachable no matter what the source did.
            if world.mint_leg.is_some() {
                world.mint_leg = Some(MintLeg::Paid);
            }
            if world.die_after_melt {
                return Err(HopError::Mint("process died after the melt".into()));
            }
            Ok(())
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
    #[derive(Default)]
    struct MemJournal {
        records: RefCell<HashMap<String, Vec<HopRecord>>>,
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
                HopRecord::Settled { attempt_id, .. } => attempt_id.clone(),
            };
            self.records
                .borrow_mut()
                .entry(attempt_id)
                .or_default()
                .push(record.clone());
            Ok(())
        }
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
        let line = strand_line(&journal("attempt-1"));
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
            fn melt(&mut self, _: &str) -> Result<(), HopError> {
                Ok(())
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
        }
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
        assert!(!store.path_for("attempt-502").exists(), "quote-planning refusal must not create an attempt journal");
        assert!(!journal_dir.exists(), "quote-planning refusal must not mutate the journal directory");
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
        assert!(!store.path_for("attempt-refused").exists(), "quote-planning refusal must not create an attempt journal");
        assert!(!journal_dir.exists(), "quote-planning refusal must not mutate the journal directory");
    }

    #[test]
    fn the_file_journal_round_trips_a_pairing_and_a_completion() {
        let store = FsHopJournal::new(scratch_dir("round-trip"));
        assert!(store.replay("attempt-1").expect("empty replays").is_empty());

        let pairing = journal("attempt-1");
        store
            .append_sync(&HopRecord::Planned(pairing.clone()))
            .expect("pairing appends");
        store
            .append_sync(&HopRecord::Settled {
                attempt_id: "attempt-1".to_owned(),
                minted_sats: 100,
            })
            .expect("completion appends");

        let records = store.replay("attempt-1").expect("replays");
        assert_eq!(records.len(), 2);
        assert_eq!(planned_of(&records), Some(&pairing));
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

    // A melt the source mint reports as FAILED stops the attempt. Nothing left the wallet, so this
    // is not the strand — but the quote is dead, and re-melting it is not the answer either.
    #[test]
    fn a_failed_melt_refuses_without_melting_again() {
        let store = MemJournal::default();
        let world = MintWorld::shared();
        world.borrow_mut().melt_leg = Some(MeltLeg::Failed);
        let mut effects = FakeMints {
            world: Rc::clone(&world),
        };
        let error = run_hop(&store, &mut effects, &journal("attempt-1"))
            .expect_err("a failed melt must refuse");
        assert!(matches!(error, HopError::MeltFailed { .. }), "got: {error}");
        assert!(world.borrow().melts.is_empty());
        assert!(world.borrow().mints.is_empty());
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
                "melt failed",
                Box::new(|world: &mut MintWorld| world.melt_leg = Some(MeltLeg::Failed)),
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
