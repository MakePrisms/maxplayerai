//! Maxplayer registration service (docs/specs: email-signup spec v2.12, §3–§6, §8, §11).
//!
//! Links an agent's Nostr key to a human's email with a 6-digit code, records the
//! terms/privacy versions accepted and the marketing setting, and tells the relay
//! (NIP-43 kind 9030) that the key may write. Emails never leave this service's own
//! database: the relay only ever sees npubs.

pub mod api;
pub mod config;
pub mod db;
pub mod email;
pub mod nip98;
pub mod relay;
pub mod secrets;
pub mod sync;
pub mod validate;
pub mod webhook;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub use config::Config;

/// The embedded schema (`migrations/`), applied at startup and by `migrate`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub struct AppState {
    pub cfg: Config,
    pub db: PgPool,
    pub relay: relay::RelayClient,
    pub email: email::EmailClient,
    pub pepper: [u8; 32],
    pub webhook_secret: Option<Vec<u8>>,
    pub clock: Arc<Clock>,
    /// The team invite key (`teams.invite_key_file`); `None` = invite links are off.
    pub invite: Option<nostr::Keys>,
    /// One team at a time for link creation, revoke and the invite key leaving (striped by
    /// channel). In-process, so no database transaction spans a relay call; it assumes one
    /// `serve` process writes invite links and sweeps (`sync` does not sweep).
    invite_locks: Vec<tokio::sync::Mutex<()>>,
}

/// Proof that one team's invite links are locked ([`AppState::lock_team`]).
pub struct TeamLock<'a>(#[allow(dead_code)] tokio::sync::MutexGuard<'a, ()>);

/// Wall clock plus a test-only offset, so code expiry, rate windows and retry
/// backoff can be tested without sleeping. The offset is always 0 in `serve`, and the clock
/// is never frozen there.
#[derive(Debug, Default)]
pub struct Clock {
    offset_secs: AtomicI64,
    /// Test-only: when non-zero, `now` is this unix second (plus the offset).
    frozen_at: AtomicI64,
}

impl Clock {
    pub fn now(&self) -> DateTime<Utc> {
        let base = match self.frozen_at.load(Ordering::Relaxed) {
            0 => Utc::now(),
            secs => DateTime::from_timestamp(secs, 0).unwrap_or_else(Utc::now),
        };
        base + TimeDelta::seconds(self.offset_secs.load(Ordering::Relaxed))
    }
    /// Test-only: stop the clock at the current second (`advance` still moves it).
    pub fn freeze(&self) {
        self.frozen_at
            .store(Utc::now().timestamp().max(1), Ordering::Relaxed);
    }
    pub fn unix(&self) -> u64 {
        self.now().timestamp().max(0) as u64
    }
    pub fn advance(&self, secs: i64) {
        self.offset_secs.fetch_add(secs, Ordering::Relaxed);
    }
}

impl AppState {
    pub fn new(
        cfg: Config,
        db: PgPool,
        admin: nostr::Keys,
        resend_api_key: Option<String>,
        webhook_secret: Option<Vec<u8>>,
    ) -> Result<Self> {
        let pepper = secrets::code_pepper(&admin);
        let relay = relay::RelayClient::new(&cfg.relay, admin)?;
        let email = email::EmailClient::new(&cfg.email, resend_api_key)?;
        Ok(Self {
            cfg,
            db,
            relay,
            email,
            pepper,
            webhook_secret,
            clock: Arc::new(Clock::default()),
            invite: None,
            invite_locks: (0..64).map(|_| tokio::sync::Mutex::new(())).collect(),
        })
    }

    /// Load every secret named by `cfg` (admin key file, Resend key, webhook secret).
    pub fn from_config(cfg: Config, db: PgPool) -> Result<Self> {
        let admin = secrets::load_admin_keys(&cfg.relay.admin_key_file)?;
        let resend = secrets::file_or_env(cfg.email.api_key_file.as_deref(), "RESEND_API_KEY")?;
        let webhook = secrets::file_or_env(
            cfg.email.webhook_secret_file.as_deref(),
            "RESEND_WEBHOOK_SECRET",
        )?
        .map(|raw| {
            webhook::parse_secret(&raw).context("Resend webhook secret is not whsec_<base64>")
        })
        .transpose()?;
        let invite = cfg
            .teams
            .invite_key_file
            .as_deref()
            .map(|p| secrets::load_keys(p, "team invite key"))
            .transpose()?;
        let mut state = Self::new(cfg, db, admin, resend, webhook)?;
        state.invite = invite;
        Ok(state)
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// The lock of one team's invite links. Not reentrant: take it once per request.
    pub async fn lock_team(&self, channel: &str) -> TeamLock<'_> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        channel.hash(&mut h);
        TeamLock(
            self.invite_locks[h.finish() as usize % self.invite_locks.len()]
                .lock()
                .await,
        )
    }
}

pub async fn connect(cfg: &Config) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(cfg.max_db_connections)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&cfg.database_url)
        .await
        .context("cannot connect to the registration database")
}

pub async fn migrate(db: &PgPool) -> Result<()> {
    MIGRATOR.run(db).await.context("database migration failed")
}
