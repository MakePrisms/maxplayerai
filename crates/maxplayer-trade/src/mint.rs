//! Journal-before-effect raw swaps. Explicit empty-preimage HTLC refunds (CDK 0.17.2).
use crate::{
    fee, gross,
    journal::Journal,
    wallet::{database, wallet},
};
use anyhow::{Context, Result, bail, ensure};
use cashu::nuts::nut00::ProofsMethods;
use cashu::{nuts::*, secret::Secret};
use cdk::{amount::SplitTarget, cdk_database::WalletDatabase};
use cdk_common::wallet::ProofInfo;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path};
#[derive(Clone, Serialize, Deserialize)]
pub struct Plan {
    pub mint: String,
    pub inputs: Proofs,
    pub keyset: Id,
    pub ppk: u64,
    pub net: u64,
    pub gross: u64,
    pub lock_fee: u64,
    pub claim_fee: u64,
    pub debit: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Output {
    message: BlindedMessage,
    secret: Secret,
    r: String,
    owned: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Attempt {
    mint: String,
    inputs: Proofs,
    outputs: Vec<Output>,
    result: Option<Proofs>,
    done: bool,
    send_before: Option<u64>,
    #[serde(default)]
    pub abandoned: bool,
    #[serde(default)]
    pub unforwardable: bool,
    #[serde(default)]
    pub quarantined: bool,
    #[serde(default)]
    pub observed_preimage: Option<String>,
}
pub const RPC_TIMEOUT_SECONDS: u64 = 20;
// Three RPC timeouts: the swap timeout plus two timeouts of scheduling/clock margin.
pub const ABANDON_GRACE_SECONDS: u64 = 3 * RPC_TIMEOUT_SECONDS;

pub async fn rpc<T: serde::de::DeserializeOwned>(
    mint: &str,
    op: &str,
    body: &impl Serialize,
) -> Result<T> {
    crate::Asset::new(mint)?.fence()?;
    let r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(RPC_TIMEOUT_SECONDS))
        .build()?
        .post(format!("{mint}/v1/{op}"))
        .json(body)
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    ensure!(
        status.is_success(),
        "mint {mint} {op}: HTTP {status} (body withheld)"
    );
    serde_json::from_str(&text).with_context(|| format!("mint {mint} {op}: invalid response"))
}
pub async fn states(mint: &str, proofs: &Proofs) -> Result<CheckStateResponse> {
    let ys = proofs.ys()?;
    let r: CheckStateResponse =
        rpc(mint, "checkstate", &CheckStateRequest { ys: ys.clone() }).await?;
    ensure!(r.states.len() == ys.len(), "incomplete NUT-07 response");
    let got: HashSet<_> = r.states.iter().map(|s| s.y).collect();
    ensure!(
        got.len() == ys.len() && ys.iter().all(|y| got.contains(y)),
        "NUT-07 Y mismatch"
    );
    Ok(r)
}
pub async fn unspent(mint: &str, p: &Proofs) -> Result<()> {
    ensure!(
        states(mint, p)
            .await?
            .states
            .iter()
            .all(|s| s.state == State::Unspent),
        "proof is not UNSPENT"
    );
    Ok(())
}
pub async fn plan(home: &Path, mint: &str, net: u64, max_fee: u64) -> Result<Plan> {
    let w = wallet(home, mint).await?;
    w.refresh_keysets().await?;
    let k = w.fetch_active_keyset().await?;
    let (gross, claim_fee) = gross(net, k.input_fee_ppk)?;
    crate::real_money::check_lock_gross(gross)?;
    let db = database(home, mint).await?;
    let mut available = db
        .get_proofs(
            Some(mint.parse()?),
            Some(CurrencyUnit::Sat),
            Some(vec![State::Unspent]),
            None,
        )
        .await?;
    available.retain(|p| p.used_by_operation.is_none() && p.spending_condition.is_none());
    available.sort_by_key(|p| p.proof.amount);
    let mut inputs = vec![];
    let mut total = 0u64;
    let mut ppks = vec![];
    for p in available {
        total = total
            .checked_add(u64::from(p.proof.amount))
            .context("overflow")?;
        ppks.push(w.get_keyset_fees_by_id(p.proof.keyset_id).await?);
        inputs.push(p.proof);
        let lock_fee = fee(ppks.clone())?;
        if total >= gross + lock_fee {
            ensure!(inputs.len() <= 128, "too many inputs");
            ensure!(claim_fee + lock_fee <= max_fee, "fee cap exceeded");
            unspent(mint, &inputs).await?;
            return Ok(Plan {
                mint: mint.into(),
                inputs,
                keyset: k.id,
                ppk: k.input_fee_ppk,
                net,
                gross,
                lock_fee,
                claim_fee,
                debit: gross + lock_fee,
            });
        }
    }
    bail!("insufficient unreserved balance (overlisting refused)")
}
pub async fn reserve(home: &Path, p: &Plan, op: &str) -> Result<()> {
    let db = database(home, &p.mint).await?;
    let id = uuid::Uuid::parse_str(op)?;
    let existing = db.get_reserved_proofs(&id).await?;
    let intended: HashSet<_> = p.inputs.ys()?.into_iter().collect();
    ensure!(
        existing.iter().all(|p| intended.contains(&p.y)),
        "reservation conflict"
    );
    let reserved: HashSet<_> = existing.iter().map(|p| p.y).collect();
    let missing = intended.difference(&reserved).copied().collect::<Vec<_>>();
    if !missing.is_empty() {
        db.reserve_proofs(missing, &id).await?;
    }
    Ok(())
}
pub async fn release(home: &Path, p: &Plan, op: &str) -> Result<()> {
    unspent(&p.mint, &p.inputs).await?;
    database(home, &p.mint)
        .await?
        .release_proofs(&uuid::Uuid::parse_str(op)?)
        .await?;
    Ok(())
}
pub fn conditions(
    hash: &str,
    receive: &str,
    refund: &str,
    deadline: u64,
) -> Result<SpendingConditions> {
    Ok(SpendingConditions::HTLCConditions {
        data: hash.parse()?,
        conditions: Some(Conditions {
            locktime: Some(deadline),
            pubkeys: Some(vec![receive.parse()?]),
            refund_keys: Some(vec![refund.parse()?]),
            num_sigs: Some(1),
            num_sigs_refund: Some(1),
            sig_flag: SigFlag::SigInputs,
            ..Default::default()
        }),
    })
}
fn outputs(p: PreMintSecrets, owned: bool) -> Vec<Output> {
    p.secrets
        .into_iter()
        .map(|p| Output {
            message: p.blinded_message,
            secret: p.secret,
            r: hex::encode(p.r.to_secret_bytes()),
            owned,
        })
        .collect()
}
pub async fn lock(
    home: &Path,
    j: &Journal,
    id: &str,
    p: &Plan,
    c: &SpendingConditions,
    send_before: u64,
) -> Result<Proofs> {
    crate::real_money::check_lock_gross(p.gross)?;
    if j.get::<Attempt>("attempt", id).await?.is_none() {
        crate::wallet::preflight(&p.mint).await?;
        let w = wallet(home, &p.mint).await?;
        w.refresh_keysets().await?;
        ensure!(
            w.get_keyset_fees_by_id(p.keyset).await? == p.ppk,
            "fee schedule changed"
        );
        ensure!(
            u64::from(w.get_proofs_fee(&p.inputs).await?.total) == p.lock_fee,
            "input fee changed"
        );
        unspent(&p.mint, &p.inputs).await?;
        let f = w.get_keyset_fees_and_amounts_by_id(p.keyset).await?;
        let mut out = outputs(
            PreMintSecrets::with_conditions(
                p.keyset,
                p.gross.into(),
                &SplitTarget::default(),
                c,
                &f,
            )?,
            false,
        );
        ensure!(
            fee(std::iter::repeat_n(p.ppk, out.len()))? == p.claim_fee,
            "own lock claim fee mismatch"
        );
        let total: u64 = p.inputs.iter().map(|p| u64::from(p.amount)).sum();
        let change = total - p.debit;
        if change > 0 {
            out.extend(outputs(
                PreMintSecrets::random(p.keyset, change.into(), &SplitTarget::default(), &f)?,
                true,
            ));
        }
        ensure!(out.len() <= 128, "output limit");
        j.put(
            "attempt",
            id,
            &Attempt {
                mint: p.mint.clone(),
                inputs: p.inputs.clone(),
                outputs: out,
                result: None,
                done: false,
                abandoned: false,
                unforwardable: false,
                quarantined: false,
                observed_preimage: None,
                send_before: Some(send_before),
            },
        )
        .await?;
    }
    execute(home, j, id).await
}
pub async fn redeem(
    home: &Path,
    j: &Journal,
    id: &str,
    mint: &str,
    proofs: &Proofs,
    key: &str,
    preimage: &str,
    max_fee: u64,
    send_before: Option<u64>,
) -> Result<Proofs> {
    if j.get::<Attempt>("attempt", id).await?.is_none() {
        let w = wallet(home, mint).await?;
        w.refresh_keysets().await?;
        let k = w.fetch_active_keyset().await?;
        let f = w.get_keyset_fees_and_amounts_by_id(k.id).await?;
        let cost = u64::from(w.get_proofs_fee(proofs).await?.total);
        ensure!(cost <= max_fee, "refund_blocked_fee");
        let total = total(proofs)?;
        ensure!(total > cost, "uneconomic redemption");
        let mut inputs = proofs.clone();
        for p in &mut inputs {
            p.witness = None;
            p.add_preimage(preimage.into());
            p.sign_p2pk(key.parse()?)?;
        }
        let out = outputs(
            PreMintSecrets::random(k.id, (total - cost).into(), &SplitTarget::default(), &f)?,
            true,
        );
        j.put(
            "attempt",
            id,
            &Attempt {
                mint: mint.into(),
                inputs,
                outputs: out,
                result: None,
                done: false,
                abandoned: false,
                unforwardable: false,
                quarantined: false,
                observed_preimage: None,
                send_before,
            },
        )
        .await?;
    }
    execute(home, j, id).await
}
pub async fn execute(home: &Path, j: &Journal, id: &str) -> Result<Proofs> {
    let mut a: Attempt = j.get("attempt", id).await?.context("missing attempt")?;
    ensure!(
        !a.quarantined,
        "owned outputs quarantined; manual recovery required"
    );
    if !a.done {
        if a.result.is_none() {
            let messages: Vec<_> = a.outputs.iter().map(|o| o.message.clone()).collect();
            let restored: RestoreResponse = rpc(
                &a.mint,
                "restore",
                &RestoreRequest {
                    outputs: messages.clone(),
                },
            )
            .await?;
            let signatures = if restored.outputs.is_empty() && restored.signatures.is_empty() {
                let now = crate::wallet::action_time(&a.mint).await?;
                if a.send_before
                    .is_some_and(|exp| now > exp.saturating_add(ABANDON_GRACE_SECONDS))
                {
                    let hash = a.inputs.first().and_then(|p| {
                        let secret: cashu::nuts::nut10::Secret = (&p.secret).try_into().ok()?;
                        (secret.kind() == cashu::nuts::nut10::Kind::HTLC)
                            .then(|| secret.secret_data().data().to_owned())
                    });
                    if fresh_not_landed(&a, hash.as_deref()).await? {
                        a.abandoned = true;
                        j.put("attempt", id, &a).await?;
                    }
                }
                ensure!(
                    !a.abandoned,
                    "attempt abandoned; outputs retained and never resubmitted"
                );
                // Restored results may settle without a new admission check. A new
                // lock POST, including replay of a prepared attempt, must pass again.
                if a.outputs.iter().any(|o| !o.owned) {
                    let gross = a
                        .outputs
                        .iter()
                        .filter(|o| !o.owned)
                        .try_fold(0u64, |n, o| {
                            n.checked_add(u64::from(o.message.amount))
                                .context("lock gross overflow")
                        })?;
                    crate::real_money::check_lock_gross(gross)?;
                    crate::wallet::preflight(&a.mint).await?;
                }
                unspent(&a.mint, &a.inputs).await?;
                // Refresh mint time after NUT-07. No RPC may separate this gate and swap POST.
                let now = crate::wallet::action_time(&a.mint).await?;
                ensure!(
                    a.send_before.is_none_or(|exp| now < exp),
                    "attempt deadline passed; retained for reconciliation, no new swap"
                );
                #[cfg(feature = "lab")]
                if std::env::var("TRADE_CRASH_BEFORE_SWAP").ok().as_deref() == Some(id) {
                    std::process::exit(87);
                }
                let r: SwapResponse = rpc(
                    &a.mint,
                    "swap",
                    &SwapRequest::new(a.inputs.clone(), messages.clone()),
                )
                .await?;
                r.signatures
            } else {
                ensure!(
                    restored.outputs.len() == messages.len()
                        && restored.signatures.len() == messages.len(),
                    "partial restore; retain ambiguous attempt"
                );
                messages
                    .iter()
                    .map(|m| {
                        let pos = restored
                            .outputs
                            .iter()
                            .position(|o| o.blinded_secret == m.blinded_secret)
                            .context("restore output mismatch")?;
                        Ok(restored.signatures[pos].clone())
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            ensure!(
                signatures.len() == a.outputs.len(),
                "signature count mismatch"
            );
            let w = wallet(home, &a.mint).await?;
            let keys = w.load_keyset_keys(a.outputs[0].message.keyset_id).await?;
            for (s, o) in signatures.iter().zip(&a.outputs) {
                ensure!(
                    s.amount == o.message.amount && s.keyset_id == o.message.keyset_id,
                    "signature amount/keyset mismatch"
                );
            }
            let result = cdk::dhke::construct_proofs(
                signatures,
                a.outputs
                    .iter()
                    .map(|o| o.r.parse())
                    .collect::<std::result::Result<Vec<SecretKey>, _>>()?,
                a.outputs.iter().map(|o| o.secret.clone()).collect(),
                &keys,
            )?;
            a.result = Some(result);
            a.abandoned = false;
            j.put("attempt", id, &a).await?;
            #[cfg(feature = "lab")]
            if std::env::var("TRADE_CRASH_AFTER_SWAP")
                .ok()
                .is_some_and(|v| v == id || v == "lock" && id.ends_with("-lock"))
            {
                std::process::exit(86);
            }
        }
        let result = a.result.as_ref().context("missing attempt result")?;
        // Persist first; a missing DLEQ must not hide recoverable owned outputs.
        let w = wallet(home, &a.mint).await?;
        if result.iter().any(|p| p.dleq.is_some()) {
            let verified = w
                .verify_token_dleq(&Token::new(
                    a.mint.parse()?,
                    result
                        .iter()
                        .filter(|p| p.dleq.is_some())
                        .cloned()
                        .collect(),
                    None,
                    CurrencyUnit::Sat,
                ))
                .await;
            if let Err(e) = verified {
                a.unforwardable = a.outputs.iter().any(|o| !o.owned);
                // Only a cryptographic failure is terminal, not unavailable keys/RPCs.
                // Full exact-output restore plus SPENT inputs binds the mint's reported
                // commit to this attempt. SPENT alone could be a competing spender.
                if !a.unforwardable && matches!(e, cdk::Error::CouldNotVerifyDleq) {
                    a.quarantined = owned_commit_evidence(&a).await?;
                }
                j.put("attempt", id, &a).await?;
                return Err(e.into());
            }
        }
        unspent(&a.mint, result).await?;
        let owned = result
            .iter()
            .zip(&a.outputs)
            .filter(|(_, o)| o.owned)
            .map(|(p, _)| {
                Ok(ProofInfo::new(
                    p.clone(),
                    a.mint.parse()?,
                    State::Unspent,
                    CurrencyUnit::Sat,
                )?)
            })
            .collect::<Result<Vec<_>>>()?;
        database(home, &a.mint)
            .await?
            .update_proofs(owned, a.inputs.ys()?)
            .await?;
        if !result
            .iter()
            .zip(&a.outputs)
            .all(|(p, o)| o.owned || p.dleq.is_some())
        {
            a.unforwardable = true;
            j.put("attempt", id, &a).await?;
            bail!("mint omitted DLEQ on forwarded proofs; owned change credited");
        }
        a.done = true;
        j.put("attempt", id, &a).await?;
    }
    ensure!(
        !a.unforwardable,
        "unforwardable lock is retained for refund, never forwarding"
    );
    Ok(a.result
        .context("missing completed attempt result")?
        .into_iter()
        .zip(a.outputs)
        .filter(|(_, o)| !o.owned)
        .map(|(p, _)| p)
        .collect())
}
pub async fn validate(
    home: &Path,
    mint: &str,
    p: &Proofs,
    net: u64,
    c: &SpendingConditions,
    ppk: u64,
    keyset: Id,
) -> Result<()> {
    ensure!(!p.is_empty() && p.len() <= 128, "proof count");
    let w = wallet(home, mint).await?;
    w.refresh_keysets().await?;
    ensure!(
        w.get_keyset_fees_by_id(keyset).await? == ppk,
        "fee schedule changed"
    );
    let expected: cashu::nuts::nut10::Secret = c.clone().into();
    let exp = expected.secret_data();
    let mut ys = HashSet::new();
    for proof in p {
        ensure!(proof.witness.is_none(), "unexpected sender witness");
        ensure!(proof.keyset_id == keyset, "unquoted keyset");
        ensure!(ys.insert(proof.y()?), "duplicate proof");
        let s: cashu::nuts::nut10::Secret = (&proof.secret).try_into()?;
        ensure!(
            s.kind() == cashu::nuts::nut10::Kind::HTLC && s.secret_data().data() == exp.data(),
            "wrong hash lock"
        );
        let mut tags = s.secret_data().tags().cloned().unwrap_or_default();
        let mut wanted = exp.tags().cloned().unwrap_or_default();
        tags.sort();
        wanted.sort();
        ensure!(tags == wanted, "HTLC conditions are not exact");
        ensure!(proof.dleq.is_some(), "DLEQ missing");
    }
    w.verify_token_dleq(&Token::new(
        mint.parse()?,
        p.clone(),
        None,
        CurrencyUnit::Sat,
    ))
    .await?;
    let total = total(p)?;
    let cost = u64::from(w.get_proofs_fee(p).await?.total);
    ensure!(total.checked_sub(cost) == Some(net), "incorrect net amount");
    unspent(mint, p).await
}
pub fn matches_preimage(preimage: &str, hash: &str) -> bool {
    use sha2::{Digest, Sha256};
    match (hex::decode(preimage), hex::decode(hash)) {
        (Ok(pre), Ok(hash)) if pre.len() == 32 && hash.len() == 32 => {
            Sha256::digest(pre).as_slice() == hash.as_slice()
        }
        _ => false,
    }
}
pub async fn witness(mint: &str, p: &Proofs, hash: &str) -> Result<Option<String>> {
    for s in states(mint, p).await?.states {
        if s.state != State::Spent {
            continue;
        }
        if let Some(Witness::HTLCWitness(w)) = s.witness {
            if matches_preimage(&w.preimage, hash) {
                return Ok(Some(hex::encode(hex::decode(w.preimage)?)));
            }
        }
    }
    Ok(None)
}
/// Fresh NUT-07 selection: never include spent or pending proofs in a refund.
pub async fn refundable(mint: &str, proofs: &Proofs) -> Result<Proofs> {
    let unspent: HashSet<_> = states(mint, proofs)
        .await?
        .states
        .into_iter()
        .filter(|s| s.state == State::Unspent)
        .map(|s| s.y)
        .collect();
    proofs
        .iter()
        .filter_map(|p| match p.y() {
            Ok(y) if unspent.contains(&y) => Some(Ok(p.clone())),
            Ok(_) => None,
            Err(e) => Some(Err(e.into())),
        })
        .collect()
}

fn total(proofs: &Proofs) -> Result<u64> {
    proofs.iter().try_fold(0u64, |sum, p| {
        sum.checked_add(u64::from(p.amount))
            .context("proof amount overflow")
    })
}

pub async fn refunded_all(j: &Journal, id: &str, outgoing: &Proofs) -> Result<bool> {
    let Some(a) = j.get::<Attempt>("attempt", id).await? else {
        return Ok(false);
    };
    Ok(a.done
        && a.inputs.ys()?.into_iter().collect::<HashSet<_>>()
            == outgoing.ys()?.into_iter().collect::<HashSet<_>>())
}

/// Snapshot evidence, never a cached authorization. Both restores must be wholly empty.
/// For claims, a SPENT input is safe only with an explicit nonmatching refund witness.
async fn fresh_not_landed(a: &Attempt, hash: Option<&str>) -> Result<bool> {
    if a.result.is_some() || a.done || !restore_empty(a).await? {
        return Ok(false);
    }
    for s in states(&a.mint, &a.inputs).await?.states {
        match s.state {
            State::Unspent => {}
            State::Spent => {
                let Some(hash) = hash else {
                    return Ok(false);
                };
                let Some(Witness::HTLCWitness(w)) = s.witness else {
                    return Ok(false);
                };
                if matches_preimage(&w.preimage, hash) {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    // A swap may have landed during NUT-07. Never infer absence from the first restore.
    restore_empty(a).await
}
async fn restore_empty(a: &Attempt) -> Result<bool> {
    let r: RestoreResponse = rpc(
        &a.mint,
        "restore",
        &RestoreRequest {
            outputs: a.outputs.iter().map(|o| o.message.clone()).collect(),
        },
    )
    .await?;
    Ok(r.outputs.is_empty() && r.signatures.is_empty())
}
/// Same-step proof required at the taker's refund, including after persisted abandonment.
pub async fn claim_not_landed(
    j: &Journal,
    id: &str,
    mint: &str,
    incoming: &Proofs,
    hash: &str,
) -> Result<bool> {
    let Some(a) = j.get::<Attempt>("attempt", id).await? else {
        // No claim was submitted, but counterparty evidence must still be unambiguous.
        if incoming.is_empty() {
            return Ok(true);
        }
        for s in states(mint, incoming).await?.states {
            match s.state {
                State::Unspent => {}
                State::Spent => match s.witness {
                    Some(Witness::HTLCWitness(w)) if !matches_preimage(&w.preimage, hash) => {}
                    _ => return Ok(false),
                },
                _ => return Ok(false),
            }
        }
        return Ok(true);
    };
    ensure!(
        a.mint == mint && a.inputs.ys()? == incoming.ys()?,
        "claim input binding mismatch"
    );
    let time = crate::wallet::action_time(mint).await?;
    if !a
        .send_before
        .is_some_and(|exp| time > exp.saturating_add(ABANDON_GRACE_SECONDS))
    {
        return Ok(false);
    }
    fresh_not_landed(&a, Some(hash)).await
}
pub async fn lock_not_landed(j: &Journal, id: &str) -> Result<bool> {
    let a: Attempt = j.get("attempt", id).await?.context("missing lock")?;
    Ok(a.abandoned && fresh_not_landed(&a, None).await?)
}
pub async fn failed_refund(j: &Journal, id: &str, hash: &str) -> Result<Option<String>> {
    let mut a: Attempt = j.get("attempt", id).await?.context("missing refund")?;
    if a.result.is_some() || a.done || !restore_empty(&a).await? {
        return Ok(None);
    }
    let observed = states(&a.mint, &a.inputs).await?;
    let preimage = observed.states.iter().find_map(|s| {
        if s.state != State::Spent {
            return None;
        }
        match &s.witness {
            Some(Witness::HTLCWitness(w)) if matches_preimage(&w.preimage, hash) => {
                Some(w.preimage.clone())
            }
            _ => None,
        }
    });
    let Some(preimage) = preimage else {
        return Ok(None);
    };
    let preimage = hex::encode(hex::decode(preimage)?);
    // Positive knowledge survives even if the final restore fails or another input is
    // PENDING. It authorizes a claim, NEVER a replacement refund without fresh absence.
    a.observed_preimage = Some(preimage.clone());
    j.put("attempt", id, &a).await?;
    if observed.states.iter().any(|s| match s.state {
        State::Unspent => false,
        State::Spent => !matches!(s.witness, Some(Witness::HTLCWitness(_))),
        _ => true,
    }) || !restore_empty(&a).await?
    {
        return Ok(None);
    }
    a.abandoned = true;
    j.put("attempt", id, &a).await?;
    Ok(Some(preimage))
}
pub async fn refund_preimage(j: &Journal, id: &str, hash: &str) -> Result<Option<String>> {
    Ok(j.get::<Attempt>("attempt", id)
        .await?
        .and_then(|a| a.observed_preimage)
        .filter(|pre| matches_preimage(pre, hash)))
}
pub async fn unforwardable(j: &Journal, id: &str) -> Result<Option<Proofs>> {
    let Some(a) = j.get::<Attempt>("attempt", id).await? else {
        return Ok(None);
    };
    if !a.unforwardable {
        return Ok(None);
    }
    Ok(Some(
        a.result
            .context("missing unforwardable result")?
            .into_iter()
            .zip(a.outputs)
            .filter(|(_, o)| !o.owned)
            .map(|(p, _)| p)
            .collect(),
    ))
}

/// Repair DLEQ metadata from the mint, without ever forwarding this lock. Invalid change
/// stays quarantined until independently verified; spent funding inputs are never released.
pub async fn settle_unforwardable(home: &Path, j: &Journal, id: &str) -> Result<()> {
    let mut a: Attempt = j.get("attempt", id).await?.context("missing lock")?;
    ensure!(a.unforwardable, "not an unforwardable lock");
    if a.done {
        return Ok(());
    }
    let r: RestoreResponse = rpc(
        &a.mint,
        "restore",
        &RestoreRequest {
            outputs: a.outputs.iter().map(|o| o.message.clone()).collect(),
        },
    )
    .await?;
    ensure!(
        r.outputs.len() == a.outputs.len() && r.signatures.len() == a.outputs.len(),
        "incomplete lock restore"
    );
    let signatures = a
        .outputs
        .iter()
        .map(|o| {
            let i = r
                .outputs
                .iter()
                .position(|m| m == &o.message)
                .context("restore output mismatch")?;
            let sig = r.signatures[i].clone();
            ensure!(
                sig.amount == o.message.amount && sig.keyset_id == o.message.keyset_id,
                "signature mismatch"
            );
            Ok(sig)
        })
        .collect::<Result<Vec<_>>>()?;
    let w = wallet(home, &a.mint).await?;
    let keys = w.load_keyset_keys(a.outputs[0].message.keyset_id).await?;
    let result = cdk::dhke::construct_proofs(
        signatures,
        a.outputs
            .iter()
            .map(|o| o.r.parse())
            .collect::<std::result::Result<Vec<SecretKey>, _>>()?,
        a.outputs.iter().map(|o| o.secret.clone()).collect(),
        &keys,
    )?;
    // Refuse invalid present DLEQ, including on change. Missing DLEQ still never forwards.
    let present: Proofs = result
        .iter()
        .filter(|p| p.dleq.is_some())
        .cloned()
        .collect();
    if !present.is_empty() {
        w.verify_token_dleq(&Token::new(
            a.mint.parse()?,
            present,
            None,
            CurrencyUnit::Sat,
        ))
        .await?;
    }
    let owned: Proofs = result
        .iter()
        .zip(&a.outputs)
        .filter(|(_, o)| o.owned)
        .map(|(p, _)| p.clone())
        .collect();
    if !owned.is_empty() {
        unspent(&a.mint, &owned).await?;
    }
    // Only the exact lock result may replace the original record.
    ensure!(
        result.ys()? == a.result.as_ref().context("missing result")?.ys()?,
        "lock output identity changed"
    );
    let mint_url = a.mint.parse::<cdk::mint_url::MintUrl>()?;
    database(home, &a.mint)
        .await?
        .update_proofs(
            owned
                .into_iter()
                .map(|p| ProofInfo::new(p, mint_url.clone(), State::Unspent, CurrencyUnit::Sat))
                .collect::<std::result::Result<Vec<_>, _>>()?,
            a.inputs.ys()?,
        )
        .await?;
    a.result = Some(result);
    a.done = true;
    j.put("attempt", id, &a).await
}

/// SPENT inputs alone cannot prove that our refund outputs were credited. Keep the
/// coordinator live until the current exact refund attempt has been reconciled.
pub async fn refund_settled(j: &Journal, id: &str) -> Result<bool> {
    Ok(j.get::<Attempt>("attempt", id)
        .await?
        .is_none_or(|a| a.done))
}

/// Mint-reported commit evidence, not trustlessness against a dishonest issuer.
async fn owned_commit_evidence(a: &Attempt) -> Result<bool> {
    if a.outputs.is_empty() || a.outputs.iter().any(|o| !o.owned) || a.result.is_none() {
        return Ok(false);
    }
    let r: RestoreResponse = rpc(
        &a.mint,
        "restore",
        &RestoreRequest {
            outputs: a.outputs.iter().map(|o| o.message.clone()).collect(),
        },
    )
    .await?;
    if r.outputs.len() != a.outputs.len() || r.signatures.len() != a.outputs.len() {
        return Ok(false);
    }
    let mut seen = HashSet::new();
    for (m, sig) in r.outputs.iter().zip(&r.signatures) {
        if !seen.insert(m.blinded_secret)
            || !a.outputs.iter().any(|o| o.message == *m)
            || sig.amount != m.amount
            || sig.keyset_id != m.keyset_id
        {
            return Ok(false);
        }
    }
    Ok(states(&a.mint, &a.inputs)
        .await?
        .states
        .iter()
        .all(|s| s.state == State::Spent))
}
