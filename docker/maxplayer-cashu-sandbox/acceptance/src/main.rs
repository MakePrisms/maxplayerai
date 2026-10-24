//! mint-acceptance — the named acceptance gate for the sandbox-local fakewallet test mint.
//!
//! Every check asserts a NUMBER or an EXACT protocol outcome, never merely "the call returned".
//! A no-op pass is a failure here: if a leg cannot be run, the harness bails rather than printing
//! a green tick.
//!
//! Four things earlier versions got wrong, all found by independent review and all fixed here,
//! because they are the difference between coverage and the appearance of coverage:
//!
//!   * negative legs accepted ANY error. A transport failure is not proof that a double spend was
//!     rejected, nor that a payment did not happen. Every negative leg now matches the EXACT cdk
//!     error variant it claims (`Error::TokenAlreadySpent`, `Error::PaymentFailed`).
//!   * post-restart "no loss" was a local tautology: `total_balance` reads the wallet's own
//!     localstore (`wallet/balance.rs:10-20`), not mint state, so comparing it either side of a
//!     mint restart proves nothing about the mint. The restart leg now SPENDS THE WHOLE residual
//!     balance against the restarted mint and reconciles the received total exactly.
//!
//!   * conservation was checked against spendable and pending only. Reserved is a THIRD, distinct
//!     pool (`wallet/balance.rs:23-34`), so value abandoned in reserve was invisible to the very
//!     check that claimed nothing was stranded. Every drain assertion now covers all three.
//!   * the sweep derived its fee as `before - locked` — that is not a fee, it is whatever went
//!     missing, so any unexplained shortfall satisfied `swept + fees == total` by construction. The
//!     fee is now the one CDK QUOTED, the fixture's zero-fee keyset is asserted against the mint
//!     rather than trusted from config, and unrelated preparation errors are propagated instead of
//!     being swallowed by an amount search.
//!
//! Recovery is likewise exercised from a genuinely non-empty unresolved state, across a wallet
//! restart backed by a real sqlite file, not with in-memory stores that never reopen — and both
//! outcomes recovery may produce are held to the same complete-value contract, since a partial
//! reclaim satisfies `reclaimed > 0` and is still a loss.
//!
//! Worthless test ecash. This binary talks to http://127.0.0.1:8085 and nothing else.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cdk::nuts::nut00::ProofsMethods;
use cdk::nuts::{CurrencyUnit, MeltQuoteState, PaymentMethod};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::{Amount, Error as CdkError};
use cdk_fake_wallet::{create_fake_invoice, FakeInvoiceDescription};
use cdk_sqlite::wallet::{memory, WalletSqliteDatabase};
use rand::random;

const MINT_URL: &str = "http://127.0.0.1:8085";
const MINT_PORT: u16 = 8085;

struct Tally {
    checks: u32,
    legs: Vec<String>,
}

impl Tally {
    fn new() -> Self {
        Self { checks: 0, legs: Vec::new() }
    }

    fn check(&mut self, what: &str, ok: bool, detail: impl AsRef<str>) -> Result<()> {
        self.checks += 1;
        let detail = detail.as_ref();
        if ok {
            println!("  ok   {what}: {detail}");
            Ok(())
        } else {
            println!("  FAIL {what}: {detail}");
            Err(anyhow!("{what}: {detail}"))
        }
    }

    fn leg(&mut self, name: &str) {
        println!("\n[{}] {name}", self.legs.len() + 1);
        self.legs.push(name.to_owned());
    }
}

async fn memory_wallet() -> Result<Wallet> {
    let store = Arc::new(memory::empty().await?);
    Ok(Wallet::new(MINT_URL, CurrencyUnit::Sat, store, random::<[u8; 64]>(), None)?)
}

/// A wallet on a real sqlite FILE, so it can be dropped and reopened — a wallet restart.
async fn file_wallet(path: &str, seed: [u8; 64]) -> Result<Wallet> {
    let store = Arc::new(WalletSqliteDatabase::new(path).await?);
    Ok(Wallet::new(MINT_URL, CurrencyUnit::Sat, store, seed, None)?)
}

async fn balance(w: &Wallet) -> Result<u64> {
    Ok(u64::from(w.total_balance().await?))
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

/// One token carrying a wallet's whole spendable balance, with the fee CDK actually QUOTED for it.
struct SweptToken {
    token: String,
    /// Spendable balance immediately before the send.
    before: u64,
    /// Amount CDK locked into the token.
    locked: u64,
    /// Fee CDK QUOTED for the send — not a residual derived from what went missing.
    quoted_fee: u64,
}

/// Send the wallet's ENTIRE spendable balance in one operation. This is what makes the restart leg
/// meaningful: the mint has to honour every remaining proof, not just one sat of them.
///
/// The first version walked down from the full balance trying every smaller amount, swallowing each
/// error, and the caller then inferred `fee = before - locked`. Two defects, both found by review.
/// An inferred fee is simply whatever went missing, so ANY unexplained shortfall satisfied the
/// conservation sum by construction; and discarding every preparation error turned unrelated
/// failures into fee-search candidates. Concretely: a wallet holding 43 that sent 42 with 1 sat
/// stranded in Reserved would have had the missing sat relabelled a fee, and passed.
///
/// So — one preparation, for the whole balance, fee RETURNED rather than derived, every error
/// propagated. The active keyset on this fixture charges `input_fee_ppk = 0`, asserted against the
/// mint itself in the recovery leg, and that is what makes a full-balance send preparable at all.
/// A failure here is therefore a real finding and must not be searched around.
async fn send_full_balance(w: &Wallet) -> Result<Option<SweptToken>> {
    let before = balance(w).await?;
    if before == 0 {
        return Ok(None);
    }
    let prepared = w
        .prepare_send(Amount::from(before), SendOptions::default())
        .await
        .with_context(|| format!("prepare_send of the full {before} sat balance"))?;
    let locked = u64::from(prepared.amount());
    let quoted_fee = u64::from(prepared.fee());
    let token = prepared.confirm(None).await?.to_string();
    Ok(Some(SweptToken { token, before, locked, quoted_fee }))
}

/// True when this is the exact cdk error the leg claims. `matches!` on the variant, not a substring
/// of the message: an error message can change wording without changing meaning, and a substring
/// match would also accept an unrelated error that happens to contain the phrase.
fn is_already_spent(e: &CdkError) -> bool {
    matches!(e, CdkError::TokenAlreadySpent)
}

fn is_payment_failed(e: &CdkError) -> bool {
    matches!(e, CdkError::PaymentFailed)
}

/// The mint's own listening socket, read from the kernel rather than inferred from config.
/// /proc/net/tcp local_address for a loopback bind is `0100007F` (127.0.0.1, little-endian hex);
/// a wildcard bind would be `00000000`.
fn listener_local_addrs() -> Result<Vec<String>> {
    let mut found = Vec::new();
    let want_port = format!("{MINT_PORT:04X}");
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for line in text.lines().skip(1) {
            let mut cols = line.split_whitespace();
            let _sl = cols.next();
            let Some(local) = cols.next() else { continue };
            let Some(_rem) = cols.next() else { continue };
            let Some(state) = cols.next() else { continue };
            if state != "0A" {
                continue; // 0A = TCP_LISTEN
            }
            if let Some((addr, port)) = local.split_once(':') {
                if port.eq_ignore_ascii_case(&want_port) {
                    found.push(addr.to_owned());
                }
            }
        }
    }
    Ok(found)
}

/// This container's own routable address, without sending a packet.
fn container_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    sock.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback())
}

fn test_mint(args: &[&str]) -> Result<String> {
    let out = Command::new("test-mint").args(args).output().context("run test-mint")?;
    if !out.status.success() {
        bail!("test-mint {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("mint-acceptance — sandbox-local fakewallet mint at {MINT_URL}");
    println!("WORTHLESS TEST ECASH. No real funds, no external payment.\n");

    let mut t = Tally::new();

    // Private scratch for the file-backed wallet. Deliberately under the mint's state root, which
    // the controller guarantees is OUTSIDE the delivered workdir.
    let state_root =
        std::env::var("TEST_MINT_STATE_ROOT").unwrap_or_else(|_| "/var/lib/cashu-test-state".into());
    let wallet_dir = format!("{state_root}/acceptance-wallet");
    std::fs::create_dir_all(&wallet_dir)?;
    let wallet_db = format!("{wallet_dir}/recovery.sqlite");
    let _ = std::fs::remove_file(&wallet_db);

    // ------------------------------------------------------------------ 1. mint info
    t.leg("mint info (NUT-06)");
    let wallet = memory_wallet().await?;
    let info = wallet.fetch_mint_info().await?.ok_or_else(|| anyhow!("mint returned no info"))?;
    let version = info.version.as_ref().map(|v| v.to_string()).unwrap_or_default();
    t.check("mint version is exactly cdk-mintd/0.17.2", version == "cdk-mintd/0.17.2", &version)?;
    t.check(
        "mint advertises NUT-04 mint support",
        !info.nuts.nut04.methods.is_empty(),
        format!("{} method(s)", info.nuts.nut04.methods.len()),
    )?;

    // ------------------------------------------------------------------ 2. issue
    t.leg("mint quote + issue");
    const ISSUE: u64 = 64;
    let minted = issue(&wallet, ISSUE).await?;
    t.check("proofs minted", minted == ISSUE, format!("{minted} sats (wanted {ISSUE})"))?;
    let after_issue = balance(&wallet).await?;
    t.check("wallet balance after issue", after_issue == ISSUE, format!("{after_issue} sats"))?;

    // ------------------------------------------------------------------ 3. send / receive
    t.leg("send + receive across wallets");
    const SEND: u64 = 21;
    let prepared = wallet.prepare_send(Amount::from(SEND), SendOptions::default()).await?;
    let send_fee = u64::from(prepared.fee());
    let token_str = prepared.confirm(None).await?.to_string();
    t.check(
        "token is a cashu token",
        token_str.starts_with("cashu"),
        format!("{} chars", token_str.len()),
    )?;
    let sender_after = balance(&wallet).await?;
    let expect_sender = ISSUE - SEND - send_fee;
    t.check(
        "sender debited exactly amount+fee",
        sender_after == expect_sender,
        format!("{sender_after} sats (issued {ISSUE} - sent {SEND} - fee {send_fee})"),
    )?;

    let receiver = memory_wallet().await?;
    let received = u64::from(receiver.receive(&token_str, ReceiveOptions::default()).await?);
    t.check("receiver credited", received == SEND, format!("{received} sats (wanted {SEND})"))?;

    // ------------------------------------------------------------------ 4. double spend
    t.leg("double spend rejected with the exact protocol error");
    let thief = memory_wallet().await?;
    match thief.receive(&token_str, ReceiveOptions::default()).await {
        Ok(a) => t.check(
            "second receive of the same token",
            false,
            format!("ACCEPTED {} sats — double spend!", u64::from(a)),
        )?,
        Err(e) => t.check(
            "second receive fails with Error::TokenAlreadySpent",
            is_already_spent(&e),
            format!("{e:?}"),
        )?,
    }
    let thief_bal = balance(&thief).await?;
    t.check("double-spender balance stays zero", thief_bal == 0, format!("{thief_bal} sats"))?;

    // ------------------------------------------------------------------ 5. melt
    t.leg("melt");
    const MELT: u64 = 5;
    let good_invoice =
        create_fake_invoice(MELT * 1_000, serde_json::to_string(&FakeInvoiceDescription::default())?);
    let before_melt = balance(&receiver).await?;
    let melt_quote = receiver
        .melt_quote(PaymentMethod::BOLT11, good_invoice.to_string(), None, None)
        .await?;
    let prepared_melt = receiver.prepare_melt(&melt_quote.id, HashMap::new()).await?;
    let melt_amount = u64::from(prepared_melt.amount());
    let confirmed = prepared_melt.confirm().await?;
    let fee_paid = u64::from(confirmed.fee_paid());
    let after_melt = balance(&receiver).await?;
    t.check("melt amount", melt_amount == MELT, format!("{melt_amount} sats"))?;
    t.check(
        "melt finalised Paid",
        format!("{:?}", confirmed.state()).eq_ignore_ascii_case("Paid"),
        format!("{:?}", confirmed.state()),
    )?;
    t.check(
        "balance fell by exactly amount+fee",
        after_melt == before_melt - MELT - fee_paid,
        format!("{before_melt} -> {after_melt} (melt {MELT}, fee {fee_paid})"),
    )?;

    // ------------------------------------------------------------------ 6. failed payment
    // cdk-fake-wallet reads the invoice DESCRIPTION as a FakeInvoiceDescription. `pay_err` alone is
    // NOT enough: the backend records `check_payment_state` into its payment-states map BEFORE it
    // honours pay_err (cdk-fake-wallet 0.17.2 src/lib.rs:706-714), so with the struct's Paid
    // default the mint's follow-up status check reports PAID and the melt finalises Paid for an
    // invoice the backend refused. Both states must say Unpaid.
    t.leg("failed payment: exact error, terminal quote state, nothing lost");
    let fail_desc = FakeInvoiceDescription {
        pay_invoice_state: MeltQuoteState::Unpaid,
        check_payment_state: MeltQuoteState::Unpaid,
        pay_err: true,
        check_err: false,
    };
    let bad_invoice = create_fake_invoice(MELT * 1_000, serde_json::to_string(&fail_desc)?);
    let before_fail = balance(&receiver).await?;
    let fail_quote = receiver
        .melt_quote(PaymentMethod::BOLT11, bad_invoice.to_string(), None, None)
        .await?;
    let fail_quote_id = fail_quote.id.clone();
    let prepared_fail = receiver.prepare_melt(&fail_quote_id, HashMap::new()).await?;
    match prepared_fail.confirm().await {
        Ok(f) => t.check(
            "melt against a refusing backend",
            false,
            format!("SUCCEEDED state={:?} amount={}", f.state(), u64::from(f.amount())),
        )?,
        Err(e) => t.check(
            "melt fails with Error::PaymentFailed",
            is_payment_failed(&e),
            format!("{e:?}"),
        )?,
    }
    // Reconcile the QUOTE's terminal state with the mint, not just the local error.
    let quote_state = receiver.check_melt_quote_status(&fail_quote_id).await?;
    let quote_state_name = format!("{:?}", quote_state.state);
    t.check(
        "mint reports the failed quote as Unpaid",
        quote_state.state == MeltQuoteState::Unpaid,
        &quote_state_name,
    )?;
    let after_fail = balance(&receiver).await?;
    let pending_after_fail = u64::from(receiver.total_pending_balance().await?);
    let reserved_after_fail = u64::from(receiver.total_reserved_balance().await?);
    t.check(
        "no test ecash lost to the failed payment",
        after_fail == before_fail,
        format!("{before_fail} -> {after_fail} sats"),
    )?;
    t.check(
        "nothing left stranded pending or reserved",
        pending_after_fail == 0 && reserved_after_fail == 0,
        format!("pending {pending_after_fail}, reserved {reserved_after_fail}"),
    )?;

    // ------------------------------------------------------------------ 7. recovery
    // A genuinely non-empty unresolved state, across a WALLET-OBJECT AND STORE REOPEN. `confirm()`
    // on a send leaves the saga in TokenCreated with the proofs committed to a token nobody has
    // redeemed; dropping the wallet and reopening the same sqlite FILE is the reopen. Then recovery
    // must find it AND give the whole funded value back.
    //
    // Scope, stated exactly so this is not read as more than it is: the wallet object is dropped and
    // the same sqlite file is reopened IN THE SAME PROCESS, with the seed held in memory. This is
    // not an OS-level process crash, not a seed-only restore with no local database, not an
    // interrupted melt, and not a crash injected at every intermediate saga stage.
    //
    // The oracle is complete expected VALUE, not "an error did not happen" and not `reclaimed > 0`:
    // a partial reclaim satisfies a positivity check and is still a loss. Both outcomes recovery is
    // allowed to produce — driven forward, or compensated — must land on the same full total with
    // pending AND reserved both empty.
    t.leg("recovery of an unresolved send across a wallet-object/store reopen");
    let seed: [u8; 64] = random();
    const RECOVER_FUND: u64 = 32;
    const RECOVER_SEND: u64 = 12;
    let stranded_amount;
    {
        let w = file_wallet(&wallet_db, seed).await?;
        // The zero-fee contract this leg and the sweep leg both rely on, read from the MINT's active
        // keyset rather than trusted from test-mint.config.toml. If the fixture ever sets a non-zero
        // input fee, every exact-value assertion below becomes wrong, so refuse to run rather than
        // reinterpret the difference as an acceptable fee.
        let keyset = w.fetch_active_keyset().await?;
        t.check(
            "fixture precondition: active keyset charges no input fee",
            keyset.input_fee_ppk == 0,
            format!("input_fee_ppk {} on keyset {}", keyset.input_fee_ppk, keyset.id),
        )?;
        let funded = issue(&w, RECOVER_FUND).await?;
        t.check("recovery wallet funded", funded == RECOVER_FUND, format!("{funded} sats"))?;
        let prep = w.prepare_send(Amount::from(RECOVER_SEND), SendOptions::default()).await?;
        let prep_fee = u64::from(prep.fee());
        t.check(
            "CDK quotes a zero fee for this send, as the keyset implies",
            prep_fee == 0,
            format!("quoted fee {prep_fee} sats"),
        )?;
        stranded_amount = u64::from(prep.amount());
        // Token created and deliberately NEVER redeemed: this is the unresolved state.
        let _token = prep.confirm(None).await?;
        let pending_sends = w.get_pending_sends().await?;
        t.check(
            "an unresolved send exists before restart",
            pending_sends.len() == 1,
            format!("{} pending send(s), {stranded_amount} sats locked", pending_sends.len()),
        )?;
        // w drops here: wallet process gone, sqlite file left as it was.
    }

    let w2 = file_wallet(&wallet_db, seed).await?;
    let pending_after_restart = w2.get_pending_sends().await?;
    t.check(
        "the unresolved send survives the wallet restart",
        pending_after_restart.len() == 1,
        format!("{} pending send(s) after reopen", pending_after_restart.len()),
    )?;
    let report = w2.recover_incomplete_sagas().await?;
    t.check(
        "recovery reports a non-empty result",
        !report.is_empty(),
        format!(
            "recovered {}, compensated {}, skipped {}, failed {}",
            report.recovered, report.compensated, report.skipped, report.failed
        ),
    )?;
    t.check("recovery failed nothing", report.failed == 0, format!("failed {}", report.failed))?;

    // Reclaim it explicitly. revoke_send swaps the proofs back — this is the call that actually
    // returns value to the spendable set, which is what `check_all_pending_proofs` does NOT do.
    //
    // Whichever branch runs, the SAME closing contract is asserted below: full funded value back,
    // nothing pending, nothing reserved. Neither branch is allowed a weaker oracle than the other.
    let still_pending = w2.get_pending_sends().await?;
    let balance_before_revoke = balance(&w2).await?;
    let recovery_route;
    if let Some(op) = still_pending.first().copied() {
        let reclaimed = u64::from(w2.revoke_send(op).await?);
        let balance_after_revoke = balance(&w2).await?;
        recovery_route = "revoke_send";
        t.check(
            "revoke_send reclaims the WHOLE stranded amount, not merely something",
            reclaimed == stranded_amount,
            format!("reclaimed {reclaimed} sats, {stranded_amount} was locked"),
        )?;
        t.check(
            "spendable balance grows by exactly the reclaimed amount",
            balance_after_revoke == balance_before_revoke + reclaimed,
            format!("{balance_before_revoke} -> {balance_after_revoke} (+{reclaimed})"),
        )?;
        t.check(
            "no unresolved sends remain",
            w2.get_pending_sends().await?.is_empty(),
            format!("{} left", w2.get_pending_sends().await?.len()),
        )?;
    } else {
        // Recovery compensated it by itself: assert THAT outcome, held to the same value contract.
        recovery_route = "compensated by recover_incomplete_sagas";
        t.check(
            "recovery compensated the send without an explicit revoke",
            report.compensated >= 1,
            format!("compensated {}", report.compensated),
        )?;
        t.check(
            "compensation alone already restored the stranded amount",
            balance_before_revoke == RECOVER_FUND,
            format!("spendable {balance_before_revoke}, funded {RECOVER_FUND}"),
        )?;
    }

    // The closing value contract, identical for both routes. input_fee_ppk is 0 on this fixture, so
    // an issue + send + reclaim cycle is exactly value-preserving; that zero is ASSERTED from the
    // mint's own active keyset above, not assumed from the config file, so if the fixture ever gains
    // an input fee this leg fails loudly instead of quietly tolerating a shortfall.
    let recovered_spendable = balance(&w2).await?;
    let recovered_pending = u64::from(w2.total_pending_balance().await?);
    let recovered_reserved = u64::from(w2.total_reserved_balance().await?);
    t.check(
        "the complete funded value is spendable again",
        recovered_spendable == RECOVER_FUND,
        format!("{recovered_spendable} sats spendable, funded {RECOVER_FUND} (via {recovery_route})"),
    )?;
    t.check(
        "nothing is left pending or RESERVED after recovery",
        recovered_pending == 0 && recovered_reserved == 0,
        format!("pending {recovered_pending}, reserved {recovered_reserved}"),
    )?;
    // What check_all_pending_proofs ACTUALLY does, asserted rather than described: it returns the
    // total of orphaned proofs still pending at the mint, and removes the spent ones. It does not
    // move survivors back to Unspent (cdk 0.17.2 src/wallet/proofs.rs:121-180).
    let still_pending_amount = u64::from(w2.check_all_pending_proofs().await?);
    t.check(
        "check_all_pending_proofs returns the residual pending total",
        still_pending_amount == u64::from(w2.total_pending_balance().await?),
        format!(
            "returned {still_pending_amount}, wallet pending {}",
            u64::from(w2.total_pending_balance().await?)
        ),
    )?;

    // ------------------------------------------------------------------ 8. mint restart
    t.leg("mint restart: full residual value still spendable, nothing duplicated");
    let sender_pre = balance(&wallet).await?;
    let receiver_pre = balance(&receiver).await?;
    let recovery_pre = balance(&w2).await?;
    let total_pre = sender_pre + receiver_pre + recovery_pre;
    t.check(
        "there is real residual value to test with",
        total_pre > 0,
        format!("sender {sender_pre} + receiver {receiver_pre} + recovery {recovery_pre} = {total_pre}"),
    )?;
    println!("  {}", test_mint(&["restart"])?.replace('\n', " | "));

    // Now prove it against the MINT: sweep every wallet's whole balance into a fresh wallet. Local
    // totals cannot lie about this, because the mint has to sign every swap.
    //
    // Every fee below is the fee CDK QUOTED for that send, and on this fixture it must be zero — the
    // active keyset's `input_fee_ppk` was asserted to be 0 against the mint in the recovery leg, so
    // this is a full-value contract, not a tolerance. Nothing here is permitted to explain a
    // shortfall as a fee after the fact: that is precisely how the previous version could have
    // called a sat stranded in Reserved a fee and passed.
    let sink = memory_wallet().await?;
    let mut swept = 0u64;
    let mut quoted_fees = 0u64;
    for (name, w) in [("sender", &wallet), ("receiver", &receiver), ("recovery", &w2)] {
        let before = balance(w).await?;
        match send_full_balance(w).await? {
            Some(s) => {
                t.check(
                    &format!("{name}: CDK quoted a zero fee, so the whole balance is sendable"),
                    s.quoted_fee == 0 && s.locked == s.before,
                    format!("locked {} of {} sats, quoted fee {}", s.locked, s.before, s.quoted_fee),
                )?;
                let got = u64::from(sink.receive(&s.token, ReceiveOptions::default()).await?);
                t.check(
                    &format!("{name}: mint honoured the swept token after restart"),
                    got == s.locked,
                    format!("swept {got} of {} sats", s.before),
                )?;
                // Per-wallet completeness, with the fee taken from the quote rather than the gap.
                t.check(
                    &format!("{name}: pre-sweep value == received + quoted fee"),
                    s.before == got + s.quoted_fee,
                    format!("{} == {got} + {}", s.before, s.quoted_fee),
                )?;
                swept += got;
                quoted_fees += s.quoted_fee;
            }
            None => t.check(
                &format!("{name}: nothing to sweep"),
                before == 0,
                format!("{before} sats but no sendable amount"),
            )?,
        }
    }
    let sink_balance = balance(&sink).await?;
    t.check(
        "swept value reconciles exactly with the pre-restart total",
        sink_balance == swept && swept + quoted_fees == total_pre,
        format!(
            "sink {sink_balance} = swept {swept}; swept + quoted fees {quoted_fees} = {total_pre} pre-restart"
        ),
    )?;
    t.check(
        "the whole pre-restart total arrived, no fee was charged at all",
        quoted_fees == 0 && sink_balance == total_pre,
        format!("sink {sink_balance} of {total_pre} pre-restart, quoted fees {quoted_fees}"),
    )?;
    // Spendable, pending AND RESERVED. Omitting reserved was the hole: an abandoned prepare_send
    // leaves value in a pool that neither of the other two queries can see (cdk 0.17.2
    // wallet/balance.rs:23-34 — pending and reserved are distinct proof sets).
    for (name, w) in [("sender", &wallet), ("receiver", &receiver), ("recovery", &w2)] {
        let left = balance(w).await?;
        let pending = u64::from(w.total_pending_balance().await?);
        let reserved = u64::from(w.total_reserved_balance().await?);
        t.check(
            &format!("{name} fully drained — spendable, pending and reserved all zero"),
            left == 0 && pending == 0 && reserved == 0,
            format!("spendable {left}, pending {pending}, reserved {reserved}"),
        )?;
    }
    // No duplicate credit from a rolled-back database: the token spent before the restart is still
    // burned, with the exact protocol error.
    match sink.receive(&token_str, ReceiveOptions::default()).await {
        Ok(a) => t.check(
            "pre-restart spent token",
            false,
            format!("ACCEPTED {} sats after restart — duplicate credit!", u64::from(a)),
        )?,
        Err(e) => t.check(
            "pre-restart spent token still fails with Error::TokenAlreadySpent",
            is_already_spent(&e),
            format!("{e:?}"),
        )?,
    }

    // ------------------------------------------------------------------ 9. loopback only
    t.leg("loopback-only reachability");
    let addrs = listener_local_addrs()?;
    t.check("a listener exists on 8085", !addrs.is_empty(), format!("{addrs:?}"))?;
    let all_loopback = addrs.iter().all(|a| {
        a.eq_ignore_ascii_case("0100007F")
            || a.eq_ignore_ascii_case("00000000000000000000000001000000")
    });
    t.check(
        "every 8085 listener is bound to loopback",
        all_loopback,
        format!("/proc/net/tcp local_address {addrs:?}"),
    )?;
    match container_ip() {
        Some(ip) => {
            let target = SocketAddr::new(ip, MINT_PORT);
            let reachable = TcpStream::connect_timeout(&target, Duration::from_millis(750)).is_ok();
            t.check(
                "mint NOT reachable on the container's own routable address",
                !reachable,
                format!("{target} refused"),
            )?;
        }
        None => bail!("could not determine a non-loopback container address; exclusion UNPROVEN"),
    }

    // ------------------------------------------------------------------ 10. state isolation
    // The mint's private state must not be inside the delivered workdir (advisor F3). The
    // controller owns this check; the gate runs it so a single command covers it.
    t.leg("private test state is outside the delivered workdir");
    let isolation = test_mint(&["isolation"])?;
    for line in isolation.lines() {
        println!("  | {line}");
    }
    t.check(
        "test-mint isolation reports state outside the delivery dir",
        isolation.contains("state inside delivered dir: no"),
        "see the lines above",
    )?;
    t.check(
        "no mint state names appear in the delivered workdir",
        !isolation.contains("FOUND:"),
        "see the lines above",
    )?;

    println!("\n=====================================================");
    println!("PASS — {} assertions across {} legs", t.checks, t.legs.len());
    for (i, leg) in t.legs.iter().enumerate() {
        println!("  {}. {leg}", i + 1);
    }
    println!("mint: {MINT_URL} (fakewallet, worthless test ecash)");
    println!("=====================================================");
    Ok(())
}
