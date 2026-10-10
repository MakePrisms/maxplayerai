#![allow(dead_code)] // Fixture owns mint/relay/tempdir lifetimes even when a test does not inspect them.
use cdk::{
    Mint,
    mint::{MintBuilder, MintMeltLimits, UnitConfig},
    nuts::{CurrencyUnit, PaymentMethod},
    types::FeeReserve,
};
use maxplayer_trade::{
    Asset, Leg,
    coordinator::{self, Swap},
    journal::Journal,
    market::Market,
    wallet,
};
use nostr_relay_builder::prelude::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Default)]
pub struct Faults {
    pub hang_keysets: std::sync::atomic::AtomicBool,
    pub melt_error: std::sync::atomic::AtomicU64,
    pub melt_proxy_error: std::sync::atomic::AtomicBool,
    pub quote_error: std::sync::atomic::AtomicBool,
    pub near_expiry: std::sync::atomic::AtomicBool,
    pub expired_funding: std::sync::atomic::AtomicBool,
    pub no_time: std::sync::atomic::AtomicBool,
    pub melt_requests: std::sync::Mutex<Vec<Vec<u8>>>,
    pub prefer_async_melt: std::sync::atomic::AtomicBool,
    pub lose_melt_reply: std::sync::atomic::AtomicBool,
    pub hold_swap: std::sync::atomic::AtomicBool,
    pub swap_entered: tokio::sync::Notify,
    pub swap_release: tokio::sync::Notify,
    pub swap_finished: tokio::sync::Notify,
    pub hold_checkstate_reply: std::sync::atomic::AtomicBool,
    pub checkstate_reply_entered: tokio::sync::Notify,
    pub checkstate_reply_release: tokio::sync::Notify,
    pub hold_checkstate: std::sync::atomic::AtomicBool,
    pub checkstate_entered: tokio::sync::Notify,
    pub checkstate_release: tokio::sync::Notify,
    pub no_sat_keyset: std::sync::atomic::AtomicBool,
    pub reject_info: std::sync::atomic::AtomicBool,
    pub reject_restore_after_swap: std::sync::atomic::AtomicBool,
    pub uppercase_witness: std::sync::atomic::AtomicBool,
    pub reject_restore: std::sync::atomic::AtomicBool,
    pub reject_swap: std::sync::atomic::AtomicBool,
    pub lose_reply: std::sync::atomic::AtomicBool,
    pub hide_witness_once: std::sync::atomic::AtomicBool,
    pub hide_witness: std::sync::atomic::AtomicBool,
    pub invalid_swap_only_off: std::sync::atomic::AtomicBool,
    pub invalid_dleq: std::sync::atomic::AtomicBool,
    pub pending_inputs: std::sync::atomic::AtomicBool,
    pub omit_dleq: std::sync::atomic::AtomicBool,
    pub missing_nut: std::sync::atomic::AtomicU64,
    pub clock_offset: std::sync::atomic::AtomicU64,
    /// 503 on every NUT-07 /checkstate (own-mint outage that refuses fast).
    pub reject_checkstate: std::sync::atomic::AtomicBool,
    /// Accept /checkstate but never answer (black hole; client RPC timeout applies).
    pub blackhole_checkstate: std::sync::atomic::AtomicBool,
    /// Process the melt POST, then answer 503 to every later request.
    pub die_after_melt: std::sync::atomic::AtomicBool,
    pub dead: std::sync::atomic::AtomicBool,
    /// Slow-but-live mint: delay every request by this many milliseconds.
    pub delay_ms: std::sync::atomic::AtomicU64,
    /// Count of HTTP requests that reached this mint.
    pub requests: std::sync::atomic::AtomicU64,
    /// Count of swap POSTs that reached this mint (including ones whose reply is lost).
    pub swaps: std::sync::atomic::AtomicU64,
    /// Answer every swap POST with a well-formed NUT error (HTTP 400 + code), unprocessed.
    pub swap_nut_error: std::sync::atomic::AtomicBool,
    /// NUT code used by `swap_nut_error` / `refuse_after_swap` (0 = 11001).
    pub swap_nut_code: std::sync::atomic::AtomicU64,
    /// Let the mint EXECUTE the next swap, then answer it with the NUT error above and
    /// blank the next restore (models a lost-reply POST landing around a refusal).
    pub refuse_after_swap: std::sync::atomic::AtomicBool,
    /// Answer this many restores with an empty (but well-formed) reply.
    pub blank_restores: std::sync::atomic::AtomicU64,
    /// Answer every swap POST with an unprocessed HTTP 400 that is NOT a NUT error:
    /// 1 = non-JSON body, 2 = JSON without a numeric `code`.
    pub swap_bad_400: std::sync::atomic::AtomicU64,
    /// On the next swap POST, start reporting UNSPENT inputs as PENDING.
    pub pending_after_swap: std::sync::atomic::AtomicBool,
    /// Drop the last output/signature pair from every non-empty restore reply.
    pub partial_restore: std::sync::atomic::AtomicBool,
}
pub struct MintFixture {
    pub faults: Arc<Faults>,
    pub url: String,
    pub mint: Mint,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for MintFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl MintFixture {
    pub async fn start(ppk: u64) -> Self {
        Self::with_melt_reserve(ppk, 1.0).await
    }
    pub async fn with_melt_reserve(ppk: u64, percent: f32) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let db = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
        let mut b = MintBuilder::new(db.clone());
        b.configure_unit(
            CurrencyUnit::Sat,
            UnitConfig {
                input_fee_ppk: ppk,
                ..Default::default()
            },
        )
        .unwrap();
        let fake = cdk_fake_wallet::FakeWallet::new(
            FeeReserve {
                min_fee_reserve: 1.into(),
                percent_fee_reserve: percent,
            },
            Default::default(),
            Default::default(),
            0,
            CurrencyUnit::Sat,
        );
        b.add_payment_processor(
            CurrencyUnit::Sat,
            PaymentMethod::BOLT11,
            MintMeltLimits::new(1, 200_000),
            Arc::new(fake),
        )
        .await
        .unwrap();
        let mut seed = [0u8; 64];
        seed[..32].copy_from_slice(&cashu::nuts::SecretKey::generate().to_secret_bytes());
        seed[32..].copy_from_slice(&cashu::nuts::SecretKey::generate().to_secret_bytes());
        let mint = b
            .with_name("trade fake-money test".into())
            .with_urls(vec![url.clone()])
            .build_with_seed(db, &seed)
            .await
            .unwrap();
        mint.set_quote_ttl(cdk::types::QuoteTTL::new(10000, 10000))
            .await
            .unwrap();
        mint.start().await.unwrap();
        let router = cdk_axum::create_mint_router(Arc::new(mint.clone()), vec!["bolt11".into()])
            .await
            .unwrap();
        let faults = Arc::new(Faults::default());
        let control = faults.clone();
        let router = router.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let control = control.clone();
                async move {
                    use axum::response::IntoResponse;
                    use std::sync::atomic::Ordering::SeqCst;
                    let path = req.uri().path().to_owned();
                    control.requests.fetch_add(1, SeqCst);
                    if path.ends_with("/swap") {
                        control.swaps.fetch_add(1, SeqCst);
                        if control.pending_after_swap.swap(false, SeqCst) {
                            control.pending_inputs.store(true, SeqCst);
                        }
                        match control.swap_bad_400.load(SeqCst) {
                            1 => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    "<html>bad request</html>",
                                )
                                    .into_response();
                            }
                            2 => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({"error":"expired","detail":"x"})),
                                )
                                    .into_response();
                            }
                            _ => {}
                        }
                        if control.swap_nut_error.load(SeqCst) {
                            let code = match control.swap_nut_code.load(SeqCst) {
                                0 => 11001,
                                c => c,
                            };
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({"code":code,"detail":"refused"})),
                            )
                                .into_response();
                        }
                        if control.refuse_after_swap.swap(false, SeqCst) {
                            let done = next.run(req).await;
                            assert!(done.status().is_success(), "fixture swap must execute");
                            control.blank_restores.store(1, SeqCst);
                            let code = match control.swap_nut_code.load(SeqCst) {
                                0 => 11001,
                                c => c,
                            };
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({"code":code,"detail":"refused"})),
                            )
                                .into_response();
                        }
                    }
                    if path.ends_with("/restore") && {
                        let n = control.blank_restores.load(SeqCst);
                        n > 0
                            && control
                                .blank_restores
                                .compare_exchange(n, n - 1, SeqCst, SeqCst)
                                .is_ok()
                    } {
                        return axum::Json(serde_json::json!({"outputs":[],"signatures":[]}))
                            .into_response();
                    }
                    let delay = control.delay_ms.load(SeqCst);
                    if delay != 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                    if control.dead.load(SeqCst) {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    if path.ends_with("/checkstate") && control.reject_checkstate.load(SeqCst) {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    if path.ends_with("/checkstate") && control.blackhole_checkstate.load(SeqCst) {
                        std::future::pending::<()>().await;
                    }
                    if path.ends_with("/keysets") && control.hang_keysets.load(SeqCst) {
                        std::future::pending::<()>().await;
                    }
                    if path.ends_with("/melt/quote/bolt11") && control.quote_error.load(SeqCst) {
                        return axum::http::StatusCode::BAD_GATEWAY.into_response();
                    }
                    let req = if path.ends_with("/melt/bolt11") {
                        let (parts, body) = req.into_parts();
                        let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
                        control.melt_requests.lock().unwrap().push(bytes.to_vec());
                        let code = control.melt_error.load(SeqCst);
                        if code != 0 {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({"code":code})),
                            )
                                .into_response();
                        }
                        if control.melt_proxy_error.load(SeqCst) {
                            return (
                                axum::http::StatusCode::BAD_GATEWAY,
                                "<html>proxy failure</html>",
                            )
                                .into_response();
                        }
                        axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes))
                    } else {
                        req
                    };
                    let req = if path.ends_with("/melt/bolt11")
                        && control.prefer_async_melt.load(SeqCst)
                    {
                        let (parts, body) = req.into_parts();
                        let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
                        let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        v["prefer_async"] = true.into();
                        let mut request = axum::extract::Request::from_parts(
                            parts,
                            axum::body::Body::from(serde_json::to_vec(&v).unwrap()),
                        );
                        request
                            .headers_mut()
                            .remove(axum::http::header::CONTENT_LENGTH);
                        request
                    } else {
                        req
                    };
                    if path.ends_with("/info") && control.reject_info.load(SeqCst)
                        || path.ends_with("/restore") && control.reject_restore.load(SeqCst)
                        || path.ends_with("/swap") && control.reject_swap.load(SeqCst)
                    {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    if path.ends_with("/checkstate") && control.hold_checkstate.swap(false, SeqCst)
                    {
                        control.checkstate_entered.notify_one();
                        control.checkstate_release.notified().await;
                    }
                    let response =
                        if path.ends_with("/swap") && control.hold_swap.swap(false, SeqCst) {
                            // Keep a real mint request alive after the HTTP client times out.
                            // This models server work already delivered, not a fabricated signature.
                            let c = control.clone();
                            tokio::spawn(async move {
                                c.swap_entered.notify_one();
                                c.swap_release.notified().await;
                                let response = next.run(req).await;
                                c.swap_finished.notify_one();
                                response
                            })
                            .await
                            .unwrap()
                        } else {
                            next.run(req).await
                        };
                    if path.ends_with("/melt/bolt11") && control.die_after_melt.load(SeqCst) {
                        control.dead.store(true, SeqCst);
                    }
                    if path.ends_with("/checkstate")
                        && control.hold_checkstate_reply.swap(false, SeqCst)
                    {
                        control.checkstate_reply_entered.notify_one();
                        control.checkstate_reply_release.notified().await;
                    }
                    if path.ends_with("/swap")
                        && control.reject_restore_after_swap.swap(false, SeqCst)
                    {
                        control.reject_restore.store(true, SeqCst);
                    }
                    if (path.ends_with("/swap") && control.lose_reply.load(SeqCst))
                        || (path.ends_with("/melt/bolt11") && control.lose_melt_reply.load(SeqCst))
                    {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    let (parts, body) = response.into_parts();
                    let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
                    if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        if path.contains("/mint/quote/bolt11")
                            && control.expired_funding.load(SeqCst)
                        {
                            v["state"] = "UNPAID".into();
                            v["expiry"] = 1.into();
                        }
                        if path.ends_with("/melt/quote/bolt11") && control.near_expiry.load(SeqCst)
                        {
                            v["expiry"] = (coordinator::now() + 60).into();
                        }
                        if path.ends_with("/keysets") && control.no_sat_keyset.load(SeqCst) {
                            v["keysets"] = serde_json::json!([]);
                        }
                        if path.ends_with("/info") {
                            let nut = control.missing_nut.load(SeqCst);
                            if nut != 0 {
                                if nut == 4 || nut == 5 {
                                    v["nuts"][nut.to_string()]["disabled"] = true.into();
                                } else {
                                    v["nuts"][nut.to_string()]["supported"] = false.into();
                                }
                            }
                            if control.no_time.load(SeqCst) {
                                v.as_object_mut().unwrap().remove("time");
                            }
                            if let Some(t) = v["time"].as_u64() {
                                v["time"] = (t + control.clock_offset.load(SeqCst)).into();
                            }
                        }
                        if path.ends_with("/checkstate") && control.uppercase_witness.load(SeqCst) {
                            if let Some(states) = v["states"].as_array_mut() {
                                for state in states {
                                    if let Some(w) = state["witness"].as_str() {
                                        if let Ok(mut witness) =
                                            serde_json::from_str::<serde_json::Value>(w)
                                        {
                                            if let Some(pre) = witness["preimage"].as_str() {
                                                witness["preimage"] = pre.to_uppercase().into();
                                                state["witness"] =
                                                    serde_json::to_string(&witness).unwrap().into();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if path.ends_with("/restore") && control.partial_restore.load(SeqCst) {
                            for k in ["outputs", "signatures"] {
                                if let Some(a) = v[k].as_array_mut() {
                                    a.pop();
                                }
                            }
                        }
                        if path.ends_with("/checkstate") && control.pending_inputs.load(SeqCst) {
                            if let Some(states) = v["states"].as_array_mut() {
                                for state in states {
                                    if state["state"] == "UNSPENT" {
                                        state["state"] = "PENDING".into();
                                    }
                                }
                            }
                        }
                        if control.invalid_dleq.load(SeqCst)
                            && !(path.ends_with("/swap")
                                && control.invalid_swap_only_off.load(SeqCst))
                        {
                            if let Some(sigs) = v["signatures"].as_array_mut() {
                                for sig in sigs {
                                    if !sig["dleq"].is_null() {
                                        sig["dleq"]["e"] = "01".repeat(32).into();
                                    }
                                }
                            }
                        }
                        if path.ends_with("/checkstate")
                            && (control.hide_witness.load(SeqCst)
                                || control.hide_witness_once.swap(false, SeqCst))
                        {
                            if let Some(states) = v["states"].as_array_mut() {
                                for state in states {
                                    state.as_object_mut().unwrap().remove("witness");
                                }
                            }
                        }
                        if control.omit_dleq.load(SeqCst) {
                            if let Some(sigs) = v["signatures"].as_array_mut() {
                                for sig in sigs {
                                    sig.as_object_mut().unwrap().remove("dleq");
                                }
                            }
                        }
                        let mut response = axum::response::Response::from_parts(
                            parts,
                            axum::body::Body::from(serde_json::to_vec(&v).unwrap()),
                        );
                        response
                            .headers_mut()
                            .remove(axum::http::header::CONTENT_LENGTH);
                        response
                    } else {
                        axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
                    }
                }
            },
        ));
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url,
            mint,
            task,
            faults,
        }
    }
}
pub async fn fund(home: &Path, mint: &str, n: u64) {
    std::fs::create_dir_all(home).unwrap();
    let w = wallet::wallet(home, mint).await.unwrap();
    let q = w
        .mint_quote(PaymentMethod::BOLT11, Some(n.into()), None, None)
        .await
        .unwrap();
    for _ in 0..100 {
        if w.check_mint_quote(&q.id).await.unwrap().state == cashu::nuts::nut23::QuoteState::Paid {
            w.mint(&q.id, cdk::amount::SplitTarget::default(), None)
                .await
                .unwrap();
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("unpaid fake quote")
}
pub async fn balance(home: &Path, mint: &str) -> u64 {
    u64::from(
        wallet::wallet(home, mint)
            .await
            .unwrap()
            .total_balance()
            .await
            .unwrap(),
    )
}
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub a: MintFixture,
    pub b: MintFixture,
    pub maker: PathBuf,
    pub taker: PathBuf,
    pub jm: Journal,
    pub jt: Journal,
    pub mm: Market,
    pub mt: Market,
    pub relay: LocalRelay,
    pub relay_url: String,
}
impl Fixture {
    pub async fn new(ppk: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        let a = MintFixture::start(ppk).await;
        let b = MintFixture::start(ppk).await;
        let maker = root.path().join("maker");
        let taker = root.path().join("taker");
        fund(&maker, &a.url, 128).await;
        fund(&taker, &b.url, 128).await;
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.unwrap();
        let relay_url = relay.url().await.to_string();
        let mm = Market::connect(wallet::identity(&maker).unwrap(), &[relay_url.clone()])
            .await
            .unwrap();
        let mt = Market::connect(wallet::identity(&taker).unwrap(), &[relay_url.clone()])
            .await
            .unwrap();
        let jm = Journal::open(&maker).await.unwrap();
        let jt = Journal::open(&taker).await.unwrap();
        Self {
            root,
            a,
            b,
            maker,
            taker,
            jm,
            jt,
            mm,
            mt,
            relay,
            relay_url,
        }
    }
    pub async fn list(&self, reverse: bool) -> String {
        let (give, want) = if reverse {
            (&self.b.url, &self.a.url)
        } else {
            (&self.a.url, &self.b.url)
        };
        coordinator::list(
            &self.maker,
            &self.jm,
            &self.mm,
            Leg {
                asset: Asset::new(give).unwrap(),
                net: 32,
            },
            Leg {
                asset: Asset::new(want).unwrap(),
                net: 24,
            },
            16,
        )
        .await
        .unwrap()
    }
    pub async fn start(&self, lot: &str) -> String {
        coordinator::start_take(&self.taker, &self.jt, &self.mt, lot, 40, 32, 16)
            .await
            .unwrap()
    }
    pub async fn step(&mut self, maker: bool) {
        let (home, j, m) = if maker {
            (&self.maker, &self.jm, &mut self.mm)
        } else {
            (&self.taker, &self.jt, &mut self.mt)
        };
        if let Ok(Some(e)) =
            tokio::time::timeout(std::time::Duration::from_millis(100), m.inbox.recv()).await
        {
            let message = m.decode(&e).unwrap();
            let started = std::time::Instant::now();
            coordinator::handle(home, j, m, &e).await.unwrap();
            if let Some(s) = j.get::<Swap>("swap", &message.swap_id).await.unwrap() {
                eprintln!(
                    "fixture maker={maker} step={} state={} elapsed_ms={} now={} quote_timing={:?}",
                    message.step,
                    s.state,
                    started.elapsed().as_millis(),
                    coordinator::now(),
                    s.quote.as_ref().map(|q| (q.issued, q.short, q.cutoff))
                );
            }
        }
    }
    pub async fn state(&self, maker: bool, id: &str) -> Option<String> {
        let j = if maker { &self.jm } else { &self.jt };
        j.get::<Swap>("swap", id).await.unwrap().map(|s| s.state)
    }
    pub async fn pump(&mut self, id: &str, target: &str) {
        // Match the CLI's three-second recovery cadence. Resending after every inbox
        // message creates an artificial request/quote storm during a reverse trade.
        let mut next_recovery = std::time::Instant::now() + std::time::Duration::from_secs(3);
        for _ in 0..100 {
            self.step(true).await;
            self.step(false).await;
            if self.state(false, id).await.as_deref() == Some(target) {
                return;
            }
            if std::time::Instant::now() >= next_recovery {
                coordinator::recover(&self.maker, &self.jm, &self.mm)
                    .await
                    .unwrap();
                coordinator::recover(&self.taker, &self.jt, &self.mt)
                    .await
                    .unwrap();
                next_recovery = std::time::Instant::now() + std::time::Duration::from_secs(3);
            }
        }
        panic!(
            "target {target}: {:?} {:?}",
            self.state(true, id).await,
            self.state(false, id).await
        )
    }
}
/// Maker in `claiming` with a known preimage after the taker claimed one outgoing proof;
/// the rest of the maker's outgoing lock is refundable once past `short + margin`.
#[cfg(feature = "lab")]
pub async fn partial_claim_fixture() -> (Fixture, String) {
    let mut f = Fixture::new(0).await;
    let lot = coordinator::list(
        &f.maker,
        &f.jm,
        &f.mm,
        Leg {
            asset: Asset::new(&f.a.url).unwrap(),
            net: 24,
        },
        Leg {
            asset: Asset::new(&f.b.url).unwrap(),
            net: 24,
        },
        16,
    )
    .await
    .unwrap();
    let id = coordinator::start_take(&f.taker, &f.jt, &f.mt, &lot, 40, 24, 16)
        .await
        .unwrap();
    f.step(true).await;
    f.step(false).await;
    f.step(true).await;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let t = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let chosen = vec![s.outgoing.iter().max_by_key(|p| p.amount).unwrap().clone()];
    maxplayer_trade::mint::redeem(
        &f.taker,
        &f.jt,
        "partial",
        &f.a.url,
        &chosen,
        &t.key,
        t.preimage.as_ref().unwrap(),
        16,
        None,
    )
    .await
    .unwrap();
    s.preimage = t.preimage;
    s.state = "claiming".into();
    f.jm.put("swap", &id, &s).await.unwrap();
    (f, id)
}
#[cfg(feature = "lab")]
pub async fn wait_past(t: u64) {
    while coordinator::now() <= t {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
