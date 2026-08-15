//! MCP / CLI pay entry: BudgetGate → [`PaymentService::run`] only.
//!
//! By construction the delivery verifier is [`PayPathDeliveryVerifier`] (allowlist sealed).
//! Stable [`PaymentKey::attempt_id`] feeds `run()`'s reconcile saga — no bespoke pay path,
//! no [`PaymentService::advance`] from this surface.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use cashu::{Amount, CurrencyUnit, PublicKey as CashuPublicKey};
use nostr_sdk::secp256k1::{Message, Secp256k1};
use nostr_sdk::Keys;
use nostr_sdk::PublicKey as NostrPublicKey;
use nostr_sdk::Timestamp;

use crate::budget::{BudgetGate, BudgetRefuse};
use crate::buyer_fund::{self, FundError};
use crate::crossmint_hop::{self, CdkHopEffects, FsHopJournal, HopError};
use crate::delivery::{CommitOid, DeliveryError, GitDelivery};
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::gateway;
use crate::home::{self, MaxplayerHome};
use crate::payment::{
    DeliveryIntegrityHash, EffectError, FsPaymentJournal, JobHash, JobId, PaymentError, PaymentKey,
    PaymentService, PaymentState, PaymentTerms, ReceiptAuthority, ReceiptEvidence, ResultId,
};
use crate::payment_send::NostrPaymentSend;
use crate::payment_wallet::{CdkPaymentEffects, PaymentWalletError, PreflightError};
use crate::receipt::{DeliveryKind, ReceiptPreimage, EXEC_METADATA_COMMITMENT_EMPTY};

/// Trusted job-class input for [`authorize_pay_async`], derived by the caller from the buyer's
/// SIGNED OFFER (never a seller echo). Sealing input: a [`JobClass::Contribution`] request whose
/// `contribution` binds are `None` is REFUSED (defense in depth). The MCP layer already
/// re-derives the class and refuses fail-closed; carrying it into the crate API makes the entry
/// point itself fail-closed so no in-crate caller can pay a contribution job as from-scratch and
/// skip the contribution gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobClass {
    /// From-scratch job — no contribution verify (byte-identical produced artifacts).
    FromScratch,
    /// Contribution job — requires `contribution` binds; the fork verify-path + authorship seam run.
    Contribution,
}

/// Inputs for the authorize_pay composed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizePayRequest {
    pub job_id: String,
    pub result_id: String,
    /// Buyer-derived job class (from the signed offer). Sealing input: `Contribution` with
    /// `contribution: None` is refused (see [`JobClass`]).
    pub job_class: JobClass,
    /// Buyer's independent commitment (full git oid) — must tip-match after verify.
    pub delivery_integrity_hash: String,
    pub job_hash: String,
    pub seller_pubkey: String,
    pub amount_sats: u64,
    pub repo: String,
    pub branch: String,
    pub commit_oid: String,
    /// Seller schnorr signature (hex) over the receipt preimage — read from the
    /// accepted result's `sig/seller` tag. Empty ⇒ the buyer cannot co-sign a valid
    /// receipt (the receipt authority fails closed at publish).
    pub seller_signature: String,
    /// SHA-256 hex of the seller-authored NUT-18 payment request (`creqA…`), sourced
    /// from the accepted claim's `creq` tag (threaded through the accept-bind). `None` for a
    /// claim with no `creq` — the attempt id and receipt preimage then bind byte-identically.
    /// Bound into the [`PaymentKey`] attempt id and the co-signed receipt preimage.
    pub creq_hash: Option<String>,
    /// The seller-authored `creq`'s accepted-mint list (`m`), read off the
    /// accepted claim. The buyer pays from a mint it holds balance at that appears here; empty for
    /// a claim with no `creq` — the buyer then pays from the pinned default mint.
    #[allow(clippy::struct_field_names)]
    pub accepted_mints: Vec<String>,
    /// The realized paying mint the buyer SELECTED for this job, sealed into the accept-bind at
    /// accept time and threaded here. When `Some`, the pay path derives the realized mint from THIS
    /// (still enforcing accepted-set membership + the real-mint fence) instead of the live config
    /// default, so the attempt id is stable across retries even if the buyer's config default
    /// changes between attempts (double-pay fence). `None` only for a legacy bind that predates the
    /// sealed field — the pay path then falls back to the live config default.
    pub realized_mint: Option<String>,
    /// Contribution binds. `None` ⇒ from-scratch job (no new verify, byte-identical produced
    /// artifacts). `Some(..)` ⇒ the fork contribution verify-path (store fetch + base-from-pin +
    /// descendant + content) + the authorship tuple seam run pre-pay, all against these
    /// buyer-controlled binds.
    pub contribution: Option<ContributionPayBinds>,
}

/// Buyer-controlled contribution binds threaded from the signed offer / accept-bind into the pay
/// path. `repo`/`branch`/`commit_oid` on the enclosing request ARE the fork (`fork_ref` + fork tip);
/// these add the pinned target + base + the seller's authorship signature. All authority is the
/// buyer's signed offer — never a seller echo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContributionPayBinds {
    /// Pinned target owner pubkey (hex) — from the buyer's signed offer.
    pub target_owner_pubkey: String,
    /// Pinned target clone URL — base_oid is fetched from HERE, never the seller echo.
    pub target_clone_url: String,
    /// Base branch the exact `base_oid` lives on in the pinned target.
    pub base_branch: String,
    /// The exact commit the delivery must descend from (from the buyer's signed offer).
    pub base_oid: String,
    /// Seller schnorr signature (hex) over the signed-result authorship tuple (`sig/seller-contribution`).
    pub tuple_signature: String,
}

/// Successful composed pay outcome (state + attempt id + spent accounting).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizePayOutcome {
    pub state: PaymentState,
    pub attempt_id: String,
    /// What the seller received — the buyer-signed offer amount.
    pub amount_sats: u64,
    /// What the budget cap was charged BEFORE the send/melt. On the direct path this is
    /// `amount_sats` plus the estimated mint input fee for the send (#185) — that fee leaves the
    /// wallet on the swap, so it must pass the cap. A cross-mint hop instead costs the buyer the
    /// Lightning fee reserve and the source mint's input fee on top, so the two differ and the gap is
    /// the hop's cost rather than anything the seller was paid. For a hop, this is the WORST-CASE
    /// charge: the unused Lightning fee reserve is reconciled and credited back to the ledger after
    /// the melt (#186), so `spent_total_sats` can be lower than this value reflects.
    pub charged_sats: u64,
    pub spent_total_sats: u64,
}

/// Inputs for the operator completion path ([`complete_recovered_locked_async`]).
///
/// Mirrors the IDENTITY inputs of [`AuthorizePayRequest`] so the SAME attempt id (and journal file)
/// is targeted, and adds only `seller_signature` — the seller cosig the completion still needs to
/// publish the receipt. It carries NO new spend authority: the token was minted and the budget
/// charged at the original award; completion REUSES that token and re-checks the same attempt id
/// against the gate (never a second charge). There is deliberately no `repo`/`branch`/`commit_oid`
/// or contribution binds — completion re-checks proof state and re-sends the existing token; it does
/// not re-verify delivery (that gated the ORIGINAL spend, which already happened).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompleteLockedRequest {
    pub job_id: String,
    pub result_id: String,
    pub delivery_integrity_hash: String,
    pub job_hash: String,
    pub seller_pubkey: String,
    pub amount_sats: u64,
    /// Seller schnorr signature (hex) over the receipt preimage — read from the accepted result's
    /// `sig/seller` tag, exactly as the pay path reads it. Required: the buyer cannot co-sign a
    /// valid receipt without it, so completion would stall at the receipt leg.
    pub seller_signature: String,
    pub creq_hash: Option<String>,
    pub accepted_mints: Vec<String>,
    pub realized_mint: Option<String>,
}

/// Successful operator-completion outcome. `state` is the folded payment state after the run —
/// `Closed` on a full completion, or an earlier state if a later leg (e.g. the receipt publish) is
/// left to a subsequent run. `spent_total_sats` is UNCHANGED from before the call when the attempt
/// was already charged (the common case), because completion never re-charges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompleteLockedOutcome {
    pub state: PaymentState,
    pub attempt_id: String,
    pub amount_sats: u64,
    pub spent_total_sats: u64,
}

#[derive(Debug)]
pub enum AuthorizePayError {
    Input(String),
    Budget(BudgetRefuse),
    Fund(FundError),
    Delivery(DeliveryError),
    /// A cross-mint hop refused. Every variant is fail-closed; the pays-once cases name the quote
    /// ids so an operator can ask the mints directly.
    Hop(HopError),
    Payment(PaymentError),
    Wallet(PaymentWalletError),
    Home(String),
    Effects(String),
    /// The first direct-payment mint request failed before any journal or budget mutation.
    CancelledMintUnreachable { mint: String, detail: String },
    /// Pre-pay seller co-signature refusal, carrying the buyer's computed preimage fields + digest
    /// (public trade data, no secrets) so the divergent field self-identifies (diagnostic).
    CosigRefused(String),
    /// Pre-pay execution-sentinel refusal (§19): the independently-fetched delivery tree carries no
    /// job-bound execution sentinel — missing, invalid, or replayed from another job. A refusal, not a
    /// crash: ZERO spend, no journal, and a durable local sentinel-refusal record for §17 reputation.
    NoSentinel(String),
}

impl fmt::Display for AuthorizePayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "authorize_pay input: {message}"),
            Self::Budget(refuse) => write!(formatter, "{refuse}"),
            Self::Fund(error) => write!(formatter, "{error}"),
            Self::Delivery(error) => write!(formatter, "authorize_pay delivery: {error}"),
            Self::Hop(error) => write!(formatter, "authorize_pay: {error}"),
            Self::Payment(error) => write!(formatter, "authorize_pay payment: {error}"),
            Self::Wallet(error) => write!(formatter, "authorize_pay wallet: {error}"),
            Self::CosigRefused(message) => write!(formatter, "authorize_pay payment: {message}"),
            Self::Home(message) => write!(formatter, "authorize_pay home: {message}"),
            Self::Effects(message) => write!(formatter, "authorize_pay effects: {message}"),
            Self::CancelledMintUnreachable { mint, detail } => write!(
                formatter,
                "authorize_pay cancelled: mint {mint} unreachable or erroring ({detail}); no funds moved"
            ),
            Self::NoSentinel(message) => write!(formatter, "authorize_pay no_sentinel: {message}"),
        }
    }
}

impl std::error::Error for AuthorizePayError {}

impl From<BudgetRefuse> for AuthorizePayError {
    fn from(value: BudgetRefuse) -> Self {
        Self::Budget(value)
    }
}

impl From<FundError> for AuthorizePayError {
    fn from(value: FundError) -> Self {
        Self::Fund(value)
    }
}

impl From<DeliveryError> for AuthorizePayError {
    fn from(value: DeliveryError) -> Self {
        Self::Delivery(value)
    }
}

impl From<PaymentError> for AuthorizePayError {
    fn from(value: PaymentError) -> Self {
        Self::Payment(value)
    }
}

impl From<PaymentWalletError> for AuthorizePayError {
    fn from(value: PaymentWalletError) -> Self {
        Self::Wallet(value)
    }
}

impl From<PreflightError> for AuthorizePayError {
    fn from(value: PreflightError) -> Self {
        match value {
            PreflightError::MintUnreachable { mint, detail, .. } => {
                Self::CancelledMintUnreachable { mint, detail }
            }
            PreflightError::Other(message) => Self::Effects(message),
        }
    }
}

impl From<HopError> for AuthorizePayError {
    fn from(value: HopError) -> Self {
        Self::Hop(value)
    }
}

/// Authorize spend under [`BudgetGate`], then pay only through [`PaymentService::run`] with a
/// [`PayPathDeliveryVerifier`]. Async — every caller is already on a Tokio runtime (MCP dispatch).
///
/// Spent is keyed by stable `PaymentKey::attempt_id()`: first authorize persists
/// spent **before** `run()` (write-before-mint); a reconciled retry does not
/// re-count. `run()` delivery-verifies first and reconciles inside the saga.
/// Verify-fetch timeout fails CLOSED (no pay / zero burn).
///
/// CALLER CONTRACT (contribution gating): this function trusts `request.contribution` —
/// `None` is treated as a from-scratch job, so the four contribution gates + the
/// authorship-tuple bind are skipped. The offer's `job-class` lives on the relay, which this
/// function deliberately does not read (no network beyond the delivery fetch). EVERY
/// production caller therefore inherits the guard obligation: refuse to pay a
/// `job-class=contribution` offer with `contribution: None` (the MCP pay tool re-derives the
/// class and refuses fail-closed; a bind-built request resolves it at accept). A new caller
/// that skips this check reopens the gate bypass this contract exists to prevent.
pub async fn authorize_pay_async(
    home: &MaxplayerHome,
    gate: &mut BudgetGate,
    request: AuthorizePayRequest,
) -> Result<AuthorizePayOutcome, AuthorizePayError> {
    // Both job_id and explicit forms: buyer tip-match hash is required and must
    // equal the seller-advertised commit_oid. Never derive/default the hash from the
    // claim/result oid — caller must supply it; mismatch refuses.
    if request.delivery_integrity_hash.trim().is_empty() {
        return Err(AuthorizePayError::Input(
            "delivery_integrity_hash is required (buyer tip-match); never auto-filled from claim/result oid".into(),
        ));
    }
    if request.delivery_integrity_hash != request.commit_oid {
        return Err(AuthorizePayError::Input(format!(
            "delivery_integrity_hash {} does not match seller-advertised commit_oid {} (buyer tip-match required; refuse mismatch)",
            request.delivery_integrity_hash, request.commit_oid
        )));
    }

    // Entry-point seal (defense in depth): a contribution-class job MUST carry contribution binds.
    // Without this the caller contract below is enforced only by every caller remembering to
    // re-derive the class; here the crate API itself refuses to pay a contribution job as
    // from-scratch (which would skip the four contribution gates + the authorship-tuple bind).
    if request.job_class == JobClass::Contribution && request.contribution.is_none() {
        return Err(AuthorizePayError::Input(
            "job_class=contribution requires contribution binds; refusing to pay a contribution job \
             as from-scratch (contribution-gate bypass)"
                .into(),
        ));
    }

    // Buyer-controlled job hash (from the accept-bind, never a seller echo) kept for the §19 sentinel
    // check below, which asserts the delivered tree carries THIS job's hash — so a replayed sentinel
    // from another job fails the match. `derive_payment` only borrows `request.job_hash`, so it stays
    // available here.
    let expected_job_hash = request.job_hash.clone();
    // Single derivation of the stable payment terms + key (⇒ attempt id) from the trade identity,
    // shared byte-for-byte with `complete_recovered_locked_async` so a completion re-derives the
    // EXACT same attempt id and journal file (a drift here would double-pay). See `derive_payment`.
    let DerivedPayment {
        terms,
        key,
        seller_nostr,
        plan,
    } = derive_payment(
        home,
        &request.job_id,
        &request.result_id,
        &request.delivery_integrity_hash,
        &request.job_hash,
        &request.seller_pubkey,
        request.amount_sats,
        &request.accepted_mints,
        request.realized_mint.as_deref(),
        request.creq_hash.clone(),
    )?;
    let attempt_id = key.attempt_id();

    let commit_oid = CommitOid::parse(request.commit_oid)?;
    // The buyer tip-match gate above stays a raw compare of `delivery_integrity_hash ==
    // commit_oid` — routing it through the parsed oid would lowercase it and reorder the
    // parse-vs-gate refusals, i.e. change behavior on the refuse path.
    let delivery = GitDelivery::new(request.repo, request.branch, commit_oid)?;
    let delivery_kind = delivery.delivery_kind();

    let secret_hex = home::read_secret_key_hex(home)
        .map_err(|error| AuthorizePayError::Home(error.to_string()))?;
    let keys = Keys::parse(&secret_hex)
        .map_err(|error| AuthorizePayError::Home(format!("buyer key parse: {error}")))?;
    let buyer_nostr = keys.public_key();
    let authority = ReceiptAuthority {
        // External anchors: buyer == the offer's author (this buyer's own
        // key), seller == the accepted-claim seller. NEVER the receipt's own p-tags.
        buyer: buyer_nostr,
        seller: seller_nostr,
    };
    // Capture receipt-publish inputs before `keys` is moved into the payment sender.
    let buyer_receipt_keys = keys.clone();
    let receipt_relay = home.config.relay_url.clone();
    let seller_hex = seller_nostr.to_hex();
    let seller_signature = request.seller_signature.clone();

    // Buyer-owned store verifier (no wallet dependency; created before the pre-pay seam so the
    // contribution verify runs against the buyer store BEFORE any spend). The payment-journal is
    // created LATER (after the pre-pay seam) so a pre-pay refusal leaves NO journal on disk.
    let store = home.root.join("store");
    // Buyer secret signs NIP-98 for the in-process relay-git READ (fork + base fetch). Public https
    // bases and local-path fixtures fetch anonymously (git_transport gates the header on relay-git).
    let mut verifier = PayPathDeliveryVerifier::new(store, Some(secret_hex.clone()));

    // Contribution verify-path — ALL PRE-PAY (before the budget gate ⇒ zero spend on any
    // refusal), ALL against BUYER-CONTROLLED binds. The fork (`delivery`) is store-fetched +
    // tip-matched, `base_oid` is fetched from the PINNED target (never the seller echo), the
    // delivery must DESCEND from base, and the content gate + buyer policy hook must pass. The
    // authorship tuple sig is then verified at the ONE pre-pay seam below (extending the receipt
    // cosig). From-scratch jobs skip this block entirely (`contribution == None`).
    let contribution_cosig = if let Some(binds) = request.contribution.as_ref() {
        let base_oid = CommitOid::parse(binds.base_oid.clone())
            .map_err(|error| AuthorizePayError::Input(format!("contribution base_oid: {error}")))?;
        let fork = delivery.clone();
        let policy = contribution_policy(home);
        verifier
            .verify_contribution(
                &fork,
                &binds.target_clone_url,
                &binds.base_branch,
                &base_oid,
                &policy,
            )
            .map_err(AuthorizePayError::Delivery)?;
        // Reconstruct the exact tuple the seller signed (from BUYER-controlled binds) and carry its
        // digest + the seller's signature to the pre-pay seam. A tuple field tampered post-signing
        // (or a sig over a different commit_oid) fails there with ZERO spend.
        let tuple = crate::contribution::AuthorshipTuple {
            job_id: request.job_id.clone(),
            seller_pubkey: seller_hex.clone(),
            target: crate::contribution::TargetRepoPin::new(
                binds.target_owner_pubkey.clone(),
                binds.target_clone_url.clone(),
            )
            .map_err(|error| AuthorizePayError::Input(error.to_string()))?,
            base_oid: binds.base_oid.clone(),
            fork: crate::contribution::ForkRef::new(fork.repo(), fork.branch())
                .map_err(|error| AuthorizePayError::Input(error.to_string()))?,
            commit_oid: fork.commit_oid().as_str().to_owned(),
        };
        Some((tuple.digest_bytes(), binds.tuple_signature.clone()))
    } else {
        None
    };

    // THE LOAD-BEARING PRE-PAY TOOTH (cross-bind / forged-cosig). Rebuild the EXACT receipt
    // preimage the pay path will co-sign and publish (same `receipt_preimage_for` constructor
    // as `build_and_publish_receipt`, so the verified bytes cannot drift from the published
    // bytes) and verify the seller's `sig/seller` over it against the claim-seller anchor —
    // BEFORE the budget gate commits spent and BEFORE the wallet opens. For a contribution the
    // SAME seam ALSO verifies the seller's signed-result authorship tuple (one seam, more binds).
    // Fail-closed here ⇒ ZERO spend: no `authorize_then_attempt`, no lock/mint/send, no receipt,
    // no journal record.
    let prepay_preimage =
        receipt_preimage_for(&key, &buyer_nostr.to_hex(), &seller_hex, delivery_kind);
    let contribution_bind = contribution_cosig
        .as_ref()
        .map(|(digest, sig)| crate::payment::ContributionCosig {
            tuple_digest: *digest,
            tuple_signature_hex: sig.as_str(),
        });
    if let Err(error) = authority.verify_seller_prepay_cosig(
        &prepay_preimage,
        &request.seller_signature,
        contribution_bind,
    ) {
        // Diagnostic: on cosig refusal, surface the buyer's EXACT computed preimage (each
        // field + digest) so the next occurrence self-identifies which field diverged from the
        // seller-signed bytes. Public trade data only — a ReceiptPreimage carries no secret key or
        // proof material (asserted by the never-echo test). Still fail-closed: zero spend.
        return Err(AuthorizePayError::CosigRefused(format!(
            "{error}; buyer preimage [{}]",
            cosig_refusal_diagnostic(&prepay_preimage)
        )));
    }

    // Verify the delivery (allowlist + fetch + tip-match) and bind-check it against the key BEFORE
    // committing any budget below, so a failed or hung delivery verification burns ZERO budget
    // (and does not even open the wallet). The budget append still precedes the wallet send inside
    // `run_verified` (write-before-mint), so the reconcile saga's idempotency is preserved.
    crate::payment::verify_pay_path_delivery(&mut verifier, &delivery, &key)
        .map_err(AuthorizePayError::Payment)?;

    // THE §19 EXECUTION-SENTINEL TOOTH (from-scratch money path). The delivery the buyer just fetched
    // + tip-matched into its OWN store MUST carry this job's execution sentinel inside the delivered
    // tree — evidence the buyer independently reads, never the seller's testimony. Runs BEFORE the
    // budget gate / wallet / journal below, so a missing / invalid / replayed sentinel refuses with
    // ZERO spend and no journal, and durably records the refusal for §17 (the JOURNAL is the artifact,
    // never the silence — an absent record is never read as a refusal, §7.0). Contribution deliveries
    // are gated by verify_contribution's content path and are not served by the node yet; their
    // buyer-side sentinel check is a later slice.
    if request.job_class == JobClass::FromScratch {
        let commit_hex = delivery.commit_oid().as_str();
        let store_ref = PayPathDeliveryVerifier::store_ref_for(commit_hex);
        // The verifier owns the store path it just fetched the delivery into (`store` was moved into
        // it); read the tree back from that same store.
        match delivery_tree_carries_sentinel(
            verifier.repository(),
            &store_ref,
            commit_hex,
            &expected_job_hash,
            &request.job_id,
        ) {
            Ok(true) => {}
            Ok(false) => {
                journal_sentinel_refusal(home, &request.job_id, commit_hex, "missing_or_replayed");
                return Err(AuthorizePayError::NoSentinel(format!(
                    "delivery {commit_hex} carries no execution sentinel bound to job {} (§19); \
                     refusing no_sentinel with zero spend",
                    request.job_id
                )));
            }
            Err(reason) => {
                // A store-read failure is a positive failure to VERIFY, not silence: fail closed
                // (refuse, zero spend) rather than pay a tree we could not read. Journalled with a
                // distinct class so §17 does not misattribute a buyer-side read fault to the seller.
                journal_sentinel_refusal(home, &request.job_id, commit_hex, "verify_error");
                return Err(AuthorizePayError::NoSentinel(format!(
                    "delivery {commit_hex} sentinel could not be verified ({reason}); fail-closed, zero spend"
                )));
            }
        }
    }

    let amount = request.amount_sats;
    // Cross-mint hop planning. Pre-budget and pre-spend: raising quotes moves no money, so a hop
    // that cannot be priced refuses having spent nothing. Delivery is PINNED here — the quote at the
    // seller's mint is raised for exactly `amount`, the buyer-signed offer amount — and the buyer's
    // COST is what floats, which is why no fee reading can reduce what the seller receives.
    let mut hop = match plan.hop_source() {
        None => None,
        Some(source) => {
            let store = FsHopJournal::new(crossmint_hop::hop_journal_dir(home));
            let effects = CdkHopEffects::open(
                home,
                &source.to_string(),
                &wallet_open_mint_url(home, &terms),
            )
            .await?;
            // A pairing already on disk WINS over freshly raised quotes. This attempt may have
            // melted on an earlier run, and a second melt quote for one attempt id is precisely the
            // double-pay the hop journal exists to prevent.
            let pairing = match crossmint_hop::journalled_pairing(&store, attempt_id.as_str())? {
                Some(existing) => existing,
                None => effects.plan_quotes(attempt_id.as_str(), amount).await?,
            };
            Some((store, effects, pairing))
        }
    };

    let wallet = buyer_fund::open_wallet_at_mint_async(home, &wallet_open_mint_url(home, &terms))
        .await?;
    // Wallet HTTP must run ONLY on the wallet worker, never on this caller runtime. A pre-spawn dust
    // check here ran on the current-thread runtime `collect_blocking` builds (collect.rs), priming a
    // reqwest pooled connection whose IO driver task lived on the caller; the worker then blocked that
    // same runtime on the effects bridge `recv`, so the pooled connection its `prepare_send` needed
    // could never be driven — both parked forever (MakePrisms/maxplayerai#387). The N=1 dust guard is
    // instead run as a WORKER preflight just below (pre-reserve, so a dead mint burns zero budget) and
    // re-checked inside lock_or_reconcile (payment_wallet.rs).
    let payment_send = NostrPaymentSend::new(home.config.relay_url.clone(), keys);
    let mut effects = CdkPaymentEffects::spawn(
        wallet,
        payment_send,
        move |key: &PaymentKey, _payment: &crate::payment_send::PaymentSent| {
            build_and_publish_receipt(
                &buyer_receipt_keys,
                &receipt_relay,
                &seller_hex,
                &seller_signature,
                delivery_kind,
                key,
            )
        },
    )
    .map_err(|error| AuthorizePayError::Effects(error.to_string()))?;

    // Pre-reserve dust/liveness guard, run on the WORKER runtime (see above). A dead/hung mint refuses
    // HERE — before the budget gate below — so it burns ZERO spend, exactly as the removed pre-spawn
    // check did, but with wallet HTTP on the worker (no cross-runtime deadlock). Bounded by
    // MINT_TOUCH_TIMEOUT (inside the check) + the bridge recv ceiling. lock_or_reconcile re-checks the
    // real input-count fee after prepare_send; this only refuses the dead-mint / N=1-dust cases early.
    // #185: the returned value is the estimated active-keyset input fee for the send. On the DIRECT
    // path that fee leaves the wallet on the swap but was never counted against the per-job cap; it is
    // folded into `charged` below so the cap sees the full outlay before the send.
    let estimated_input_fee = effects
        .preflight_fee(terms.amount)
        .map_err(AuthorizePayError::from)?;

    // Payment journal — created only AFTER the pre-pay seam passed (a pre-pay refusal leaves no
    // journal on disk, preserving the zero-spend / no-record invariant).
    let journal_dir = home.root.join("payment-journal");
    std::fs::create_dir_all(&journal_dir)
        .map_err(|error| AuthorizePayError::Home(format!("payment journal dir: {error}")))?;
    let journal = FsPaymentJournal::new(journal_dir.join(format!("{}.jsonl", attempt_id.as_str())));

    // What the cap is asked to cover. On the direct path that is the amount PLUS the estimated mint
    // input fee for the send (#185): that fee leaves the wallet on the swap, so the cap must see it
    // before the send — charging the bare amount let the input fee past the per-job cap. A hop costs
    // the buyer more than it delivers, and every sat of that difference must pass the cap BEFORE the
    // melt, so the hop is charged its planned cost: melt amount + the source mint's Lightning fee
    // reserve + its input fee (the hop's own fees are already counted there; the direct estimate does
    // not apply to it).
    //
    // The hop's fee reserve is charged worst-case here, then reconciled against the fee actually paid
    // AFTER the melt (#186, below), crediting the unused reserve back to the ledger.
    let charged = cap_charge(
        hop.as_ref().map(|(_, _, pairing)| pairing),
        amount,
        estimated_input_fee,
    );
    // Delivery already verified + bind-checked above (pre-budget). The budget append happens here,
    // before any melt and before the wallet send inside `run_verified`. `hop_reserve_credit` captures
    // the unused Lightning fee reserve the hop reconciled (#186); it is credited back after the gate
    // closure returns, never inside it (the gate is borrowed there).
    let mut hop_reserve_credit: u64 = 0;
    let state = gate.authorize_then_attempt(attempt_id.as_str(), charged, || {
        if let Some((store, hop_effects, pairing)) = hop.as_mut() {
            let settled = crossmint_hop::run_hop(store, hop_effects, pairing)?;
            hop_reserve_credit = settled.unused_fee_reserve_sats;
        }
        let state =
            PaymentService::new(&journal).run_verified(&key, &terms, &authority, &mut effects)?;
        Ok::<_, AuthorizePayError>(state)
    })??;

    // #186: the hop reserved a worst-case Lightning fee against the cap before the melt; the melt has
    // now settled and returned the unused reserve as change. Credit that difference back so the ledger
    // reflects real outlay rather than the worst-case reservation. Keyed by an id NAMESPACED away from
    // the spend's attempt id, so the credit dedupes independently and cannot be applied twice. A
    // failed credit must NOT fail an already-completed payment (the seller is paid); it leaves spent
    // at the safe over-counted value, so we log and carry on rather than propagate.
    if hop_reserve_credit > 0 {
        let reconcile_id = format!("{}:hop-fee-reconcile", attempt_id.as_str());
        if let Err(error) = gate.credit_reserve(&reconcile_id, hop_reserve_credit) {
            eprintln!(
                "budget reconcile: could not credit {hop_reserve_credit} unused hop fee-reserve sats \
                 for attempt {} ({error}); spent stays over-counted (safe)",
                attempt_id.as_str()
            );
        }
    }

    Ok(AuthorizePayOutcome {
        state,
        attempt_id: attempt_id.as_str().to_owned(),
        amount_sats: amount,
        charged_sats: charged,
        spent_total_sats: gate.spent(),
    })
}

/// Whether the delivered tree retained in the buyer store carries THIS job's execution sentinel
/// (§19). Opens the buyer store (the bare repo the pay path fetched the delivery into), resolves the
/// delivery commit's tree, reads the sentinel manifest at its well-known path, and matches it through
/// the SHARED [`crate::delivery_sentinel::content_carries_sentinel`] — one definition with the seller
/// writer, so a format drift at either end fails the match rather than passing silently. `subtract_path`
/// (the `job_id`, the seller's workdir label the buyer already knows) is removed before matching so a
/// token reachable only through a path echo cannot count. `Ok(false)` = read fine, no job-bound
/// sentinel present (missing or replayed); `Err` = the tree could not be read (the caller fails closed).
fn delivery_tree_carries_sentinel(
    store: &Path,
    store_ref: &str,
    commit_hex: &str,
    expected_job_hash: &str,
    subtract_path: &str,
) -> Result<bool, String> {
    let repo = git2::Repository::open_bare(store)
        .map_err(|error| format!("open buyer store {}: {error}", store.display()))?;
    let commit = repo
        .revparse_single(store_ref)
        .or_else(|_| repo.revparse_single(commit_hex))
        .map_err(|error| {
            format!("delivery {commit_hex} not found in buyer store (ref {store_ref}): {error}")
        })?
        .peel_to_commit()
        .map_err(|error| error.to_string())?;
    let tree = commit.tree().map_err(|error| error.to_string())?;

    // The node writes the manifest at a fixed, well-known path (both ends share the const). An absent
    // entry is a delivery with no sentinel — present, but not a read failure — so it is Ok(false), a
    // refusal, never an Err.
    let entry = match tree.get_path(Path::new(crate::delivery_sentinel::SENTINEL_FILE)) {
        Ok(entry) => entry,
        Err(_) => return Ok(false),
    };
    let object = entry
        .to_object(&repo)
        .map_err(|error| format!("read sentinel object: {error}"))?;
    let blob = object
        .as_blob()
        .ok_or_else(|| "sentinel path is not a blob".to_string())?;
    // A non-UTF-8 sentinel cannot be a genuine manifest (it is text); treat it as not-present rather
    // than a hard read error — the delivery simply lacks a usable sentinel.
    let Ok(text) = std::str::from_utf8(blob.content()) else {
        return Ok(false);
    };
    Ok(crate::delivery_sentinel::content_carries_sentinel(
        text,
        expected_job_hash,
        subtract_path,
    ))
}

/// Durably record a pre-pay execution-sentinel refusal as a LOCAL artifact for §17 reputation to
/// consume later. The record's PRESENCE is the refusal — never the silence (§7.0: an absent record is
/// not a refusal). `class` distinguishes a genuine missing/replayed sentinel (a seller-attributable
/// `no_sentinel`) from a buyer-side verify error, so reputation does not misattribute the latter to
/// the seller. Best-effort: a journal write that itself fails is logged, never converted into a spend
/// — the refusal already stands on the returned error.
fn journal_sentinel_refusal(home: &MaxplayerHome, job_id: &str, commit_oid: &str, class: &str) {
    let dir = home.root.join("sentinel-refusals");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("authorize_pay: sentinel-refusal journal dir failed (continuing): {error}");
        return;
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = serde_json::json!({
        "job_id": job_id,
        "commit_oid": commit_oid,
        "reason_code": crate::gateway::ReasonCode::NoSentinel.as_str(),
        "class": class,
        "at": at,
    });
    let path = dir.join(format!("{job_id}-{commit_oid}.json"));
    if let Err(error) = std::fs::write(&path, format!("{record}\n")) {
        eprintln!("authorize_pay: sentinel-refusal journal write failed (continuing): {error}");
    }
}

/// Operator-invoked completion of ONE payment wedged at a recovered `Locked` — the state the AUTO
/// pay path fails closed on as [`PaymentError::AmbiguousSendRefused`].
///
/// Re-derives the SAME terms/key/attempt id as the original pay (via the shared [`derive_payment`]),
/// opens the SAME `<attempt_id>.jsonl` journal, and drives
/// [`PaymentService::complete_recovered_locked`] — which proof-gates the already-minted P2PK-locked
/// token at the mint (LIVE, never cached) and, only if every proof is Unspent, REUSES that token
/// through the same settlement legs. It NEVER re-mints and NEVER re-verifies delivery (that gated
/// the original spend, which already happened).
///
/// Budget (constraint #4): routed THROUGH [`BudgetGate::authorize_then_attempt`] keyed by the same
/// attempt id, which the original award already counted, so the reserve is a no-op — no bypass, no
/// double charge. The `amount_sats` passed is only ever charged if the id is somehow uncounted (a
/// fail-closed safety net; a genuinely-`Locked` attempt is always already counted).
///
/// NEVER wired into boot or the settle watcher — the only caller is the explicit
/// `maxplayer wallet complete-locked` CLI subcommand.
pub async fn complete_recovered_locked_async(
    home: &MaxplayerHome,
    gate: &mut BudgetGate,
    request: CompleteLockedRequest,
) -> Result<CompleteLockedOutcome, AuthorizePayError> {
    if request.seller_signature.trim().is_empty() {
        return Err(AuthorizePayError::Input(
            "seller_signature is required to co-sign the receipt on completion".into(),
        ));
    }

    let DerivedPayment {
        terms,
        key,
        seller_nostr,
        plan: _,
    } = derive_payment(
        home,
        &request.job_id,
        &request.result_id,
        &request.delivery_integrity_hash,
        &request.job_hash,
        &request.seller_pubkey,
        request.amount_sats,
        &request.accepted_mints,
        request.realized_mint.as_deref(),
        request.creq_hash.clone(),
    )?;
    let attempt_id = key.attempt_id();

    let secret_hex = home::read_secret_key_hex(home)
        .map_err(|error| AuthorizePayError::Home(error.to_string()))?;
    let keys = Keys::parse(&secret_hex)
        .map_err(|error| AuthorizePayError::Home(format!("buyer key parse: {error}")))?;
    let authority = ReceiptAuthority {
        // External anchors: buyer == this buyer's own key, seller == the accepted-claim seller.
        buyer: keys.public_key(),
        seller: seller_nostr,
    };
    // Receipt-publish inputs captured before `keys` moves into the payment sender.
    let buyer_receipt_keys = keys.clone();
    let receipt_relay = home.config.relay_url.clone();
    let seller_hex = seller_nostr.to_hex();
    let seller_signature = request.seller_signature.clone();
    // Live delivery is fork-only ([`DeliveryKind::Fork`]); the receipt preimage the seller signed at
    // delivery used that kind, so completion reconstructs byte-identical bytes.
    let delivery_kind = DeliveryKind::Fork;

    let wallet = buyer_fund::open_wallet_at_mint_async(home, &wallet_open_mint_url(home, &terms))
        .await?;
    let payment_send = NostrPaymentSend::new(home.config.relay_url.clone(), keys);
    let mut effects = CdkPaymentEffects::spawn(
        wallet,
        payment_send,
        move |key: &PaymentKey, _payment: &crate::payment_send::PaymentSent| {
            build_and_publish_receipt(
                &buyer_receipt_keys,
                &receipt_relay,
                &seller_hex,
                &seller_signature,
                delivery_kind,
                key,
            )
        },
    )
    .map_err(|error| AuthorizePayError::Effects(error.to_string()))?;

    // Same journal file the pay path writes — completion advances THIS attempt's wedge, never a new one.
    let journal_dir = home.root.join("payment-journal");
    let journal = FsPaymentJournal::new(journal_dir.join(format!("{}.jsonl", attempt_id.as_str())));

    // Route through the gate (no bypass) keyed by the already-counted attempt id (no double charge).
    // The completion runs under the same cross-process spend lock + ledger discipline as the pay path.
    let state = gate.authorize_then_attempt(attempt_id.as_str(), request.amount_sats, || {
        PaymentService::new(&journal).complete_recovered_locked(&key, &terms, &authority, &mut effects)
    })??;

    Ok(CompleteLockedOutcome {
        state,
        attempt_id: attempt_id.as_str().to_owned(),
        amount_sats: request.amount_sats,
        spent_total_sats: gate.spent(),
    })
}

/// Resolve the buyer's content policy hook from `[contribution]` config, or the
/// FLOOR (refuse only empty diffs) when unconfigured. Buyer-side; never seller-influenced.
fn contribution_policy(home: &MaxplayerHome) -> crate::contribution::ContentPolicy {
    match &home.config.contribution {
        Some(cfg) => crate::contribution::ContentPolicy {
            allowed_paths: cfg.allowed_paths.clone(),
            forbidden_paths: cfg.forbidden_paths.clone(),
            max_diff_bytes: cfg.max_diff_bytes,
        },
        None => crate::contribution::ContentPolicy::floor(),
    }
}

/// The mint URL the pay path opens the buyer wallet at: the FROZEN realized mint sealed into the
/// payment terms (`terms.mint`), NEVER `home.config.default_mint()`. The realized mint is already
/// planned from the sealed accept-bind ([`crate::crossmint::plan_payment`]) and bound into `terms`;
/// opening the wallet at the live config default instead would, after accept seals mint A and the
/// buyer flips its config default to B, bind the wallet to B while the attempt id + send target A —
/// the budget is appended, then the send refuses on mint mismatch and strands the reservation.
/// Taking the mint from the sealed terms keeps the wallet, the attempt id, and the send all on one
/// mint. `home` is passed so the already-fenced invariant is asserted at this seam (the realized
/// mint was fenced while planning; `open_wallet_at_mint_async` re-checks, redundant-safe).
pub(crate) fn wallet_open_mint_url(home: &MaxplayerHome, terms: &PaymentTerms) -> String {
    let mint_url = terms.mint.to_string();
    debug_assert!(
        crate::home::mint_allowed(&mint_url, home.config.allow_real_mints),
        "frozen realized mint must already be fenced before wallet open"
    );
    mint_url
}

/// What the budget cap is asked to cover for one payment.
///
/// The direct path is charged the amount PLUS the estimated mint input fee for the send (#185). That
/// input fee leaves the wallet on the swap that produces the seller-locked token, so charging the
/// bare amount let it past the per-job cap — a hop's fees were counted, the direct path's were not.
/// `estimated_input_fee` is the N=1 active-keyset floor from the pre-send preflight; it is a floor,
/// so the cap counts at least one input's worth of fee before the send.
///
/// A hop costs the buyer more than it delivers, and every sat of that difference has to pass the cap
/// BEFORE the melt, so a hop is charged its planned cost (which already folds in the hop's own fee
/// reserve and input fee); the direct estimate does not apply to it. A hop charged the delivered
/// amount would put the Lightning fee reserve and the source mint's input fee on the wire without the
/// cap ever seeing them.
fn cap_charge(
    hop: Option<&crate::crossmint::HopJournal>,
    amount: u64,
    estimated_input_fee: u64,
) -> u64 {
    match hop {
        Some(pairing) => pairing.planned_cost,
        None => amount.saturating_add(estimated_input_fee),
    }
}

/// Render a co-signed [`ReceiptPreimage`] as a single-line diagnostic: the digest plus every
/// covered field. EVERY field here is public trade data already on the relay (offer/claim/result/
/// receipt tags) — a `ReceiptPreimage` never holds a secret key or proof/token material — so this
/// is safe to log/return on a cosig refusal. The never-echo test asserts no secret leaks.
fn cosig_refusal_diagnostic(preimage: &ReceiptPreimage) -> String {
    format!(
        "digest={} job_hash={} offer_id={} amount={} unit={} buyer_pubkey={} \
         seller_pubkey={} delivery_integrity_hash={} delivery_kind={} exec_metadata_commitment={} \
         creq_hash={}",
        preimage.digest_hex(),
        preimage.job_hash,
        preimage.offer_id,
        preimage.amount,
        preimage.unit,
        preimage.buyer_pubkey,
        preimage.seller_pubkey,
        preimage.delivery_integrity_hash,
        preimage.delivery_kind,
        preimage.exec_metadata_commitment,
        preimage.creq_hash.as_deref().unwrap_or("none"),
    )
}

fn cashu_compressed_from_nostr(key: &NostrPublicKey) -> Result<CashuPublicKey, AuthorizePayError> {
    CashuPublicKey::from_str(&format!("02{}", key.to_hex())).map_err(|error| {
        AuthorizePayError::Input(format!("cashu pubkey from nostr key: {error}"))
    })
}

/// The stable payment terms + key (⇒ attempt id) derived from a trade's identity inputs, plus the
/// pay plan. Returned by [`derive_payment`], the SINGLE derivation shared by the pay path and the
/// operator completion path.
struct DerivedPayment {
    terms: PaymentTerms,
    key: PaymentKey,
    /// The seller's nostr key (Copy) — reused for the receipt authority + receipt co-sign so callers
    /// do not re-parse it.
    seller_nostr: NostrPublicKey,
    plan: crate::crossmint::PayPlan,
}

/// Derive the stable [`PaymentTerms`] + [`PaymentKey`] (and thus the attempt id) from a trade's
/// identity inputs. This is the ONE derivation both [`authorize_pay_async`] and
/// [`complete_recovered_locked_async`] call, so both compute the IDENTICAL attempt id for the same
/// job — a re-derivation drift would target a different journal file and could double-pay. Pure
/// beyond reading the home config's default mint + real-mint policy.
///
/// The realized mint is chosen from the seller's `creq` `m` list via the SELECTION frozen into the
/// accept-bind (`realized_mint`), not the live config default — so a config-default change between
/// attempts cannot shift the mint and mint a second attempt id. `plan_payment` still enforces
/// accepted-set membership + the real-mint fence over that selection, and plans a cross-mint hop
/// when the selected mint is NOT in the seller's accepted set — a membership test over the mint
/// URLs that reads no wallet balances; a legacy bind (no sealed mint) falls back to the live default.
#[allow(clippy::too_many_arguments)]
fn derive_payment(
    home: &MaxplayerHome,
    job_id: &str,
    result_id: &str,
    delivery_integrity_hash: &str,
    job_hash: &str,
    seller_pubkey: &str,
    amount_sats: u64,
    accepted_mints: &[String],
    realized_mint: Option<&str>,
    creq_hash: Option<String>,
) -> Result<DerivedPayment, AuthorizePayError> {
    let job_id =
        JobId::new(job_id.to_owned()).map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let result_id = ResultId::new(result_id.to_owned())
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let delivery_integrity_hash = DeliveryIntegrityHash::from_hex(delivery_integrity_hash)
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let job_hash =
        JobHash::from_hex(job_hash).map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let seller_nostr = NostrPublicKey::parse(seller_pubkey)
        .map_err(|error| AuthorizePayError::Input(format!("seller_pubkey: {error}")))?;
    let seller_p2pk = cashu_compressed_from_nostr(&seller_nostr)?;
    let buyer_selected_mint = realized_mint.unwrap_or_else(|| home.config.default_mint());
    let plan = crate::crossmint::plan_payment(
        buyer_selected_mint,
        accepted_mints,
        home.config.allow_real_mints,
    )?;
    let terms = PaymentTerms::new(
        plan.realized_mint().clone(),
        Amount::from(amount_sats),
        CurrencyUnit::Sat,
        seller_nostr,
        seller_p2pk,
    );
    let key = PaymentKey::new(
        job_id,
        result_id,
        delivery_integrity_hash,
        job_hash,
        &terms,
        creq_hash,
    );
    Ok(DerivedPayment {
        terms,
        key,
        seller_nostr,
        plan,
    })
}

/// The SINGLE co-signed-receipt-preimage constructor for this trade.
///
/// Used by BOTH the pre-pay seller-cosig tooth (before any spend) and
/// [`build_and_publish_receipt`] (at publish), so the bytes the buyer verifies pre-spend are
/// byte-identical to the bytes it later co-signs and publishes — the two can never drift.
/// `delivery_kind` is derived from the typed [`Delivery`] variant (`Commit` → `"fork"`);
/// `exec_metadata_commitment` is the empty marker (exec-metadata is seller-claimed, not
/// co-signed). Field set / order matches `receipt.rs` `ReceiptPreimage`.
fn receipt_preimage_for(
    key: &PaymentKey,
    buyer_pubkey_hex: &str,
    seller_pubkey_hex: &str,
    delivery_kind: DeliveryKind,
) -> ReceiptPreimage {
    ReceiptPreimage {
        job_hash: key.job_hash.as_str().to_owned(),
        offer_id: key.job_id.as_str().to_owned(),
        amount: key.amount.to_u64(),
        unit: key.unit.to_string(),
        buyer_pubkey: buyer_pubkey_hex.to_owned(),
        seller_pubkey: seller_pubkey_hex.to_owned(),
        delivery_integrity_hash: key.delivery_integrity_hash.as_str().to_owned(),
        delivery_kind: delivery_kind.as_str().to_owned(),
        exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
        // Bind the seller-authored request hash the key carries, so the pre-pay tooth
        // and the published receipt co-sign the same bytes (byte-identical when `None`).
        creq_hash: key.creq_hash.clone(),
    }
}

/// Build + publish the buyer-authored kind-3400 receipt for a sent
/// payment, and return the co-signature evidence the [`ReceiptAuthority`] verifies.
///
/// The buyer reconstructs the SAME receipt preimage the seller signed at delivery (binds
/// the trade + the delivered git object; `exec_metadata_commitment` = empty marker —
/// exec-metadata is seller-claimed, not co-signed), counter-signs it deterministically,
/// builds the kind-3400 with a FRESH wall-clock `created_at`, and publishes it. `receipt_id`
/// is that 3400 event id — NOT the kind-1059 payment envelope — and is NON-deterministic
/// per publish attempt (see [`receipt_created_at`]). Empty `relay_success` is enforced
/// fail-closed by [`ReceiptAuthority::verify`]; recovery re-runs this publish (a fresh
/// id each attempt — verify-irrelevant, never a re-sent payment).
fn build_and_publish_receipt(
    buyer_keys: &Keys,
    relay_url: &str,
    seller_hex: &str,
    seller_signature: &str,
    delivery_kind: DeliveryKind,
    key: &PaymentKey,
) -> Result<ReceiptEvidence, EffectError> {
    let buyer_hex = buyer_keys.public_key().to_hex();
    let mint = key.mint.to_string();
    let amount = key.amount.to_u64();
    // offer_id == job_id in this codebase (the offer event id is the job id). Built via the
    // SINGLE shared constructor the pre-pay tooth also uses, so the co-signed bytes published
    // here are byte-identical to the bytes verified before the spend (they cannot drift).
    let preimage = receipt_preimage_for(key, &buyer_hex, seller_hex, delivery_kind);
    let digest = preimage.digest_bytes();
    // Buyer counter-signature (no aux-rand): a `sig/buyer` tag that is a pure function of the
    // preimage. This makes only the co-SIGNATURE deterministic — NOT the event id, which also
    // hashes the fresh `created_at` and so differs per publish (see `receipt_created_at`).
    let secp = Secp256k1::new();
    let keypair = buyer_keys.secret_key().keypair(&secp);
    let buyer_signature = secp
        .sign_schnorr_no_aux_rand(&Message::from_digest(digest), &keypair)
        .to_string();

    let draft = gateway::receipt_draft(
        key.job_id.as_str(),
        key.result_id.as_str(),
        &buyer_hex,
        seller_hex,
        &mint,
        amount,
        key.job_hash.as_str(),
        seller_signature,
        &buyer_signature,
        // The receipt event carries the bound request hash (absent for a trade with no creq).
        key.creq_hash.as_deref(),
        Some(gateway::ReceiptDelivery {
            integrity_hash: key.delivery_integrity_hash.as_str(),
            kind: delivery_kind.as_str(),
        }),
        // No exec-metadata echo: the commitment is the empty marker, so echoing
        // seller-claimed tags here would be cosmetic-only.
        &[],
    );
    let builder = gateway::nostr::event_builder(&draft)
        .map_err(|error| EffectError::new(format!("receipt event builder: {error}")))?;
    let event = builder
        .custom_created_at(receipt_created_at(&digest))
        .sign_with_keys(buyer_keys)
        .map_err(|error| EffectError::new(format!("receipt sign: {error}")))?;
    // Non-deterministic per publish attempt (fresh `created_at`); `receipt_id` records
    // whichever id the accepted publish produced — verify-irrelevant metadata.
    let receipt_id = event.id.to_hex();
    let relay_success = publish_receipt_event(relay_url, buyer_keys, &event)?;

    Ok(ReceiptEvidence {
        receipt_id,
        author: buyer_keys.public_key(),
        preimage,
        seller_signature: seller_signature.to_owned(),
        buyer_signature,
        relay_success,
    })
}

/// FRESH wall-clock `created_at` for each kind-3400 receipt publish attempt.
///
/// A digest-derived `created_at` (windowed into 2023-11 .. ~2027) would reproduce the SAME
/// event id on a recovery republish — relay-native idempotency (a relay stores an event once,
/// by id) — but that timestamp almost never falls inside a real relay's accept window
/// (maxplayer-relay ≈ ±30 min of server time), so the receipt is rejected and the payment holds at
/// `Sent` forever. A fresh wall-clock timestamp satisfies the relay window, so the receipt
/// publishes.
///
/// DELIBERATE TRADE-OFF (a deterministic id and a fresh timestamp are mutually exclusive — the
/// event id hashes `created_at`): the receipt event id is NON-deterministic per attempt.
/// Money-safe: [`ReceiptAuthority::verify`] never uses the id (it gates on relay acceptance +
/// author + preimage + both schnorr co-signatures), and re-publishing a receipt never re-sends
/// money (the send is durable at `Sent`; the reducer re-runs only the receipt leg). In the
/// normal path the first attempt publishes and the state advances `Sent`→`ReceiptPublished`,
/// so there is no second attempt. A duplicate (inert) kind-3400 is possible ONLY if the process
/// crashes AFTER the relay accepts but BEFORE the WAL records `ReceiptPublished`; nothing in the
/// money path reads kind-3400 back, so it is harmless.
///
/// If a Rust receipts-reader is ever added it MUST dedup on read by (author, job-hash), NOT by
/// event id, to collapse such a duplicate — in place of relay-native id-dedup.
///
/// `_digest` is accepted only for call-site parity with a digest-derived form and is
/// intentionally unused: the timestamp must track wall-clock, never the preimage.
fn receipt_created_at(_digest: &[u8; 32]) -> Timestamp {
    Timestamp::now()
}

/// Publish the signed kind-3400 to the relay and return the accepted relay set.
///
/// Runs on a fresh OS thread with its own current-thread runtime: publishing is async and
/// the caller may already hold a Tokio runtime (a nested `block_on` would panic).
///
/// maxplayer-relay requires NIP-42 AUTH for ALL writes, so this path completes + WAITS FOR the
/// auth handshake before `send_event_to` (via the shared `wait_for_nip42_auth`); the
/// payment WRAP path already authenticates, as does this receipt path. On auth
/// timeout/failure the send is NOT reached and an empty `relay_success` is returned (never a
/// forced success) ⇒ [`ReceiptAuthority::verify`] fails closed, the payment reducer holds at
/// `Sent`, and the receipt republishes on recovery (a FRESH id per attempt — see
/// [`receipt_created_at`] — verify-irrelevant and never a re-sent payment).
fn publish_receipt_event(
    relay_url: &str,
    keys: &Keys,
    event: &nostr_sdk::Event,
) -> Result<Vec<String>, EffectError> {
    use nostr_sdk::prelude::{Client, RelayUrl};
    use std::time::Duration;
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| EffectError::new(format!("receipt runtime: {error}")))?;
                runtime.block_on(async {
                    let client = Client::new(keys.clone());
                    // Enable the auto-AUTH responder explicitly (default true; set to mirror
                    // the seller and guard against option drift) so the client answers the
                    // relay's NIP-42 challenge — otherwise the write is rejected auth-required.
                    client.automatic_authentication(true);
                    client.add_relay(relay_url).await.map_err(|error| {
                        EffectError::new(format!("receipt add relay: {error}"))
                    })?;
                    // Subscribe to the relay's notification stream BEFORE connect —
                    // `Authenticated` is emitted once and is not re-emitted (relay quirk; see
                    // `relay_auth::wait_for_nip42_auth`).
                    let parsed_relay = RelayUrl::parse(relay_url).map_err(|error| {
                        EffectError::new(format!("receipt parse relay url: {error}"))
                    })?;
                    let relay = client
                        .relays()
                        .await
                        .get(&parsed_relay)
                        .cloned()
                        .ok_or_else(|| {
                            EffectError::new("receipt relay missing after add_relay")
                        })?;
                    let mut relay_notifications = relay.notifications();
                    client.connect().await;
                    client.wait_for_connection(Duration::from_secs(20)).await;
                    // Auth gate: the receipt write MUST NOT be sent until the relay confirms
                    // NIP-42 AUTH. On timeout/failure we fail CLOSED with an empty relay set
                    // (send not reached, never a forced success) — the designed-safe
                    // direction (no double-pay; payment holds at `Sent` and retries).
                    let relay_success = if matches!(
                        crate::relay_auth::wait_for_nip42_auth(
                            &mut relay_notifications,
                            Duration::from_secs(20),
                        )
                        .await,
                        Ok(crate::relay_auth::AuthWait::Authenticated)
                    ) {
                        let output = client.send_event_to([relay_url], event).await;
                        client.disconnect().await;
                        let output = output
                            .map_err(|error| EffectError::new(format!("receipt send: {error}")))?;
                        // Diagnostic (NOT money-semantics): surface the relay's per-relay
                        // rejection reason (e.g. "invalid: event timestamp too far from server
                        // time") — previously discarded. Relay URL + reason only; no key
                        // material.
                        if !output.failed.is_empty() {
                            let reasons: Vec<String> = output
                                .failed
                                .iter()
                                .map(|(url, reason)| format!("{url}: {reason}"))
                                .collect();
                            eprintln!(
                                "receipt publish: relay rejected kind-3400 ({})",
                                reasons.join("; ")
                            );
                        }
                        output.success.iter().map(|url| url.to_string()).collect()
                    } else {
                        client.disconnect().await;
                        Vec::new()
                    };
                    Ok::<Vec<String>, EffectError>(relay_success)
                })
            })
            .join()
            .map_err(|_| EffectError::new("receipt publisher thread panicked"))?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cashu::MintUrl;

    use crate::budget::BudgetGate;
    use crate::home::{self, DEFAULT_MINT_URL};

    // A real (non-testnut) mint — admissible ONLY when `allow_real_mints` is true.
    const REAL_MINT: &str = "https://minibits.example";

    // Empty creq list → pay from the buyer's configured mint (config-driven).
    // Default flag (false): the configured testnut/dev mint plans a direct payment.
    #[test]
    fn pay_plan_empty_creq_uses_configured_mint() {
        let plan = crate::crossmint::plan_payment(DEFAULT_MINT_URL, &[], false).unwrap();
        assert!(!plan.is_hop());
        assert_eq!(
            plan.realized_mint(),
            &MintUrl::from_str(DEFAULT_MINT_URL).unwrap()
        );
    }

    // Direct path: the buyer's configured mint is one the seller listed → pay from it directly.
    #[test]
    fn pay_plan_is_direct_when_configured_mint_is_listed() {
        let plan = crate::crossmint::plan_payment(
            DEFAULT_MINT_URL,
            &[
                "https://other.example".to_string(),
                DEFAULT_MINT_URL.to_string(),
            ],
            false,
        )
        .unwrap();
        assert!(!plan.is_hop(), "overlap must not hop");
        assert_eq!(
            plan.realized_mint(),
            &MintUrl::from_str(DEFAULT_MINT_URL).unwrap()
        );
    }

    // The boundary, half one. A configured mint outside the creq list used to be the end of the
    // road for this claim; it is now a hop to a mint the seller does accept.
    //
    // `allow_real_mints` is on because it has to be: with the flag off the fence admits exactly one
    // mint (the testnut default), so a buyer and a seller can never be at two DIFFERENT admissible
    // mints and a hop is structurally unreachable in the default posture. The flag is what makes
    // two distinct admissible mints possible at all.
    #[test]
    fn pay_plan_hops_when_the_configured_mint_is_not_listed_but_the_target_is_admissible() {
        let plan = crate::crossmint::plan_payment(
            "https://buyer-only.example",
            &[DEFAULT_MINT_URL.to_string()],
            true,
        )
        .unwrap();
        assert!(plan.is_hop(), "no overlap must plan a hop, not refuse");
        assert_eq!(
            plan.hop_source(),
            Some(&MintUrl::from_str("https://buyer-only.example").unwrap())
        );
        assert_eq!(
            plan.realized_mint(),
            &MintUrl::from_str(DEFAULT_MINT_URL).unwrap()
        );
    }

    // The same no-overlap shape under the DEFAULT posture refuses, because the buyer's own mint is
    // not admissible there — the fence stops it before a target is even considered. Together with
    // the two tests around it this pins the whole boundary: a hop needs the operator's opt-in AND an
    // admissible landing, and the fence refuses first when either is missing.
    #[test]
    fn pay_plan_refuses_a_no_overlap_hop_under_the_default_posture() {
        let error = crate::crossmint::plan_payment(
            "https://buyer-only.example",
            &[DEFAULT_MINT_URL.to_string()],
            false,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("real-mint fence"),
            "got: {error}"
        );
    }

    // The boundary, half two. No overlap AND no admissible landing still refuses fail-closed — the
    // target fence is the refusal that now covers what the old membership check covered. This pair
    // pins exactly where "hop" ends and "refuse" begins.
    #[test]
    fn pay_plan_refuses_when_no_overlap_and_no_accepted_mint_is_admissible() {
        let error =
            crate::crossmint::plan_payment(DEFAULT_MINT_URL, &[REAL_MINT.to_string()], false)
                .unwrap_err();
        assert!(matches!(error, AuthorizePayError::Input(_)));
        let rendered = error.to_string();
        assert!(rendered.contains("real-mint fence"), "got: {rendered}");
        assert!(
            rendered.contains("nowhere permitted to land"),
            "got: {rendered}"
        );
    }

    // Real-mint switch: a buyer configured at a real mint X is REFUSED by the fence when the
    // operator sets `allow_real_mints = false` (opt-out; since #378 the default is true)...
    #[test]
    fn pay_plan_real_mint_refused_when_flag_false() {
        let error =
            crate::crossmint::plan_payment(REAL_MINT, &[REAL_MINT.to_string()], false).unwrap_err();
        assert!(matches!(error, AuthorizePayError::Input(_)));
        assert!(error.to_string().contains("real-mint fence"));
    }

    // ...and ADMITTED (pays at X when the creq lists X) once the operator opts in with the flag.
    #[test]
    fn pay_plan_real_mint_admitted_when_flag_true() {
        let plan =
            crate::crossmint::plan_payment(REAL_MINT, &[REAL_MINT.to_string()], true).unwrap();
        assert!(!plan.is_hop());
        assert_eq!(plan.realized_mint(), &MintUrl::from_str(REAL_MINT).unwrap());

        // With the flag on, a creq that lists a DIFFERENT admissible mint is now reachable by hop
        // rather than refused for non-membership.
        let hopped =
            crate::crossmint::plan_payment(REAL_MINT, &[DEFAULT_MINT_URL.to_string()], true)
                .unwrap();
        assert!(hopped.is_hop());
        assert_eq!(
            hopped.realized_mint(),
            &MintUrl::from_str(DEFAULT_MINT_URL).unwrap()
        );
    }

    // Build a current-thread runtime and block on `authorize_pay_async` — the pattern the MCP
    // dispatch's own runtime provides in production. Lets the sync `#[test]` cases drive the async
    // authorize path directly.
    fn authorize_pay_blocking(
        home: &MaxplayerHome,
        gate: &mut BudgetGate,
        request: AuthorizePayRequest,
    ) -> Result<AuthorizePayOutcome, AuthorizePayError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime")
            .block_on(authorize_pay_async(home, gate, request))
    }

    // ── #374 §19 buyer-side execution-sentinel gate (the from-scratch money decision) ──────────
    //
    // The buyer's pre-pay decision turns on `delivery_tree_carries_sentinel`, reading the delivered
    // tree the pay path retained into the buyer store. These drive it directly against a seeded store
    // (the same store `verify_pay_path_delivery` populates), so the artifact predicate is proven
    // without a live mint / network fetch. On `Ok(false)` the from-scratch arm of `authorize_pay_async`
    // returns `NoSentinel` BEFORE the budget gate — the identical pre-spend return that
    // `collect_forged_cosig_blocks_pay_and_materialize_zero_spend` proves burns zero spend and leaves
    // no journal. The full-path spent==0 red-prove (through `collect_async`) is integration-level
    // (needs the git_http delivery fixture) — see `tests/collect_integrity.rs`.

    fn temp_dir_374(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "maxplayer-sentinel-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Seed a buyer store (bare repo) with a delivered commit (README + optionally the sentinel
    /// manifest at its well-known path) retained under its store ref — mirrors what the pay path
    /// retains post-verify. Returns the delivery commit hex.
    fn seed_store_delivery(store: &Path, sentinel: Option<&str>) -> String {
        let repo = git2::Repository::init_bare(store).expect("init store");
        let readme = repo.blob(b"# delivered\n").expect("blob readme");
        let mut top = repo.treebuilder(None).expect("tree");
        top.insert("README.md", readme, 0o100644).expect("insert readme");
        if let Some(content) = sentinel {
            let blob = repo.blob(content.as_bytes()).expect("blob sentinel");
            top.insert(crate::delivery_sentinel::SENTINEL_FILE, blob, 0o100644)
                .expect("insert sentinel");
        }
        let tree_oid = top.write().expect("write tree");
        let tree = repo.find_tree(tree_oid).expect("find tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let commit_oid = repo
            .commit(None, &sig, &sig, "delivery", &tree, &[])
            .expect("commit");
        let commit_hex = commit_oid.to_string();
        repo.reference(
            &PayPathDeliveryVerifier::store_ref_for(&commit_hex),
            commit_oid,
            true,
            "retain",
        )
        .expect("retain ref");
        commit_hex
    }

    // Positive control — a delivery carrying THIS job's sentinel is accepted, so the red refusals
    // below are meaningful (the predicate reaches its healthy state).
    #[test]
    fn buyer_accepts_a_delivery_carrying_this_jobs_sentinel() {
        let root = temp_dir_374("ok");
        let store = root.join("store");
        let job_hash = "1a".repeat(32);
        let manifest = crate::delivery_sentinel::render_manifest(
            &job_hash,
            crate::delivery_sentinel::DeliveryMode::FromScratch,
            1,
            12,
        );
        let commit = seed_store_delivery(&store, Some(&manifest));
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit);
        let job_id = "9c".repeat(32);
        assert_eq!(
            delivery_tree_carries_sentinel(&store, &store_ref, &commit, &job_hash, &job_id),
            Ok(true),
            "a delivery carrying THIS job's sentinel is accepted (positive control)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // RED-PROVE (missing) — a sentinel-less delivery (old seller build, or a quota-dead run the seller
    // did not catch) refuses. This is the `Ok(false)` that drives `NoSentinel` + zero spend in the
    // from-scratch arm. Revert `delivery_tree_carries_sentinel` to a blanket `Ok(true)` and the buyer
    // pays a sentinel-less delivery — this assertion goes red.
    #[test]
    fn buyer_refuses_a_delivery_with_no_sentinel() {
        let root = temp_dir_374("missing");
        let store = root.join("store");
        let commit = seed_store_delivery(&store, None);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit);
        let job_hash = "1a".repeat(32);
        let job_id = "9c".repeat(32);
        assert_eq!(
            delivery_tree_carries_sentinel(&store, &store_ref, &commit, &job_hash, &job_id),
            Ok(false),
            "a delivery with no sentinel must refuse (drives NoSentinel + zero spend)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // RED-PROVE (replay) — a delivery carrying a VALID sentinel minted for a DIFFERENT job must still
    // refuse: presence is not enough, the job binding must match. Proves job-binding, not mere
    // presence — a blanket-`Ok(true)` revert also fails this.
    #[test]
    fn buyer_refuses_a_replayed_sentinel_from_a_different_job() {
        let root = temp_dir_374("replay");
        let store = root.join("store");
        let other_job = "bb".repeat(32);
        let this_job = "1a".repeat(32);
        let manifest = crate::delivery_sentinel::render_manifest(
            &other_job,
            crate::delivery_sentinel::DeliveryMode::FromScratch,
            1,
            12,
        );
        let commit = seed_store_delivery(&store, Some(&manifest));
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit);
        let job_id = "9c".repeat(32);
        assert_eq!(
            delivery_tree_carries_sentinel(&store, &store_ref, &commit, &this_job, &job_id),
            Ok(false),
            "a sentinel bound to another job is a replay and must refuse (job-binding, not presence)"
        );
        // The SAME tree validates for the job it WAS minted for — the refusal above is binding, not a
        // broken read.
        assert_eq!(
            delivery_tree_carries_sentinel(&store, &store_ref, &commit, &other_job, &job_id),
            Ok(true),
            "and it validates for its own job (the refusal is binding, not a read failure)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn authorize_pay_refuses_empty_buyer_hash_without_burn() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-authorize-pay-d2-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let request = AuthorizePayRequest {
            job_id: "job-d2-empty".into(),
            result_id: "result-d2".into(),
            job_class: JobClass::FromScratch,
            delivery_integrity_hash: String::new(),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 1,
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            // Even if commit_oid is set, empty buyer hash must refuse (no auto-fill).
            commit_oid: "aa".repeat(20),
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        let err = authorize_pay_blocking(&home, &mut gate, request).expect_err("empty tip-match hash");
        let message = err.to_string();
        assert!(
            message.contains("delivery_integrity_hash is required"),
            "unexpected error: {message}"
        );
        assert_eq!(gate.spent(), 0, "empty-hash refuse must not burn spent");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Invariant 3 at the seam that chooses the number. The direct path charges what it always did;
    // a hop charges what it costs, not what it delivers, because the fee reserve and the input fee
    // reach the wire and must therefore pass the cap.
    #[test]
    fn the_cap_is_charged_the_hop_cost_and_the_direct_amount() {
        let pairing = crate::crossmint::HopJournal {
            attempt_id: "attempt-1".to_owned(),
            source_mint: "https://a.example".to_owned(),
            melt_quote_id: "melt-1".to_owned(),
            target_mint: "https://b.example".to_owned(),
            mint_quote_id: "mint-1".to_owned(),
            delivered_sats: 100,
            planned_cost: 109,
            fee_reserve: 7,
            input_fee: 2,
        };
        // Direct path with no input fee is the amount, exactly as before.
        assert_eq!(cap_charge(None, 100, 0), 100, "zero-fee direct is the amount");
        // #185: the direct path folds the estimated mint input fee into the cap charge, so the swap
        // fee cannot reach the wire uncounted.
        assert_eq!(
            cap_charge(None, 100, 3),
            103,
            "the direct path must charge amount + estimated input fee (#185)"
        );
        // The hop is charged its planned cost regardless of the direct estimate.
        assert_eq!(
            cap_charge(Some(&pairing), 100, 3),
            109,
            "a hop must be charged its cost, not the amount it delivers"
        );
        assert_ne!(
            cap_charge(Some(&pairing), pairing.delivered_sats, 0),
            pairing.delivered_sats,
            "charging the delivered amount would let the hop's fees past the cap"
        );
    }

    // #185 RED-PROVE at the cap seam: when the per-job cap equals the bare amount and the swap input
    // fee is >= 1, the direct trade's true outlay (amount + input fee) exceeds the cap, so it MUST
    // refuse with PerJob and burn zero spend. On the pre-#185 code `cap_charge(None, amount)` returned
    // the bare amount, which passed the cap and sent amount + fee — this assertion fails there.
    #[test]
    fn direct_swap_input_fee_must_not_bypass_the_per_job_cap() {
        use crate::budget::{BudgetGate, BudgetRefuse};
        let amount = 100u64;
        let input_fee = 1u64;
        let mut gate = BudgetGate::new(amount); // per_job_cap == amount
        let charged = cap_charge(None, amount, input_fee);
        assert_eq!(charged, amount + input_fee, "the fee must be inside the charge");
        let refusal = gate
            .authorize_then_attempt("attempt-185", charged, || unreachable!("send must not run"))
            .expect_err("amount + input fee exceeds a cap equal to the bare amount");
        assert!(
            matches!(
                refusal,
                BudgetRefuse::PerJob {
                    amount: a,
                    per_job_cap
                } if a == amount + input_fee && per_job_cap == amount
            ),
            "expected a PerJob refusal on amount+fee, got: {refusal}"
        );
        assert_eq!(gate.spent(), 0, "a refused direct trade must burn zero spend");
    }

    // The replacement invariant, at the pay entry point. `mint_unreachable_pay` no longer refuses a
    // buyer that cannot settle at the seller's mint — the hop does, or the fence does. This pins the
    // fence half END TO END: a hop with nowhere admissible to land refuses through the real pay
    // path, burns zero budget, and leaves no pairing on disk for a later run to resume.
    #[test]
    fn authorize_pay_refuses_an_inadmissible_hop_with_zero_spend_and_no_pairing() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-authorize-pay-hop-fence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        // Issue #378 made allow_real_mints default TRUE; force the fenced posture this test needs so
        // the seller's real mint stays inadmissible and the hop has nowhere to land.
        home.config.allow_real_mints = false;
        assert!(
            !home.config.allow_real_mints,
            "fenced posture (allow_real_mints = false) is what makes this refuse"
        );
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let request = AuthorizePayRequest {
            job_id: "job-hop-fence".into(),
            result_id: "result-hop-fence".into(),
            job_class: JobClass::FromScratch,
            delivery_integrity_hash: "aa".repeat(20),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 1,
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            commit_oid: "aa".repeat(20),
            seller_signature: String::new(),
            creq_hash: None,
            // The buyer sits on the one fenced mint; the seller accepts only an unfenced one, so
            // there is nowhere the hop is permitted to land.
            accepted_mints: vec![REAL_MINT.to_string()],
            realized_mint: Some(DEFAULT_MINT_URL.to_string()),
            contribution: None,
        };
        let error = authorize_pay_blocking(&home, &mut gate, request)
            .expect_err("an inadmissible hop target must refuse");
        let message = error.to_string();
        assert!(message.contains("real-mint fence"), "unexpected: {message}");
        assert!(
            message.contains("nowhere permitted to land"),
            "the refusal must say the hop had no permitted target: {message}"
        );
        assert_eq!(gate.spent(), 0, "a refused hop must not burn budget");
        assert!(
            !crate::crossmint_hop::hop_journal_dir(&home).exists(),
            "a refused hop must leave no pairing for a later run to resume"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn authorize_pay_refuses_buyer_hash_mismatch_vs_advertised_commit() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-authorize-pay-d2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let request = AuthorizePayRequest {
            job_id: "job-d2".into(),
            result_id: "result-d2".into(),
            job_class: JobClass::FromScratch,
            delivery_integrity_hash: "aa".repeat(20),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 1,
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            commit_oid: "cc".repeat(20),
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        let err = authorize_pay_blocking(&home, &mut gate, request).expect_err("tip-match mismatch");
        let message = err.to_string();
        assert!(
            message.contains("does not match seller-advertised commit_oid"),
            "unexpected error: {message}"
        );
        assert_eq!(gate.spent(), 0, "tip-match refuse must not burn spent");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding C: the crate pay entry itself refuses a contribution-class job with no contribution
    // binds (defense in depth — a caller that skips the class re-derivation cannot pay it as
    // from-scratch and thereby skip the contribution gates). Zero spend.
    #[test]
    fn authorize_pay_refuses_contribution_class_without_binds() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-authorize-pay-jobclass-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let oid = "aa".repeat(20);
        let request = AuthorizePayRequest {
            job_id: "job-jc".into(),
            result_id: "result-jc".into(),
            job_class: JobClass::Contribution,
            delivery_integrity_hash: oid.clone(),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 2,
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            commit_oid: oid,
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        let err = authorize_pay_blocking(&home, &mut gate, request).expect_err("contribution no binds");
        assert!(
            err.to_string().contains("job_class=contribution requires contribution binds"),
            "unexpected error: {err}"
        );
        assert_eq!(gate.spent(), 0, "seal refuse must not burn spent");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn authorize_pay_refuses_ext_locator_via_pay_path_verifier() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-authorize-pay-ext-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        // Finding N: delivery verification (here the transport allowlist refusing an `ext::`
        // locator) runs and must PASS before any budget is committed, so a failed verify burns
        // ZERO budget. A valid seller co-signature lets the pre-pay cosig seam PASS, so the refusal
        // is exercised at the delivery-verify step (not the cosig).
        let valid_sig = seller_cosig(
            &home,
            &prepay_preimage(&home, "job-ext", "result-ext", &"bb".repeat(32), &"aa".repeat(20), 2),
        );
        let request = AuthorizePayRequest {
            job_id: "job-ext".into(),
            result_id: "result-ext".into(),
            job_class: JobClass::FromScratch,
            delivery_integrity_hash: "aa".repeat(20),
            job_hash: "bb".repeat(32),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 2,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: "aa".repeat(20),
            seller_signature: valid_sig,
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        let err = authorize_pay_blocking(&home, &mut gate, request.clone()).expect_err("ext refused");
        let message = err.to_string();
        assert!(
            message.contains("ext") || message.contains("refused") || message.contains("transport"),
            "unexpected error: {message}"
        );
        // Verify-before-budget: a delivery-verify refusal burns ZERO budget.
        assert_eq!(gate.spent(), 0, "failed delivery verify must not commit budget");

        // A retry still refuses at the verify step, still at zero spend.
        let err2 = authorize_pay_blocking(&home, &mut gate, request).expect_err("retry still refuses");
        let message2 = err2.to_string();
        assert!(
            message2.contains("ext")
                || message2.contains("refused")
                || message2.contains("transport"),
            "unexpected retry error: {message2}"
        );
        assert_eq!(gate.spent(), 0, "retry verify refusal must stay zero spend");
        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 0, "durable spent must stay 0 after verify refusal");
        let _ = std::fs::remove_dir_all(&root);
    }

    // --- PRE-PAY seller-cosig tooth (the cross-bind / forged-cosig fix) ------------------
    // Rebuild the co-signed receipt preimage EXACTLY as `authorize_pay_async` does (via the
    // shared `receipt_preimage_for`), for a home where buyer == seller == the home key. Used to
    // mint a REAL seller co-signature (or one over tampered bytes) for the pre-pay tooth.
    // Finding I: the co-signed receipt preimage is mint-agnostic, so a buyer paying at a NON-default
    // accepted mint builds the SAME digest the seller co-signed at delivery (the seller no longer
    // pins a mint). Two payment keys identical except for the realized mint yield identical digests.
    #[test]
    fn receipt_preimage_digest_is_independent_of_realized_mint() {
        let seller_keys = Keys::generate();
        let seller_nostr = seller_keys.public_key();
        let seller = seller_nostr.to_hex();
        let buyer = Keys::generate().public_key().to_hex();
        let seller_p2pk = cashu_compressed_from_nostr(&seller_nostr).expect("p2pk");
        let key_at = |mint: &str| {
            let terms = PaymentTerms::new(
                MintUrl::from_str(mint).expect("mint"),
                Amount::from(7),
                CurrencyUnit::Sat,
                seller_nostr,
                seller_p2pk,
            );
            PaymentKey::new(
                JobId::new("job").expect("job id"),
                ResultId::new("result").expect("result id"),
                DeliveryIntegrityHash::from_hex(&"11".repeat(32)).expect("oid"),
                JobHash::from_hex(&"22".repeat(32)).expect("job hash"),
                &terms,
                None,
            )
        };
        let default_mint =
            receipt_preimage_for(&key_at(DEFAULT_MINT_URL), &buyer, &seller, DeliveryKind::Fork);
        let other_mint = receipt_preimage_for(
            &key_at("https://other-accepted.testnut.example"),
            &buyer,
            &seller,
            DeliveryKind::Fork,
        );
        assert_eq!(
            default_mint.digest_hex(),
            other_mint.digest_hex(),
            "co-signed preimage digest must not depend on the realized mint"
        );
    }

    // Finding CC (load-bearing): the pay-path attempt id is derived from the SEALED realized-mint
    // SELECTION frozen in the accept-bind — NOT the live config default — so a config-default change
    // BETWEEN attempts (e.g. after a receipt-publish failure, before the buyer retries) cannot shift
    // the realized mint into a DIFFERENT attempt id and mint a SECOND payment for one job. Two
    // attempts for the same accepted job under two different live config defaults must yield the SAME
    // attempt id when the bind seals the mint (budget/journal idempotency then dedups the retry).
    // The control asserts the pre-CC legacy (unsealed) path DIVERGES — exactly the double-pay vector.
    #[test]
    fn sealed_realized_mint_stabilizes_attempt_id_across_config_default_change() {
        // Two distinct admissible paying mints, both in the seller's accepted set. `allow_real_mints`
        // is on so two DIFFERENT mints can both pass the fence (with it off only DEFAULT_MINT_URL
        // does, and there'd be no second admissible mint to shift to).
        let m1 = "https://mint-a.example";
        let m2 = "https://mint-b.example";
        let accepted = vec![m1.to_string(), m2.to_string()];

        let seller_nostr = Keys::generate().public_key();
        let seller_p2pk = cashu_compressed_from_nostr(&seller_nostr).expect("p2pk");
        let attempt_id_for = |mint: MintUrl| {
            let terms = PaymentTerms::new(
                mint,
                Amount::from(7),
                CurrencyUnit::Sat,
                seller_nostr,
                seller_p2pk,
            );
            PaymentKey::new(
                JobId::new("job").expect("job id"),
                ResultId::new("result").expect("result id"),
                DeliveryIntegrityHash::from_hex(&"11".repeat(32)).expect("oid"),
                JobHash::from_hex(&"22".repeat(32)).expect("job hash"),
                &terms,
                None,
            )
            .attempt_id()
        };
        // The pay path's mint selection, mirroring `authorize_pay_async`: the sealed selection wins;
        // a legacy bind (`None`) falls back to the live config default.
        let select = |sealed: Option<&str>, config_default: &str| {
            let chosen = sealed.unwrap_or(config_default);
            crate::crossmint::plan_payment(chosen, &accepted, true)
                .expect("plans")
                .realized_mint()
                .clone()
        };

        // SEALED bind (realized_mint = Some(m1)): the first attempt runs with config default m1, then
        // the buyer changes config default to m2 and RETRIES — both resolve to the sealed m1, so the
        // attempt id is identical and the retry dedups against the already-counted attempt.
        let sealed_first = attempt_id_for(select(Some(m1), m1));
        let sealed_retry = attempt_id_for(select(Some(m1), m2));
        assert_eq!(
            sealed_first.as_str(),
            sealed_retry.as_str(),
            "sealed realized mint must keep the attempt id stable across a config-default change \
             (retry dedups → no second payment)"
        );
        // The stable mint is the SEALED one (m1), never the changed config default (m2).
        assert_eq!(select(Some(m1), m2), MintUrl::from_str(m1).unwrap());

        // CONTROL — legacy unsealed bind (realized_mint = None): the selection follows the live
        // config default, so the m1→m2 change shifts the mint → a DIFFERENT attempt id. This is the
        // pre-CC double-pay vector (budget/journal see a brand-new identity → a second send);
        // sealing (above) is what closes it.
        let legacy_first = attempt_id_for(select(None, m1));
        let legacy_retry = attempt_id_for(select(None, m2));
        assert_ne!(
            legacy_first.as_str(),
            legacy_retry.as_str(),
            "control: the unsealed path lets a config-default change shift the attempt id (the \
             double-pay vector CC closes)"
        );
    }

    fn prepay_preimage(
        home: &MaxplayerHome,
        job_id: &str,
        result_id: &str,
        job_hash: &str,
        oid: &str,
        amount_sats: u64,
    ) -> ReceiptPreimage {
        let hex = home::public_key_hex(home).expect("pubkey");
        let seller_nostr = NostrPublicKey::parse(&hex).expect("seller nostr");
        let seller_p2pk = cashu_compressed_from_nostr(&seller_nostr).expect("p2pk");
        let terms = PaymentTerms::new(
            MintUrl::from_str(DEFAULT_MINT_URL).expect("mint"),
            Amount::from(amount_sats),
            CurrencyUnit::Sat,
            seller_nostr,
            seller_p2pk,
        );
        let key = PaymentKey::new(
            JobId::new(job_id).expect("job id"),
            ResultId::new(result_id).expect("result id"),
            DeliveryIntegrityHash::from_hex(oid).expect("oid"),
            JobHash::from_hex(job_hash).expect("job hash"),
            &terms,
            None,
        );
        // buyer == seller == home key in these tests; `Commit` → delivery_kind "fork".
        receipt_preimage_for(&key, &hex, &hex, DeliveryKind::Fork)
    }

    fn seller_cosig(home: &MaxplayerHome, preimage: &ReceiptPreimage) -> String {
        let secret = home::read_secret_key_hex(home).expect("secret");
        let keys = Keys::parse(&secret).expect("keys");
        keys.sign_schnorr(&Message::from_digest(preimage.digest_bytes()))
            .to_string()
    }

    // THE LOAD-BEARING TOOTH: a forged/mismatched seller signature — a REAL schnorr sig by an
    // unrelated key over the CORRECT preimage (buyer-cosig would PASS / seller-cosig FAILs: the
    // live 21-sat receipt shape) — refuses BEFORE any spend. gate.spent()==0, no wallet opened,
    // no payment journal, never Sent. `repo: ext::…` is chosen so that a REVERTED gate
    // (red-on-revert) still refuses hermetically at the pay-path verifier — but only AFTER
    // committing spent, so removing this tooth flips gate.spent() 0→2.
    #[test]
    fn authorize_pay_refuses_forged_seller_signature_with_zero_spend() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-authorize-pay-forged-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let oid = "aa".repeat(20);
        let job_hash = "bb".repeat(32);
        let preimage = prepay_preimage(&home, "job-forged", "result-forged", &job_hash, &oid, 2);
        // Real schnorr signature, but by an unrelated key — not the claim seller.
        let attacker = Keys::generate();
        let forged_sig = attacker
            .sign_schnorr(&Message::from_digest(preimage.digest_bytes()))
            .to_string();
        let request = AuthorizePayRequest {
            job_id: "job-forged".into(),
            result_id: "result-forged".into(),
            job_class: JobClass::FromScratch,
            delivery_integrity_hash: oid.clone(),
            job_hash,
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 2,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: oid,
            seller_signature: forged_sig,
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        let err = authorize_pay_blocking(&home, &mut gate, request).expect_err("forged sig refused pre-pay");
        assert!(
            err.to_string().contains("pre-pay seller co-signature invalid"),
            "must be the pre-pay tooth refusal, got: {err}"
        );
        assert_eq!(gate.spent(), 0, "forged-sig refuse must be ZERO spend (pre-pay tooth)");
        assert!(
            !home.root.join("payment-journal").exists(),
            "no payment journal may be created (refused before the payment SM / any Sent)"
        );
        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 0, "durable spent must stay 0");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Diagnostic: on a pre-pay cosig refusal the returned error carries the buyer's computed
    // preimage — the digest AND every covered field — so the next live occurrence self-identifies
    // the divergent field. Never-echo: the buyer secret key must not appear (a ReceiptPreimage
    // holds only public trade data).
    #[test]
    fn cosig_refusal_diagnostic_carries_every_field_and_no_secret() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-authorize-pay-diag-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let oid = "aa".repeat(20);
        let job_hash = "bb".repeat(32);
        let creq_hash = "2ad9b34c".repeat(8);
        let preimage = prepay_preimage(&home, "job-diag", "result-diag", &job_hash, &oid, 2);
        let attacker = Keys::generate();
        let forged_sig = attacker
            .sign_schnorr(&Message::from_digest(preimage.digest_bytes()))
            .to_string();
        let request = AuthorizePayRequest {
            job_id: "job-diag".into(),
            result_id: "result-diag".into(),
            job_class: JobClass::FromScratch,
            delivery_integrity_hash: oid.clone(),
            job_hash: job_hash.clone(),
            seller_pubkey: home::public_key_hex(&home).expect("pubkey"),
            amount_sats: 2,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: oid.clone(),
            seller_signature: forged_sig,
            creq_hash: Some(creq_hash.clone()),
            accepted_mints: vec![DEFAULT_MINT_URL.to_string()],
            realized_mint: None,
            contribution: None,
        };
        let seller_pubkey = home::public_key_hex(&home).expect("pubkey");
        let msg = authorize_pay_blocking(&home, &mut gate, request)
            .expect_err("forged sig refused")
            .to_string();

        // Still the pre-pay tooth refusal, and it now carries the full preimage diagnostic.
        assert!(msg.contains("pre-pay seller co-signature invalid"), "got: {msg}");
        for needle in [
            "digest=".to_string(),
            format!("job_hash={job_hash}"),
            "offer_id=job-diag".to_string(),
            "amount=2".to_string(),
            "unit=sat".to_string(),
            format!("buyer_pubkey={seller_pubkey}"),
            format!("seller_pubkey={seller_pubkey}"),
            format!("delivery_integrity_hash={oid}"),
            "delivery_kind=fork".to_string(),
            "exec_metadata_commitment=".to_string(),
            format!("creq_hash={creq_hash}"),
        ] {
            assert!(msg.contains(&needle), "diagnostic missing {needle:?}: {msg}");
        }

        // Never-echo: the buyer secret key never appears in the rendered diagnostic.
        let secret = home::read_secret_key_hex(&home).expect("secret");
        assert!(!secret.is_empty());
        assert!(!msg.contains(&secret), "diagnostic leaked the buyer secret key");
        assert_eq!(gate.spent(), 0, "cosig refusal is zero spend");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Tampered-field parity: a seller signature over the honest preimage no longer verifies
    // once ANY covered field is flipped post-signing (the sig covers the exact canonical
    // bytes). Same refusal, zero spend — checked for the amount field and the delivery oid.
    #[test]
    fn authorize_pay_refuses_tampered_preimage_field_with_zero_spend() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-authorize-pay-tamper-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let seller_hex = home::public_key_hex(&home).expect("pubkey");
        let honest_oid = "aa".repeat(20);
        let honest_hash = "bb".repeat(32);

        // (a) amount tampered: seller signed amount=2, request carries amount=3.
        let sig_over_2 = seller_cosig(
            &home,
            &prepay_preimage(&home, "job-tamper", "result-tamper", &honest_hash, &honest_oid, 2),
        );
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let tampered_amount = AuthorizePayRequest {
            job_id: "job-tamper".into(),
            result_id: "result-tamper".into(),
            job_class: JobClass::FromScratch,
            delivery_integrity_hash: honest_oid.clone(),
            job_hash: honest_hash.clone(),
            seller_pubkey: seller_hex.clone(),
            amount_sats: 3,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: honest_oid.clone(),
            seller_signature: sig_over_2,
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        let err = authorize_pay_blocking(&home, &mut gate, tampered_amount).expect_err("tampered amount");
        assert!(
            err.to_string().contains("pre-pay seller co-signature invalid"),
            "amount tamper must refuse at the pre-pay tooth, got: {err}"
        );
        assert_eq!(gate.spent(), 0, "tampered amount must be zero spend");

        // (b) delivery oid tampered: seller signed oid=aa.., request binds oid=cc..
        let tampered_oid = "cc".repeat(20);
        let sig_over_aa = seller_cosig(
            &home,
            &prepay_preimage(&home, "job-tamper2", "result-tamper2", &honest_hash, &honest_oid, 2),
        );
        let mut gate2 = BudgetGate::from_home(&home).expect("gate");
        let tampered_delivery = AuthorizePayRequest {
            job_id: "job-tamper2".into(),
            result_id: "result-tamper2".into(),
            job_class: JobClass::FromScratch,
            delivery_integrity_hash: tampered_oid.clone(),
            job_hash: honest_hash,
            seller_pubkey: seller_hex,
            amount_sats: 2,
            repo: "ext::sh -c evil".into(),
            branch: "main".into(),
            commit_oid: tampered_oid,
            seller_signature: sig_over_aa,
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        let err2 = authorize_pay_blocking(&home, &mut gate2, tampered_delivery).expect_err("tampered oid");
        assert!(
            err2.to_string().contains("pre-pay seller co-signature invalid"),
            "oid tamper must refuse at the pre-pay tooth, got: {err2}"
        );
        assert_eq!(gate2.spent(), 0, "tampered oid must be zero spend");
        let _ = std::fs::remove_dir_all(&root);
    }

    // --- NIP-42 receipt auth-wait gate --------------------------
    // Smallest testable seam: the decision that gates the receipt
    // `send_event_to` on a confirmed relay AUTH. The full live publish is real relay I/O
    // (proven by the coordinator's live re-run); the auth-ordering / fail-closed decision
    // is pure and is asserted here (red-on-revert: defeating the gate turns the
    // fail-closed cases green→red).
    use crate::relay_auth::{wait_for_nip42_auth, AuthWait, RelayAuthError};
    use nostr_sdk::pool::RelayNotification;
    use std::time::Duration;

    // The buyer receipt gate opens ONLY on `Authenticated`; every other outcome of the shared
    // `wait_for_nip42_auth` (the seller's `NoChallenge` degrade included) fails the buyer closed.
    fn buyer_gate_open(outcome: Result<AuthWait, RelayAuthError>) -> bool {
        matches!(outcome, Ok(AuthWait::Authenticated))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_true_only_on_authenticated() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        tx.send(RelayNotification::Authenticated).expect("send");
        assert!(
            buyer_gate_open(wait_for_nip42_auth(&mut rx, Duration::from_secs(20)).await),
            "Authenticated must gate the send open"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_fails_closed_on_timeout() {
        // Sender kept alive, no Authenticated ever arrives ⇒ the bounded wait elapses ⇒ the
        // send is NOT reached (empty relay_success upstream ⇒ verify holds at `Sent`).
        let (_tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        assert!(
            !buyer_gate_open(wait_for_nip42_auth(&mut rx, Duration::from_millis(50)).await),
            "auth timeout must fail closed (never a forced success)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_fails_closed_on_authentication_failed() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        tx.send(RelayNotification::AuthenticationFailed).expect("send");
        assert!(
            !buyer_gate_open(wait_for_nip42_auth(&mut rx, Duration::from_secs(20)).await),
            "AuthenticationFailed must fail closed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_fails_closed_on_shutdown() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        tx.send(RelayNotification::Shutdown).expect("send");
        assert!(
            !buyer_gate_open(wait_for_nip42_auth(&mut rx, Duration::from_secs(20)).await),
            "relay Shutdown before auth must fail closed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nip42_auth_wait_fails_closed_on_channel_closed() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<RelayNotification>(8);
        drop(tx);
        assert!(
            !buyer_gate_open(wait_for_nip42_auth(&mut rx, Duration::from_secs(20)).await),
            "notification channel closed before auth must fail closed"
        );
    }

    // --- created_at freshness --------------------------------
    // The receipt event's `created_at` must be FRESH wall-clock per publish (so a real relay's
    // ±time-window accepts it), NOT derived from the preimage digest. Red-on-revert: restoring
    // a digest-derived body makes `created` land in 2023..2027 (≈1_747_303_441 for
    // this fixed digest), OUTSIDE [before, after], and this assert FAILS.
    #[test]
    fn receipt_created_at_is_fresh_wall_clock_not_digest_derived() {
        let digest = [0x11u8; 32];
        let before = Timestamp::now().as_secs();
        let created = receipt_created_at(&digest).as_secs();
        let after = Timestamp::now().as_secs();
        assert!(
            (before..=after).contains(&created),
            "receipt created_at {created} is not fresh wall-clock (expected within [{before}, {after}])"
        );
    }

    // A fresh `created_at` must NOT disturb the co-signed receipt CONTENT: the built + signed
    // kind-3400 still carries the job-hash and BOTH schnorr co-signature tags (only `created_at`
    // — and therefore the event id — changed).
    #[test]
    fn receipt_event_binds_cosigned_content_with_fresh_created_at() {
        let buyer = Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_hash = "cc".repeat(32);
        let integrity = "aa".repeat(20); // 40-char oid
        let draft = gateway::receipt_draft(
            "offer-id",
            "result-id",
            &buyer_hex,
            "seller-hex",
            "https://testnut.cashu.space",
            7,
            &job_hash,
            "seller-sig-hex",
            "buyer-sig-hex",
            None,
            Some(gateway::ReceiptDelivery {
                integrity_hash: &integrity,
                kind: "fork",
            }),
            &[],
        );
        let before = Timestamp::now().as_secs();
        let event = gateway::nostr::event_builder(&draft)
            .expect("event builder")
            .custom_created_at(receipt_created_at(&[0x22u8; 32]))
            .sign_with_keys(&buyer)
            .expect("sign");
        let after = Timestamp::now().as_secs();
        assert!(
            (before..=after).contains(&event.created_at.as_secs()),
            "signed receipt created_at is not fresh wall-clock"
        );
        assert_eq!(event.kind.as_u16(), gateway::JOB_RECEIPT_KIND);
        let tag_value = |name: &str, at: usize| -> Option<String> {
            event.tags.iter().find_map(|tag| {
                let slice = tag.as_slice();
                if slice.first().map(String::as_str) == Some(name) {
                    slice.get(at).cloned()
                } else {
                    None
                }
            })
        };
        assert_eq!(tag_value("job-hash", 1).as_deref(), Some(job_hash.as_str()));
        let sig_labels: Vec<String> = event
            .tags
            .iter()
            .filter_map(|tag| {
                let slice = tag.as_slice();
                if slice.first().map(String::as_str) == Some("sig") {
                    slice.get(1).cloned()
                } else {
                    None
                }
            })
            .collect();
        assert!(
            sig_labels.iter().any(|label| label == "seller"),
            "sig/seller tag missing"
        );
        assert!(
            sig_labels.iter().any(|label| label == "buyer"),
            "sig/buyer tag missing"
        );
    }
}
