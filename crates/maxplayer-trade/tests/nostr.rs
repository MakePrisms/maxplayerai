//! `nostr://` mints end to end: the real Maxplayer credits sidecar (`crates/maxplayer-mint`) served
//! in-process on a local relay, driven through the trade CLI's own coordinator and money code.
//! No real mints, wallets or relays. Each test asserts its SAFETY property before return values.
//!
//! A write-policy tap on the mint relay holds the mints' keys, so it can read (never forge) the
//! encrypted 23410/23411 traffic: it counts identical re-sends, drops or delays one swap reply,
//! and flags any market event on the mint relay or mint event on the market relay.
mod support;
use cdk::{
    Mint,
    amount::SplitTarget,
    dhke::construct_proofs,
    nuts::{PreMintSecrets, Proofs, SecretKey},
};
use maxplayer_core::mint_wire::{REQUEST_KIND, RESPONSE_KIND, Request};
use maxplayer_mint::{backend, home::mint_url, issue, server::Server};
use maxplayer_trade::{
    Asset, LOT, Leg, STATUS, TRADE,
    coordinator::{self, Swap},
    journal::Journal,
    market::Market,
    mint, money, receive, transport, wallet,
};
use nostr_relay_builder::{
    LocalRelay, RelayBuilder,
    builder::{PolicyResult, RateLimit, WritePolicy},
};
use nostr_sdk::nips::nip44;
use nostr_sdk::prelude::{Event, EventId, Keys};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};
use support::{MintFixture, balance};

/// The mint-relay list is process-global (one CLI invocation, one list); serialize these tests.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone)]
struct Seen {
    op: String,
    exp: u64,
    /// `exp - created_at`: the connector window this request was published with.
    window: u64,
    count: u32,
}
#[derive(Debug, Default)]
struct Tap {
    mints: Mutex<Vec<Keys>>,
    requests: Mutex<HashMap<EventId, Seen>>,
    lose_first_swap_reply: AtomicBool,
    lost: Mutex<HashSet<EventId>>,
    delay_next_swap_reply_ms: AtomicU64,
    delivered_swap_replies: AtomicU64,
    misrouted: AtomicU64,
}
impl Tap {
    async fn admit(&self, event: &Event, mint_relay: bool) -> PolicyResult {
        let kind = event.kind.as_u16();
        if !mint_relay {
            if kind == REQUEST_KIND || kind == RESPONSE_KIND {
                self.misrouted.fetch_add(1, SeqCst);
            }
            return PolicyResult::Accept;
        }
        if [LOT, STATUS, TRADE].contains(&kind) {
            self.misrouted.fetch_add(1, SeqCst);
        }
        if kind == REQUEST_KIND {
            let mints = self.mints.lock().unwrap().clone();
            if let Some(k) = mints
                .iter()
                .find(|k| event.tags.public_keys().any(|p| *p == k.public_key()))
            {
                if let Ok(plain) = nip44::decrypt(k.secret_key(), &event.pubkey, &event.content) {
                    if let Ok(r) = serde_json::from_str::<Request>(&plain) {
                        self.requests
                            .lock()
                            .unwrap()
                            .entry(event.id)
                            .or_insert(Seen {
                                op: r.op,
                                exp: r.exp,
                                window: r.exp.saturating_sub(event.created_at.as_secs()),
                                count: 0,
                            })
                            .count += 1;
                    }
                }
            }
        } else if kind == RESPONSE_KIND {
            let swap = event.tags.event_ids().find_map(|id| {
                self.requests
                    .lock()
                    .unwrap()
                    .get(id)
                    .is_some_and(|s| s.op == "swap")
                    .then_some(*id)
            });
            if let Some(request) = swap {
                if self.lose_first_swap_reply.load(SeqCst)
                    && self.lost.lock().unwrap().insert(request)
                {
                    return PolicyResult::Reject("tap: reply lost".into());
                }
                let delay = self.delay_next_swap_reply_ms.swap(0, SeqCst);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                self.delivered_swap_replies.fetch_add(1, SeqCst);
            }
        }
        PolicyResult::Accept
    }
    fn swaps(&self) -> Vec<Seen> {
        self.requests
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.op == "swap")
            .cloned()
            .collect()
    }
    fn total_requests(&self) -> u32 {
        self.requests
            .lock()
            .unwrap()
            .values()
            .map(|s| s.count)
            .sum()
    }
    fn reset(&self) {
        self.requests.lock().unwrap().clear();
        self.lost.lock().unwrap().clear();
    }
}
#[derive(Debug, Clone)]
struct Policy(Arc<Tap>, bool);
impl WritePolicy for Policy {
    fn admit_event<'a>(
        &'a self,
        event: &'a Event,
        _addr: &'a SocketAddr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PolicyResult> + Send + 'a>> {
        Box::pin(async move { self.0.admit(event, self.1).await })
    }
}

struct NostrMint {
    mint: Mint,
    keys: Keys,
    url: String,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl NostrMint {
    /// The sidecar stops answering; relays stay up (a black hole, the slowest failure).
    #[cfg(feature = "lab")]
    fn stop(&self) {
        self.task.abort();
    }
}
impl Drop for NostrMint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Env {
    tap: Arc<Tap>,
    mint_relay_url: String,
    maker: PathBuf,
    taker: PathBuf,
    jm: Journal,
    jt: Journal,
    mm: Market,
    mt: Market,
    _mint_relay: LocalRelay,
    _market_relay: LocalRelay,
    root: tempfile::TempDir,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}
async fn env() -> Env {
    let serial = SERIAL.lock().await;
    let tap = Arc::new(Tap::default());
    let mint_relay = LocalRelay::new(
        RelayBuilder::default()
            .rate_limit(RateLimit {
                max_reqs: 10_000,
                notes_per_minute: 1_000_000,
            })
            .write_policy(Policy(tap.clone(), true)),
    );
    mint_relay.run().await.unwrap();
    let mint_relay_url = mint_relay.url().await.to_string();
    transport::configure_mint_relays(std::slice::from_ref(&mint_relay_url)).unwrap();
    let market_relay =
        LocalRelay::new(RelayBuilder::default().write_policy(Policy(tap.clone(), false)));
    market_relay.run().await.unwrap();
    let market_url = market_relay.url().await.to_string();
    let root = tempfile::tempdir().unwrap();
    let maker = root.path().join("maker");
    let taker = root.path().join("taker");
    std::fs::create_dir_all(&maker).unwrap();
    std::fs::create_dir_all(&taker).unwrap();
    let mm = Market::connect(
        wallet::identity(&maker).unwrap(),
        std::slice::from_ref(&market_url),
    )
    .await
    .unwrap();
    let mt = Market::connect(
        wallet::identity(&taker).unwrap(),
        std::slice::from_ref(&market_url),
    )
    .await
    .unwrap();
    Env {
        tap,
        mint_relay_url,
        jm: Journal::open(&maker).await.unwrap(),
        jt: Journal::open(&taker).await.unwrap(),
        maker,
        taker,
        mm,
        mt,
        _mint_relay: mint_relay,
        _market_relay: market_relay,
        root,
        _serial: serial,
    }
}
impl Env {
    async fn nostr_mint(&self) -> NostrMint {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::generate();
        let url = mint_url(&keys).unwrap();
        let mut seed = [0u8; 64];
        seed[..32].copy_from_slice(&SecretKey::generate().to_secret_bytes());
        seed[32..].copy_from_slice(&SecretKey::generate().to_secret_bytes());
        let mint = backend::open(&dir.path().join("mint.sqlite"), &seed, &url)
            .await
            .unwrap();
        self.tap.mints.lock().unwrap().push(keys.clone());
        let server = Server::connect(
            mint.clone(),
            keys.clone(),
            std::slice::from_ref(&self.mint_relay_url),
            10_000,
        )
        .await
        .unwrap();
        let task = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        NostrMint {
            mint,
            keys,
            url,
            task,
            _dir: dir,
        }
    }
    /// Operator `issue` into a token file, then the trade CLI's own `receive` (library entry
    /// point) imports it into the home over the mint relay.
    async fn fund_nostr(&self, home: &Path, m: &NostrMint, amount: u64) {
        let dir = tempfile::tempdir().unwrap();
        let issued = issue::issue(&m.mint, dir.path(), &m.url, amount)
            .await
            .unwrap();
        let token = std::fs::read_to_string(issued.file).unwrap();
        let j = if home == self.maker {
            &self.jm
        } else {
            &self.jt
        };
        let r = receive::receive(home, j, &m.url, &token).await.unwrap();
        assert_eq!(r.state, receive::ReceiveState::Done, "{}", r.summary());
        assert_eq!(balance(home, &m.url).await, amount, "fee-free sidecar");
    }
    /// The same through the built `maxplayer-trade receive --token-file` binary.
    async fn fund_nostr_cli(&self, home: &Path, m: &NostrMint, amount: u64) {
        let dir = tempfile::tempdir().unwrap();
        let issued = issue::issue(&m.mint, dir.path(), &m.url, amount)
            .await
            .unwrap();
        let hex_input = format!("nostr://{}", m.keys.public_key().to_hex());
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
            .arg("--home")
            .arg(home)
            .args(["--mint-relay", &self.mint_relay_url, "receive", &hex_input])
            .arg("--token-file")
            .arg(&issued.file)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(summary["mint"], m.url.as_str(), "canonical nostr mint");
        assert_eq!(summary["credited"], amount);
        assert_eq!(balance(home, &m.url).await, amount);
    }
    async fn list(&self, give: &str, give_net: u64, want: &str, want_net: u64) -> String {
        coordinator::list(
            &self.maker,
            &self.jm,
            &self.mm,
            Leg {
                asset: Asset::new(give).unwrap(),
                net: give_net,
            },
            Leg {
                asset: Asset::new(want).unwrap(),
                net: want_net,
            },
            16,
        )
        .await
        .unwrap()
    }
    async fn take(&self, lot: &str, max_give: u64, min_receive: u64) -> String {
        coordinator::start_take(
            &self.taker,
            &self.jt,
            &self.mt,
            lot,
            max_give,
            min_receive,
            16,
        )
        .await
        .unwrap()
    }
    async fn step(&mut self, maker: bool) {
        let (home, j, m) = if maker {
            (&self.maker, &self.jm, &mut self.mm)
        } else {
            (&self.taker, &self.jt, &mut self.mt)
        };
        if let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(200), m.inbox.recv()).await
        {
            if let Err(error) = coordinator::handle(home, j, m, &e).await {
                eprintln!("nostr fixture maker={maker}: {error:#}");
            }
        }
    }
    async fn state(&self, maker: bool, id: &str) -> Option<String> {
        let j = if maker { &self.jm } else { &self.jt };
        j.get::<Swap>("swap", id).await.unwrap().map(|s| s.state)
    }
    #[cfg(feature = "lab")]
    async fn step_until(&mut self, maker: bool, id: &str, target: &str) {
        for _ in 0..50 {
            self.step(maker).await;
            if self.state(maker, id).await.as_deref() == Some(target) {
                return;
            }
        }
        panic!("maker={maker} never reached {target}");
    }
    async fn pump(&mut self, id: &str, target: &str) {
        let mut next_recovery = Instant::now() + Duration::from_secs(3);
        for _ in 0..200 {
            self.step(true).await;
            self.step(false).await;
            if self.state(false, id).await.as_deref() == Some(target) {
                return;
            }
            if Instant::now() >= next_recovery {
                coordinator::recover(&self.maker, &self.jm, &self.mm)
                    .await
                    .unwrap();
                coordinator::recover(&self.taker, &self.jt, &self.mt)
                    .await
                    .unwrap();
                next_recovery = Instant::now() + Duration::from_secs(3);
            }
        }
        panic!(
            "target {target}: {:?} {:?}",
            self.state(true, id).await,
            self.state(false, id).await
        )
    }
}
const TERMINAL: [&str; 6] = [
    "complete",
    "complete_unclaimed",
    "refunded",
    "expired",
    "refund_quarantined",
    "claim_quarantined",
];
impl Env {
    /// Pump until BOTH sides are terminal (any terminal state), returning (maker, taker).
    async fn pump_terminal(&mut self, id: &str) -> (String, String) {
        let mut next_recovery = Instant::now() + Duration::from_secs(3);
        for _ in 0..400 {
            self.step(true).await;
            self.step(false).await;
            let (m, t) = (
                self.state(true, id).await.unwrap_or_default(),
                self.state(false, id).await.unwrap_or_default(),
            );
            if TERMINAL.contains(&m.as_str()) && TERMINAL.contains(&t.as_str()) {
                return (m, t);
            }
            if Instant::now() >= next_recovery {
                coordinator::recover(&self.maker, &self.jm, &self.mm)
                    .await
                    .unwrap();
                coordinator::recover(&self.taker, &self.jt, &self.mt)
                    .await
                    .unwrap();
                next_recovery = Instant::now() + Duration::from_secs(3);
            }
        }
        panic!(
            "never terminal: {:?} {:?}",
            self.state(true, id).await,
            self.state(false, id).await
        )
    }
}
fn invoice(n: u64) -> String {
    cdk_fake_wallet::create_fake_invoice(n * 1000, String::new()).to_string()
}
#[cfg(feature = "lab")]
async fn until(t: u64) {
    while coordinator::now() <= t {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
/// `net` locked on `m` by the maker home to a fresh hash; returns (proofs, receiver key, preimage).
async fn locked(e: &Env, m: &NostrMint, net: u64) -> (Proofs, String, String) {
    let p = mint::plan(&e.maker, &m.url, net, 16).await.unwrap();
    let rid = uuid::Uuid::new_v4().to_string();
    mint::reserve(&e.maker, &p, &rid).await.unwrap();
    let preimage = hex::encode(SecretKey::generate().to_secret_bytes());
    let hash = hex::encode(Sha256::digest(hex::decode(&preimage).unwrap()));
    let recv = SecretKey::generate();
    let refund = SecretKey::generate();
    let c = mint::conditions(
        &hash,
        &recv.public_key().to_string(),
        &refund.public_key().to_string(),
        coordinator::now() + 3600,
    )
    .unwrap();
    let proofs = mint::lock(&e.maker, &e.jm, &rid, &p, &c, coordinator::now() + 300)
        .await
        .unwrap();
    (proofs, hex::encode(recv.to_secret_bytes()), preimage)
}
async fn attempt(j: &Journal, id: &str) -> serde_json::Value {
    j.get::<serde_json::Value>("attempt", id)
        .await
        .unwrap()
        .expect("attempt journaled")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_issue_receive_then_https_trade_completes_on_mint_relays_only() {
    let mut e = env().await;
    let n = e.nostr_mint().await;
    let b = MintFixture::start(0).await;
    // issue -> `maxplayer-trade receive` (CLI) -> trade nostr<->https.
    e.fund_nostr_cli(&e.maker.clone(), &n, 128).await;
    support::fund(&e.taker, &b.url, 128).await;
    let lot = e.list(&n.url, 32, &b.url, 24).await;
    let id = e.take(&lot, 40, 32).await;
    e.pump(&id, "complete").await;
    assert_eq!(
        balance(&e.maker, &n.url).await,
        96,
        "SAFETY: maker debit on nostr mint"
    );
    assert_eq!(balance(&e.taker, &n.url).await, 32, "SAFETY: taker claim");
    assert_eq!(balance(&e.maker, &b.url).await, 24, "SAFETY: maker claim");
    assert_eq!(balance(&e.taker, &b.url).await, 104, "SAFETY: taker debit");
    assert_eq!(
        e.tap.misrouted.load(SeqCst),
        0,
        "SAFETY: mint kinds never on market relays, market kinds never on mint relays"
    );
    assert!(e.tap.total_requests() > 0, "fixture: tap saw mint traffic");
    assert_eq!(e.state(true, &id).await.as_deref(), Some("complete"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_nostr_trade_completes_through_lost_swap_replies() {
    let mut e = env().await;
    let n1 = e.nostr_mint().await;
    let n2 = e.nostr_mint().await;
    e.fund_nostr(&e.maker.clone(), &n1, 128).await;
    e.fund_nostr(&e.taker.clone(), &n2, 128).await;
    e.tap.reset();
    // Every swap's first reply is lost: two locks and two claims each need the identical re-send.
    e.tap.lose_first_swap_reply.store(true, SeqCst);
    let lot = e.list(&n1.url, 32, &n2.url, 24).await;
    let id = e.take(&lot, 40, 32).await;
    // Production deadlines leave ample room for a 5 s re-send per swap, so the trade completes.
    // Lab deadlines (TRADE_LAB_SECONDS: 16 s short lock) may not; then both legs must refund.
    let lab = cfg!(feature = "lab") && std::env::var("TRADE_LAB_SECONDS").is_ok();
    let (maker, taker) = e.pump_terminal(&id).await;
    let held = [
        balance(&e.maker, &n1.url).await,
        balance(&e.taker, &n1.url).await,
        balance(&e.maker, &n2.url).await,
        balance(&e.taker, &n2.url).await,
    ];
    assert!(
        held[0] + held[1] == 128 && held[2] + held[3] == 128,
        "SAFETY: no sat created or lost on either nostr mint: {held:?} ({maker}/{taker})"
    );
    let swaps = e.tap.swaps();
    // A lost reply is recovered by re-sending the IDENTICAL event (count >= 2). The only
    // exception: a deadline-clamped window shorter than one re-send interval (lab deadlines),
    // where the call ends ambiguous and recovery restores instead of publishing anything new.
    let resend = maxplayer_core::nostr_mint::DEFAULT_RESEND_EVERY.as_secs();
    assert!(
        swaps
            .iter()
            .all(|s| s.count >= 2 || (lab && s.window <= resend)),
        "SAFETY: each lost reply was recovered by re-sending the IDENTICAL event: {swaps:?}"
    );
    if (maker.as_str(), taker.as_str()) == ("complete", "complete") {
        assert_eq!(held, [96, 32, 24, 104], "SAFETY: completed trade balances");
        assert_eq!(
            swaps.len(),
            4,
            "SAFETY: one signed request per lock/claim; a lost reply never becomes a new swap"
        );
    } else {
        assert!(
            lab,
            "production deadlines must complete through lost replies: {maker}/{taker}"
        );
        assert_eq!(
            (maker.as_str(), taker.as_str(), held),
            ("refunded", "refunded", [128, 0, 0, 128]),
            "SAFETY: a lab-deadline miss refunds both legs in full"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_dleq_verified_on_lock_admission() {
    let e = env().await;
    let n = e.nostr_mint().await;
    e.fund_nostr(&e.maker, &n, 64).await;
    let p = mint::plan(&e.maker, &n.url, 24, 16).await.unwrap();
    let rid = uuid::Uuid::new_v4().to_string();
    mint::reserve(&e.maker, &p, &rid).await.unwrap();
    let recv = SecretKey::generate();
    let refund = SecretKey::generate();
    let c = mint::conditions(
        &"ab".repeat(32),
        &recv.public_key().to_string(),
        &refund.public_key().to_string(),
        coordinator::now() + 3600,
    )
    .unwrap();
    let proofs = mint::lock(&e.maker, &e.jm, &rid, &p, &c, coordinator::now() + 300)
        .await
        .unwrap();
    assert!(
        proofs.iter().all(|p| p.dleq.is_some()),
        "SAFETY: nostr-mint lock outputs carry DLEQ"
    );
    let validate = |proofs: Proofs| {
        let (home, url, c) = (e.taker.clone(), n.url.clone(), c.clone());
        let (gross, fee, ppk, keyset) = (p.gross, p.claim_fee, p.ppk, p.keyset);
        async move { mint::validate(&home, &url, &proofs, 24, gross, fee, &c, ppk, keyset).await }
    };
    let mut forged = proofs.clone();
    forged[0].dleq.as_mut().unwrap().e = SecretKey::generate();
    let forged = validate(forged).await;
    assert!(
        forged
            .as_ref()
            .is_err_and(|e| format!("{e:#}").to_lowercase().contains("dleq")),
        "SAFETY: forged DLEQ on a nostr-mint lock must be refused: {forged:?}"
    );
    let mut missing = proofs.clone();
    missing[0].dleq = None;
    assert!(
        validate(missing).await.is_err(),
        "SAFETY: missing DLEQ refused"
    );
    validate(proofs).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_lock_with_too_many_proofs_refused_at_admission() {
    let e = env().await;
    let n = e.nostr_mint().await;
    let keyset = backend::active_keyset(&n.mint).unwrap();
    let keys = n.mint.keyset(&keyset).unwrap().keys;
    let recv = SecretKey::generate();
    let refund = SecretKey::generate();
    let c = mint::conditions(
        &"cd".repeat(32),
        &recv.public_key().to_string(),
        &refund.public_key().to_string(),
        coordinator::now() + 3600,
    )
    .unwrap();
    let sign = |target: SplitTarget| {
        let (mint, c, keys) = (n.mint.clone(), c.clone(), keys.clone());
        async move {
            let pre = PreMintSecrets::with_conditions(
                keyset,
                128.into(),
                &target,
                &c,
                &backend::fee_and_amounts(),
            )
            .unwrap();
            let sigs = mint.blind_sign(pre.blinded_messages()).await.unwrap();
            construct_proofs(sigs, pre.rs(), pre.secrets(), &keys).unwrap()
        }
    };
    // A dishonest maker: a valid, exact, fee-free HTLC lock split into 128 one-sat proofs. Its
    // claim (one preimage + signature per input) cannot fit one NIP-44 request.
    let many = sign(SplitTarget::Values(vec![1.into(); 128])).await;
    assert_eq!(many.len(), 128);
    let before = e.tap.total_requests();
    let refused = mint::validate(&e.taker, &n.url, &many, 128, 128, 0, &c, 0, keyset).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| format!("{e:#}").contains("NIP-44")),
        "SAFETY: unclaimable nostr lock must be refused at admission: {refused:?}"
    );
    assert_eq!(
        e.tap.total_requests(),
        before,
        "SAFETY: refused before any mint request"
    );
    let honest = sign(SplitTarget::default()).await;
    mint::validate(&e.taker, &n.url, &honest, 128, 128, 0, &c, 0, keyset)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_preflight_refuses_missing_nut_and_cli_canonicalizes() {
    let e = env().await;
    let n = e.nostr_mint().await;
    let hex_upper = format!("NOSTR://{}", n.keys.public_key().to_hex().to_uppercase());
    let ok = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            e.root.path().join("cli").to_str().unwrap(),
            "--mint-relay",
            &e.mint_relay_url,
            "preflight",
            &hex_upper,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(
        String::from_utf8_lossy(&ok.stderr).contains(&n.url),
        "CLI canonicalized hex/uppercase to nostr://<npub>"
    );
    // Fund while the mint is still compliant (`receive` preflights too).
    e.fund_nostr(&e.maker, &n, 64).await;
    let info = n.mint.mint_info().await.unwrap();
    let nuts = info.nuts.clone().nut14(false);
    n.mint
        .set_mint_info(cdk::nuts::MintInfo { nuts, ..info })
        .await
        .unwrap();
    let refused = wallet::preflight(&n.url).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.to_string().contains("NUT-14")),
        "SAFETY: nostr mint without NUT-14 refused: {refused:?}"
    );
    let b = MintFixture::start(0).await;
    let listed = coordinator::list(
        &e.maker,
        &e.jm,
        &e.mm,
        Leg {
            asset: Asset::new(&n.url).unwrap(),
            net: 32,
        },
        Leg {
            asset: Asset::new(&b.url).unwrap(),
            net: 24,
        },
        16,
    )
    .await;
    assert!(
        e.jm.all::<coordinator::Listing>("listing")
            .await
            .unwrap()
            .is_empty(),
        "SAFETY: no listing or reservation for a mint failing preflight"
    );
    assert!(listed.is_err());
    let cli = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            e.root.path().join("cli").to_str().unwrap(),
            "--mint-relay",
            &e.mint_relay_url,
            "preflight",
            &n.url,
        ])
        .output()
        .await
        .unwrap();
    assert!(!cli.status.success());
    assert!(String::from_utf8_lossy(&cli.stderr).contains("NUT-14"));
    let fenced = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            e.root.path().join("cli").to_str().unwrap(),
            "--mint-relay",
            "wss://relay.maxplayer.ai",
            "preflight",
            &n.url,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        !fenced.status.success()
            && String::from_utf8_lossy(&fenced.stderr).contains("not allowed for mint traffic"),
        "SAFETY: production relay refused for mint traffic"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_fund_and_withdraw_refused_before_any_intent() {
    let e = env().await;
    let n = e.nostr_mint().await;
    let fund = money::fund(&e.maker, &e.jm, &n.url, 10).await;
    let withdraw = money::withdraw(&e.maker, &e.jm, &n.url, &invoice(10)).await;
    assert!(
        e.jm.all::<money::Funding>("funding")
            .await
            .unwrap()
            .is_empty()
            && e.jm
                .all::<money::Withdrawal>("withdrawal")
                .await
                .unwrap()
                .is_empty(),
        "SAFETY: no funding or withdrawal intent journaled for a nostr:// mint"
    );
    assert_eq!(
        e.tap.total_requests(),
        0,
        "SAFETY: nothing sent to the mint"
    );
    for r in [fund.err(), withdraw.err()] {
        assert!(
            r.is_some_and(|e| e.to_string().contains("not available for nostr:// mints")),
            "clear refusal"
        );
    }
    for args in [
        vec![
            "fund".to_string(),
            n.url.clone(),
            "--amount".into(),
            "10".into(),
        ],
        vec![
            "withdraw".to_string(),
            n.url.clone(),
            "--invoice".into(),
            invoice(10),
        ],
    ] {
        let home = e.root.path().join("never-created");
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
            .arg("--home")
            .arg(&home)
            .args(&args)
            .output()
            .await
            .unwrap();
        assert!(
            !home.exists(),
            "SAFETY: CLI {} refused before creating or locking the home",
            args[0]
        );
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("not available for nostr:// mints"));
    }
}

/// A valid swap reply slower than the old 20 s HTTP cut-off, inside the 30 s connector window,
/// completes in the same call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_slow_claim_inside_window_completes() {
    let e = env().await;
    let n = e.nostr_mint().await;
    e.fund_nostr(&e.maker, &n, 64).await;
    let (proofs, key, pre) = locked(&e, &n, 24).await;
    e.tap.reset();
    e.tap.delay_next_swap_reply_ms.store(25_000, SeqCst);
    let started = Instant::now();
    let send_before = coordinator::now() + 300;
    let claimed = mint::redeem(
        &e.taker,
        &e.jt,
        "slow-claim",
        &n.url,
        &proofs,
        &key,
        &pre,
        16,
        Some(send_before),
    )
    .await;
    let elapsed = started.elapsed();
    let a = attempt(&e.jt, "slow-claim").await;
    assert_eq!(
        a["done"], true,
        "SAFETY: slow-but-valid nostr claim must complete, not be cut and retried: {claimed:?}"
    );
    assert_eq!(a["abandoned"], false);
    assert_eq!(balance(&e.taker, &n.url).await, 24);
    assert!(
        e.tap.swaps().iter().all(|s| s.exp <= send_before),
        "SAFETY: no swap exp past send_before"
    );
    assert!(
        elapsed > Duration::from_secs(20),
        "fixture: slower than the old 20 s cut-off ({elapsed:?})"
    );
    claimed.unwrap();
}

/// A swap whose reply arrives after the full window: the call fails AMBIGUOUSLY (attempt retained,
/// never abandoned, never re-swapped with new outputs) and recovery restores the exact result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_claim_past_window_stays_ambiguous_then_restores() {
    let e = env().await;
    let n = e.nostr_mint().await;
    e.fund_nostr(&e.maker, &n, 64).await;
    let (proofs, key, pre) = locked(&e, &n, 24).await;
    e.tap.reset();
    e.tap.delay_next_swap_reply_ms.store(36_000, SeqCst);
    let first = mint::redeem(
        &e.taker,
        &e.jt,
        "late-claim",
        &n.url,
        &proofs,
        &key,
        &pre,
        16,
        Some(coordinator::now() + 300),
    )
    .await;
    let a = attempt(&e.jt, "late-claim").await;
    assert!(
        a["done"] == false && a["abandoned"] == false && a["result"].is_null(),
        "SAFETY: a timed-out nostr swap is retained as ambiguous, never definitive: {a}"
    );
    assert!(first.is_err(), "fixture: reply came after the window");
    for _ in 0..600 {
        if e.tap.delivered_swap_replies.load(SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    mint::execute(&e.taker, &e.jt, "late-claim").await.unwrap();
    assert_eq!(
        e.tap.swaps().len(),
        1,
        "SAFETY: recovery restored the first swap, never sent a second one"
    );
    assert_eq!(attempt(&e.jt, "late-claim").await["done"], true);
    assert_eq!(balance(&e.taker, &n.url).await, 24);
}

/// A deadline-bound nostr swap is only published with `exp <= send_before`, and not at all when
/// too little time remains.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_swap_exp_never_passes_send_before() {
    let e = env().await;
    let n = e.nostr_mint().await;
    e.fund_nostr(&e.maker, &n, 64).await;
    let (proofs, key, pre) = locked(&e, &n, 24).await;
    e.tap.reset();
    let tight = coordinator::now() + 1;
    let refused = mint::redeem(
        &e.taker,
        &e.jt,
        "tight",
        &n.url,
        &proofs,
        &key,
        &pre,
        16,
        Some(tight),
    )
    .await;
    assert!(
        e.tap.swaps().is_empty(),
        "SAFETY: no swap published with under 2 s to its deadline"
    );
    assert!(refused.is_err());
    let send_before = coordinator::now() + 10;
    mint::redeem(
        &e.taker,
        &e.jt,
        "bounded",
        &n.url,
        &proofs,
        &key,
        &pre,
        16,
        Some(send_before),
    )
    .await
    .unwrap();
    let swaps = e.tap.swaps();
    assert!(
        !swaps.is_empty() && swaps.iter().all(|s| s.exp <= send_before),
        "SAFETY: swap exp {swaps:?} must not pass send_before {send_before}"
    );
    assert_eq!(balance(&e.taker, &n.url).await, 24);
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_taker_timed_refund() {
    let mut e = env().await;
    let a = MintFixture::start(0).await;
    let n = e.nostr_mint().await;
    support::fund(&e.maker, &a.url, 128).await;
    e.fund_nostr(&e.taker.clone(), &n, 128).await;
    let lot = e.list(&a.url, 32, &n.url, 24).await;
    let id = e.take(&lot, 40, 32).await;
    e.step_until(true, &id, "quoted").await;
    e.step_until(false, &id, "first_locked").await;
    assert_eq!(balance(&e.taker, &n.url).await, 104);
    // The maker never locks.
    let q =
        e.jt.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .quote
            .unwrap();
    until(q.long + q.margin).await;
    coordinator::recover(&e.taker, &e.jt, &e.mt).await.unwrap();
    assert_eq!(
        balance(&e.taker, &n.url).await,
        128,
        "SAFETY: taker lock refunded on the nostr mint"
    );
    assert_eq!(e.state(false, &id).await.as_deref(), Some("refunded"));
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_maker_refund_survives_lost_reply() {
    let mut e = env().await;
    let n = e.nostr_mint().await;
    let b = MintFixture::start(0).await;
    e.fund_nostr(&e.maker.clone(), &n, 128).await;
    support::fund(&e.taker, &b.url, 128).await;
    let lot = e.list(&n.url, 32, &b.url, 24).await;
    let id = e.take(&lot, 40, 32).await;
    e.step_until(true, &id, "quoted").await;
    e.step_until(false, &id, "first_locked").await;
    e.step_until(true, &id, "second_locked").await;
    // The taker never claims. The maker's refund reply is lost once.
    let q =
        e.jm.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .quote
            .unwrap();
    e.tap.reset();
    e.tap.lose_first_swap_reply.store(true, SeqCst);
    until(q.short + q.margin).await;
    coordinator::recover(&e.maker, &e.jm, &e.mm).await.unwrap();
    assert_eq!(
        balance(&e.maker, &n.url).await,
        128,
        "SAFETY: maker lock refunded on the nostr mint despite the lost reply"
    );
    let swaps = e.tap.swaps();
    assert!(
        swaps.len() == 1 && swaps[0].count >= 2,
        "SAFETY: one refund swap, recovered by an identical re-send: {swaps:?}"
    );
    assert_eq!(e.state(true, &id).await.as_deref(), Some("refunded"));
}

/// The counterparty's nostr mint goes silent while the maker holds the preimage: the own-mint
/// (HTTPS) refund of the unclaimed remainder still lands within the advance budget.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_counterparty_unreachable_refund_still_lands() {
    let mut e = env().await;
    let a = MintFixture::start(0).await;
    let n = e.nostr_mint().await;
    support::fund(&e.maker, &a.url, 128).await;
    e.fund_nostr(&e.taker.clone(), &n, 128).await;
    let lot = e.list(&a.url, 24, &n.url, 24).await;
    let id = e.take(&lot, 40, 24).await;
    e.step_until(true, &id, "quoted").await;
    e.step_until(false, &id, "first_locked").await;
    e.step_until(true, &id, "second_locked").await;
    let mut s = e.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let t = e.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    // The taker claims one of the maker's proofs (revealing the preimage), never the rest.
    let chosen = vec![s.outgoing.iter().max_by_key(|p| p.amount).unwrap().clone()];
    mint::redeem(
        &e.taker,
        &e.jt,
        "partial",
        &a.url,
        &chosen,
        &t.key,
        t.preimage.as_ref().unwrap(),
        16,
        None,
    )
    .await
    .unwrap();
    s.preimage = t.preimage.clone();
    s.state = "claiming".into();
    e.jm.put("swap", &id, &s).await.unwrap();
    n.stop();
    let q = s.quote.clone().unwrap();
    until(q.short + q.margin).await;
    coordinator::lab_set_advance_budgets_ms(Some((12_000, 6_000)));
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        coordinator::advance(&e.maker, &e.jm, &e.mm, &mut s),
    )
    .await;
    coordinator::lab_set_advance_budgets_ms(None);
    assert_eq!(
        balance(&e.maker, &a.url).await,
        112,
        "SAFETY: own-mint remainder refunded although the counterparty nostr mint is silent"
    );
    assert_eq!(
        attempt(&e.jm, &format!("{id}-refund")).await["done"],
        true,
        "SAFETY: refund settled"
    );
    assert!(result.is_ok(), "advance bounded by its deadline");
}
