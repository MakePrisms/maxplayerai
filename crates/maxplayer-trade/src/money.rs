//! Money effects retain exact output material; no CDK melt saga is used.
use crate::{
    journal::Journal,
    mint,
    wallet::{database, wallet},
};
use anyhow::{Context, Result, ensure};
use cashu::nuts::nut00::ProofsMethods;
use cashu::{nuts::*, secret::Secret};
use cdk::{amount::SplitTarget, cdk_database::WalletDatabase};
use cdk_common::wallet::ProofInfo;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path};

#[derive(Clone, Serialize, Deserialize)]
struct Output {
    message: BlindedMessage,
    secret: Secret,
    r: String,
}
fn outputs(p: PreMintSecrets) -> Vec<Output> {
    p.secrets
        .into_iter()
        .map(|o| Output {
            message: o.blinded_message,
            secret: o.secret,
            r: hex::encode(o.r.to_secret_bytes()),
        })
        .collect()
}
async fn restore(mint: &str, out: &[Output]) -> Result<(Vec<Output>, Vec<BlindSignature>)> {
    let r: RestoreResponse = mint::rpc(
        mint,
        "restore",
        &RestoreRequest {
            outputs: out.iter().map(|o| o.message.clone()).collect(),
        },
    )
    .await?;
    ensure!(
        r.outputs.len() == r.signatures.len(),
        "incomplete restore pairs"
    );
    let mut seen = HashSet::new();
    let mut selected = vec![];
    for (m, s) in r.outputs.iter().zip(&r.signatures) {
        ensure!(seen.insert(m.blinded_secret), "duplicate restored output");
        let o = out
            .iter()
            .find(|o| o.message.blinded_secret == m.blinded_secret)
            .context("unknown restored output")?;
        ensure!(
            m.keyset_id == o.message.keyset_id
                && s.keyset_id == m.keyset_id
                && (m.amount == o.message.amount || s.amount == m.amount),
            "restore binding mismatch"
        );
        ensure!(
            o.message.amount == 0.into() || o.message.amount == s.amount,
            "restore amount mismatch"
        );
        selected.push(o.clone());
    }
    Ok((selected, r.signatures))
}
async fn unblind(
    home: &Path,
    url: &str,
    out: &[Output],
    sigs: Vec<BlindSignature>,
) -> Result<Proofs> {
    ensure!(out.len() == sigs.len(), "signature count mismatch");
    if out.is_empty() {
        return Ok(vec![]);
    }
    let w = wallet(home, url).await?;
    let keys = crate::wallet::bounded_for(url, w.load_keyset_keys(out[0].message.keyset_id))
        .await
        .context("CDK wallet request timed out")??;
    let proofs = cdk::dhke::construct_proofs(
        sigs,
        out.iter()
            .map(|o| o.r.parse())
            .collect::<std::result::Result<Vec<SecretKey>, _>>()?,
        out.iter().map(|o| o.secret.clone()).collect(),
        &keys,
    )?;
    ensure!(
        proofs.iter().all(|p| p.dleq.is_some()),
        "change requires DLEQ"
    );
    crate::wallet::bounded_for(
        url,
        w.verify_token_dleq(&Token::new(
            url.parse()?,
            proofs.clone(),
            None,
            CurrencyUnit::Sat,
        )),
    )
    .await
    .context("CDK wallet request timed out")??;
    Ok(proofs)
}
async fn credit(
    home: &Path,
    url: &str,
    proofs: &Proofs,
    inputs: &Proofs,
    operation: &str,
) -> Result<()> {
    if !proofs.is_empty() {
        mint::unspent(url, proofs).await?;
    }
    let rows = proofs
        .iter()
        .map(|p| {
            Ok(ProofInfo::new_with_operations(
                p.clone(),
                url.parse()?,
                State::Reserved,
                CurrencyUnit::Sat,
                Some(operation.parse()?),
                Some(operation.parse()?),
            )?)
        })
        .collect::<Result<Vec<_>>>()?;
    database(home, url)
        .await?
        .update_proofs(rows, inputs.ys()?)
        .await?;
    Ok(())
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Funding {
    pub id: String,
    pub mint: String,
    pub amount: u64,
    pub quote: Option<String>,
    pub invoice: Option<String>,
    pub done: bool,
    #[serde(default)]
    pub expired_unpaid: bool,
    quote_key: String,
    outputs: Vec<Output>,
    result: Option<Proofs>,
}
pub async fn fund(home: &Path, j: &Journal, url: &str, amount: u64) -> Result<Funding> {
    crate::Asset::new(url)?.fence()?;
    // Before ANY journal intent: a nostr:// credits mint has no NUT-04/NUT-20 to fund through.
    crate::transport::require_http(url, "fund")?;
    ensure!(
        amount > 0 && amount <= crate::real_money::CAP,
        "funding cap exceeded"
    );
    let received = crate::receive::charged(j, url).await?;
    let total = j
        .all::<Funding>("funding")
        .await?
        .iter()
        .filter(|f| f.mint == url)
        .try_fold(0u64, |s, f| {
            s.checked_add(f.amount).context("funding overflow")
        })?
        .checked_add(received)
        .context("funding overflow")?;
    if !total
        .checked_add(amount)
        .is_some_and(|n| n <= crate::real_money::CAP)
    {
        if received > 0 {
            anyhow::bail!(
                "cumulative funding and receives exceed 100,000 sats for this mint \
                 ({received} sats held by receives, gross, including unresolved or quarantined)"
            );
        }
        anyhow::bail!("cumulative funding exceeds 100,000 sats");
    }
    crate::wallet::preflight_for(url, Some("fund")).await?;
    let mut f = Funding {
        id: uuid::Uuid::new_v4().to_string(),
        mint: url.into(),
        amount,
        quote: None,
        invoice: None,
        done: false,
        expired_unpaid: false,
        quote_key: hex::encode(SecretKey::generate().to_secret_bytes()),
        outputs: vec![],
        result: None,
    };
    // Charge the cap BEFORE creating a quote. A lost quote reply remains charged.
    j.put("funding", &f.id, &f).await?;
    let q: serde_json::Value = mint::rpc(
        url,
        "mint/quote/bolt11",
        &serde_json::json!({"amount":amount,"unit":"sat","pubkey":f.quote_key.parse::<SecretKey>()?.public_key()}),
    )
    .await?;
    f.quote = Some(q["quote"].as_str().context("missing quote")?.into());
    f.invoice = Some(q["request"].as_str().context("missing invoice")?.into());
    j.put("funding", &f.id, &f).await?;
    ensure!(
        q["pubkey"].as_str() == Some(&f.quote_key.parse::<SecretKey>()?.public_key().to_string()),
        "mint did not bind funding quote to NUT-20 key"
    );
    let invoice_check = MeltQuoteBolt11Request {
        request: f
            .invoice
            .as_ref()
            .context("missing funding invoice")?
            .parse()?,
        unit: CurrencyUnit::Sat,
        options: None,
    };
    ensure!(
        invoice_check.request.amount_milli_satoshis() == amount.checked_mul(1000),
        "funding invoice amount mismatch"
    );
    j.put("funding", &f.id, &f).await?;
    let _ = home;
    Ok(f)
}
async fn get<T: serde::de::DeserializeOwned>(url: &str, path: &str, id: &str) -> Result<T> {
    crate::Asset::new(url)?.fence()?;
    crate::transport::require_http(url, "quote status")?;
    let mut endpoint = url::Url::parse(&format!("{url}/v1/{path}/"))?;
    endpoint
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid endpoint"))?
        .pop_if_empty()
        .push(id);
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()?
        .get(endpoint)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}
pub async fn resume_fund(home: &Path, j: &Journal, f: &mut Funding) -> Result<()> {
    if f.done {
        database(home, &f.mint)
            .await?
            .release_proofs(&f.id.parse()?)
            .await?;
        return Ok(());
    }
    let id = f
        .quote
        .as_ref()
        .context("quote reply lost; funding intent retained against cap")?;
    let q: serde_json::Value = get(&f.mint, "mint/quote/bolt11", id).await?;
    ensure!(q["quote"].as_str() == Some(id), "fund quote mismatch");
    ensure!(
        q["pubkey"].as_str() == Some(&f.quote_key.parse::<SecretKey>()?.public_key().to_string()),
        "fund quote ownership changed"
    );
    if q["state"] == "UNPAID" {
        if q["expiry"]
            .as_u64()
            .is_some_and(|exp| cdk::util::unix_time() > exp.saturating_add(60))
        {
            f.expired_unpaid = true;
            j.put("funding", &f.id, f).await?;
        }
        return Ok(());
    }
    f.expired_unpaid = false;
    ensure!(
        q["state"] == "PAID" || q["state"] == "ISSUED",
        "unknown funding state"
    );
    if f.outputs.is_empty() {
        ensure!(
            q["state"] == "PAID",
            "issued quote without recorded outputs"
        );
        let w = wallet(home, &f.mint).await?;
        crate::wallet::bounded_for(&f.mint, w.refresh_keysets())
            .await
            .context("CDK wallet request timed out")??;
        let k = crate::wallet::bounded_for(&f.mint, w.fetch_active_keyset())
            .await
            .context("CDK wallet request timed out")??;
        let fees = crate::wallet::bounded_for(&f.mint, w.get_keyset_fees_and_amounts_by_id(k.id))
            .await
            .context("CDK wallet request timed out")??;
        f.outputs = outputs(PreMintSecrets::random(
            k.id,
            f.amount.into(),
            &SplitTarget::default(),
            &fees,
        )?);
        j.put("funding", &f.id, f).await?;
    }
    if f.result.is_none() {
        let (out, mut sigs) = restore(&f.mint, &f.outputs).await?;
        let selected = if out.is_empty() {
            ensure!(q["state"] == "PAID", "issued outputs not yet restored");
            // Issuing an already-paid quote adds no new exposure. Keyset and DLEQ checks remain.
            let mut request = MintRequest {
                quote: id.clone(),
                outputs: f.outputs.iter().map(|o| o.message.clone()).collect(),
                signature: None,
            };
            request.sign(f.quote_key.parse()?)?;
            let r: MintResponse = mint::rpc(&f.mint, "mint/bolt11", &request).await?;
            sigs = r.signatures;
            f.outputs.clone()
        } else {
            out
        };
        ensure!(selected.len() == f.outputs.len(), "partial funding restore");
        let p = unblind(home, &f.mint, &selected, sigs).await?;
        ensure!(
            u64::from(p.total_amount()?) == f.amount,
            "funding amount mismatch"
        );
        f.result = Some(p);
        j.put("funding", &f.id, f).await?;
    }
    credit(
        home,
        &f.mint,
        f.result.as_ref().context("missing funding result")?,
        &vec![],
        &f.id,
    )
    .await?;
    f.done = true;
    j.put("funding", &f.id, f).await?;
    database(home, &f.mint)
        .await?
        .release_proofs(&f.id.parse()?)
        .await?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum MeltState {
    QuoteCreated,
    Refused,
    RequestSent,
    Pending,
    PaidChangeUnreconciled,
    Done,
    UnpaidReleased,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Withdrawal {
    pub id: String,
    pub mint: String,
    invoice: String,
    pub state: MeltState,
    quote: Option<MeltQuoteBolt11Response<String>>,
    #[serde(default)]
    pub max_debit: Option<u64>,
    pub input_fee: u64,
    inputs: Proofs,
    outputs: Vec<Output>,
    // Completed replies authorize UNPAID release; PAID alone never proves zero change.
    final_reply: bool,
    seen_pending: bool,
    result: Option<Proofs>,
    pub change: Option<u64>,
}
impl Withdrawal {
    pub fn terminal(&self) -> bool {
        matches!(
            self.state,
            MeltState::Done | MeltState::UnpaidReleased | MeltState::Refused
        )
    }
}
/// True when a withdrawal for this invoice's payment hash was SUBMITTED (past
/// QuoteCreated) and is not yet terminal. Such a record is unresolved work, never a
/// refusal, even if the command that drove it returned an error.
pub async fn submitted_unresolved(j: &Journal, invoice: &str) -> Result<bool> {
    // An unparsable invoice never created a record: that is a plain command error.
    let Ok(parsed) = invoice.parse::<cashu::Bolt11Invoice>() else {
        return Ok(false);
    };
    let hash = parsed.payment_hash().to_owned();
    for a in j.all::<Withdrawal>("withdrawal").await? {
        let Ok(prior) = a.invoice.parse::<cashu::Bolt11Invoice>() else {
            continue;
        };
        if *prior.payment_hash() == hash && a.state != MeltState::QuoteCreated && !a.terminal() {
            return Ok(true);
        }
    }
    Ok(false)
}
pub async fn withdraw(home: &Path, j: &Journal, url: &str, invoice: &str) -> Result<Withdrawal> {
    withdraw_bounded(home, j, url, invoice, None).await
}
pub async fn withdraw_bounded(
    home: &Path,
    j: &Journal,
    url: &str,
    invoice: &str,
    max_debit: Option<u64>,
) -> Result<Withdrawal> {
    let canonical = crate::Asset::new(url)?;
    canonical.fence()?;
    // Before ANY journal intent (including dedupe updates): no NUT-05 melt on a nostr:// mint.
    crate::transport::require_http(&canonical.mint_url, "withdraw")?;
    let url = canonical.mint_url.as_str();
    let req = MeltQuoteBolt11Request {
        request: invoice.parse()?,
        unit: CurrencyUnit::Sat,
        options: None,
    };
    let invoice = req.request.to_string();
    // The payment hash identifies the obligation across case variants AND mints.
    for mut a in j.all::<Withdrawal>("withdrawal").await? {
        let prior = MeltQuoteBolt11Request {
            request: a.invoice.parse()?,
            unit: CurrencyUnit::Sat,
            options: None,
        };
        if prior.request.payment_hash() == req.request.payment_hash()
            && a.state != MeltState::Refused
        {
            ensure!(
                a.mint == url,
                "invoice already authorized on another mint; resume the original mint"
            );
            if let Some(limit) = max_debit {
                a.max_debit = Some(a.max_debit.map_or(limit, |old| old.min(limit)));
                if a.state != MeltState::QuoteCreated {
                    let q = a.quote.as_ref().context("missing quote")?;
                    ensure!(
                        u64::from(q.amount) + a.input_fee + u64::from(q.fee_reserve) <= limit,
                        "existing authorization exceeds max-debit; it may already be paying"
                    );
                }
                j.put("withdrawal", &a.id, &a).await?;
            }
            resume_withdraw_inner(home, j, &mut a, true).await?;
            return Ok(a);
        }
    }
    let amount = req
        .request
        .amount_milli_satoshis()
        .context("amountless invoices refused")?
        .div_ceil(1000);
    ensure!(
        amount > 0 && amount <= crate::real_money::CAP,
        "withdrawal exceeds 100,000-sat invoice cap"
    );
    let mut a = Withdrawal {
        id: uuid::Uuid::new_v4().to_string(),
        mint: url.into(),
        invoice: invoice.clone(),
        state: MeltState::QuoteCreated,
        quote: None,
        max_debit,
        input_fee: 0,
        inputs: vec![],
        outputs: vec![],
        final_reply: false,
        seen_pending: false,
        result: None,
        change: None,
    };
    j.put("withdrawal", &a.id, &a).await?;
    let validated: Result<MeltQuoteBolt11Response<String>> = async {
        crate::wallet::preflight_for(url, Some("withdraw")).await?;
        let mut q: MeltQuoteBolt11Response<String> =
            mint::rpc(url, "melt/quote/bolt11", &req).await?;
        ensure!(
            u64::from(q.amount) == amount && q.state == MeltQuoteState::Unpaid,
            "invalid initial melt quote"
        );
        ensure!(
            q.request.as_ref().is_none_or(|v| v == &invoice)
                && q.unit.as_ref().is_none_or(|v| *v == CurrencyUnit::Sat),
            "quote invoice/unit mismatch"
        );
        ensure!(
            u64::from(q.fee_reserve) <= reserve_ceiling(amount),
            "withdrawal reserve exceeds max(32 sat, 2%) ceiling"
        );
        ensure!(
            q.expiry > cdk::util::unix_time().saturating_add(60),
            "melt quote needs more than 60 seconds before expiry"
        );
        q.payment_preimage = None;
        Ok(q)
    }
    .await;
    match validated {
        Ok(q) => {
            a.quote = Some(q);
            j.put("withdrawal", &a.id, &a).await?;
        }
        Err(error) => {
            // No spend authorization or reservation exists, including on a lost quote reply.
            a.state = MeltState::Refused;
            j.put("withdrawal", &a.id, &a).await?;
            return Err(error);
        }
    }
    resume_withdraw_inner(home, j, &mut a, true).await?;
    Ok(a)
}
pub fn reserve_ceiling(amount: u64) -> u64 {
    32.max(amount.div_ceil(50))
}

async fn prepare_withdraw(home: &Path, j: &Journal, a: &mut Withdrawal) -> Result<()> {
    if !a.inputs.is_empty() {
        return Ok(());
    }
    let q = a
        .quote
        .as_ref()
        .context("melt quote reply lost; no inputs reserved")?;
    ensure!(
        q.expiry > cdk::util::unix_time().saturating_add(60),
        "melt quote expired before submission"
    );
    let w = wallet(home, &a.mint).await?;
    crate::wallet::bounded_for(&a.mint, w.refresh_keysets())
        .await
        .context("CDK wallet request timed out")??;
    let k = crate::wallet::bounded_for(&a.mint, w.fetch_active_keyset())
        .await
        .context("CDK wallet request timed out")??;
    let db = database(home, &a.mint).await?;
    let mut available = db
        .get_proofs(
            Some(a.mint.parse()?),
            Some(CurrencyUnit::Sat),
            Some(vec![State::Unspent]),
            None,
        )
        .await?;
    available.retain(|p| p.used_by_operation.is_none() && p.spending_condition.is_none());
    available.sort_by_key(|p| p.proof.amount);
    let mut input = vec![];
    for p in available {
        input.push(p.proof);
        let total = u64::from(input.total_amount()?);
        let input_fee = u64::from(
            crate::wallet::bounded_for(&a.mint, w.get_proofs_fee(&input))
                .await
                .context("CDK wallet request timed out")??
                .total,
        );
        let net_required = u64::from(q.amount)
            .checked_add(input_fee)
            .context("melt amount overflow")?;
        let required = net_required
            .checked_add(u64::from(q.fee_reserve))
            .context("melt reserve overflow")?;
        let provably_no_change = total == net_required;
        // Require positive change even if the entire reserve is spent, unless
        // arithmetic alone proves none can exist. A fee-reserve-equals-change
        // zero result cannot be distinguished from delayed mint finalization.
        if total >= required && (total > required || provably_no_change) {
            ensure!(
                a.max_debit.is_none_or(|limit| required <= limit),
                "withdrawal exceeds max-debit"
            );
            ensure!(input.len() <= 128, "too many melt inputs");
            mint::unspent(&a.mint, &input).await?;
            a.outputs = outputs(PreMintSecrets::blank(
                k.id,
                (total - u64::from(q.amount) - input_fee).into(),
            )?);
            a.input_fee = input_fee;
            a.inputs = input;
            j.put("withdrawal", &a.id, a).await?;
            return Ok(());
        }
    }
    anyhow::bail!("insufficient unreserved funds for melt, reserve and provable change")
}
async fn reserve_withdraw(home: &Path, a: &Withdrawal) -> Result<()> {
    let db = database(home, &a.mint).await?;
    let id = a.id.parse()?;
    let existing = db.get_reserved_proofs(&id).await?;
    let intended: HashSet<_> = a.inputs.ys()?.into_iter().collect();
    ensure!(
        existing.iter().all(|p| intended.contains(&p.y)),
        "withdraw reservation conflict"
    );
    let held: HashSet<_> = existing.iter().map(|p| p.y).collect();
    let missing = intended.difference(&held).copied().collect::<Vec<_>>();
    if !missing.is_empty() {
        db.reserve_proofs(missing, &id).await?;
    }
    Ok(())
}
fn bind(a: &Withdrawal, q: &MeltQuoteBolt11Response<String>) -> Result<()> {
    let old = a.quote.as_ref().context("missing bound quote")?;
    ensure!(
        old.state != MeltQuoteState::Paid || q.state == MeltQuoteState::Paid,
        "paid quote regressed"
    );
    ensure!(
        q.quote == old.quote && q.amount == old.amount && q.fee_reserve == old.fee_reserve,
        "melt quote binding changed"
    );
    ensure!(
        q.request.as_ref().is_none_or(|v| v == &a.invoice)
            && q.unit.as_ref().is_none_or(|v| *v == CurrencyUnit::Sat),
        "melt request binding changed"
    );
    Ok(())
}
async fn submit_melt(
    url: &str,
    request: &MeltRequest<String>,
) -> Result<Option<MeltQuoteBolt11Response<String>>> {
    crate::Asset::new(url)?.fence()?;
    crate::transport::require_http(url, "melt")?;
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(mint::RPC_TIMEOUT_SECONDS))
        .build()?
        .post(format!("{url}/v1/melt/bolt11"))
        .json(request)
        .send()
        .await?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .context("invalid melt response (body withheld)")?;
    if !status.is_success() {
        if status.is_client_error() && body["code"].as_u64().is_some() {
            return Ok(None);
        }
        anyhow::bail!("melt response HTTP {status}; body withheld; intent retained");
    }
    Ok(Some(
        serde_json::from_value(body).context("invalid melt quote response")?,
    ))
}
pub async fn resume_withdraw(home: &Path, j: &Journal, a: &mut Withdrawal) -> Result<()> {
    resume_withdraw_inner(home, j, a, false).await
}
async fn resume_withdraw_inner(
    home: &Path,
    j: &Journal,
    a: &mut Withdrawal,
    explicit: bool,
) -> Result<()> {
    if a.terminal() {
        database(home, &a.mint)
            .await?
            .release_proofs(&a.id.parse()?)
            .await?;
        return Ok(());
    }
    crate::Asset::new(&a.mint)?.fence()?;
    if a.state == MeltState::QuoteCreated {
        if a.quote
            .as_ref()
            .is_none_or(|q| q.expiry <= cdk::util::unix_time().saturating_add(60))
        {
            // No POST can have occurred: RequestSent is persisted before that effect.
            // Classify as Refused (never submitted), which dedupe skips, so the same
            // invoice can be retried explicitly with a fresh quote.
            if !a.inputs.is_empty() {
                mint::unspent(&a.mint, &a.inputs).await?;
            }
            a.state = MeltState::Refused;
            j.put("withdrawal", &a.id, a).await?;
            database(home, &a.mint)
                .await?
                .release_proofs(&a.id.parse()?)
                .await?;
            return Ok(());
        }
        // Recovery must never turn an unsent failed command into a payment.
        if !explicit {
            return Ok(());
        }
        if let Err(error) = prepare_withdraw(home, j, a).await {
            if a.inputs.is_empty() {
                a.state = MeltState::Refused;
                j.put("withdrawal", &a.id, a).await?;
            }
            return Err(error);
        }
        let q = a.quote.as_ref().context("missing withdrawal quote")?;
        ensure!(
            a.max_debit.is_none_or(|limit| u64::from(q.amount)
                .checked_add(a.input_fee)
                .and_then(|v| v.checked_add(u64::from(q.fee_reserve)))
                .is_some_and(|debit| debit <= limit)),
            "withdrawal exceeds max-debit"
        );
        reserve_withdraw(home, a).await?;
        mint::unspent(&a.mint, &a.inputs).await?;
        ensure!(
            q.expiry > cdk::util::unix_time().saturating_add(60),
            "melt quote expired; reservation retained"
        );
        a.state = MeltState::RequestSent;
        j.put("withdrawal", &a.id, a).await?;
    }
    // RequestSent is already journaled: retry ONLY the identical authorization.
    // GET/restore runs even if the replay fails, so a lost PAID reply can reconcile.
    if a.state == MeltState::RequestSent && !a.final_reply && !a.seen_pending {
        let req = MeltRequest::new(
            a.quote.as_ref().context("missing quote")?.quote.clone(),
            a.inputs.clone(),
            Some(a.outputs.iter().map(|o| o.message.clone()).collect()),
        );
        let reply = submit_melt(&a.mint, &req).await;
        #[cfg(feature = "lab")]
        if std::env::var("TRADE_CRASH_AFTER_MELT").ok().as_deref() == Some("1") {
            std::process::exit(86);
        }
        if let Ok(Some(mut reply)) = reply {
            bind(a, &reply)?;
            a.final_reply = matches!(reply.state, MeltQuoteState::Paid | MeltQuoteState::Unpaid);
            reply.payment_preimage = None;
            if reply.state == MeltQuoteState::Pending {
                a.seen_pending = true;
                a.state = MeltState::Pending;
                a.quote = Some(reply);
                return j.put("withdrawal", &a.id, a).await;
            }
            a.quote = Some(reply);
        } else if matches!(reply, Ok(None)) {
            // A well-formed NUT error is definitive. Release still requires fresh
            // UNPAID and UNSPENT observations, never the error body alone.
            a.final_reply = true;
        } else {
            eprintln!(
                "withdrawal {}: no definitive melt reply; identical request retained",
                a.id
            );
        }
        j.put("withdrawal", &a.id, a).await?;
    }
    let mut q = get::<MeltQuoteBolt11Response<String>>(
        &a.mint,
        "melt/quote/bolt11",
        &a.quote.as_ref().context("missing quote")?.quote,
    )
    .await?;
    bind(a, &q)?;
    q.payment_preimage = None;
    a.quote = Some(q.clone());
    match q.state {
        MeltQuoteState::Pending => {
            a.state = MeltState::Pending;
            a.seen_pending = true;
        }
        MeltQuoteState::Unpaid if a.final_reply || a.seen_pending => {
            // No inference from a status snapshot while a timed-out POST may still arrive.
            mint::unspent(&a.mint, &a.inputs).await?;
            // Journal terminal status before release so a crash cannot leave an
            // old failure trying to re-check proofs spent by a subsequent trade.
            a.state = MeltState::UnpaidReleased;
        }
        MeltQuoteState::Paid => {
            a.state = MeltState::PaidChangeUnreconciled;
            j.put("withdrawal", &a.id, a).await?;
            if a.result.is_none() {
                let (out, sigs) = restore(&a.mint, &a.outputs).await?;
                let advertised = q.change.as_deref().unwrap_or_default();
                // CDK commits PAID before its change transaction. Empty polling evidence
                // is NOT finality. Keep it non-terminal even after repeated empty restores.
                ensure!(
                    !advertised.is_empty()
                        || u64::from(a.inputs.total_amount()?) == u64::from(q.amount) + a.input_fee,
                    "paid but zero change not proven; journal retained"
                );
                ensure!(
                    advertised.len() == sigs.len() && advertised.iter().all(|s| sigs.contains(s)),
                    "change restore incomplete"
                );
                let proofs = unblind(home, &a.mint, &out, sigs).await?;
                let change = u64::from(proofs.total_amount()?);
                let available =
                    u64::from(a.inputs.total_amount()?) - u64::from(q.amount) - a.input_fee;
                ensure!(
                    change <= available && available - change <= u64::from(q.fee_reserve),
                    "change outside bound; cannot account for melt"
                );
                a.change = Some(change);
                a.result = Some(proofs);
                j.put("withdrawal", &a.id, a).await?;
            }
            ensure!(
                mint::states(&a.mint, &a.inputs)
                    .await?
                    .states
                    .iter()
                    .all(|s| s.state == State::Spent),
                "paid melt has unspent or pending inputs"
            );
            credit(
                home,
                &a.mint,
                a.result.as_ref().context("missing withdrawal change")?,
                &a.inputs,
                &a.id,
            )
            .await?;
            a.state = MeltState::Done;
        }
        _ => {} // Unknown, failed, or ambiguous UNPAID: preserve all reservations.
    }
    j.put("withdrawal", &a.id, a).await?;
    if a.terminal() {
        database(home, &a.mint)
            .await?
            .release_proofs(&a.id.parse()?)
            .await?;
    }
    Ok(())
}
pub async fn recover(home: &Path, j: &Journal) -> Result<bool> {
    let mut failed = false;
    for mut f in j.all::<Funding>("funding").await? {
        if f.expired_unpaid {
            continue;
        }
        if !matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                resume_fund(home, j, &mut f)
            )
            .await,
            Ok(Ok(()))
        ) {
            failed = true;
            eprintln!(
                "funding {}: recovery deferred (error or timeout); authorization retained",
                f.id
            );
        }
    }
    for mut a in j.all::<Withdrawal>("withdrawal").await? {
        if !matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                resume_withdraw(home, j, &mut a)
            )
            .await,
            Ok(Ok(()))
        ) {
            failed = true;
            eprintln!(
                "withdrawal {}: recovery deferred (error or timeout); authorization retained",
                a.id
            );
        }
    }
    // Receives share the item budget and pass; see receive::RECOVERY_ITEM_SECONDS.
    let receives = crate::receive::recover(home, j).await?;
    Ok(pending(j).await? || failed || receives)
}

pub async fn pending(j: &Journal) -> Result<bool> {
    Ok(j.all::<Funding>("funding")
        .await?
        .iter()
        .any(|f| !f.done && !f.expired_unpaid)
        || j.all::<Withdrawal>("withdrawal")
            .await?
            .iter()
            .any(|a| !a.terminal())
        || crate::receive::pending(j).await?)
}

impl Withdrawal {
    /// Public accounting view. Never serialize the recovery record to logs.
    pub fn summary(&self) -> serde_json::Value {
        let input = self.inputs.iter().map(|p| u64::from(p.amount)).sum::<u64>();
        let amount = self.quote.as_ref().map(|q| u64::from(q.amount));
        let fee = if self.state == MeltState::Done {
            amount
                .and_then(|n| input.checked_sub(n))
                .and_then(|n| n.checked_sub(self.input_fee))
                .and_then(|n| n.checked_sub(self.change?))
        } else {
            None
        };
        serde_json::json!({"withdrawal":self.id,"mint":self.mint,"state":self.state,
            "quote":self.quote.as_ref().map(|q| &q.quote), "amount":amount,
            "fee_reserve":self.quote.as_ref().map(|q| u64::from(q.fee_reserve)),
            "max_debit":self.max_debit,"input_total":input,"mint_fee":self.input_fee,"change":self.change,"lightning_fee":fee})
    }
}
