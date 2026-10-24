//! predicate-regression — targeted proof that the corrected F4/F5 predicates DISCRIMINATE.
//!
//! The accepted 39-assertion gate run is not re-run here and is not evidence for this round. What
//! is in question is narrower and sharper: the old oracles could pass while value was stranded, and
//! a green transcript from a healthy fixture cannot tell you whether that hole is closed. Only a
//! case the old predicate accepts and the new predicate rejects can.
//!
//! So this binary builds the reviewer's own counterexample for real, against the real mint, and
//! requires the old predicate to PASS on it and the new predicate to FAIL on it. If a future edit
//! weakens a predicate back, this exits non-zero.
//!
//! Two fixtures, both real wallets against the loopback fakewallet mint:
//!
//!   A. HEALTHY — issue, sweep the whole balance, everything arrives. Both old and new predicates
//!      must pass. This is the control: a regression gate that only ever fails proves nothing.
//!   B. STRANDED RESERVE — the reviewer's case. Fund a wallet, abandon a `prepare_send` so the
//!      proofs sit in Reserved, then sweep what is left. The old drain predicate (spendable and
//!      pending only) and the old derived-fee conservation sum both PASS while most of the value is
//!      stranded. The corrected predicates must both FAIL.
//!
//! Worthless test ecash, loopback only. No real funds and no external payment.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cdk::nuts::nut00::ProofsMethods;
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_sqlite::wallet::memory;
use rand::random;

const MINT_URL: &str = "http://127.0.0.1:8085";

// ---------------------------------------------------------------------------------------------
// The predicates themselves, as pure functions, so the regression compares ORACLES and not prose.
// The `old_*` forms are the exact shapes that shipped and were faulted; they are kept here only so
// the difference can be measured, and are never used to judge the mint.
// ---------------------------------------------------------------------------------------------

/// Shipped and faulted: spendable and pending only. Reserved is a third, distinct pool
/// (cdk 0.17.2 `wallet/balance.rs:23-34`), so this cannot see value abandoned in reserve.
fn old_drained(spendable: u64, pending: u64) -> bool {
    spendable == 0 && pending == 0
}

/// Corrected: all three pools.
fn new_drained(spendable: u64, pending: u64, reserved: u64) -> bool {
    spendable == 0 && pending == 0 && reserved == 0
}

/// Shipped and faulted: the "fee" is whatever went missing (`before - locked`), so
/// `received + fee == before` holds for ANY shortfall, by construction.
fn old_conserved_derived_fee(before: u64, received: u64) -> bool {
    let derived_fee = before.saturating_sub(received);
    received + derived_fee == before
}

/// Corrected: the fee must be the one CDK QUOTED for the send.
fn new_conserved_quoted_fee(before: u64, received: u64, quoted_fee: u64) -> bool {
    received + quoted_fee == before
}

/// Shipped and faulted: any positive reclaim counts, so a partial reclaim passes.
fn old_recovery_complete(reclaimed: u64) -> bool {
    reclaimed > 0
}

/// Corrected: the whole stranded amount, and the funded total spendable again with nothing left in
/// either non-spendable pool.
fn new_recovery_complete(
    reclaimed: u64,
    stranded: u64,
    spendable: u64,
    funded: u64,
    pending: u64,
    reserved: u64,
) -> bool {
    reclaimed == stranded && spendable == funded && pending == 0 && reserved == 0
}

struct Report {
    rows: Vec<(String, bool)>,
}

impl Report {
    fn new() -> Self {
        Self { rows: Vec::new() }
    }

    /// `expected` is what this regression REQUIRES the oracle to say about this fixture.
    fn require(&mut self, what: &str, actual: bool, expected: bool, detail: &str) -> Result<()> {
        let ok = actual == expected;
        let verdict = if actual { "PASS" } else { "FAIL" };
        println!(
            "  {}  {what}\n        oracle says {verdict}, required {} — {detail}",
            if ok { "ok  " } else { "BAD " },
            if expected { "PASS" } else { "FAIL" },
        );
        self.rows.push((what.to_owned(), ok));
        if ok {
            Ok(())
        } else {
            Err(anyhow!("{what}: oracle said {verdict}, required {}", if expected { "PASS" } else { "FAIL" }))
        }
    }
}

async fn wallet() -> Result<Wallet> {
    let store = Arc::new(memory::empty().await?);
    Ok(Wallet::new(MINT_URL, CurrencyUnit::Sat, store, random::<[u8; 64]>(), None)?)
}

async fn issue(w: &Wallet, amount: u64) -> Result<u64> {
    let quote = w
        .mint_quote(PaymentMethod::BOLT11, Some(Amount::from(amount)), None, None)
        .await
        .context("mint_quote")?;
    let proofs = w
        .wait_and_mint_quote(quote, Default::default(), Default::default(), Duration::from_secs(30))
        .await
        .context("wait_and_mint_quote")?;
    Ok(u64::from(proofs.total_amount()?))
}

async fn pools(w: &Wallet) -> Result<(u64, u64, u64)> {
    Ok((
        u64::from(w.total_balance().await?),
        u64::from(w.total_pending_balance().await?),
        u64::from(w.total_reserved_balance().await?),
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("predicate-regression — do the corrected F4/F5 oracles actually discriminate?");
    println!("mint {MINT_URL} (fakewallet, worthless test ecash)\n");
    let mut r = Report::new();

    // The zero-fee contract every exact-value assertion here depends on, read from the MINT.
    let probe = wallet().await?;
    let keyset = probe.fetch_active_keyset().await?;
    if keyset.input_fee_ppk != 0 {
        bail!(
            "fixture precondition broken: active keyset charges input_fee_ppk {}; the exact-value \
             predicates below would be invalid, so refusing to report on them",
            keyset.input_fee_ppk
        );
    }
    println!("[precondition] active keyset {} charges input_fee_ppk 0\n", keyset.id);

    // ----------------------------------------------------------------- fixture A: healthy
    println!("[A] HEALTHY sweep — the control. Both oracles must pass.");
    let a = wallet().await?;
    let funded_a = issue(&a, 40).await?;
    let prep_a = a.prepare_send(Amount::from(funded_a), SendOptions::default()).await?;
    let quoted_a = u64::from(prep_a.fee());
    let locked_a = u64::from(prep_a.amount());
    let token_a = prep_a.confirm(None).await?.to_string();
    let sink_a = wallet().await?;
    let got_a = u64::from(sink_a.receive(&token_a, ReceiveOptions::default()).await?);
    let (sp_a, pe_a, rs_a) = pools(&a).await?;
    println!(
        "      funded {funded_a}, locked {locked_a}, quoted fee {quoted_a}, received {got_a}; \
         source pools spendable {sp_a} pending {pe_a} reserved {rs_a}"
    );
    r.require(
        "A: old drain oracle",
        old_drained(sp_a, pe_a),
        true,
        "nothing is actually stranded, so the weak oracle is right here too",
    )?;
    r.require(
        "A: corrected drain oracle",
        new_drained(sp_a, pe_a, rs_a),
        true,
        "all three pools empty",
    )?;
    r.require(
        "A: corrected quoted-fee conservation",
        new_conserved_quoted_fee(funded_a, got_a, quoted_a),
        true,
        "every sat arrived and the quoted fee was zero",
    )?;

    // ----------------------------------------------------------------- fixture B: stranded reserve
    // The reviewer's counterexample, built for real: abandon a prepare_send so its proofs stay in
    // Reserved, then sweep only what is still spendable.
    println!("\n[B] STRANDED RESERVE — the counterexample. Old oracles must pass, corrected must fail.");
    let b = wallet().await?;
    let funded_b = issue(&b, 43).await?;
    let abandoned = b.prepare_send(Amount::from(42), SendOptions::default()).await?;
    let abandoned_amount = u64::from(abandoned.amount());
    drop(abandoned); // never confirmed, never cancelled: the proofs stay reserved
    let (sp_b0, pe_b0, rs_b0) = pools(&b).await?;
    if rs_b0 == 0 {
        bail!(
            "fixture B did not strand anything (reserved 0 after abandoning a {abandoned_amount} \
             sat prepare_send); the counterexample would be vacuous, so refusing to report a pass"
        );
    }
    println!("      funded {funded_b}, abandoned prepare_send of {abandoned_amount}; pools now spendable {sp_b0} pending {pe_b0} reserved {rs_b0}");

    // Sweep what is left, exactly as the gate's sweep leg would.
    let (got_b, quoted_b) = if sp_b0 > 0 {
        let prep_b = b.prepare_send(Amount::from(sp_b0), SendOptions::default()).await?;
        let quoted = u64::from(prep_b.fee());
        let token_b = prep_b.confirm(None).await?.to_string();
        let sink_b = wallet().await?;
        (u64::from(sink_b.receive(&token_b, ReceiveOptions::default()).await?), quoted)
    } else {
        (0, 0)
    };
    let (sp_b, pe_b, rs_b) = pools(&b).await?;
    println!(
        "      swept {got_b} with quoted fee {quoted_b}; pools now spendable {sp_b} pending {pe_b} \
         reserved {rs_b} — {rs_b} sats stranded out of {funded_b}"
    );

    r.require(
        "B: old drain oracle accepts a wallet with value stranded in Reserved",
        old_drained(sp_b, pe_b),
        true,
        "this is the defect, reproduced: it reports fully drained",
    )?;
    r.require(
        "B: corrected drain oracle REJECTS it",
        new_drained(sp_b, pe_b, rs_b),
        false,
        "reserved is non-zero, so the wallet is not drained",
    )?;
    r.require(
        "B: old derived-fee conservation accepts the shortfall as a fee",
        old_conserved_derived_fee(funded_b, got_b),
        true,
        "this is the defect, reproduced: the missing value is relabelled a fee",
    )?;
    r.require(
        "B: corrected quoted-fee conservation REJECTS it",
        new_conserved_quoted_fee(funded_b, got_b, quoted_b),
        false,
        "CDK quoted no fee, so the shortfall has no explanation",
    )?;

    // ----------------------------------------------------------------- F4 recovery oracle
    // Arithmetic discrimination on the recovery oracles. Stated plainly: this does NOT force the
    // mint to perform a partial reclaim — the observed reclaim in the accepted run was complete.
    // What is checked is that the corrected oracle would reject a partial one, using the same real
    // magnitudes (12 stranded of 32 funded) the gate uses.
    println!("\n[C] RECOVERY oracles on a partial reclaim (arithmetic on the oracles, not a forced mint failure).");
    let (funded_c, stranded_c, partial_c) = (32u64, 12u64, 1u64);
    r.require(
        "C: old recovery oracle accepts reclaiming 1 of 12",
        old_recovery_complete(partial_c),
        true,
        "this is the defect: any positive reclaim passed",
    )?;
    r.require(
        "C: corrected recovery oracle REJECTS reclaiming 1 of 12",
        new_recovery_complete(partial_c, stranded_c, funded_c - stranded_c + partial_c, funded_c, 0, 0),
        false,
        "partial reclaim, so the funded total is not restored",
    )?;
    r.require(
        "C: corrected recovery oracle accepts a COMPLETE reclaim",
        new_recovery_complete(stranded_c, stranded_c, funded_c, funded_c, 0, 0),
        true,
        "whole stranded amount back, funded total spendable, both other pools empty",
    )?;

    let bad = r.rows.iter().filter(|(_, ok)| !ok).count();
    println!("\n=====================================================");
    if bad == 0 {
        println!("PASS — {} oracle requirements met across 3 fixtures", r.rows.len());
        println!("The corrected predicates reject a case the shipped ones accepted.");
    } else {
        println!("FAIL — {bad} of {} oracle requirements unmet", r.rows.len());
    }
    println!("=====================================================");
    if bad == 0 {
        Ok(())
    } else {
        bail!("{bad} oracle requirement(s) unmet")
    }
}
