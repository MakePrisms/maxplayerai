//! wallet-roundtrip — the smallest useful CDK 0.17.2 wallet program, against the sandbox-local
//! fakewallet mint. Worthless test ecash only.
//!
//! Compiled from source inside the job container by `cashu-toolchain-check`, offline. Its job is
//! twofold: show a specialist the shortest correct issue → send → receive → melt sequence at this
//! exact CDK version, and prove the image's Rust toolchain and dependency cache actually work.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use cdk::nuts::nut00::ProofsMethods;
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_sqlite::wallet::memory;
use rand::random;

const MINT_URL: &str = "http://127.0.0.1:8085";

async fn wallet() -> Result<Wallet> {
    let store = Arc::new(memory::empty().await?);
    Ok(Wallet::new(MINT_URL, CurrencyUnit::Sat, store, random::<[u8; 64]>(), None)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let alice = wallet().await?;
    let bob = wallet().await?;

    // 1. Mint info proves we are talking to a mint at all.
    let info = alice.fetch_mint_info().await?.ok_or_else(|| anyhow!("no mint info"))?;
    println!("mint: {:?}", info.version.map(|v| v.to_string()));

    // 2. Issue. Fakewallet auto-settles the quote, so this returns without any payment.
    let quote = alice.mint_quote(PaymentMethod::BOLT11, Some(Amount::from(32)), None, None).await?;
    let proofs = alice
        .wait_and_mint_quote(quote, Default::default(), Default::default(), Duration::from_secs(30))
        .await?;
    println!("issued: {} sats", u64::from(proofs.total_amount()?));

    // 3. Send. prepare_send reserves proofs; the debit is final only at confirm.
    let prepared = alice.prepare_send(Amount::from(8), SendOptions::default()).await?;
    let token = prepared.confirm(None).await?;
    println!("alice after send: {} sats", u64::from(alice.total_balance().await?));

    // 4. Receive into a different wallet.
    let got = bob.receive(&token.to_string(), ReceiveOptions::default()).await?;
    println!("bob received: {} sats", u64::from(got));

    // 5. Melt. The invoice is a fake one the mint's own backend will settle.
    //    A real specialist task would build this from a genuine BOLT11 string.
    let quote = bob
        .melt_quote(PaymentMethod::BOLT11, cdk_fake_invoice(4_000), None, None)
        .await?;
    let melt = bob.prepare_melt(&quote.id, HashMap::new()).await?;
    let done = melt.confirm().await?;
    println!(
        "bob melted: state={:?} amount={} fee={}",
        done.state(),
        u64::from(done.amount()),
        u64::from(done.fee_paid())
    );
    println!("bob final: {} sats", u64::from(bob.total_balance().await?));

    Ok(())
}

/// A fakewallet-settleable BOLT11 string, read from the environment so this example does not need
/// `cdk-fake-wallet` as a dependency. `cashu-toolchain-check` fills it in with
/// `test-mint invoice <msat>`.
fn cdk_fake_invoice(msat: u64) -> String {
    std::env::var("FAKE_INVOICE").unwrap_or_else(|_| {
        panic!("set FAKE_INVOICE to a fakewallet invoice for {msat} msat (see: test-mint invoice)")
    })
}
