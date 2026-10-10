//! Import one plain-sat Cashu token into this home.
//!
//! Journal-before-effect: the token's input proofs and our fresh blinded outputs with
//! their secrets are journaled BEFORE the swap POST (the token text itself is not).
//! Every later pass first restores (NUT-09) those exact outputs and only ever replays
//! the identical swap; a second output set is never created. Only DLEQ-verified proofs
//! are credited. A definitive refusal is FINAL (see [`ReceiveState::Refused`]).
use crate::{
    journal::Journal,
    mint,
    wallet::{bounded_for, database, wallet},
};
use anyhow::{Context, Result, anyhow, ensure};
use cashu::nuts::nut00::ProofsMethods;
use cashu::{nuts::*, secret::Secret};
use cdk::{
    amount::SplitTarget, cdk_database::WalletDatabase, mint_url::MintUrl, wallet::KeysetFilter,
};
use cdk_common::wallet::ProofInfo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::Path, str::FromStr};

/// Most proofs accepted from one token (and most fresh outputs created for it).
pub const MAX_PROOFS: usize = 128;
/// Largest token text read from a file or stdin.
pub const MAX_TOKEN_BYTES: usize = 1 << 20;
/// Per-item recovery budget; identical to the funding/withdrawal/swap item budget.
pub const RECOVERY_ITEM_SECONDS: u64 = 120;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ReceiveState {
    /// Journaled; the swap has not been POSTed.
    Prepared,
    /// The swap may have reached the mint; reconcile only by restore + identical replay.
    Submitted,
    /// Terminal: DLEQ-verified proofs credited.
    Done,
    /// Terminal and FINAL: the mint returned a definitive NUT refusal
    /// ([`mint::MintRefusal::definitive`]) and a fresh probe found none of our outputs
    /// and every input UNSPENT. Nothing is retried (not by `recover`, not by repeating
    /// `receive`): against cdk 0.17.2 every allowlisted code is deterministic for the
    /// same inputs and outputs, so the identical swap can never succeed (PR #1119,
    /// review round 3). The token was not imported and can be redeemed elsewhere.
    Refused,
    /// Terminal: inputs SPENT elsewhere and none of our outputs restorable.
    AlreadySpent,
    /// Terminal manual recovery: our outputs were signed but failed DLEQ verification.
    Quarantined,
}

#[derive(Clone, Serialize, Deserialize)]
struct Output {
    message: BlindedMessage,
    secret: Secret,
    r: String,
}

/// Private recovery record. Never print it; use [`Receipt::summary`].
#[derive(Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub id: String,
    pub mint: String,
    pub state: ReceiveState,
    /// Gross token value, charged to the cumulative per-mint funding cap.
    pub amount: u64,
    /// Mint input fee deducted by the swap.
    pub fee: u64,
    /// Credited amount on success (amount - fee).
    pub net: u64,
    inputs: Proofs,
    outputs: Vec<Output>,
    result: Option<Proofs>,
    /// Credited rows are inserted RESERVED under this receipt's operation id and only
    /// released after `done` is journaled; true once that release ran.
    #[serde(default)]
    released: bool,
    /// The definitive NUT code that made this record `refused`. `None` on records
    /// written before round 3 (still final).
    #[serde(default)]
    pub refusal_code: Option<u64>,
}

impl Receipt {
    /// `done` and its credited rows released (spendable).
    pub fn settled(&self) -> bool {
        self.state == ReceiveState::Done && self.released
    }
    /// Recovery still owes work: non-terminal, or `done` with the release outstanding.
    pub fn unresolved(&self) -> bool {
        !self.terminal() || (self.state == ReceiveState::Done && !self.released)
    }
    pub fn terminal(&self) -> bool {
        matches!(
            self.state,
            ReceiveState::Done
                | ReceiveState::Refused
                | ReceiveState::AlreadySpent
                | ReceiveState::Quarantined
        )
    }
    /// Counts against the cap unless the mint definitively credited nothing.
    fn charged(&self) -> bool {
        !matches!(
            self.state,
            ReceiveState::Refused | ReceiveState::AlreadySpent
        )
    }
    /// Public accounting view: amounts, mint, state and attempt id only.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({"receive":self.id,"mint":self.mint,"state":self.state,
            "amount":self.amount,"fee":self.fee,"net":self.net,
            "credited":if self.state == ReceiveState::Done {self.net} else {0},
            "settled":self.settled(),"refusal_code":self.refusal_code,
            "terminal":self.terminal(),"manual_recovery":self.state == ReceiveState::Quarantined})
    }
}

/// Statically validated token: no network, nothing journaled.
pub struct Inspected {
    token: Token,
    pub id: String,
    pub amount: u64,
    ys: Vec<PublicKey>,
}

/// Refuse anything but plain sat proofs from exactly `mint` (canonical), <= 128 proofs,
/// no duplicates, within the per-receive cap. Errors never echo token material.
pub fn inspect(mint: &str, raw: &str) -> Result<Inspected> {
    let raw = raw.trim();
    ensure!(
        !raw.is_empty() && raw.len() <= MAX_TOKEN_BYTES,
        "token missing or oversized"
    );
    let token =
        Token::from_str(raw).map_err(|_| anyhow!("invalid Cashu token (contents withheld)"))?;
    if let Token::TokenV3(t) = &token {
        ensure!(t.token.len() == 1, "multi-mint token refused");
    }
    let token_mint = token
        .mint_url()
        .map_err(|_| anyhow!("multi-mint token refused"))?;
    let canonical = url::Url::parse(&token_mint.to_string())
        .ok()
        .and_then(|u| crate::Asset::new(u.as_str().trim_end_matches('/')).ok())
        .context("token mint is not a supported canonical mint")?;
    ensure!(canonical.mint_url == mint, "token is for a different mint");
    ensure!(
        token.unit().is_none_or(|u| u == CurrencyUnit::Sat),
        "only sat tokens are accepted"
    );
    let secrets = token.token_secrets();
    ensure!(!secrets.is_empty(), "empty token");
    ensure!(
        secrets.len() <= MAX_PROOFS,
        "token has more than {MAX_PROOFS} proofs"
    );
    let mut ys = Vec::with_capacity(secrets.len());
    let mut seen = HashSet::new();
    for s in secrets {
        // Any NUT-10 secret (P2PK, HTLC, ...) is locked: only plain proofs are accepted.
        ensure!(
            cashu::nuts::nut10::Secret::try_from(s).is_err(),
            "locked (P2PK/HTLC) proofs refused; only plain proofs are accepted"
        );
        let y = cashu::dhke::hash_to_curve(s.as_bytes())?;
        ensure!(seen.insert(y), "duplicate proof in token");
        ys.push(y);
    }
    let amount = u64::from(token.value().map_err(|_| anyhow!("invalid token amount"))?);
    ensure!(amount > 0, "zero-value token");
    ensure!(
        amount <= crate::real_money::CAP,
        "receive exceeds 100,000-sat cap"
    );
    let mut hex: Vec<_> = ys.iter().map(|y| y.to_hex()).collect();
    hex.sort();
    let id = hex::encode(Sha256::digest(format!(
        "maxplayer-trade/receive/v1|{}",
        hex.join(",")
    )))[..32]
        .to_owned();
    Ok(Inspected {
        token,
        id,
        amount,
        ys,
    })
}

/// Sats this home has taken in from `mint` by receiving (cap accounting).
pub async fn charged(j: &Journal, mint: &str) -> Result<u64> {
    j.all::<Receipt>("receive")
        .await?
        .iter()
        .filter(|r| r.mint == mint && r.charged())
        .try_fold(0u64, |s, r| {
            s.checked_add(r.amount).context("receive overflow")
        })
}

/// The one shared-cap rule: (funding intents, charged receives) on `mint`, gross.
/// `fund` and `receive` both check `funded + received + new <= 100,000`.
pub async fn cap_used(j: &Journal, mint: &str) -> Result<(u64, u64)> {
    let funded = j
        .all::<crate::money::Funding>("funding")
        .await?
        .iter()
        .filter(|f| f.mint == mint)
        .try_fold(0u64, |s, f| {
            s.checked_add(f.amount).context("funding overflow")
        })?;
    Ok((funded, charged(j, mint).await?))
}

/// Shared lifetime cap: funding intents + charged receives + this receive <= 100,000.
async fn check_cap(j: &Journal, mint: &str, amount: u64) -> Result<()> {
    let (funded, received) = cap_used(j, mint).await?;
    ensure!(
        funded
            .checked_add(received)
            .and_then(|n| n.checked_add(amount))
            .is_some_and(|n| n <= crate::real_money::CAP),
        "cumulative funding and receives exceed 100,000 sats for this mint"
    );
    Ok(())
}

/// Receive `raw` into `mint`. Repeat calls for the same Y-set resume the existing
/// attempt (identical swap only; a `refused` record is final and is returned as is).
/// Errors before the journal write are refusals (nothing recorded). After it, the
/// returned record's state is authoritative.
pub async fn receive(home: &Path, j: &Journal, mint: &str, raw: &str) -> Result<Receipt> {
    let asset = crate::Asset::new(mint)?;
    asset.fence()?;
    let mint = asset.mint_url.as_str();
    let p = inspect(mint, raw)?;
    let ys: HashSet<_> = p.ys.iter().copied().collect();
    let mut existing = None;
    for r in j.all::<Receipt>("receive").await? {
        if r.id == p.id {
            ensure!(r.mint == mint, "token already journaled for another mint");
            existing = Some(r);
        } else if r.charged() && r.inputs.ys()?.iter().any(|y| ys.contains(y)) {
            anyhow::bail!("token overlaps earlier receive {}", r.id);
        }
    }
    if let Some(mut r) = existing {
        // `refused` is terminal, so `resume` below only no-ops: a refusal is final.
        if let Err(error) = resume(home, j, &mut r).await {
            eprintln!("receive {}: {error:#}; attempt retained", r.id);
        }
        return Ok(r);
    }
    let db = database(home, mint).await?;
    ensure!(
        db.get_proofs_by_ys(p.ys.clone()).await?.is_empty(),
        "token proofs already belong to this wallet"
    );
    check_cap(j, mint, p.amount).await?;
    crate::wallet::preflight(mint).await?;
    let w = wallet(home, mint).await?;
    bounded_for(mint, w.refresh_keysets())
        .await
        .context("CDK wallet request timed out")??;
    let keysets = bounded_for(mint, w.get_mint_keysets(KeysetFilter::All))
        .await
        .context("CDK wallet request timed out")??;
    let inputs = p
        .token
        .proofs(&keysets)
        .map_err(|_| anyhow!("token keyset is not a sat keyset of this mint"))?;
    ensure!(inputs.len() == p.ys.len(), "token proof count mismatch");
    for (proof, y) in inputs.iter().zip(&p.ys) {
        ensure!(proof.witness.is_none(), "unexpected witness on token proof");
        ensure!(proof.y()? == *y, "token proof identity mismatch");
    }
    let with_dleq: Proofs = inputs
        .iter()
        .filter(|p| p.dleq.is_some())
        .cloned()
        .collect();
    if !with_dleq.is_empty() {
        bounded_for(
            mint,
            w.verify_token_dleq(&Token::new(
                MintUrl::from_str(mint)?,
                with_dleq,
                None,
                CurrencyUnit::Sat,
            )),
        )
        .await
        .context("CDK wallet request timed out")?
        .map_err(|e| match e {
            cdk::Error::CouldNotVerifyDleq => anyhow!("incoming token DLEQ verification failed"),
            e => anyhow!("incoming token DLEQ check could not run: {e}"),
        })?;
    }
    let fee = u64::from(
        bounded_for(mint, w.get_proofs_fee(&inputs))
            .await
            .context("CDK wallet request timed out")??
            .total,
    );
    ensure!(p.amount > fee, "uneconomic token: input fee consumes it");
    let net = p.amount - fee;
    mint::unspent(mint, &inputs)
        .await
        .context("token is not UNSPENT at the mint (already spent or pending)")?;
    let k = bounded_for(mint, w.fetch_active_keyset())
        .await
        .context("CDK wallet request timed out")??;
    let amounts = bounded_for(mint, w.get_keyset_fees_and_amounts_by_id(k.id))
        .await
        .context("CDK wallet request timed out")??;
    let outputs: Vec<Output> =
        PreMintSecrets::random(k.id, net.into(), &SplitTarget::default(), &amounts)?
            .secrets
            .into_iter()
            .map(|o| Output {
                message: o.blinded_message,
                secret: o.secret,
                r: hex::encode(o.r.to_secret_bytes()),
            })
            .collect();
    ensure!(
        !outputs.is_empty() && outputs.len() <= MAX_PROOFS,
        "output limit"
    );
    // A `nostr://` swap must fit one NIP-44 request. Plain secrets are arbitrary strings, so a
    // token under MAX_TOKEN_BYTES can still build an unsendable swap; journaling it would leave a
    // `submitted` attempt that can never be sent and that holds the cap. Refuse it here, before
    // any journal write (exit 1, nothing recorded, cap untouched).
    crate::transport::check_swap_fits(mint, &swap_request(&inputs, &outputs))
        .context("token refused: its receive swap cannot be sent to this nostr mint")?;
    let mut r = Receipt {
        id: p.id,
        mint: mint.into(),
        state: ReceiveState::Prepared,
        amount: p.amount,
        fee,
        net,
        inputs,
        outputs,
        result: None,
        released: false,
        refusal_code: None,
    };
    j.put("receive", &r.id, &r).await?;
    if let Err(error) = resume(home, j, &mut r).await {
        eprintln!("receive {}: {error:#}; attempt retained", r.id);
    }
    Ok(r)
}

/// The one receive swap body (DLEQ stripped by `SwapRequest::new`), for sizing and sending.
fn swap_request(inputs: &Proofs, outputs: &[Output]) -> SwapRequest {
    SwapRequest::new(
        inputs.clone(),
        outputs.iter().map(|o| o.message.clone()).collect(),
    )
}

enum Inputs {
    Unspent,
    Spent,
    Pending,
}
async fn inputs_state(r: &Receipt) -> Result<Inputs> {
    let s = mint::states(&r.mint, &r.inputs).await?;
    Ok(if s.states.iter().all(|s| s.state == State::Unspent) {
        Inputs::Unspent
    } else if s
        .states
        .iter()
        .all(|s| matches!(s.state, State::Unspent | State::Spent))
    {
        Inputs::Spent
    } else {
        Inputs::Pending
    })
}

/// NUT-09 restore of the SAME outputs: None when wholly absent, else all of them.
async fn restore(r: &Receipt) -> Result<Option<Vec<BlindSignature>>> {
    let messages: Vec<_> = r.outputs.iter().map(|o| o.message.clone()).collect();
    let got: RestoreResponse = mint::rpc(
        &r.mint,
        "restore",
        &RestoreRequest {
            outputs: messages.clone(),
        },
    )
    .await?;
    if got.outputs.is_empty() && got.signatures.is_empty() {
        return Ok(None);
    }
    ensure!(
        got.outputs.len() == messages.len() && got.signatures.len() == messages.len(),
        "partial restore; receive attempt retained"
    );
    messages
        .iter()
        .map(|m| {
            let i = got
                .outputs
                .iter()
                .position(|o| o.blinded_secret == m.blinded_secret)
                .context("restore output mismatch")?;
            Ok(got.signatures[i].clone())
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

enum Probe {
    /// Our outputs are signed: use these signatures.
    Signed(Vec<BlindSignature>),
    Unspent,
    Spent,
    Pending,
}

/// The ONE absence check for every path: restore; when absent, NUT-07; when SPENT,
/// restore AGAIN before calling our outputs absent (a swap may land during NUT-07,
/// and CDK writes signatures and SPENT in one transaction).
async fn probe(r: &Receipt) -> Result<Probe> {
    if let Some(s) = restore(r).await? {
        return Ok(Probe::Signed(s));
    }
    Ok(match inputs_state(r).await? {
        Inputs::Unspent => Probe::Unspent,
        Inputs::Pending => Probe::Pending,
        Inputs::Spent => match restore(r).await? {
            Some(s) => Probe::Signed(s),
            None => Probe::Spent,
        },
    })
}

/// Operation id of the reserved credit rows (the 128-bit receipt id).
fn operation(r: &Receipt) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(&r.id).context("receive id is not a 128-bit hex id")
}

/// Make credited rows spendable. Only after `done` is journaled; idempotent.
async fn release(home: &Path, j: &Journal, r: &mut Receipt) -> Result<()> {
    if r.state != ReceiveState::Done || r.released {
        return Ok(());
    }
    database(home, &r.mint)
        .await?
        .release_proofs(&operation(r)?)
        .await?;
    let mut next = r.clone();
    next.released = true;
    j.put("receive", &next.id, &next).await?;
    *r = next;
    Ok(())
}

/// Persist `state` FIRST; only then let the caller-visible record change. A failed
/// journal write returns the error and leaves `r` at its previous (journaled) state.
async fn finish(j: &Journal, r: &mut Receipt, state: ReceiveState) -> Result<()> {
    let mut next = r.clone();
    next.state = state;
    persist(j, &next).await?;
    *r = next;
    Ok(())
}

async fn persist(j: &Journal, r: &Receipt) -> Result<()> {
    #[cfg(feature = "lab")]
    if r.terminal()
        && std::env::var("TRADE_FAIL_RECEIVE_TERMINAL_WRITE")
            .ok()
            .as_deref()
            == Some("1")
    {
        anyhow::bail!("lab fault: receive journal write failed");
    }
    j.put("receive", &r.id, r).await
}

/// Advance one attempt. Terminal outcomes return Ok with the terminal state set.
pub async fn resume(home: &Path, j: &Journal, r: &mut Receipt) -> Result<()> {
    if r.terminal() {
        return release(home, j, r).await;
    }
    crate::Asset::new(&r.mint)?.fence()?;
    if r.result.is_none() {
        let signatures = match probe(r).await? {
            Probe::Signed(s) => s,
            Probe::Pending => anyhow::bail!("token inputs PENDING at the mint; retained"),
            Probe::Spent => return finish(j, r, ReceiveState::AlreadySpent).await,
            Probe::Unspent => {
                if r.state != ReceiveState::Submitted {
                    finish(j, r, ReceiveState::Submitted).await?;
                }
                #[cfg(feature = "lab")]
                if std::env::var("TRADE_CRASH_BEFORE_RECEIVE_SWAP")
                    .ok()
                    .as_deref()
                    == Some("1")
                {
                    std::process::exit(87);
                }
                // No size gate here: admission sized the swap with headroom, and an older attempt
                // that does not fit is refused locally by the connector (413, never published),
                // which stays ambiguous, so it remains `submitted` and is never `refused`.
                let request = swap_request(&r.inputs, &r.outputs);
                let sent: Result<SwapResponse> = mint::rpc(&r.mint, "swap", &request).await;
                match sent {
                    Ok(reply) => reply.signatures,
                    // Definitive only for an allowlisted NUT code AND fresh absence of
                    // our outputs (same probe as above, double restore included).
                    Err(error) if mint::definitive_refusal(&error) => match probe(r).await? {
                        Probe::Signed(s) => s,
                        Probe::Unspent => {
                            let mut next = r.clone();
                            next.state = ReceiveState::Refused;
                            next.refusal_code = mint::MintRefusal::of(&error).map(|e| e.code);
                            persist(j, &next).await?;
                            *r = next;
                            return Ok(());
                        }
                        Probe::Spent => return finish(j, r, ReceiveState::AlreadySpent).await,
                        Probe::Pending => {
                            return Err(error.context("inputs PENDING after refusal; retained"));
                        }
                    },
                    // 50000, TokenPending, unknown codes, non-NUT 400s, 429/5xx, timeouts.
                    Err(error) => {
                        return Err(error.context(
                            "no definitive swap reply; identical swap retained for replay",
                        ));
                    }
                }
            }
        };
        ensure!(
            signatures.len() == r.outputs.len(),
            "signature count mismatch"
        );
        for (s, o) in signatures.iter().zip(&r.outputs) {
            ensure!(
                s.amount == o.message.amount && s.keyset_id == o.message.keyset_id,
                "signature amount/keyset mismatch"
            );
        }
        let w = wallet(home, &r.mint).await?;
        let keys = bounded_for(&r.mint, w.load_keyset_keys(r.outputs[0].message.keyset_id))
            .await
            .context("CDK wallet request timed out")??;
        let proofs = cdk::dhke::construct_proofs(
            signatures,
            r.outputs
                .iter()
                .map(|o| o.r.parse())
                .collect::<std::result::Result<Vec<SecretKey>, _>>()?,
            r.outputs.iter().map(|o| o.secret.clone()).collect(),
            &keys,
        )?;
        let mut next = r.clone();
        next.result = Some(proofs);
        next.state = ReceiveState::Submitted;
        persist(j, &next).await?;
        *r = next;
        #[cfg(feature = "lab")]
        if std::env::var("TRADE_CRASH_AFTER_RECEIVE_SWAP")
            .ok()
            .as_deref()
            == Some("1")
        {
            std::process::exit(86);
        }
    }
    let result = r.result.clone().context("missing receive result")?;
    // Credit only DLEQ-verified proofs. Signed-but-unverifiable outputs need a human.
    if result.iter().any(|p| p.dleq.is_none()) {
        eprintln!("receive {}: mint omitted DLEQ; quarantined", r.id);
        return finish(j, r, ReceiveState::Quarantined).await;
    }
    let w = wallet(home, &r.mint).await?;
    match bounded_for(
        &r.mint,
        w.verify_token_dleq(&Token::new(
            MintUrl::from_str(&r.mint)?,
            result.clone(),
            None,
            CurrencyUnit::Sat,
        )),
    )
    .await
    .context("CDK wallet request timed out")?
    {
        Ok(()) => {}
        Err(cdk::Error::CouldNotVerifyDleq) => {
            eprintln!("receive {}: DLEQ verification failed; quarantined", r.id);
            return finish(j, r, ReceiveState::Quarantined).await;
        }
        Err(e) => return Err(e.into()),
    }
    // Insert only rows not yet present: a crash after the commit must not reset proofs
    // that a later command already reserved or spent.
    let db = database(home, &r.mint).await?;
    let present: HashSet<_> = db
        .get_proofs_by_ys(result.ys()?)
        .await?
        .into_iter()
        .map(|p| p.y)
        .collect();
    let missing: Proofs = result
        .into_iter()
        .filter(|p| p.y().is_ok_and(|y| !present.contains(&y)))
        .collect();
    if !missing.is_empty() {
        mint::unspent(&r.mint, &missing).await?;
        let url = MintUrl::from_str(&r.mint)?;
        let op = operation(r)?;
        // RESERVED under this receipt until `done` is journaled: withdraw/send select
        // only unreserved UNSPENT rows, so nothing can spend them before that.
        db.update_proofs(
            missing
                .into_iter()
                .map(|p| {
                    ProofInfo::new_with_operations(
                        p,
                        url.clone(),
                        State::Reserved,
                        CurrencyUnit::Sat,
                        Some(op),
                        Some(op),
                    )
                })
                .collect::<std::result::Result<Vec<_>, _>>()?,
            vec![],
        )
        .await?;
    }
    #[cfg(feature = "lab")]
    if std::env::var("TRADE_CRASH_BEFORE_RECEIVE_DONE")
        .ok()
        .as_deref()
        == Some("1")
    {
        std::process::exit(85);
    }
    finish(j, r, ReceiveState::Done).await?;
    #[cfg(feature = "lab")]
    if std::env::var("TRADE_CRASH_AFTER_RECEIVE_DONE")
        .ok()
        .as_deref()
        == Some("1")
    {
        std::process::exit(84);
    }
    release(home, j, r).await
}

/// One bounded pass over non-terminal receives. True when any remain unresolved.
pub async fn recover(home: &Path, j: &Journal) -> Result<bool> {
    let mut failed = false;
    for mut r in j.all::<Receipt>("receive").await? {
        if r.terminal() {
            if let Err(error) = release(home, j, &mut r).await {
                failed = true;
                eprintln!("receive {}: release deferred: {error:#}", r.id);
            }
            continue;
        }
        let item = tokio::time::timeout(
            std::time::Duration::from_secs(RECOVERY_ITEM_SECONDS),
            resume(home, j, &mut r),
        )
        .await;
        if !matches!(item, Ok(Ok(()))) || !r.terminal() {
            failed = true;
            eprintln!(
                "receive {}: recovery deferred (error or timeout); attempt retained",
                r.id
            );
        }
    }
    Ok(failed)
}

pub async fn pending(j: &Journal) -> Result<bool> {
    Ok(j.all::<Receipt>("receive")
        .await?
        .iter()
        .any(Receipt::unresolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOSTR_MINT: &str =
        "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
    fn proof(secret: Secret) -> Proof {
        Proof::new(
            1.into(),
            Id::from_str("009a1f293253e41e").unwrap(),
            secret,
            SecretKey::generate().public_key(),
        )
    }
    /// cdk 0.17.2 MintUrl/Token accept a canonical nostr:// mint; whether `receive`
    /// accepts it is decided solely by this crate's `Asset` canonicalization.
    #[test]
    fn cdk_token_parsing_accepts_nostr_mint_url() {
        let url = MintUrl::from_str(NOSTR_MINT).unwrap();
        assert_eq!(url.to_string(), NOSTR_MINT);
        let encoded = Token::new(
            url.clone(),
            vec![proof(Secret::generate())],
            None,
            CurrencyUnit::Sat,
        )
        .to_string();
        let decoded = Token::from_str(&encoded).unwrap();
        assert_eq!(decoded.mint_url().unwrap(), url);
        assert_eq!(
            inspect(NOSTR_MINT, &encoded).is_ok(),
            crate::Asset::new(NOSTR_MINT).is_ok(),
            "receive gating must follow Asset for nostr:// mints"
        );
    }
    #[test]
    fn locked_and_duplicate_proofs_refused_statically() {
        let mint = "http://127.0.0.1:1";
        let token = |proofs| {
            Token::new(
                MintUrl::from_str(mint).unwrap(),
                proofs,
                None,
                CurrencyUnit::Sat,
            )
            .to_string()
        };
        let locked: Secret = cashu::nuts::nut10::Secret::from(SpendingConditions::new_p2pk(
            SecretKey::generate().public_key(),
            None,
        ))
        .try_into()
        .unwrap();
        assert!(
            inspect(mint, &token(vec![proof(locked)])).is_err(),
            "SAFETY: P2PK refused"
        );
        let htlc: Secret = cashu::nuts::nut10::Secret::from(
            SpendingConditions::new_htlc_hash(&"ab".repeat(32), None).unwrap(),
        )
        .try_into()
        .unwrap();
        assert!(
            inspect(mint, &token(vec![proof(htlc)]))
                .is_err_and(|e| e.to_string().contains("locked")),
            "SAFETY: HTLC refused"
        );
        let s = Secret::generate();
        let dup = inspect(mint, &token(vec![proof(s.clone()), proof(s)]));
        assert!(
            dup.as_ref()
                .is_err_and(|e| e.to_string().contains("duplicate proof")),
            "SAFETY: duplicate refused by our own check"
        );
        let many = (0..=MAX_PROOFS)
            .map(|_| proof(Secret::generate()))
            .collect();
        assert!(
            inspect(mint, &token(many)).is_err(),
            "SAFETY: >128 proofs refused"
        );
        let mut big = proof(Secret::generate());
        big.amount = (crate::real_money::CAP + 1).into();
        assert!(
            inspect(mint, &token(vec![big])).is_err(),
            "SAFETY: >100,000 sats refused statically"
        );
        assert!(inspect(mint, &token(vec![proof(Secret::generate())])).is_ok());
        assert!(
            inspect(
                "http://127.0.0.1:2",
                &token(vec![proof(Secret::generate())])
            )
            .is_err(),
            "SAFETY: wrong mint refused"
        );
    }
}
