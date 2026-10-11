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
use maxplayer_core::mint_wire::{
    ErrorBody, ErrorCode, Outcome, PROTOCOL_VERSION, REQUEST_KIND, RESPONSE_KIND, Request,
    Response, code,
};
use maxplayer_mint::{backend, home::mint_url, issue, server::Server};
use maxplayer_trade::{
    Asset, LOT, Leg, STATUS, TRADE,
    coordinator::{self, Swap},
    journal::Journal,
    market::Market,
    mint, money, receive, send, transport, wallet,
};
use nostr_relay_builder::{
    LocalRelay, RelayBuilder,
    builder::{PolicyResult, RateLimit, WritePolicy},
};
use nostr_sdk::nips::nip44;
use nostr_sdk::prelude::{Client, Event, EventBuilder, EventId, Keys, Kind, Tag};
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
    /// Answer the next swap request with a mint-signed `expired` reply and never let the sidecar
    /// see it (every copy of that event is rejected): "not executed", as the mint promises.
    expire_next_swap: AtomicBool,
    expired: Mutex<HashSet<EventId>>,
    relay: Mutex<Option<String>>,
    /// One-shot: skip `n` replies to `op`, then delay the next one by `ms` (e.g. the CDK wallet's
    /// `keysets` read after preflight's own).
    delay_op_reply: Mutex<Option<(String, u32, u64)>>,
    /// Slow mint relay: every mint reply (kind 23411) reaches the wallet this long after the mint
    /// sent it. The first reply to a request is refused and the identical mint-signed event is
    /// re-published after the delay (nothing blocks the sidecar or the relay connection); later
    /// replies to the same request are dropped. Each nostr:// call then takes >= this long.
    delay_replies_ms: AtomicU64,
    delayed: Mutex<HashSet<EventId>>,
    /// The delay (ms) each delayed reply got, in order.
    delays: Mutex<Vec<u64>>,
    released: Mutex<HashSet<EventId>>,
    /// Market relay down for writes: refuse every event published to the market relay.
    reject_market_writes: AtomicBool,
    /// `(n, ms)`: once the n-th distinct swap request is seen, set `delay_replies_ms` to `ms`
    /// (e.g. slow every reply from the maker's lock swap on, after both locks raced their deadlines).
    slow_after_swaps: Mutex<Option<(usize, u64)>>,
}
impl Tap {
    async fn admit(&self, event: &Event, mint_relay: bool) -> PolicyResult {
        let kind = event.kind.as_u16();
        if !mint_relay {
            if kind == REQUEST_KIND || kind == RESPONSE_KIND {
                self.misrouted.fetch_add(1, SeqCst);
            }
            if self.reject_market_writes.load(SeqCst) {
                return PolicyResult::Reject("tap: market relay down".into());
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
                        let (swap, request_id) = (r.op == "swap", r.id.clone());
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
                        if swap {
                            let mut slow = self.slow_after_swaps.lock().unwrap();
                            if let Some((n, ms)) = *slow {
                                let seen = self
                                    .requests
                                    .lock()
                                    .unwrap()
                                    .values()
                                    .filter(|s| s.op == "swap")
                                    .count();
                                if seen >= n {
                                    self.delay_replies_ms.store(ms, SeqCst);
                                    *slow = None;
                                }
                            }
                            drop(slow);
                            let mut expired = self.expired.lock().unwrap();
                            if expired.contains(&event.id) {
                                return PolicyResult::Reject("tap: expired request".into());
                            }
                            if self.expire_next_swap.swap(false, SeqCst) {
                                expired.insert(event.id);
                                let relay = self.relay.lock().unwrap().clone().unwrap();
                                tokio::spawn(answer_expired(
                                    k.clone(),
                                    event.clone(),
                                    request_id,
                                    relay,
                                ));
                                return PolicyResult::Reject("tap: expired request".into());
                            }
                        }
                    }
                }
            }
        } else if kind == RESPONSE_KIND {
            let delay = self.delay_replies_ms.load(SeqCst);
            if !self.released.lock().unwrap().remove(&event.id) && delay > 0 {
                if let Some(request) = event.tags.event_ids().next().copied() {
                    if self.delayed.lock().unwrap().insert(request) {
                        self.delays.lock().unwrap().push(delay);
                        self.released.lock().unwrap().insert(event.id);
                        let relay = self.relay.lock().unwrap().clone().unwrap();
                        tokio::spawn(republish(event.clone(), relay, delay));
                    }
                    return PolicyResult::Reject("tap: reply delayed".into());
                }
            }
            let op = event
                .tags
                .event_ids()
                .find_map(|id| self.requests.lock().unwrap().get(id).map(|s| s.op.clone()));
            let delay = {
                let mut d = self.delay_op_reply.lock().unwrap();
                match d.as_mut() {
                    Some((o, skip, _)) if op.as_deref() == Some(o.as_str()) && *skip > 0 => {
                        *skip -= 1;
                        None
                    }
                    Some((o, _, _)) if op.as_deref() == Some(o.as_str()) => d.take(),
                    _ => None,
                }
            };
            if let Some((_, _, ms)) = delay {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
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
/// Publish the reply the sidecar gives a request it refuses as `expired` (signed by the mint key,
/// NIP-44 to the requester, tagged to the request).
async fn answer_expired(mint: Keys, to: Event, request_id: String, relay: String) {
    let response = Response {
        v: PROTOCOL_VERSION,
        id: request_id,
        outcome: Outcome::Err(ErrorBody {
            code: ErrorCode::Named(code::EXPIRED.into()),
            detail: "request expired".into(),
        }),
    };
    let content = nip44::encrypt(
        mint.secret_key(),
        &to.pubkey,
        serde_json::to_string(&response).unwrap(),
        nip44::Version::V2,
    )
    .unwrap();
    let reply = EventBuilder::new(Kind::Custom(RESPONSE_KIND), content)
        .tag(Tag::public_key(to.pubkey))
        .tag(Tag::event(to.id))
        .sign_with_keys(&mint)
        .unwrap();
    let client = Client::default();
    client.add_relay(relay.as_str()).await.unwrap();
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(5)).await;
    client.send_event(&reply).await.unwrap();
    client.disconnect().await;
}
/// Publish `event` (already signed) to `relay` after `delay_ms`.
async fn republish(event: Event, relay: String, delay_ms: u64) {
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    let client = Client::default();
    client.add_relay(relay.as_str()).await.unwrap();
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(5)).await;
    let _ = client.send_event(&event).await;
    client.disconnect().await;
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
    *tap.relay.lock().unwrap() = Some(mint_relay_url.clone());
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
}

/// Advisor HIGH (#1119): a nostr `expired` reply means "not executed", and a skewed clock can
/// produce it for a valid token. It must leave the receive attempt `submitted` (never
/// `refused`), and a later pass completes it with the identical swap body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_expired_swap_reply_during_receive_stays_submitted_then_completes() {
    let e = env().await;
    let n = e.nostr_mint().await;
    let dir = tempfile::tempdir().unwrap();
    let issued = issue::issue(&n.mint, dir.path(), &n.url, 64).await.unwrap();
    let token = std::fs::read_to_string(&issued.file).unwrap();
    e.tap.reset();
    e.tap.expire_next_swap.store(true, SeqCst);
    let first = receive::receive(&e.maker, &e.jm, &n.url, &token)
        .await
        .unwrap();
    assert!(
        !e.tap.expire_next_swap.load(SeqCst) && e.tap.expired.lock().unwrap().len() == 1,
        "fixture: the receive swap was answered `expired`"
    );
    assert_eq!(
        first.state,
        receive::ReceiveState::Submitted,
        "SAFETY: an `expired` reply is not a refusal: {}",
        first.summary()
    );
    assert_eq!(balance(&e.maker, &n.url).await, 0, "nothing credited yet");
    assert!(
        !receive::recover(&e.maker, &e.jm).await.unwrap(),
        "a later pass settles the retained attempt"
    );
    let done = receive::receive(&e.maker, &e.jm, &n.url, &token)
        .await
        .unwrap();
    assert_eq!(
        done.state,
        receive::ReceiveState::Done,
        "{}",
        done.summary()
    );
    assert_eq!(
        balance(&e.maker, &n.url).await,
        64,
        "SAFETY: the token credited exactly once"
    );
    assert_eq!(
        e.tap.swaps().len(),
        2,
        "SAFETY: the expired request plus one replay; no other swap"
    );
}

/// `count` one-sat plain proofs signed by `m`, each with a `secret_len`-character plain secret.
async fn plain_proofs(m: &NostrMint, secret_len: usize, count: usize) -> Proofs {
    let keyset = backend::active_keyset(&m.mint).unwrap();
    let keys = m.mint.keyset(&keyset).unwrap().keys;
    let secrets = (0..count)
        .map(|_| {
            // 64 random hex characters (a normal wallet secret), padded to `secret_len`.
            let base = hex::encode(SecretKey::generate().to_secret_bytes());
            let pad = "a".repeat(secret_len - base.len());
            cdk::secret::Secret::new(format!("{base}{pad}"))
        })
        .collect::<Vec<_>>();
    let pre =
        PreMintSecrets::from_secrets(keyset, vec![cdk::Amount::from(1); count], secrets).unwrap();
    let sigs = m.mint.blind_sign(pre.blinded_messages()).await.unwrap();
    construct_proofs(sigs, pre.rs(), pre.secrets(), &keys).unwrap()
}
fn token_of(m: &NostrMint, proofs: Proofs) -> String {
    cdk::nuts::Token::new(
        std::str::FromStr::from_str(&m.url).unwrap(),
        proofs,
        None,
        cdk::nuts::CurrencyUnit::Sat,
    )
    .to_string()
}

/// Review round 2 N1: plain secrets are arbitrary strings, so a token far under MAX_TOKEN_BYTES
/// can build a receive swap over the NIP-44 limit. It must be refused at admission: nothing
/// journaled, cap untouched, no swap published. A normal 64-character-secret token still passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_oversized_receive_refused_before_journal() {
    let e = env().await;
    let n = e.nostr_mint().await;
    let long = token_of(&n, plain_proofs(&n, 400, 128).await);
    assert!(
        long.len() < receive::MAX_TOKEN_BYTES,
        "fixture: under the token limit"
    );
    e.tap.reset();
    let refused = receive::receive(&e.maker, &e.jm, &n.url, &long).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| format!("{e:#}").contains("NIP-44")),
        "SAFETY: an unsendable nostr receive must be refused at admission: {:?}",
        refused.as_ref().map(|r| r.summary())
    );
    assert!(
        e.jm.all::<serde_json::Value>("receive")
            .await
            .unwrap()
            .is_empty(),
        "SAFETY: nothing journaled"
    );
    assert_eq!(
        receive::cap_used(&e.jm, &n.url).await.unwrap(),
        (0, 0),
        "SAFETY: cap untouched"
    );
    assert!(e.tap.swaps().is_empty(), "SAFETY: no swap published");
    assert!(!receive::pending(&e.jm).await.unwrap());
    let normal = token_of(&n, plain_proofs(&n, 64, 128).await);
    let done = receive::receive(&e.maker, &e.jm, &n.url, &normal)
        .await
        .unwrap();
    assert_eq!(
        done.state,
        receive::ReceiveState::Done,
        "{}",
        done.summary()
    );
    assert_eq!(
        balance(&e.maker, &n.url).await,
        128,
        "normal token credited"
    );
}

/// An oversized attempt journaled before admission sized nostr swaps (older branch builds): the
/// connector refuses it locally (413) every pass, so it is never published, never `refused` and
/// stays `submitted` holding its cap until the token is redeemed elsewhere (then `already_spent`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_preexisting_oversized_receive_is_never_sent() {
    let e = env().await;
    let n = e.nostr_mint().await;
    let inputs = plain_proofs(&n, 400, 128).await;
    let keyset = backend::active_keyset(&n.mint).unwrap();
    let pre = PreMintSecrets::random(
        keyset,
        128.into(),
        &SplitTarget::default(),
        &backend::fee_and_amounts(),
    )
    .unwrap();
    let outputs: Vec<_> = pre
        .secrets
        .iter()
        .map(|o| {
            serde_json::json!({"message":o.blinded_message,"secret":o.secret,
                "r":hex::encode(o.r.to_secret_bytes())})
        })
        .collect();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let record = serde_json::json!({"id":id,"mint":n.url,"state":"submitted","amount":128,
        "fee":0,"net":128,"inputs":inputs,"outputs":outputs,"result":null,"released":false});
    e.jm.put("receive", &id, &record).await.unwrap();
    e.tap.reset();
    for _ in 0..2 {
        assert!(
            receive::recover(&e.maker, &e.jm).await.unwrap(),
            "the oversized attempt stays unresolved"
        );
    }
    let now =
        e.jm.get::<serde_json::Value>("receive", &id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        now["state"], "submitted",
        "SAFETY: a local 413 is never a refusal"
    );
    assert!(e.tap.swaps().is_empty(), "SAFETY: never published");
    assert_eq!(
        receive::cap_used(&e.jm, &n.url).await.unwrap(),
        (0, 128),
        "it holds its cap until the token is redeemed elsewhere"
    );
    assert_eq!(balance(&e.maker, &n.url).await, 0);
}

#[test]
fn default_mint_relays_match_the_sidecar_defaults() {
    assert_eq!(
        transport::default_mint_relays(),
        maxplayer_mint::home::MintConfig::default().effective_relays(),
        "the trade CLI asks the relays a default sidecar answers on, in the same order"
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

// ---- send x nostr:// (PR #1123 integration review) ----

/// Run the built CLI against `home` over the test mint relay; (exit code, stdout, stderr).
async fn cli(e: &Env, home: &Path, args: &[&str]) -> (i32, String, String) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .arg("--home")
        .arg(home)
        .args(["--mint-relay", &e.mint_relay_url])
        .args(args)
        .output()
        .await
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Every Unspent wallet row on `mint` and whether any is reserved by an operation.
async fn rows(home: &Path, mint: &str) -> (usize, bool) {
    use cdk::cdk_database::WalletDatabase;
    let db = wallet::database(home, mint).await.unwrap();
    let p = db
        .get_proofs(Some(mint.parse().unwrap()), None, None, None)
        .await
        .unwrap();
    (p.len(), p.iter().any(|p| p.used_by_operation.is_some()))
}

/// Integration review (advisor Medium / crd L2): send, `status --check-sends` and both reclaim
/// outcomes complete on a nostr:// mint over the credits sidecar, mint traffic on mint relays only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_send_check_sends_and_reclaim_round_trip() {
    let e = env().await;
    let n = e.nostr_mint().await;
    e.fund_nostr(&e.maker, &n, 128).await;
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.token");
    let r = send::send(&e.maker, &e.jm, &n.url, 37, &first, None)
        .await
        .unwrap();
    assert_eq!(r.state, send::SendState::Sent, "{}", r.summary());
    assert_eq!(balance(&e.maker, &n.url).await, 91, "SAFETY: debited once");
    let token = std::fs::read_to_string(&first).unwrap();
    assert!(token.starts_with("cashuB"), "token file written");
    let got = receive::receive(&e.taker, &e.jt, &n.url, &token)
        .await
        .unwrap();
    assert_eq!(got.state, receive::ReceiveState::Done, "{}", got.summary());
    assert_eq!(balance(&e.taker, &n.url).await, 37);
    let second = dir.path().join("second.token");
    let r2 = send::send(&e.maker, &e.jm, &n.url, 20, &second, None)
        .await
        .unwrap();
    assert_eq!(r2.state, send::SendState::Sent, "{}", r2.summary());
    assert_eq!(balance(&e.maker, &n.url).await, 71);
    // `status --check-sends` is read-only NUT-07 over the mint relay.
    let (code, out, err) = cli(&e, &e.maker, &["status", "--check-sends"]).await;
    assert_eq!(code, 0, "{err}");
    let line = |id: &str| -> serde_json::Value {
        out.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["send"] == id)
            .unwrap_or_else(|| panic!("no check-sends line for {id}: {out}"))
    };
    assert_eq!(line(&r.id)["redeemed"], "redeemed");
    assert_eq!(line(&r2.id)["redeemed"], "unredeemed");
    // Reclaim the redeemed send: nothing comes back. Reclaim the outstanding one: it does.
    let gone = send::reclaim(&e.maker, &e.jm, &r.id, None).await.unwrap();
    assert_eq!(gone.state, send::SendState::Redeemed);
    let back = send::reclaim(&e.maker, &e.jm, &r2.id, None).await.unwrap();
    assert_eq!(back.state, send::SendState::Reclaimed, "{}", back.summary());
    assert_eq!(
        balance(&e.maker, &n.url).await,
        91,
        "SAFETY: reclaimed exactly once"
    );
    assert!(!send::pending(&e.jm).await.unwrap());
    assert!(!rows(&e.maker, &n.url).await.1, "nothing left reserved");
    assert_eq!(
        e.tap.misrouted.load(SeqCst),
        0,
        "SAFETY: no misrouted kinds"
    );
    assert!(e.tap.swaps().len() >= 3, "fixture: swaps went over nostr");
}

/// Integration review (advisor High / crd L1): a send whose exact swap exceeds the NIP-44 limit
/// is refused before the journal (exit 1): nothing journaled, nothing reserved, nothing published.
/// The oversized wallet rows (128 one-sat proofs with 400-character plain secrets, signed by the
/// sidecar) are written straight into the CDK wallet database: test-only, no CLI path builds them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_oversized_send_refused_before_journal() {
    use cdk::cdk_database::WalletDatabase;
    let e = env().await;
    let n = e.nostr_mint().await;
    let long = plain_proofs(&n, 400, 128).await;
    let db = wallet::database(&e.maker, &n.url).await.unwrap();
    let url: cdk::mint_url::MintUrl = n.url.parse().unwrap();
    db.update_proofs(
        long.into_iter()
            .map(|p| {
                cdk_common::wallet::ProofInfo::new(
                    p,
                    url.clone(),
                    cdk::nuts::State::Unspent,
                    cdk::nuts::CurrencyUnit::Sat,
                )
                .unwrap()
            })
            .collect(),
        vec![],
    )
    .await
    .unwrap();
    drop(db);
    assert_eq!(
        balance(&e.maker, &n.url).await,
        128,
        "fixture: rows imported"
    );
    e.tap.reset();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("big.token");
    let (code, stdout, err) = cli(
        &e,
        &e.maker,
        &[
            "send",
            &n.url,
            "--amount",
            "127",
            "--out",
            out.to_str().unwrap(),
        ],
    )
    .await;
    assert!(
        err.contains("NIP-44") && err.contains("nothing journaled"),
        "clear refusal: {err}"
    );
    assert_eq!(
        code, 1,
        "SAFETY: a refusal, not a retained attempt: {stdout}"
    );
    assert!(
        e.jm.all::<serde_json::Value>("send")
            .await
            .unwrap()
            .is_empty(),
        "SAFETY: nothing journaled"
    );
    assert_eq!(
        rows(&e.maker, &n.url).await,
        (128, false),
        "SAFETY: nothing reserved"
    );
    assert!(e.tap.swaps().is_empty(), "SAFETY: no swap published");
    assert!(!out.exists(), "no token file");
    assert!(!send::pending(&e.jm).await.unwrap());
    assert_eq!(balance(&e.maker, &n.url).await, 128);
}

/// The same for a reclaim: a sent token whose proofs carry long plain secrets (a journal row
/// injected by the test; own sends cannot produce it) is refused before the Reclaiming /
/// reclaim_submitted write, so the token stays `sent` and no swap is published.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_oversized_reclaim_refused_before_journal() {
    let e = env().await;
    let n = e.nostr_mint().await;
    let sent = plain_proofs(&n, 400, 128).await;
    let keyset = backend::active_keyset(&n.mint).unwrap();
    let pre = PreMintSecrets::random(
        keyset,
        128.into(),
        &SplitTarget::Value(cdk::Amount::from(1)),
        &backend::fee_and_amounts(),
    )
    .unwrap();
    let outputs: Vec<_> = pre
        .secrets
        .iter()
        .map(|o| {
            serde_json::json!({"message":o.blinded_message,"secret":o.secret,
                "r":hex::encode(o.r.to_secret_bytes()),"send":true})
        })
        .collect();
    assert_eq!(
        outputs.len(),
        sent.len(),
        "fixture: one output per sent proof"
    );
    let id = uuid::Uuid::new_v4().to_string();
    let record = serde_json::json!({"id":id,"mint":n.url,"state":"sent","amount":128,"fee":0,
        "out":e.root.path().join("sent.token"),"swap":{"inputs":[],"outputs":outputs,
        "result":sent},"token":"cashuB-fixture","committed":true,"released":true});
    e.jm.put("send", &id, &record).await.unwrap();
    e.tap.reset();
    let (code, stdout, err) = cli(&e, &e.maker, &["send", "--reclaim", &id]).await;
    assert!(
        err.contains("NIP-44") && err.contains("nothing journaled"),
        "clear refusal: {err}"
    );
    assert_eq!(
        code, 1,
        "SAFETY: a refusal, not a retained reclaim: {stdout}"
    );
    let now: serde_json::Value = e.jm.get("send", &id).await.unwrap().unwrap();
    assert_eq!(now["state"], "sent", "SAFETY: no Reclaiming write");
    assert!(now["reclaim"].is_null(), "SAFETY: no reclaim leg journaled");
    assert!(
        now["reclaim_submitted"].is_null(),
        "SAFETY: reclaim_submitted never written"
    );
    assert!(e.tap.swaps().is_empty(), "SAFETY: no swap published");
    assert!(!send::pending(&e.jm).await.unwrap());
}

/// crd M1: a CDK wallet read (keysets) answered after 20 s, inside the 30 s connector window,
/// must not fail a nostr send: send's wallet calls use the 40 s nostr bound, not 15 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_send_survives_slow_keysets_reply() {
    let e = env().await;
    let n = e.nostr_mint().await;
    e.fund_nostr(&e.maker, &n, 64).await;
    // Skip preflight's keysets read; delay the CDK wallet's (refresh_keysets) by 20 s.
    *e.tap.delay_op_reply.lock().unwrap() = Some(("keysets".into(), 1, 20_000));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("slow.token");
    let started = Instant::now();
    let r = send::send(&e.maker, &e.jm, &n.url, 24, &out, None).await;
    *e.tap.delay_op_reply.lock().unwrap() = None;
    let r = r.unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(20),
        "fixture: a keysets reply was delayed"
    );
    assert_eq!(r.state, send::SendState::Sent, "{}", r.summary());
    assert_eq!(balance(&e.maker, &n.url).await, 40, "SAFETY: debited once");
}

/// Bug 2 (external tester): a maker swap whose lock swap never executed must reach `expired` and
/// stop blocking that taker ("taker quote limit"). The lock swap is answered `expired` and never
/// reaches the mint; past `send_before + NOSTR_ABANDON_GRACE_SECONDS` maker recovery runs with
/// every mint reply delayed `reply_delay_ms`, and/or with the market relay refusing writes. Returns
/// the maker swap state after each recovery tick and the same taker's next quote request result.
#[cfg(feature = "lab")]
async fn abandoned_maker_lock_case(reply_delay_ms: u64, market_down: bool) {
    let mut e = env().await;
    let n1 = e.nostr_mint().await;
    let n2 = e.nostr_mint().await;
    e.fund_nostr(&e.maker.clone(), &n1, 128).await;
    // A second maker proof, so a second lot can be listed while the first lot's input is reserved.
    let dir = tempfile::tempdir().unwrap();
    let issued = issue::issue(&n1.mint, dir.path(), &n1.url, 64)
        .await
        .unwrap();
    let token = std::fs::read_to_string(issued.file).unwrap();
    receive::receive(&e.maker, &e.jm, &n1.url, &token)
        .await
        .unwrap();
    e.fund_nostr(&e.taker.clone(), &n2, 128).await;
    let lot = e.list(&n1.url, 32, &n2.url, 24).await;
    let lot2 = e.list(&n1.url, 16, &n2.url, 8).await;
    let id = e.take(&lot, 40, 32).await;
    e.step_until(true, &id, "quoted").await;
    e.step_until(false, &id, "first_locked").await;
    e.tap.reset();
    e.tap.expire_next_swap.store(true, SeqCst);
    e.step_until(true, &id, "first_validated").await;
    let lock_id = format!("{id}-lock");
    let lock = attempt(&e.jm, &lock_id).await;
    assert!(
        lock["done"] == false && lock["abandoned"] == false && lock["result"].is_null(),
        "fixture: lock journaled and unanswered: {lock}"
    );
    assert_eq!(
        e.tap.expired.lock().unwrap().len(),
        1,
        "fixture: the maker's lock swap never reached the mint"
    );
    let q =
        e.jm.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .quote
            .unwrap();
    let send_before = q.exp.min(q.short - q.cutoff);
    assert_eq!(lock["send_before"], send_before, "fixture: lock deadline");
    until(send_before + transport::NOSTR_ABANDON_GRACE_SECONDS).await;
    // The taker's own lock refunds after long + margin; it may then take another lot.
    coordinator::recover(&e.taker, &e.jt, &e.mt).await.unwrap();
    assert_eq!(e.state(false, &id).await.as_deref(), Some("refunded"));
    e.tap.delay_replies_ms.store(reply_delay_ms, SeqCst);
    e.tap.reject_market_writes.store(market_down, SeqCst);
    let mut observed = vec![];
    for _ in 0..2 {
        let started = Instant::now();
        coordinator::recover(&e.maker, &e.jm, &e.mm).await.unwrap();
        let state = e.state(true, &id).await.unwrap_or_default();
        observed.push(format!("{state} after {:?}", started.elapsed()));
        if state == "expired" {
            break;
        }
    }
    let expired_while_degraded = e.state(true, &id).await;
    e.tap.delay_replies_ms.store(0, SeqCst);
    e.tap.reject_market_writes.store(false, SeqCst);
    let id2 = e.take(&lot2, 24, 16).await;
    let mut admission = None;
    for _ in 0..100 {
        if let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_millis(200), e.mm.inbox.recv()).await
        {
            if e.mm.decode(&ev).is_ok_and(|m| m.swap_id == id2) {
                admission = Some(
                    coordinator::handle(&e.maker, &e.jm, &e.mm, &ev)
                        .await
                        .map_err(|x| format!("{x:#}")),
                );
                break;
            }
        }
    }
    eprintln!(
        "bug2 (reply delay {reply_delay_ms} ms, market down {market_down}): maker swap per recovery tick {observed:?}; the same taker's next request: {admission:?}"
    );
    let lock = attempt(&e.jm, &lock_id).await;
    assert!(
        lock["done"] == false && lock["result"].is_null(),
        "SAFETY: the abandoned lock never executed: {lock}"
    );
    assert_eq!(
        balance(&e.taker, &n1.url).await,
        0,
        "SAFETY: nothing paid out"
    );
    // Unspent proofs, reserved or not (lot 2 still reserves its input): every sat stays the maker's.
    let held: u64 = {
        use cdk::cdk_database::WalletDatabase;
        let db = wallet::database(&e.maker, &n1.url).await.unwrap();
        db.get_proofs(Some(n1.url.parse().unwrap()), None, None, None)
            .await
            .unwrap()
            .iter()
            .filter(|p| {
                matches!(
                    p.state,
                    cdk::nuts::State::Unspent | cdk::nuts::State::Reserved
                )
            })
            .map(|p| u64::from(p.proof.amount))
            .sum()
    };
    assert_eq!(held, 192, "SAFETY: maker keeps every sat");
    assert_eq!(
        expired_while_degraded.as_deref(),
        Some("expired"),
        "an abandoned, not-landed maker lock must expire: {observed:?}"
    );
    // Admission passed the per-taker guard once the swap is journaled `quoted`. Sending the quote
    // may still fail while the market relay's publication backoff from the outage runs.
    assert!(
        admission
            .as_ref()
            .is_some_and(|r| r.as_ref().err().is_none_or(|x| !x.contains("quote limit"))),
        "the taker must no longer be refused: {admission:?}"
    );
    assert_eq!(e.state(true, &id2).await.as_deref(), Some("quoted"));
}
/// Every mint reply 19 s late: the expiry chain (9 calls, >= 171 s) outran the old 140 s item budget.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_abandoned_maker_lock_expires_on_slow_relay_and_frees_the_taker() {
    abandoned_maker_lock_case(19_000, false).await;
}
/// Market relay down: the cancelled-listing publication must not keep the swap open.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_abandoned_maker_lock_expires_while_market_relay_is_down() {
    abandoned_maker_lock_case(0, true).await;
}

/// Bug 3: on a slow relay (every mint reply 2.5 s late) the taker's claim runs into its deadline. Safe
/// either way: no swap sent before the maker's refund locktime outlives `short - cutoff` unless
/// the trade completed, and no sat is created or lost on either mint.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_slow_relay_near_deadline_claim_stays_safe() {
    let mut e = env().await;
    let n1 = e.nostr_mint().await;
    let n2 = e.nostr_mint().await;
    e.fund_nostr(&e.maker.clone(), &n1, 128).await;
    e.fund_nostr(&e.taker.clone(), &n2, 128).await;
    let lot = e.list(&n1.url, 32, &n2.url, 24).await;
    let id = e.take(&lot, 40, 32).await;
    e.step_until(true, &id, "quoted").await;
    e.step_until(false, &id, "first_locked").await;
    e.step_until(true, &id, "second_locked").await;
    let q =
        e.jm.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .quote
            .unwrap();
    let send_before = q.short - q.cutoff;
    e.tap.reset();
    e.tap.delay_replies_ms.store(2_500, SeqCst);
    let (maker, taker) = e.pump_terminal(&id).await;
    e.tap.delay_replies_ms.store(0, SeqCst);
    let held = [
        balance(&e.maker, &n1.url).await,
        balance(&e.taker, &n1.url).await,
        balance(&e.maker, &n2.url).await,
        balance(&e.taker, &n2.url).await,
    ];
    let swaps = e.tap.swaps();
    let claim =
        e.jt.get::<serde_json::Value>("attempt", &format!("{id}-claim"))
            .await
            .unwrap();
    eprintln!(
        "bug3 near-deadline: {maker}/{taker} {held:?}; send_before {send_before}; claim attempt {}; swaps {swaps:?}",
        claim.is_some()
    );
    assert!(
        held[0] + held[1] == 128 && held[2] + held[3] == 128,
        "SAFETY: no sat created or lost: {held:?}"
    );
    if let Some(c) = &claim {
        assert_eq!(
            c["send_before"], send_before,
            "SAFETY: claim bound to short - cutoff"
        );
    }
    match (maker.as_str(), taker.as_str()) {
        ("complete", "complete") => assert_eq!(held, [96, 32, 24, 104]),
        ("refunded", "refunded") => {
            assert_eq!(held, [128, 0, 0, 128], "SAFETY: both legs refunded in full");
            assert!(
                swaps
                    .iter()
                    .all(|s| s.exp - s.window > q.short || s.exp <= send_before),
                "SAFETY: a claim swap never outlives short - cutoff ({send_before}): {swaps:?}"
            );
        }
        other => panic!("SAFETY: mixed outcome {other:?} {held:?}"),
    }
}

/// Start `maxplayer-trade serve` for `home` over the test relays; returns the child and its log.
#[cfg(not(feature = "lab"))]
async fn serve(e: &Env, home: &Path, market: &str) -> (tokio::process::Child, PathBuf) {
    let log = e.root.path().join(format!(
        "{}-serve.log",
        home.file_name().unwrap().to_string_lossy()
    ));
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .arg("--home")
        .arg(home)
        .args([
            "--mint-relay",
            &e.mint_relay_url,
            "--relay",
            market,
            "serve",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("{\"serve_socket\":\"")
    {
        assert!(started.elapsed() < Duration::from_secs(30), "serve socket");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (child, log)
}

/// Bug 3: an all-nostr trade completes end to end through two `serve` processes on a slow mint
/// relay. Every mint reply is 0.6 s late while the locks race their 40/60 s send deadlines, then
/// 6 s late from the maker's lock swap on (switched by the tap, not by polling), so the taker's
/// `second` message (>= 10 delayed calls before and after its claim swap, >= 60 s) outlasts the old
/// 60 s HTTP message budget; no message may be dropped.
#[cfg(not(feature = "lab"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nostr_serve_completes_all_nostr_trade_on_slow_relay() {
    let e = env().await;
    let n1 = e.nostr_mint().await;
    let n2 = e.nostr_mint().await;
    e.fund_nostr(&e.maker, &n1, 128).await;
    e.fund_nostr(&e.taker, &n2, 128).await;
    let market = e._market_relay.url().await.to_string();
    e.tap.reset();
    e.tap.delay_replies_ms.store(600, SeqCst);
    *e.tap.slow_after_swaps.lock().unwrap() = Some((2, 6_000));
    let serve_started = Instant::now();
    let (mut maker, maker_log) = serve(&e, &e.maker, &market).await;
    let (mut taker, taker_log) = serve(&e, &e.taker, &market).await;
    let field = |out: &str, key: &str| {
        out.lines()
            .find_map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()?
                    .get(key)?
                    .as_str()
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("no {key} in {out}"))
    };
    let (code, out, err) = cli(
        &e,
        &e.maker,
        &[
            "--relay",
            &market,
            "list",
            "--give-mint",
            &n1.url,
            "--give",
            "32",
            "--want-mint",
            &n2.url,
            "--want",
            "24",
        ],
    )
    .await;
    assert_eq!(code, 0, "list via serve: {err}");
    let lot = field(&out, "lot_id");
    // `take` handed to serve streams until the swap is terminal: run it in the background.
    let mut take = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .arg("--home")
        .arg(&e.taker)
        .args([
            "--mint-relay",
            &e.mint_relay_url,
            "--relay",
            &market,
            "take",
            &lot,
        ])
        .args(["--max-give", "40", "--min-receive", "32"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let swap_id = || {
        wallet::read_status(&e.taker).ok().and_then(|v| {
            v["records"].as_array()?.iter().find_map(|r| {
                (r["kind"] == "swap").then(|| r["id"].as_str().unwrap_or_default().to_owned())
            })
        })
    };
    let waited = Instant::now();
    let id = loop {
        if let Some(id) = swap_id() {
            break id;
        }
        assert!(
            waited.elapsed() < Duration::from_secs(60),
            "take journaled no swap"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let state = |home: &Path| {
        wallet::read_status(home)
            .ok()
            .and_then(|v| {
                v["records"].as_array()?.iter().find_map(|r| {
                    (r["kind"] == "swap" && r["id"] == id.as_str())
                        .then(|| r["state"].as_str().unwrap_or_default().to_owned())
                })
            })
            .unwrap_or_default()
    };
    let started = Instant::now();
    loop {
        let (m, t) = (state(&e.maker), state(&e.taker));
        if m == "complete" && t == "complete" {
            break;
        }
        if started.elapsed() > Duration::from_secs(900)
            || TERMINAL.contains(&m.as_str()) && TERMINAL.contains(&t.as_str())
        {
            panic!(
                "slow-relay serve trade ended {m}/{t} after {:?}\nmaker:\n{}\ntaker:\n{}",
                started.elapsed(),
                std::fs::read_to_string(&maker_log).unwrap_or_default(),
                std::fs::read_to_string(&taker_log).unwrap_or_default()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let slowed = e.tap.slow_after_swaps.lock().unwrap().is_none()
        && e.tap.delay_replies_ms.load(SeqCst) == 6_000;
    let delays = e.tap.delays.lock().unwrap().clone();
    let delayed = delays.iter().filter(|d| **d == 6_000).count();
    eprintln!(
        "bug3 serve: complete {:?} after the taker journaled the swap, {:?} after serve start \
         (slowed={slowed}, {} delayed replies, {delayed} of them by 6 s)",
        started.elapsed(),
        serve_started.elapsed(),
        delays.len()
    );
    let _ = take.wait().await;
    maker.kill().await.unwrap();
    taker.kill().await.unwrap();
    e.tap.delay_replies_ms.store(0, SeqCst);
    let logs = [&maker_log, &taker_log].map(|l| std::fs::read_to_string(l).unwrap_or_default());
    let dropped = logs
        .iter()
        .map(|l| l.matches("trade message timed out").count())
        .sum::<usize>();
    assert!(slowed, "fixture: the claim phases ran on the 6 s relay");
    assert!(
        delayed >= 20,
        "fixture: the trade's mint calls ran on the 6 s relay ({delayed} replies delayed 6 s)"
    );
    assert_eq!(
        dropped, 0,
        "a nostr:// trade message must not hit the HTTP-sized 60 s message budget"
    );
    assert_eq!(
        [
            balance(&e.maker, &n1.url).await,
            balance(&e.taker, &n1.url).await,
            balance(&e.maker, &n2.url).await,
            balance(&e.taker, &n2.url).await,
        ],
        [96, 32, 24, 104],
        "SAFETY: completed trade balances"
    );
    assert_eq!(
        e.tap.misrouted.load(SeqCst),
        0,
        "SAFETY: no misrouted kinds"
    );
}
