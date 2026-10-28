//! `maxplayer-registration` binary.
//!
//! ```text
//! maxplayer-registration serve   [--config <path>]   run the API + sync loops
//! maxplayer-registration migrate [--config <path>]   apply database migrations and exit
//! maxplayer-registration remove <npub|hex> [--config <path>]
//!                                                    operator ban: delete the registration
//!                                                    and send a NIP-43 remove (9031)
//! maxplayer-registration sync    [--config <path>]   one outbox pass + one reconcile, then exit
//! ```
//!
//! The config path defaults to `$MAXPLAYER_REGISTRATION_CONFIG`, then
//! `/etc/maxplayer-registration/config.toml`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use maxplayer_registration::{AppState, Config, api, config, db, sync};
use tracing::{info, warn};

const USAGE: &str =
    "Usage: maxplayer-registration <serve|migrate|sync|remove <npub|hex>> [--config <path>]";

struct Args {
    command: String,
    target: Option<String>,
    config: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut target = None;
    let mut config = std::env::var_os("MAXPLAYER_REGISTRATION_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(config::DEFAULT_CONFIG_PATH));
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config = PathBuf::from(args.next().context("--config needs a path")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ if command.is_none() => command = Some(a),
            _ if target.is_none() => target = Some(a),
            _ => bail!("unexpected argument {a:?}\n{USAGE}"),
        }
    }
    let command = command.context(USAGE)?;
    Ok(Args {
        command,
        target,
        config,
    })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

/// Background loops: outbox retries, reconcile, cleanup. Errors are logged and retried.
fn spawn_loops(state: Arc<AppState>) {
    let s = state.clone();
    tokio::spawn(async move {
        let mut t =
            tokio::time::interval(Duration::from_secs(s.cfg.sync.outbox_interval_secs.max(1)));
        loop {
            t.tick().await;
            if let Err(e) = sync::run_outbox_once(&s).await {
                warn!(error = %format!("{e:#}"), "outbox pass failed");
            }
        }
    });
    let s = state.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(
            s.cfg.sync.reconcile_interval_secs.max(1),
        ));
        loop {
            t.tick().await;
            if let Err(e) = sync::reconcile_once(&s).await {
                warn!(error = %format!("{e:#}"), "reconcile failed");
            }
        }
    });
    let s = state;
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(
            s.cfg.sync.cleanup_interval_secs.max(60),
        ));
        loop {
            t.tick().await;
            match db::cleanup(&s.db, s.now()).await {
                Ok(n) if n > 0 => info!(rows = n, "cleanup removed old rows"),
                Ok(_) => {}
                Err(e) => warn!(error = %format!("{e:#}"), "cleanup failed"),
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let args = parse_args()?;
    let cfg = Config::load(&args.config)?;
    let pool = maxplayer_registration::connect(&cfg).await?;

    match args.command.as_str() {
        "migrate" => {
            maxplayer_registration::migrate(&pool).await?;
            info!("migrations applied");
        }
        "serve" => {
            maxplayer_registration::migrate(&pool).await?;
            let bind = cfg.bind_addr;
            let state = Arc::new(AppState::from_config(cfg, pool)?);
            if !state.email.has_api_key() {
                warn!(
                    "no Resend API key (email.api_key_file / RESEND_API_KEY): code emails will fail"
                );
            }
            if state.webhook_secret.is_none() {
                warn!("no Resend webhook secret: /account/webhooks/resend answers 503");
            }
            info!(admin = %state.relay.admin_pubkey(), "relay admin key loaded");
            spawn_loops(state.clone());
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .with_context(|| format!("cannot bind {bind}"))?;
            info!(%bind, "registration service listening");
            api::serve(listener, state, shutdown_signal()).await?;
        }
        "sync" => {
            maxplayer_registration::migrate(&pool).await?;
            let state = AppState::from_config(cfg, pool)?;
            let sent = sync::run_outbox_once(&state).await?;
            let report = sync::reconcile_once(&state).await?;
            let sent_after = sync::run_outbox_once(&state).await?;
            println!(
                "confirmed {} command(s); reconcile: {report:?}",
                sent + sent_after
            );
        }
        "remove" => {
            let target = args.target.context("remove needs an npub or hex pubkey")?;
            let pk = nostr::PublicKey::parse(&target).context("not an npub or hex pubkey")?;
            maxplayer_registration::migrate(&pool).await?;
            let state = AppState::from_config(cfg, pool)?;
            let (existed, confirmed) = sync::remove_key(&state, &pk).await?;
            println!(
                "registration {}; relay remove {}",
                if existed {
                    "deleted"
                } else {
                    "was not present"
                },
                if confirmed {
                    "confirmed"
                } else {
                    "queued for retry (see journal)"
                }
            );
        }
        other => bail!("unknown command {other:?}\n{USAGE}"),
    }
    Ok(())
}
