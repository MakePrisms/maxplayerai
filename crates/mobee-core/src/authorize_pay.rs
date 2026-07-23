//! MCP / CLI pay entry: BudgetGate → [`PaymentService::run`] only.
//!
//! By construction the delivery verifier is [`PayPathDeliveryVerifier`] (allowlist sealed).
//! Stable [`PaymentKey::attempt_id`] feeds `run()`'s reconcile saga — no bespoke pay path,
//! no [`PaymentService::advance`] from this surface.

use std::fmt;
use std::str::FromStr;

use cashu::{Amount, CurrencyUnit, MintUrl, PublicKey as CashuPublicKey};
use nostr_sdk::secp256k1::{Message, Secp256k1};
use nostr_sdk::Keys;
use nostr_sdk::PublicKey as NostrPublicKey;
use nostr_sdk::Timestamp;

use crate::budget::{BudgetGate, BudgetRefuse};
use crate::buyer_fund::{self, FundError};
use crate::delivery::{CommitOid, DeliveryError, GitDelivery};
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::gateway;
use crate::home::{self, MobeeHome};
use crate::payment::{
    DeliveryIntegrityHash, EffectError, FsPaymentJournal, JobHash, JobId, PaymentError, PaymentKey,
    PaymentService, PaymentState, PaymentTerms, ReceiptAuthority, ReceiptEvidence, ResultId,
};
use crate::payment_send::NostrPaymentSend;
use crate::payment_wallet::{CdkPaymentEffects, PaymentWalletError};
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
    pub amount_sats: u64,
    pub spent_total_sats: u64,
    pub remaining_sats: u64,
}

#[derive(Debug)]
pub enum AuthorizePayError {
    Input(String),
    Budget(BudgetRefuse),
    Fund(FundError),
    Delivery(DeliveryError),
    Payment(PaymentError),
    Wallet(PaymentWalletError),
    Home(String),
    Effects(String),
    /// Pre-pay seller co-signature refusal, carrying the buyer's computed preimage fields + digest
    /// (public trade data, no secrets) so the divergent field self-identifies (diagnostic).
    CosigRefused(String),
}

impl fmt::Display for AuthorizePayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "authorize_pay input: {message}"),
            Self::Budget(refuse) => write!(formatter, "{refuse}"),
            Self::Fund(error) => write!(formatter, "{error}"),
            Self::Delivery(error) => write!(formatter, "authorize_pay delivery: {error}"),
            Self::Payment(error) => write!(formatter, "authorize_pay payment: {error}"),
            Self::Wallet(error) => write!(formatter, "authorize_pay wallet: {error}"),
            Self::CosigRefused(message) => write!(formatter, "authorize_pay payment: {message}"),
            Self::Home(message) => write!(formatter, "authorize_pay home: {message}"),
            Self::Effects(message) => write!(formatter, "authorize_pay effects: {message}"),
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
    home: &MobeeHome,
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

    let job_id = JobId::new(request.job_id.clone())
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let result_id = ResultId::new(request.result_id.clone())
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let delivery_integrity_hash = DeliveryIntegrityHash::from_hex(request.delivery_integrity_hash)
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let job_hash = JobHash::from_hex(request.job_hash)
        .map_err(|error| AuthorizePayError::Input(error.to_string()))?;
    let seller_nostr = NostrPublicKey::parse(&request.seller_pubkey)
        .map_err(|error| AuthorizePayError::Input(format!("seller_pubkey: {error}")))?;
    let seller_p2pk = cashu_compressed_from_nostr(&seller_nostr)?;
    // Choose the realized mint the buyer pays at from the seller's `creq` `m` list. The SELECTION is
    // the mint FROZEN into the accept-bind at accept (`request.realized_mint`), not the live config
    // default — so a config-default change between attempts cannot shift the mint and mint a second
    // attempt id (double-pay). `resolve_realized_mint` still enforces accepted-set membership + the
    // real-mint fence over that frozen selection. A legacy bind (no sealed mint) falls back to the
    // live config default (pre-seal behavior).
    let buyer_selected_mint = request
        .realized_mint
        .as_deref()
        .unwrap_or_else(|| home.config.default_mint());
    let mint = resolve_realized_mint(
        buyer_selected_mint,
        &request.accepted_mints,
        home.config.allow_real_mints,
    )?;
    let terms = PaymentTerms::new(
        mint,
        Amount::from(request.amount_sats),
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
        request.creq_hash.clone(),
    );
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

    let wallet = buyer_fund::open_wallet_at_mint_async(home, &wallet_open_mint_url(home, &terms))
        .await?;
    // Dust guard (live keyset N=1 floor, fail-closed). lock_or_reconcile re-checks
    // against CDK input-count send_fee after prepare_send.
    crate::payment_wallet::require_fee_safe_amount(&wallet, terms.amount)
        .await
        .map_err(AuthorizePayError::Wallet)?;
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

    // Payment journal — created only AFTER the pre-pay seam passed (a pre-pay refusal leaves no
    // journal on disk, preserving the zero-spend / no-record invariant).
    let journal_dir = home.root.join("payment-journal");
    std::fs::create_dir_all(&journal_dir)
        .map_err(|error| AuthorizePayError::Home(format!("payment journal dir: {error}")))?;
    let journal = FsPaymentJournal::new(journal_dir.join(format!("{}.jsonl", attempt_id.as_str())));

    let amount = request.amount_sats;
    // Delivery already verified + bind-checked above (pre-budget). The budget append happens here,
    // before the wallet send inside `run_verified`.
    let state = gate.authorize_then_attempt(attempt_id.as_str(), amount, || {
        PaymentService::new(&journal).run_verified(&key, &terms, &authority, &mut effects)
    })??;

    Ok(AuthorizePayOutcome {
        state,
        attempt_id: attempt_id.as_str().to_owned(),
        amount_sats: amount,
        spent_total_sats: gate.spent(),
        remaining_sats: gate.remaining(),
    })
}

/// Resolve the buyer's content policy hook from `[contribution]` config, or the
/// FLOOR (refuse only empty diffs) when unconfigured. Buyer-side; never seller-influenced.
fn contribution_policy(home: &MobeeHome) -> crate::contribution::ContentPolicy {
    match &home.config.contribution {
        Some(cfg) => crate::contribution::ContentPolicy {
            allowed_paths: cfg.allowed_paths.clone(),
            forbidden_paths: cfg.forbidden_paths.clone(),
            max_diff_bytes: cfg.max_diff_bytes,
        },
        None => crate::contribution::ContentPolicy::floor(),
    }
}

/// Buyer mint selection for the pay path, config-driven.
///
/// `buyer_mint_url` is the mint the buyer's wallet spends from — the home config's default mint
/// ([`crate::home::MobeeConfig::default_mint`]), the SAME source `buyer_fund` opens the wallet at.
/// The buyer pays from a mint it holds balance at that the seller listed in its `creq` `m` array;
/// since the buyer wallet is single-mint, that reduces to: is the buyer's configured mint listed?
///
/// - **empty creq list (no creq):** pay from the buyer's configured mint.
/// - **configured mint listed:** pay directly from it (the direct path).
/// - **configured mint NOT listed:** the single-mint buyer wallet holds no balance at any mint the
///   seller listed, so it cannot pay this claim and refuses `mint_unreachable_pay`; no funds move,
///   no binding is committed.
/// The mint URL the pay path opens the buyer wallet at: the FROZEN realized mint sealed into the
/// payment terms (`terms.mint`), NEVER `home.config.default_mint()`. The realized mint is already
/// resolved from the sealed accept-bind ([`resolve_realized_mint`]) and bound into `terms`; opening
/// the wallet at the live config default instead would, after accept seals mint A and the buyer
/// flips its config default to B, bind the wallet to B while the attempt id + send target A — the
/// budget is appended, then the send refuses on mint mismatch and strands the reservation. Taking
/// the mint from the sealed terms keeps the wallet, the attempt id, and the send all on one mint.
/// `home` is passed so the already-fenced invariant is asserted at this seam (the realized mint was
/// fenced by `resolve_realized_mint`; `open_wallet_at_mint_async` re-checks, redundant-safe).
pub(crate) fn wallet_open_mint_url(home: &MobeeHome, terms: &PaymentTerms) -> String {
    let mint_url = terms.mint.to_string();
    debug_assert!(
        crate::home::mint_allowed(&mint_url, home.config.allow_real_mints),
        "frozen realized mint must already be fenced before wallet open"
    );
    mint_url
}

pub(crate) fn resolve_realized_mint(
    buyer_mint_url: &str,
    accepted_mints: &[String],
    allow_real_mints: bool,
) -> Result<MintUrl, AuthorizePayError> {
    // Real-mint fence: the buyer's own paying mint must be admissible under the flag.
    // Default (`allow_real_mints=false`) admits only the testnut/dev allow-list; a real mint is
    // refused fail-closed before any spend unless the operator opts in.
    if !crate::home::mint_allowed(buyer_mint_url, allow_real_mints) {
        return Err(AuthorizePayError::Input(format!(
            "real-mint fence: buyer mint {buyer_mint_url} is not an allow-listed testnut/dev mint; \
             set allow_real_mints=true to pay at a real mint"
        )));
    }
    let buyer_mint = MintUrl::from_str(buyer_mint_url)
        .map_err(|error| AuthorizePayError::Input(format!("buyer mint url: {error}")))?;
    if accepted_mints.is_empty() {
        return Ok(buyer_mint);
    }
    let listed = accepted_mints
        .iter()
        .map(|entry| MintUrl::from_str(entry))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            AuthorizePayError::Input(format!("creq accepted mint url: {error}"))
        })?;
    if listed.contains(&buyer_mint) {
        return Ok(buyer_mint);
    }
    Err(AuthorizePayError::Wallet(PaymentWalletError::Wallet(format!(
        "mint_unreachable_pay: buyer mint {buyer_mint} is not in the creq mint list {listed:?}; \
         the single-mint buyer wallet holds no balance at any accepted mint"
    ))))
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
/// (mobee-relay ≈ ±30 min of server time), so the receipt is rejected and the payment holds at
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
/// mobee-relay requires NIP-42 AUTH for ALL writes, so this path completes + WAITS FOR the
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
                    // `seller_daemon::wait_for_nip42_auth`).
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
                        crate::seller_daemon::wait_for_nip42_auth(
                            &mut relay_notifications,
                            Duration::from_secs(20),
                        )
                        .await,
                        Ok(crate::seller_daemon::AuthWait::Authenticated)
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
    use crate::budget::BudgetGate;
    use crate::home::{self, DEFAULT_MINT_URL};

    // A real (non-testnut) mint — admissible ONLY when `allow_real_mints` is true.
    const REAL_MINT: &str = "https://minibits.example";

    // Empty creq list → pay from the buyer's configured mint (config-driven).
    // Default flag (false): the configured testnut/dev mint resolves.
    #[test]
    fn resolve_realized_mint_empty_creq_uses_configured_mint() {
        let mint = resolve_realized_mint(DEFAULT_MINT_URL, &[], false).unwrap();
        assert_eq!(mint, MintUrl::from_str(DEFAULT_MINT_URL).unwrap());
    }

    // Direct path: the buyer's configured mint is one the seller listed → pay from it directly.
    #[test]
    fn resolve_realized_mint_direct_when_configured_mint_is_listed() {
        let mint = resolve_realized_mint(
            DEFAULT_MINT_URL,
            &[
                "https://other.example".to_string(),
                DEFAULT_MINT_URL.to_string(),
            ],
            false,
        )
        .unwrap();
        assert_eq!(mint, MintUrl::from_str(DEFAULT_MINT_URL).unwrap());
    }

    // Configured mint NOT in the creq list → refuse `mint_unreachable_pay` fail-closed (no spend).
    #[test]
    fn resolve_realized_mint_refuses_when_configured_mint_not_listed() {
        let error = resolve_realized_mint(
            DEFAULT_MINT_URL,
            &["https://other.example".to_string()],
            false,
        )
        .unwrap_err();
        assert!(matches!(error, AuthorizePayError::Wallet(_)));
        assert!(error.to_string().contains("mint_unreachable_pay"));
    }

    // Real-mint switch: a buyer configured at a real mint X is REFUSED by the fence when
    // `allow_real_mints` is false (default safety posture)...
    #[test]
    fn resolve_realized_mint_real_mint_refused_by_default() {
        let error = resolve_realized_mint(REAL_MINT, &[REAL_MINT.to_string()], false).unwrap_err();
        assert!(matches!(error, AuthorizePayError::Input(_)));
        assert!(error.to_string().contains("real-mint fence"));
    }

    // ...and ADMITTED (pays at X when the creq lists X) once the operator opts in with the flag.
    #[test]
    fn resolve_realized_mint_real_mint_admitted_when_flag_true() {
        let paid = resolve_realized_mint(REAL_MINT, &[REAL_MINT.to_string()], true).unwrap();
        assert_eq!(paid, MintUrl::from_str(REAL_MINT).unwrap());

        // Even with the flag on, a mint the creq did NOT list still refuses (membership unchanged).
        let refused =
            resolve_realized_mint(REAL_MINT, &[DEFAULT_MINT_URL.to_string()], true).unwrap_err();
        assert!(refused.to_string().contains("mint_unreachable_pay"));
    }

    // Build a current-thread runtime and block on `authorize_pay_async` — the pattern the MCP
    // dispatch's own runtime provides in production. Lets the sync `#[test]` cases drive the async
    // authorize path directly.
    fn authorize_pay_blocking(
        home: &MobeeHome,
        gate: &mut BudgetGate,
        request: AuthorizePayRequest,
    ) -> Result<AuthorizePayOutcome, AuthorizePayError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime")
            .block_on(authorize_pay_async(home, gate, request))
    }

    #[test]
    fn authorize_pay_refuses_empty_buyer_hash_without_burn() {
        let root = std::env::temp_dir().join(format!(
            "mobee-authorize-pay-d2-empty-{}-{}",
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

    #[test]
    fn authorize_pay_refuses_buyer_hash_mismatch_vs_advertised_commit() {
        let root = std::env::temp_dir().join(format!(
            "mobee-authorize-pay-d2-{}-{}",
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
            "mobee-authorize-pay-jobclass-{}-{}",
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
            "mobee-authorize-pay-ext-{}-{}",
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
            resolve_realized_mint(chosen, &accepted, true).expect("resolve")
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
        home: &MobeeHome,
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

    fn seller_cosig(home: &MobeeHome, preimage: &ReceiptPreimage) -> String {
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
            "mobee-authorize-pay-forged-{}-{}",
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
            "mobee-authorize-pay-diag-{}-{}",
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
            "mobee-authorize-pay-tamper-{}-{}",
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
    use crate::seller_daemon::{wait_for_nip42_auth, AuthWait};
    use nostr_sdk::pool::RelayNotification;
    use std::time::Duration;

    // The buyer receipt gate opens ONLY on `Authenticated`; every other outcome of the shared
    // `wait_for_nip42_auth` (the seller's `NoChallenge` degrade included) fails the buyer closed.
    fn buyer_gate_open(outcome: Result<AuthWait, crate::seller_daemon::DaemonError>) -> bool {
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
