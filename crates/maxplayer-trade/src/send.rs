//! Export exactly `amount` sats from this home as one single-mint V4 Cashu token file.
//!
//! Journal-before-effect: the selected inputs, every blinded output (send + change)
//! and their secrets are journaled BEFORE the swap POST. Every later pass first
//! restores (NUT-09) those exact outputs and only ever replays the identical swap;
//! replacement outputs are never created. The token is written (0600, never over an
//! existing path) only after the swap is definitive and every output verifies (DLEQ).
//! The token is journaled privately so `recover` rewrites the SAME token after a crash.
//! It is never printed, logged or accepted through argv.
use crate::{
    journal::Journal,
    mint,
    wallet::{bounded, database, wallet},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use cashu::nuts::nut00::ProofsMethods;
use cashu::{nuts::*, secret::Secret};
use cdk::{amount::SplitTarget, cdk_database::WalletDatabase, mint_url::MintUrl};
use cdk_common::wallet::ProofInfo;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    str::FromStr,
};

/// The single send budget: per-item recovery bound, equal to the other money items.
pub const RECOVERY_ITEM_SECONDS: u64 = 120;
const MAX_PROOFS: usize = crate::receive::MAX_PROOFS;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SendState {
    /// Journaled and inputs reserved; the swap has not been POSTed.
    Prepared,
    /// The swap may have reached the mint; reconcile only by restore + identical replay.
    Submitted,
    /// Swap definitive and DLEQ-verified; wallet commit and/or token file pending.
    Swapped,
    /// Token final and journaled; sent proofs left the balance. Reclaimable until
    /// redeemed. The file is normally written, but a reclaim of a `swapped` send that
    /// was refused/partially redeemed also returns here with no file placed; the same
    /// command (same `--out`) writes it.
    Sent,
    /// Terminal: the mint definitively refused the swap; inputs released, unspent.
    Refused,
    /// Terminal manual recovery: our reserved inputs were spent without our outputs.
    InputsSpent,
    /// Terminal manual recovery: outputs signed but missing/invalid DLEQ; no token.
    Quarantined,
    /// A reclaim of the still-UNSPENT sent proofs is journaled and may be in flight.
    Reclaiming,
    /// Terminal: unredeemed sent proofs swapped back into this wallet.
    Reclaimed,
    /// Terminal: the recipient redeemed every sent proof; nothing reclaimed.
    Redeemed,
    /// Terminal manual recovery: reclaim outputs failed DLEQ; nothing credited.
    ReclaimQuarantined,
    /// Terminal manual recovery: every reclaim input is SPENT and our reclaim outputs
    /// were absent on two separate passes (recipient redeemed, or a mint whose restore
    /// lags). The reclaim leg and its secrets stay in the journal.
    ReclaimUnresolved,
}

#[derive(Clone, Serialize, Deserialize)]
struct Output {
    message: BlindedMessage,
    secret: Secret,
    r: String,
    /// True for an output that belongs in the token, false for change/reclaim.
    send: bool,
}

/// One journaled swap: exact inputs, exact outputs, verified result.
#[derive(Clone, Serialize, Deserialize)]
struct Leg {
    inputs: Proofs,
    outputs: Vec<Output>,
    result: Option<Proofs>,
}

/// Private recovery record. Never print it; use [`SendAttempt::summary`].
#[derive(Clone, Serialize, Deserialize)]
pub struct SendAttempt {
    pub id: String,
    pub mint: String,
    pub state: SendState,
    /// Exact token value.
    pub amount: u64,
    /// Sender-paid mint input fee of the send swap.
    pub fee: u64,
    out: String,
    swap: Leg,
    token: Option<String>,
    #[serde(default)]
    committed: bool,
    #[serde(default)]
    reclaim: Option<Leg>,
    #[serde(default)]
    pub reclaim_fee: u64,
    #[serde(default)]
    pub reclaimed: u64,
    /// Earlier reclaim legs that provably never executed (archived, never deleted:
    /// their blinded outputs and secrets stay available for manual restore).
    #[serde(default)]
    reclaim_history: Vec<Leg>,
    /// Passes on which the current reclaim's inputs were SPENT with our outputs absent.
    #[serde(default)]
    reclaim_absent: u32,
    /// `Some(false)`: the current reclaim leg was never POSTed (journaled before every
    /// POST). `None` (no leg, or a record from before round 3) counts as possibly POSTed.
    #[serde(default)]
    reclaim_submitted: Option<bool>,
    /// Not journaled: this command only rewrote an earlier send's journaled token.
    #[serde(skip)]
    rewritten: bool,
    /// Rows reserved under this attempt's operation id were released after the
    /// current terminal state was journaled.
    #[serde(default)]
    released: bool,
}

impl SendAttempt {
    pub fn terminal(&self) -> bool {
        !matches!(
            self.state,
            SendState::Prepared | SendState::Submitted | SendState::Swapped | SendState::Reclaiming
        )
    }
    pub fn manual_recovery(&self) -> bool {
        matches!(
            self.state,
            SendState::InputsSpent
                | SendState::Quarantined
                | SendState::ReclaimQuarantined
                | SendState::ReclaimUnresolved
        )
    }
    /// Rows reserved under this attempt (held inputs, or credited change/reclaim
    /// proofs) may be released: after a journaled refusal, or once the wallet commit
    /// is journaled and no reclaim credit is in flight.
    fn releasable(&self) -> bool {
        self.state == SendState::Refused
            || (self.committed
                && !matches!(
                    self.state,
                    SendState::Reclaiming | SendState::Prepared | SendState::Submitted
                ))
    }
    /// Public accounting view: attempt id, mint, state and amounts only.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({"send":self.id,"mint":self.mint,"state":self.state,
            "amount":self.amount,"fee":self.fee,"reclaimed":self.reclaimed,
            "reclaim_fee":self.reclaim_fee,"terminal":self.terminal(),
            "manual_recovery":self.manual_recovery(),"rewritten":self.rewritten})
    }
    fn sent_proofs(&self) -> Result<Proofs> {
        let result = self.swap.result.as_ref().context("send has no result")?;
        Ok(result
            .iter()
            .zip(&self.swap.outputs)
            .filter(|(_, o)| o.send)
            .map(|(p, _)| p.clone())
            .collect())
    }
    fn op(&self) -> Result<uuid::Uuid> {
        Ok(uuid::Uuid::parse_str(&self.id)?)
    }
}

/// `status` view of one journal row; no network.
pub fn public(bytes: &[u8]) -> Result<serde_json::Value> {
    let mut v = serde_json::from_slice::<SendAttempt>(bytes)?.summary();
    v["kind"] = "send".into();
    Ok(v)
}

fn lab_crash(_var: &str, _code: i32) {
    #[cfg(feature = "lab")]
    if std::env::var(_var).ok().as_deref() == Some("1") {
        std::process::exit(_code);
    }
}

fn outputs(p: PreMintSecrets, send: bool) -> Vec<Output> {
    p.secrets
        .into_iter()
        .map(|o| Output {
            message: o.blinded_message,
            secret: o.secret,
            r: hex::encode(o.r.to_secret_bytes()),
            send,
        })
        .collect()
}

/// Absolute target path: canonical existing parent + file name. Never created here.
fn target(out: &Path) -> Result<PathBuf> {
    let name = out.file_name().context("--out must name a file")?;
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let path = fs::canonicalize(parent)
        .context("--out directory does not exist")?
        .join(name);
    ensure!(path.to_str().is_some(), "--out path must be UTF-8");
    Ok(path)
}

/// Some(true) when `path` holds exactly `token`; Some(false) when it holds anything
/// else (including a dangling symlink); None when absent.
fn holds(path: &Path, token: &str) -> Result<Option<bool>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(m) if !m.is_file() => Ok(Some(false)),
        Ok(_) => {
            let mut text = String::new();
            let read = fs::File::open(path)
                .and_then(|f| f.take(token.len() as u64 + 2).read_to_string(&mut text));
            Ok(Some(read.is_ok() && text.trim() == token))
        }
    }
}

/// Exclusive 0600 creation: a private O_EXCL temporary, fsynced, then hard-linked to
/// `out` (link fails if `out` exists, so nothing is ever overwritten and a crash never
/// leaves a partial token at `out`).
fn write_token(out: &Path, id: &str, token: &str) -> Result<()> {
    let name = out.file_name().context("--out must name a file")?;
    let tmp = out.with_file_name(format!(
        ".{}.{id}.partial",
        name.to_str().context("--out path must be UTF-8")?
    ));
    match holds(out, token)? {
        Some(true) => {
            // A crash after the link may have left the private temporary copy.
            let _ = fs::remove_file(&tmp);
            return Ok(());
        }
        Some(false) => bail!("--out path exists with other content; token retained in journal"),
        None => {}
    }
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .context("cannot create token file")?;
    let written = f
        .write_all(token.as_bytes())
        .and_then(|()| f.write_all(b"\n"))
        .and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = written {
        // Never leave a full bearer-token copy behind (e.g. disk full).
        let _ = fs::remove_file(&tmp);
        return Err(anyhow!(e).context("cannot write token file; token retained in journal"));
    }
    let linked = fs::hard_link(&tmp, out);
    let _ = fs::remove_file(&tmp);
    if let Err(e) = linked {
        if holds(out, token)? == Some(true) {
            return Ok(());
        }
        return Err(anyhow!(e).context("cannot place token file; token retained in journal"));
    }
    if let Some(dir) = out.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Persist `state` FIRST; only then let the caller-visible record change. A failed
/// journal write returns the error and leaves `r` at its previous (journaled) state.
async fn finish(j: &Journal, r: &mut SendAttempt, state: SendState) -> Result<()> {
    let mut next = r.clone();
    next.state = state;
    persist(j, &next).await?;
    *r = next;
    Ok(())
}

/// Apply `f` to a copy, persist it, then publish it to `r` (persist-before-report).
async fn update(
    j: &Journal,
    r: &mut SendAttempt,
    f: impl FnOnce(&mut SendAttempt) -> Result<()>,
) -> Result<()> {
    let mut next = r.clone();
    f(&mut next)?;
    persist(j, &next).await?;
    *r = next;
    Ok(())
}

async fn persist(j: &Journal, r: &SendAttempt) -> Result<()> {
    #[cfg(feature = "lab")]
    if r.terminal()
        && std::env::var("TRADE_FAIL_SEND_TERMINAL_WRITE")
            .ok()
            .as_deref()
            == Some("1")
    {
        bail!("lab fault: send journal write failed");
    }
    j.put("send", &r.id, r).await
}

/// Max-fee rule shared by send and reclaim: required unless the fee is 0.
fn check_fee(fee: u64, max_fees: Option<u64>) -> Result<()> {
    if fee > 0 {
        let cap = max_fees.context("--max-fees is required: this mint charges input fees")?;
        ensure!(fee <= cap, "mint input fee {fee} exceeds --max-fees {cap}");
    }
    Ok(())
}

/// SendAttempt `amount` sats from `mint` to the token file `out`. Re-running with the same
/// `--out` resumes a non-terminal attempt and never starts a second send. Errors before
/// the journal write are refusals; afterwards the returned state is authoritative.
pub async fn send(
    home: &Path,
    j: &Journal,
    mint: &str,
    amount: u64,
    out: &Path,
    max_fees: Option<u64>,
) -> Result<SendAttempt> {
    ensure!(
        amount > 0 && amount <= crate::real_money::CAP,
        "send exceeds 100,000-sat cap"
    );
    let asset = crate::Asset::new(mint)?;
    asset.fence()?;
    let mint = asset.mint_url.as_str();
    let out = target(out)?;
    let out_str = out.to_str().context("--out path must be UTF-8")?;
    let attempts = j.all::<SendAttempt>("send").await?;
    // Idempotency is keyed on the journaled --out, whatever the file now holds: a
    // delivered (moved/deleted) token file never authorizes a second send.
    for mut r in attempts.iter().filter(|r| r.out == out_str).cloned() {
        if r.state == SendState::Refused {
            continue; // nothing was sent; the path is free
        }
        ensure!(
            r.mint == mint && r.amount == amount,
            "--out already belongs to send {} with other terms; choose another --out",
            r.id
        );
        ensure!(
            r.state != SendState::Reclaiming,
            "--out belongs to send {} which is being reclaimed; run send --reclaim {}",
            r.id,
            r.id
        );
        if !r.terminal() {
            if let Err(error) = resume(home, j, &mut r).await {
                eprintln!("send {}: {error:#}; attempt retained", r.id);
            }
            return Ok(r);
        }
        ensure!(
            r.state == SendState::Sent,
            "--out already belongs to send {} ({:?}); choose another --out",
            r.id,
            r.state
        );
        // Same command again: (re)write the SAME journaled token, never a new send, and
        // only while no sent proof is redeemed (a reused --out must never hand out a
        // spent token). PENDING or an unreachable mint refuses too (fail closed).
        let token = r.token.clone().context("missing journaled token")?;
        if holds(&out, &token)? == Some(true) {
            // Already in place: an idempotent no-op, nothing is (re)emitted.
            write_token(&out, &r.id, &token)?;
            return Ok(r);
        }
        let states = mint::states(&r.mint, &r.sent_proofs()?).await?;
        ensure!(
            states.states.iter().all(|s| s.state == State::Unspent),
            "--out belongs to send {} whose token is already (partially) redeemed or pending; choose another --out",
            r.id
        );
        write_token(&out, &r.id, &token)?;
        eprintln!(
            "send {}: --out belongs to send {} (already sent); rewrote the same token; no new value was sent",
            r.id, r.id
        );
        r.rewritten = true;
        return Ok(r);
    }
    ensure!(
        holds(&out, "")?.is_none(),
        "--out path already exists; refusing to overwrite"
    );
    let db = database(home, mint).await?;
    let mut available = db
        .get_proofs(
            Some(mint.parse()?),
            Some(CurrencyUnit::Sat),
            Some(vec![State::Unspent]),
            None,
        )
        .await?;
    // Ordinary balance only: no reservations (swap/withdraw/receive/send) and no locks.
    available.retain(|p| p.used_by_operation.is_none() && p.spending_condition.is_none());
    // Also exclude inputs named by any unfinished journaled intent whose reservation
    // may not be applied yet (crash between journal write and reserve).
    let intended = journaled_inputs(j, mint).await?;
    available.retain(|p| !intended.contains(&p.y));
    let spendable = available
        .iter()
        .try_fold(0u64, |s, p| s.checked_add(u64::from(p.proof.amount)))
        .context("balance overflow")?;
    ensure!(spendable >= amount, "insufficient unreserved balance");
    crate::wallet::preflight(mint).await?;
    let w = wallet(home, mint).await?;
    bounded(w.refresh_keysets())
        .await
        .context("CDK wallet request timed out")??;
    let k = bounded(w.fetch_active_keyset())
        .await
        .context("CDK wallet request timed out")??;
    available.sort_by_key(|p| p.proof.amount);
    let mut inputs = vec![];
    let mut chosen = None;
    for p in available {
        inputs.push(p.proof);
        let total = u64::from(inputs.total_amount()?);
        let fee = u64::from(
            bounded(w.get_proofs_fee(&inputs))
                .await
                .context("CDK wallet request timed out")??
                .total,
        );
        if total >= amount.checked_add(fee).context("fee overflow")? {
            chosen = Some((total, fee));
            break;
        }
    }
    let (total, fee) = chosen.context("insufficient unreserved balance for amount plus fee")?;
    check_fee(fee, max_fees)?;
    ensure!(inputs.len() <= MAX_PROOFS, "too many send inputs");
    mint::unspent(mint, &inputs)
        .await
        .context("selected proofs are not UNSPENT at the mint")?;
    let amounts = bounded(w.get_keyset_fees_and_amounts_by_id(k.id))
        .await
        .context("CDK wallet request timed out")??;
    let mut planned = outputs(
        PreMintSecrets::random(k.id, amount.into(), &SplitTarget::default(), &amounts)?,
        true,
    );
    // The recipient pays the input fee on these proofs when redeeming.
    let redeem_fee = (planned.len() as u64)
        .checked_mul(k.input_fee_ppk)
        .context("fee overflow")?
        .div_ceil(1000);
    ensure!(
        amount > redeem_fee,
        "uneconomic send: the recipient's input fee ({redeem_fee} sats) would consume it"
    );
    let change = total - amount - fee;
    if change > 0 {
        planned.extend(outputs(
            PreMintSecrets::random(k.id, change.into(), &SplitTarget::default(), &amounts)?,
            false,
        ));
    }
    ensure!(planned.len() <= MAX_PROOFS, "output limit");
    let mut r = SendAttempt {
        id: uuid::Uuid::new_v4().to_string(),
        mint: mint.into(),
        state: SendState::Prepared,
        amount,
        fee,
        out: out_str.into(),
        swap: Leg {
            inputs,
            outputs: planned,
            result: None,
        },
        token: None,
        committed: false,
        reclaim: None,
        reclaim_fee: 0,
        reclaimed: 0,
        reclaim_history: vec![],
        reclaim_absent: 0,
        reclaim_submitted: None,
        rewritten: false,
        released: false,
    };
    j.put("send", &r.id, &r).await?;
    lab_crash("TRADE_CRASH_AFTER_SEND_JOURNAL", 84);
    if let Err(error) = reserve(home, &r).await {
        // Nothing was POSTed: record the refusal, then release anything held.
        finish(j, &mut r, SendState::Refused).await?;
        release(home, j, &mut r).await?;
        return Err(error.context("send inputs could not be reserved; refused"));
    }
    if let Err(error) = resume(home, j, &mut r).await {
        eprintln!("send {}: {error:#}; attempt retained", r.id);
    }
    Ok(r)
}

/// Ys named by unfinished journaled intents on `mint`: sends, withdrawals, listings
/// (not cancelled) and coordinator swaps. Coin selection never takes them.
async fn journaled_inputs(j: &Journal, mint: &str) -> Result<HashSet<PublicKey>> {
    let mut ys = HashSet::new();
    for r in j.all::<SendAttempt>("send").await? {
        if r.mint == mint && !r.terminal() {
            ys.extend(r.swap.inputs.ys()?);
        }
    }
    for a in j.all::<crate::money::Withdrawal>("withdrawal").await? {
        if a.mint == mint && !a.terminal() {
            ys.extend(a.inputs().ys()?);
        }
    }
    for l in j.all::<crate::coordinator::Listing>("listing").await? {
        if l.plan.mint == mint && !l.cancelled {
            ys.extend(l.plan.inputs.ys()?);
        }
    }
    for s in j.all::<crate::coordinator::Swap>("swap").await? {
        if s.plan.mint == mint && !crate::coordinator::swap_terminal(&s) {
            ys.extend(s.plan.inputs.ys()?);
        }
    }
    Ok(ys)
}

/// Release rows reserved under this attempt once [`SendAttempt::releasable`];
/// idempotent, recorded once per terminal state.
async fn release(home: &Path, j: &Journal, r: &mut SendAttempt) -> Result<()> {
    if !r.releasable() || r.released {
        return Ok(());
    }
    database(home, &r.mint)
        .await?
        .release_proofs(&r.op()?)
        .await?;
    if r.terminal() {
        update(j, r, |n| {
            n.released = true;
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// Hold the journaled inputs under this attempt's operation id (idempotent).
async fn reserve(home: &Path, r: &SendAttempt) -> Result<()> {
    let db = database(home, &r.mint).await?;
    let op = r.op()?;
    let held: HashSet<_> = db
        .get_reserved_proofs(&op)
        .await?
        .into_iter()
        .map(|p| p.y)
        .collect();
    let intended: HashSet<_> = r.swap.inputs.ys()?.into_iter().collect();
    ensure!(held.is_subset(&intended), "send reservation conflict");
    let missing: Vec<_> = intended.difference(&held).copied().collect();
    if !missing.is_empty() {
        db.reserve_proofs(missing, &op).await?;
    }
    Ok(())
}

/// NUT-09 restore of the SAME outputs: None when wholly absent, else all of them.
async fn restore(mint: &str, leg: &Leg) -> Result<Option<Vec<BlindSignature>>> {
    let messages: Vec<_> = leg.outputs.iter().map(|o| o.message.clone()).collect();
    let got: RestoreResponse = mint::rpc(
        mint,
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
        "partial restore; send attempt retained"
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
    /// Our outputs are signed (restored or returned).
    Signed(Vec<BlindSignature>),
    /// Inputs UNSPENT and our outputs absent: the identical swap may be (re)POSTed.
    Unspent,
    /// Inputs SPENT and our outputs absent on a fresh restore.
    Spent,
    /// Definitive NUT error with fresh absence and UNSPENT inputs.
    Refused,
}

async fn inputs_probe(mint: &str, leg: &Leg) -> Result<Probe> {
    let s = mint::states(mint, &leg.inputs).await?;
    if s.states.iter().all(|s| s.state == State::Unspent) {
        return Ok(Probe::Unspent);
    }
    ensure!(
        s.states
            .iter()
            .all(|s| matches!(s.state, State::Unspent | State::Spent)),
        "inputs PENDING at the mint; attempt retained"
    );
    // A swap may land during NUT-07; never infer absence from one restore.
    Ok(match restore(mint, leg).await? {
        Some(sigs) => Probe::Signed(sigs),
        None => Probe::Spent,
    })
}

/// Restore first; only report Unspent when the identical swap may be POSTed.
async fn probe(mint: &str, leg: &Leg) -> Result<Probe> {
    match restore(mint, leg).await? {
        Some(sigs) => Ok(Probe::Signed(sigs)),
        None => inputs_probe(mint, leg).await,
    }
}

/// POST the identical journaled swap. Only a definitive NUT error
/// ([`mint::definitive_refusal`]: allowlisted code) can be a refusal; `50000`,
/// TokenPending, unknown codes, any other 400, timeouts and 5xx are ambiguous.
async fn post(mint: &str, leg: &Leg) -> Result<Probe> {
    let sent: Result<SwapResponse> = mint::rpc(
        mint,
        "swap",
        &SwapRequest::new(
            leg.inputs.clone(),
            leg.outputs.iter().map(|o| o.message.clone()).collect(),
        ),
    )
    .await;
    match sent {
        Ok(reply) => Ok(Probe::Signed(reply.signatures)),
        Err(error) if mint::definitive_refusal(&error) => {
            if let Some(sigs) = restore(mint, leg).await? {
                return Ok(Probe::Signed(sigs));
            }
            Ok(match inputs_probe(mint, leg).await? {
                Probe::Unspent => Probe::Refused,
                other => other,
            })
        }
        Err(error) => {
            Err(error.context("no definitive swap reply; identical swap retained for replay"))
        }
    }
}

/// Unblind and verify every output. None means missing/invalid DLEQ (quarantine).
async fn verified(
    home: &Path,
    mint: &str,
    leg: &Leg,
    signatures: Vec<BlindSignature>,
) -> Result<Option<Proofs>> {
    ensure!(
        signatures.len() == leg.outputs.len() && !leg.outputs.is_empty(),
        "signature count mismatch"
    );
    for (s, o) in signatures.iter().zip(&leg.outputs) {
        ensure!(
            s.amount == o.message.amount && s.keyset_id == o.message.keyset_id,
            "signature amount/keyset mismatch"
        );
    }
    let w = wallet(home, mint).await?;
    let keys = bounded(w.load_keyset_keys(leg.outputs[0].message.keyset_id))
        .await
        .context("CDK wallet request timed out")??;
    let proofs = cdk::dhke::construct_proofs(
        signatures,
        leg.outputs
            .iter()
            .map(|o| o.r.parse())
            .collect::<std::result::Result<Vec<SecretKey>, _>>()?,
        leg.outputs.iter().map(|o| o.secret.clone()).collect(),
        &keys,
    )?;
    if proofs.iter().any(|p| p.dleq.is_none()) {
        return Ok(None);
    }
    match bounded(w.verify_token_dleq(&Token::new(
        MintUrl::from_str(mint)?,
        proofs.clone(),
        None,
        CurrencyUnit::Sat,
    )))
    .await
    .context("CDK wallet request timed out")?
    {
        Ok(()) => Ok(Some(proofs)),
        Err(cdk::Error::CouldNotVerifyDleq) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Insert only rows not yet present, RESERVED under `op` (released only after the
/// commit/terminal state is journaled, so nothing can spend them earlier); remove `spent`.
async fn credit(
    home: &Path,
    mint: &str,
    proofs: Proofs,
    spent: Vec<PublicKey>,
    op: uuid::Uuid,
) -> Result<()> {
    let db = database(home, mint).await?;
    if proofs.is_empty() && spent.is_empty() {
        return Ok(());
    }
    let present: HashSet<_> = if proofs.is_empty() {
        HashSet::new()
    } else {
        db.get_proofs_by_ys(proofs.ys()?)
            .await?
            .into_iter()
            .map(|p| p.y)
            .collect()
    };
    let missing: Proofs = proofs
        .into_iter()
        .filter(|p| p.y().is_ok_and(|y| !present.contains(&y)))
        .collect();
    if !missing.is_empty() {
        mint::unspent(mint, &missing).await?;
    }
    let url = MintUrl::from_str(mint)?;
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
        spent,
    )
    .await?;
    Ok(())
}

/// Advance one attempt. Terminal outcomes return Ok with the terminal state set.
pub async fn resume(home: &Path, j: &Journal, r: &mut SendAttempt) -> Result<()> {
    if r.terminal() {
        return release(home, j, r).await;
    }
    crate::Asset::new(&r.mint)?.fence()?;
    if r.state == SendState::Reclaiming {
        resume_reclaim(home, j, r).await?;
        return release(home, j, r).await;
    }
    if r.swap.result.is_none() {
        if let Err(error) = reserve(home, r).await {
            // `prepared` proves nothing was POSTed (`submitted` is journaled first).
            // Inputs taken by another operation/spent locally can never be reserved.
            if r.state == SendState::Prepared
                && error.chain().any(|c| {
                    matches!(
                        c.downcast_ref::<cdk_common::database::Error>(),
                        Some(cdk_common::database::Error::ProofNotUnspent)
                    )
                })
            {
                finish(j, r, SendState::Refused).await?;
                release(home, j, r).await?;
                return Err(error.context("send inputs no longer available; refused, nothing sent"));
            }
            return Err(error);
        }
        let mut outcome = probe(&r.mint, &r.swap).await?;
        if matches!(outcome, Probe::Unspent) {
            if r.state != SendState::Submitted {
                finish(j, r, SendState::Submitted).await?;
            }
            lab_crash("TRADE_CRASH_BEFORE_SEND_SWAP", 87);
            outcome = post(&r.mint, &r.swap).await?;
        }
        let signatures = match outcome {
            Probe::Signed(s) => s,
            Probe::Refused => {
                // Journal the terminal refusal BEFORE releasing: a released input must
                // never be replayed by a later pass.
                finish(j, r, SendState::Refused).await?;
                return release(home, j, r).await;
            }
            Probe::Spent => {
                eprintln!("send {}: inputs spent without our outputs", r.id);
                return finish(j, r, SendState::InputsSpent).await;
            }
            Probe::Unspent => bail!("swap neither signed nor refused; retained"),
        };
        let Some(proofs) = verified(home, &r.mint, &r.swap, signatures).await? else {
            eprintln!("send {}: missing or invalid DLEQ; quarantined", r.id);
            return finish(j, r, SendState::Quarantined).await;
        };
        update(j, r, |n| {
            n.swap.result = Some(proofs);
            let sent = n.sent_proofs()?;
            ensure!(
                u64::from(sent.total_amount()?) == n.amount,
                "sent proofs do not total the send amount"
            );
            n.token = Some(
                Token::new(MintUrl::from_str(&n.mint)?, sent, None, CurrencyUnit::Sat).to_string(),
            );
            n.state = SendState::Swapped;
            Ok(())
        })
        .await?;
        lab_crash("TRADE_CRASH_AFTER_SEND_SWAP", 86);
    }
    if !r.committed {
        let change = r
            .swap
            .result
            .iter()
            .flatten()
            .zip(&r.swap.outputs)
            .filter(|(_, o)| !o.send)
            .map(|(p, _)| p.clone())
            .collect();
        credit(home, &r.mint, change, r.swap.inputs.ys()?, r.op()?).await?;
        lab_crash("TRADE_CRASH_BEFORE_SEND_COMMIT", 83);
        update(j, r, |n| {
            n.committed = true;
            n.state = SendState::Swapped;
            Ok(())
        })
        .await?;
        release(home, j, r).await?;
    }
    lab_crash("TRADE_CRASH_BEFORE_SEND_FILE", 85);
    let token = r.token.clone().context("missing journaled token")?;
    write_token(Path::new(&r.out), &r.id, &token)?;
    finish(j, r, SendState::Sent).await?;
    release(home, j, r).await
}

/// Swap the still-UNSPENT proofs of a sent token back into this wallet. If the
/// recipient already redeemed everything, the attempt becomes `redeemed`.
pub async fn reclaim(
    home: &Path,
    j: &Journal,
    id: &str,
    max_fees: Option<u64>,
) -> Result<SendAttempt> {
    let mut r: SendAttempt = j.get("send", id).await?.context("unknown send attempt")?;
    crate::Asset::new(&r.mint)?.fence()?;
    match r.state {
        SendState::Reclaiming => {
            if let Err(error) = resume(home, j, &mut r).await {
                eprintln!("send {}: {error:#}; reclaim retained", r.id);
            }
            return Ok(r);
        }
        SendState::Reclaimed | SendState::Redeemed => return Ok(r),
        SendState::Sent => {}
        // The token file could not be placed (unwritable/vanished directory, no hard
        // links, path taken): the journaled token is reclaimable like a sent one.
        SendState::Swapped if r.committed => {}
        state => bail!("send {id} is {state:?}; only a sent token can be reclaimed"),
    }
    crate::wallet::preflight(&r.mint).await?;
    let sent = r.sent_proofs()?;
    let states = mint::states(&r.mint, &sent).await?;
    ensure!(
        states.states.iter().all(|s| s.state != State::Pending),
        "sent proofs are PENDING at the mint; retry later"
    );
    let unspent: HashSet<_> = states
        .states
        .iter()
        .filter(|s| s.state == State::Unspent)
        .map(|s| s.y)
        .collect();
    let inputs: Proofs = sent
        .into_iter()
        .filter(|p| p.y().is_ok_and(|y| unspent.contains(&y)))
        .collect();
    if inputs.is_empty() {
        finish(j, &mut r, SendState::Redeemed).await?;
        return Ok(r);
    }
    let w = wallet(home, &r.mint).await?;
    bounded(w.refresh_keysets())
        .await
        .context("CDK wallet request timed out")??;
    let fee = u64::from(
        bounded(w.get_proofs_fee(&inputs))
            .await
            .context("CDK wallet request timed out")??
            .total,
    );
    check_fee(fee, max_fees)?;
    let total = u64::from(inputs.total_amount()?);
    ensure!(total > fee, "uneconomic reclaim: input fee consumes it");
    let k = bounded(w.fetch_active_keyset())
        .await
        .context("CDK wallet request timed out")??;
    let amounts = bounded(w.get_keyset_fees_and_amounts_by_id(k.id))
        .await
        .context("CDK wallet request timed out")??;
    let leg = Leg {
        inputs,
        outputs: outputs(
            PreMintSecrets::random(
                k.id,
                (total - fee).into(),
                &SplitTarget::default(),
                &amounts,
            )?,
            false,
        ),
        result: None,
    };
    update(j, &mut r, |n| {
        n.reclaim = Some(leg);
        n.reclaim_fee = fee;
        n.reclaim_absent = 0;
        n.reclaim_submitted = Some(false);
        n.released = false;
        n.state = SendState::Reclaiming;
        Ok(())
    })
    .await?;
    if let Err(error) = resume(home, j, &mut r).await {
        eprintln!("send {}: {error:#}; reclaim retained", r.id);
    }
    Ok(r)
}

async fn resume_reclaim(home: &Path, j: &Journal, r: &mut SendAttempt) -> Result<()> {
    let mut leg = r.reclaim.clone().context("missing reclaim record")?;
    if leg.result.is_none() {
        // Was any POST of this leg possibly executed before this pass?
        let earlier_post = r.reclaim_submitted != Some(false);
        let mut outcome = probe(&r.mint, &leg).await?;
        if matches!(outcome, Probe::Unspent) {
            lab_crash("TRADE_CRASH_BEFORE_RECLAIM_SWAP", 87);
            if !earlier_post {
                update(j, r, |n| {
                    n.reclaim_submitted = Some(true);
                    Ok(())
                })
                .await?;
            }
            // `post` returns Spent only after a DEFINITIVE refusal (this POST did not
            // execute) with every input SPENT and our outputs absent twice.
            outcome = post(&r.mint, &leg).await?;
        }
        let signatures = match outcome {
            Probe::Signed(s) => s,
            // Inputs SPENT and our outputs absent on a double restore. A swap spends all
            // its inputs at once, so any still-UNSPENT input proves ours never ran.
            Probe::Spent => {
                let st = mint::states(&r.mint, &leg.inputs).await?;
                if st.states.iter().any(|s| s.state == State::Unspent) {
                    update(j, r, |n| {
                        archive_reclaim(n);
                        n.state = SendState::Sent;
                        Ok(())
                    })
                    .await?;
                    release_quietly(home, j, r).await;
                    bail!(
                        "token partially redeemed during reclaim; still outstanding; run send --reclaim {} again",
                        r.id
                    );
                }
                if !earlier_post {
                    // No POST of this leg ever executed (never sent, or definitively
                    // refused): the recipient redeemed first. Secrets stay archived.
                    update(j, r, |n| {
                        archive_reclaim(n);
                        n.state = SendState::Redeemed;
                        Ok(())
                    })
                    .await?;
                    return Ok(());
                }
                if r.reclaim_absent == 0 {
                    // Never conclude from one pass: a lagging restore may still show them.
                    update(j, r, |n| {
                        n.reclaim_absent = 1;
                        Ok(())
                    })
                    .await?;
                    bail!(
                        "reclaim inputs SPENT and our outputs not restorable yet; reclaim retained"
                    );
                }
                eprintln!(
                    "send {}: reclaim inputs spent and outputs still absent; manual recovery, secrets retained",
                    r.id
                );
                return finish(j, r, SendState::ReclaimUnresolved).await;
            }
            Probe::Refused => {
                update(j, r, |n| {
                    archive_reclaim(n);
                    n.state = SendState::Sent;
                    Ok(())
                })
                .await?;
                release_quietly(home, j, r).await;
                bail!(
                    "reclaim refused by the mint; token still outstanding; run send --reclaim {} again",
                    r.id
                );
            }
            Probe::Unspent => bail!("reclaim neither signed nor refused; retained"),
        };
        let Some(proofs) = verified(home, &r.mint, &leg, signatures).await? else {
            eprintln!("send {}: reclaim DLEQ invalid; quarantined", r.id);
            return finish(j, r, SendState::ReclaimQuarantined).await;
        };
        leg.result = Some(proofs);
        let journaled = leg.clone();
        update(j, r, |n| {
            n.reclaim = Some(journaled);
            Ok(())
        })
        .await?;
    }
    let proofs = leg.result.context("missing reclaim result")?;
    let net = u64::from(proofs.total_amount()?);
    credit(home, &r.mint, proofs, vec![], r.op()?).await?;
    update(j, r, |n| {
        n.reclaimed = net;
        n.state = SendState::Reclaimed;
        Ok(())
    })
    .await?;
    release(home, j, r).await
}

/// Move the current reclaim leg to the history (secrets kept) and reset its counters.
fn archive_reclaim(n: &mut SendAttempt) {
    if let Some(leg) = n.reclaim.take() {
        n.reclaim_history.push(leg);
    }
    n.reclaim_fee = 0;
    n.reclaim_absent = 0;
    n.reclaim_submitted = None;
}

/// [`release`] on a path that is about to report its own error.
async fn release_quietly(home: &Path, j: &Journal, r: &mut SendAttempt) {
    if let Err(error) = release(home, j, r).await {
        eprintln!("send {}: release deferred: {error:#}", r.id);
    }
}

/// One bounded pass over unfinished sends and reclaims. True when any remain unresolved.
pub async fn recover(home: &Path, j: &Journal) -> Result<bool> {
    let mut failed = false;
    for mut r in j.all::<SendAttempt>("send").await? {
        if r.terminal() {
            // Idempotent: a crash between journaling the state and the release.
            if let Err(error) = release(home, j, &mut r).await {
                failed = true;
                eprintln!("send {}: release deferred: {error:#}", r.id);
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
                "send {}: recovery deferred (error or timeout); attempt retained",
                r.id
            );
        }
    }
    Ok(failed)
}

pub async fn pending(j: &Journal) -> Result<bool> {
    Ok(j.all::<SendAttempt>("send")
        .await?
        .iter()
        .any(|r| !r.terminal() || (r.releasable() && !r.released)))
}

/// Lock-free, read-only: NUT-07 check of every sent token. Prints one public line each.
pub async fn check_redeemed(home: &Path) -> Result<()> {
    let path = home.join("trade.sqlite");
    if !path.exists() {
        return Ok(());
    }
    let rows: Vec<Vec<u8>> = {
        let db = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        db.busy_timeout(std::time::Duration::from_secs(2))?;
        let mut stmt = db.prepare(
            "SELECT value FROM kv_store WHERE primary_namespace='trade-v1' AND secondary_namespace='send'",
        )?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?
    };
    for bytes in rows {
        let r: SendAttempt = serde_json::from_slice(&bytes)?;
        if r.state != SendState::Sent {
            continue;
        }
        let redeemed = match mint::states(&r.mint, &r.sent_proofs()?).await {
            Ok(s) if s.states.iter().all(|s| s.state == State::Spent) => "redeemed",
            Ok(s) if s.states.iter().all(|s| s.state == State::Unspent) => "unredeemed",
            Ok(s) if s.states.iter().any(|s| s.state == State::Pending) => "pending",
            Ok(_) => "partially_redeemed",
            Err(_) => "unknown",
        };
        println!(
            "{}",
            serde_json::json!({"send":r.id,"mint":r.mint,"state":r.state,"amount":r.amount,"redeemed":redeemed})
        );
    }
    Ok(())
}

/// CLI entry: print one public summary line; map the state to the exit code.
pub async fn command(
    home: &Path,
    j: &Journal,
    mint: Option<&str>,
    amount: Option<u64>,
    out: Option<&Path>,
    max_fees: Option<u64>,
    reclaim_id: Option<&str>,
) -> Result<()> {
    let reclaiming = reclaim_id.is_some();
    let r = match reclaim_id {
        Some(id) => reclaim(home, j, id, max_fees).await?,
        None => {
            send(
                home,
                j,
                mint.context("mint required")?,
                amount.context("--amount required")?,
                out.context("--out required")?,
                max_fees,
            )
            .await?
        }
    };
    println!("{}", r.summary());
    use crate::coordinator::{ManualRecovery, RecoveryIncomplete};
    match r.state {
        SendState::Sent if !reclaiming => Ok(()),
        // Reclaim refused, or the recipient redeemed part of it meanwhile: nothing was
        // reclaimed and the token is still outstanding (recover has nothing to do).
        SendState::Sent => bail!(
            "reclaim did not complete; token still outstanding; run send --reclaim {} again",
            r.id
        ),
        SendState::Reclaimed if reclaiming => Ok(()),
        SendState::Redeemed => bail!("redeemed by recipient; nothing reclaimed"),
        SendState::Refused => bail!("send refused by the mint; inputs released, nothing sent"),
        SendState::Prepared | SendState::Submitted | SendState::Swapped | SendState::Reclaiming => {
            Err(RecoveryIncomplete.into())
        }
        _ if r.manual_recovery() => Err(ManualRecovery.into()),
        state => bail!("unexpected send state {state:?}"),
    }
}
