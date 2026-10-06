//! Real CDK HTTPS mint fixture; only the Lightning backend is simulated.
use cdk::{
    Mint,
    mint::{MintBuilder, MintMeltLimits},
    nuts::{CurrencyUnit, PaymentMethod},
    types::FeeReserve,
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    service::TowerToHyperService,
};
use std::sync::Arc;
use tokio_rustls::{TlsAcceptor, rustls};

pub struct HttpsMint {
    pub mint: Mint,
    pub url: String,
    pub certificate: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for HttpsMint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn start() -> HttpsMint {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    let db = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
    let mut builder = MintBuilder::new(db.clone());
    let backend = cdk_fake_wallet::FakeWallet::new(
        FeeReserve {
            min_fee_reserve: 1.into(),
            percent_fee_reserve: 1.0,
        },
        Default::default(),
        Default::default(),
        0,
        CurrencyUnit::Sat,
    );
    builder
        .add_payment_processor(
            CurrencyUnit::Sat,
            PaymentMethod::BOLT11,
            MintMeltLimits::new(1, 10_000),
            Arc::new(backend),
        )
        .await
        .unwrap();
    let mint = builder
        .with_name("credits-first local test".into())
        .with_urls(vec![url.clone()])
        .build_with_seed(db, &super::seed())
        .await
        .unwrap();
    mint.start().await.unwrap();
    let router = cdk_axum::create_mint_router(Arc::new(mint.clone()), vec!["bolt11".into()])
        .await
        .unwrap();
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let certificate = certified.cert.pem();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certified.cert.der().clone()], key.into())
    .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (acceptor, router) = (acceptor.clone(), router.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(
                        TokioIo::new(tls),
                        TowerToHyperService::new(router),
                    )
                    .await;
            });
        }
    });
    HttpsMint {
        mint,
        url,
        certificate,
        task,
    }
}
