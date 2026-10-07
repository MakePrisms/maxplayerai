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
}
pub async fn rpc<T: serde::de::DeserializeOwned>(
    mint: &str,
    op: &str,
    body: &impl Serialize,
) -> Result<T> {
    crate::Asset::new(mint)?.fence()?;
    let r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()?
        .post(format!("{mint}/v1/{op}"))
        .json(body)
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    ensure!(
        status.is_success(),
        "mint {mint} {op}: HTTP {status}: {text}"
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
    if j.get::<Attempt>("attempt", id).await?.is_none() {
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
                send_before,
            },
        )
        .await?;
    }
    execute(home, j, id).await
}
pub async fn execute(home: &Path, j: &Journal, id: &str) -> Result<Proofs> {
    let mut a: Attempt = j.get("attempt", id).await?.context("missing attempt")?;
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
            let signatures = if restored.outputs.is_empty() {
                ensure!(
                    a.send_before.is_none_or(|exp| cdk::util::unix_time() < exp),
                    "attempt deadline passed; retained for reconciliation, no new swap"
                );
                unspent(&a.mint, &a.inputs).await?;
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
            ensure!(result.iter().all(|p| p.dleq.is_some()), "mint omitted DLEQ");
            w.verify_token_dleq(&Token::new(
                a.mint.parse()?,
                result.clone(),
                None,
                CurrencyUnit::Sat,
            ))
            .await?;
            a.result = Some(result);
            j.put("attempt", id, &a).await?;
            #[cfg(feature = "lab")]
            if std::env::var("TRADE_CRASH_AFTER_SWAP")
                .ok()
                .is_some_and(|v| v == id || v == "lock" && id.ends_with("-lock"))
            {
                std::process::exit(86);
            }
        }
        let result = a.result.as_ref().unwrap();
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
        a.done = true;
        j.put("attempt", id, &a).await?;
    }
    Ok(a.result
        .unwrap()
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
pub async fn witness(mint: &str, p: &Proofs, hash: &str) -> Result<Option<String>> {
    let s = states(mint, p).await?;
    if s.states.iter().all(|s| s.state == State::Unspent) {
        return Ok(None);
    }
    ensure!(
        s.states.iter().all(|s| s.state == State::Spent),
        "partial/pending outgoing state"
    );
    let mut found = None;
    for s in s.states {
        let Witness::HTLCWitness(witness) = s.witness.context("SPENT without witness")? else {
            bail!("missing HTLC witness")
        };
        let pre = witness.preimage.as_str();
        use sha2::{Digest, Sha256};
        ensure!(
            hex::encode(Sha256::digest(hex::decode(pre)?)) == hash,
            "SPENT without matching preimage (possibly refund)"
        );
        if let Some(ref previous) = found {
            ensure!(previous == pre, "witness conflict");
        }
        found = Some(pre.to_string());
    }
    Ok(found)
}

fn total(proofs: &Proofs) -> Result<u64> {
    proofs.iter().try_fold(0u64, |sum, p| {
        sum.checked_add(u64::from(p.amount))
            .context("proof amount overflow")
    })
}
