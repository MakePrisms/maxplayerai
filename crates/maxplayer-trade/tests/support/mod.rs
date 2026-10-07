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
pub struct MintFixture {
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
                percent_fee_reserve: 1.0,
            },
            Default::default(),
            Default::default(),
            0,
            CurrencyUnit::Sat,
        );
        b.add_payment_processor(
            CurrencyUnit::Sat,
            PaymentMethod::BOLT11,
            MintMeltLimits::new(1, 10000),
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
        mint.start().await.unwrap();
        let router = cdk_axum::create_mint_router(Arc::new(mint.clone()), vec!["bolt11".into()])
            .await
            .unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { url, mint, task }
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
            coordinator::handle(home, j, m, &e).await.unwrap();
        }
    }
    pub async fn state(&self, maker: bool, id: &str) -> Option<String> {
        let j = if maker { &self.jm } else { &self.jt };
        j.get::<Swap>("swap", id).await.unwrap().map(|s| s.state)
    }
    pub async fn pump(&mut self, id: &str, target: &str) {
        for _ in 0..100 {
            self.step(true).await;
            self.step(false).await;
            if self.state(false, id).await.as_deref() == Some(target) {
                return;
            }
            coordinator::recover(&self.maker, &self.jm, &self.mm)
                .await
                .unwrap();
            coordinator::recover(&self.taker, &self.jt, &self.mt)
                .await
                .unwrap();
        }
        panic!(
            "target {target}: {:?} {:?}",
            self.state(true, id).await,
            self.state(false, id).await
        )
    }
}
