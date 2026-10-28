//! Service configuration: one TOML file plus a few environment overrides.
//!
//! Secrets are never in this file. It only names the *paths* of the secret files
//! (relay admin key, Resend API key, Resend webhook secret); see [`crate::secrets`].

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Where `serve` looks for its config when neither `--config` nor
/// `MAXPLAYER_REGISTRATION_CONFIG` is given.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/maxplayer-registration/config.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Loopback bind; nginx on the relay box proxies `/account/` here.
    pub bind_addr: SocketAddr,
    /// The service's own public origin. NIP-98 `u` tags must equal
    /// `<public_base_url>/account/...` exactly (spec §11: no Vercel rewrite, so the
    /// service checks its own public URL).
    pub public_base_url: String,
    /// Postgres URL. Default: the service's own database and role over the local
    /// unix socket (peer auth), never the relay's database.
    pub database_url: String,
    pub max_db_connections: u32,
    /// Use nginx's `X-Real-IP` for per-IP rate limits. Only enable behind a proxy
    /// that sets it; otherwise any client could pick its own rate-limit bucket.
    pub trust_x_real_ip: bool,
    pub relay: RelayConfig,
    pub email: EmailConfig,
    pub legal: LegalConfig,
    pub limits: Limits,
    pub sync: SyncConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:3100".parse().expect("static addr"),
            public_base_url: "https://relay.maxplayer.ai".into(),
            database_url:
                "postgresql:///maxplayer_registration?host=/run/postgresql&user=maxplayer_registration"
                    .into(),
            max_db_connections: 4,
            trust_x_real_ip: false,
            relay: RelayConfig::default(),
            email: EmailConfig::default(),
            legal: LegalConfig::default(),
            limits: Limits::default(),
            sync: SyncConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelayConfig {
    /// Where NIP-43 commands are POSTed (`<base_url>/events`) and the member list is
    /// read (`<base_url>/query`).
    pub base_url: String,
    /// The origin the relay expects in NIP-98 `u` tags, when it differs from
    /// `base_url` (e.g. `base_url = "http://127.0.0.1:3000"` on the box). When set,
    /// requests also carry `Host: <its host>`, because the relay binds the tenant from
    /// the Host header. Unset = `base_url`.
    pub auth_base_url: Option<String>,
    /// Private file holding the relay admin secret key (64 hex or nsec). Never in
    /// the repo; must not be readable by other users.
    pub admin_key_file: PathBuf,
    /// The relay's own pubkey (hex or npub): the signer of the kind-13534 member list
    /// the reconcile job reads. Unset = no reconcile (the outbox still runs).
    pub relay_pubkey: Option<String>,
    pub timeout_secs: u64,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            base_url: "https://relay.maxplayer.ai".into(),
            auth_base_url: None,
            admin_key_file: PathBuf::from("/var/lib/secrets/maxplayer-registration-admin-key"),
            relay_pubkey: None,
            timeout_secs: 10,
        }
    }
}

impl RelayConfig {
    pub fn effective_auth_base(&self) -> &str {
        self.auth_base_url
            .as_deref()
            .unwrap_or(&self.base_url)
            .trim_end_matches('/')
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmailConfig {
    pub resend_api_base: String,
    /// Sent explicitly on every email so the sender never depends on a template default.
    pub from: String,
    /// Resend template id or alias of the published code email.
    pub code_template: String,
    /// Resend template id or alias of the published welcome email.
    pub welcome_template: String,
    /// File holding the Resend API key. Unset = `RESEND_API_KEY` from the environment.
    pub api_key_file: Option<PathBuf>,
    /// File holding the Resend webhook signing secret (`whsec_...`). Unset =
    /// `RESEND_WEBHOOK_SECRET`; if neither is set the webhook answers 503.
    pub webhook_secret_file: Option<PathBuf>,
    pub timeout_secs: u64,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            resend_api_base: "https://api.resend.com".into(),
            from: "Maxplayer <noreply@mail.maxplayer.ai>".into(),
            code_template: "maxplayer-code".into(),
            welcome_template: "maxplayer-welcome".into(),
            api_key_file: None,
            webhook_secret_file: None,
            timeout_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LegalConfig {
    /// Terms versions a link request may name. Each has a stable page (spec §7.1).
    pub terms_versions: Vec<String>,
    pub privacy_versions: Vec<String>,
    /// Versions of the CLI's marketing notice text (spec §8.1). The last one is
    /// recorded when the CLI does not say which it showed.
    pub marketing_notice_versions: Vec<String>,
    /// `{version}` is replaced by the accepted version.
    pub terms_url: String,
    pub privacy_url: String,
}

impl Default for LegalConfig {
    fn default() -> Self {
        Self {
            terms_versions: vec!["v1".into()],
            privacy_versions: vec!["v1".into()],
            marketing_notice_versions: vec!["v1".into()],
            terms_url: "https://www.maxplayer.ai/terms/{version}".into(),
            privacy_url: "https://www.maxplayer.ai/privacy/{version}".into(),
        }
    }
}

impl LegalConfig {
    pub fn terms_url_for(&self, version: &str) -> String {
        self.terms_url.replace("{version}", version)
    }
    pub fn privacy_url_for(&self, version: &str) -> String {
        self.privacy_url.replace("{version}", version)
    }
    pub fn default_notice_version(&self) -> &str {
        self.marketing_notice_versions
            .last()
            .map(String::as_str)
            .unwrap_or("v1")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Code lifetime (spec §3 step 4: 10 minutes).
    pub code_ttl_secs: i64,
    /// Wrong tries per code (spec §3 step 4: 5).
    pub max_attempts: i32,
    /// At most one code email per key per this many seconds.
    pub resend_cooldown_secs: i64,
    pub sends_per_email_per_hour: i32,
    pub sends_per_pubkey_per_hour: i32,
    pub sends_per_ip_per_hour: i32,
    pub confirms_per_pubkey_per_hour: i32,
    pub confirms_per_ip_per_hour: i32,
    /// NIP-98 `created_at` must be within this many seconds of now.
    pub nip98_max_skew_secs: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            code_ttl_secs: 600,
            max_attempts: 5,
            resend_cooldown_secs: 60,
            sends_per_email_per_hour: 5,
            sends_per_pubkey_per_hour: 5,
            sends_per_ip_per_hour: 20,
            confirms_per_pubkey_per_hour: 30,
            confirms_per_ip_per_hour: 60,
            nip98_max_skew_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// How often pending NIP-43 commands are retried.
    pub outbox_interval_secs: u64,
    /// How often the relay's member list (13534) is compared with `registrations`.
    pub reconcile_interval_secs: u64,
    /// When true, the reconcile sends a 9031 for every `member`-role key on the relay
    /// that has no registration row (this is how operator removal-by-SQL works,
    /// spec §4). Off by default so turning the service on cannot drop keys that were
    /// added by hand before they were registered (spec §10). Admins and owners are
    /// never removed by the reconcile.
    pub reconcile_removals: bool,
    /// Keys (hex or npub) the reconcile never removes, e.g. the deploy probe.
    pub keep_members: Vec<String>,
    pub cleanup_interval_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            outbox_interval_secs: 30,
            reconcile_interval_secs: 300,
            reconcile_removals: false,
            keep_members: Vec::new(),
            cleanup_interval_secs: 3600,
        }
    }
}

fn is_version_token(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn check_origin(name: &str, value: &str) -> Result<()> {
    let url = url::Url::parse(value).with_context(|| format!("{name} is not a URL"))?;
    if !matches!(url.scheme(), "https" | "http") {
        bail!("{name} must be http(s)");
    }
    if url.host_str().is_none() || url.query().is_some() || !url.path().trim_matches('/').is_empty()
    {
        bail!("{name} must be a bare origin like https://relay.maxplayer.ai");
    }
    Ok(())
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text).context("invalid config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Read the file, apply `MAXPLAYER_REGISTRATION_DATABASE_URL` /
    /// `MAXPLAYER_REGISTRATION_BIND` overrides, and validate.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).context("invalid config TOML")?;
        if let Ok(url) = std::env::var("MAXPLAYER_REGISTRATION_DATABASE_URL") {
            cfg.database_url = url;
        }
        if let Ok(bind) = std::env::var("MAXPLAYER_REGISTRATION_BIND") {
            cfg.bind_addr = bind
                .parse()
                .context("MAXPLAYER_REGISTRATION_BIND is not host:port")?;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        check_origin("public_base_url", &self.public_base_url)?;
        check_origin("relay.base_url", &self.relay.base_url)?;
        if let Some(auth) = &self.relay.auth_base_url {
            check_origin("relay.auth_base_url", auth)?;
        }
        if let Some(pk) = &self.relay.relay_pubkey {
            nostr::PublicKey::parse(pk).context("relay.relay_pubkey is not a pubkey")?;
        }
        for pk in &self.sync.keep_members {
            nostr::PublicKey::parse(pk)
                .with_context(|| format!("sync.keep_members entry {pk:?} is not a pubkey"))?;
        }
        url::Url::parse(&self.email.resend_api_base).context("email.resend_api_base")?;
        let legal = &self.legal;
        for (name, list) in [
            ("legal.terms_versions", &legal.terms_versions),
            ("legal.privacy_versions", &legal.privacy_versions),
            (
                "legal.marketing_notice_versions",
                &legal.marketing_notice_versions,
            ),
        ] {
            if list.is_empty() {
                bail!("{name} must not be empty");
            }
            if let Some(bad) = list.iter().find(|v| !is_version_token(v)) {
                bail!("{name} entry {bad:?} must be 1-32 of [A-Za-z0-9._-]");
            }
        }
        for (name, tpl) in [
            ("legal.terms_url", &legal.terms_url),
            ("legal.privacy_url", &legal.privacy_url),
        ] {
            if !tpl.contains("{version}") {
                bail!("{name} must contain {{version}}");
            }
            url::Url::parse(&tpl.replace("{version}", "v1"))
                .with_context(|| format!("{name} is not a URL"))?;
        }
        let l = &self.limits;
        if l.code_ttl_secs <= 0
            || l.max_attempts <= 0
            || l.resend_cooldown_secs <= 0
            || l.sends_per_email_per_hour <= 0
            || l.sends_per_pubkey_per_hour <= 0
            || l.sends_per_ip_per_hour <= 0
            || l.confirms_per_pubkey_per_hour <= 0
            || l.confirms_per_ip_per_hour <= 0
            || l.nip98_max_skew_secs <= 0
        {
            bail!("every [limits] value must be positive");
        }
        if self.max_db_connections == 0 {
            bail!("max_db_connections must be positive");
        }
        Ok(())
    }

    /// `<public_base_url><path>`: the exact URL a NIP-98 `u` tag must carry.
    pub fn public_url(&self, path: &str) -> String {
        format!("{}{}", self.public_base_url.trim_end_matches('/'), path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_spec_defaults() {
        let cfg = Config::from_toml("").unwrap();
        assert_eq!(cfg.limits.code_ttl_secs, 600);
        assert_eq!(cfg.limits.max_attempts, 5);
        assert_eq!(
            cfg.public_url("/account/link/request"),
            "https://relay.maxplayer.ai/account/link/request"
        );
        assert_eq!(cfg.email.code_template, "maxplayer-code");
        assert_eq!(cfg.email.welcome_template, "maxplayer-welcome");
        assert_eq!(cfg.email.from, "Maxplayer <noreply@mail.maxplayer.ai>");
        assert!(!cfg.sync.reconcile_removals);
        assert!(!cfg.trust_x_real_ip);
        assert!(cfg.database_url.contains("maxplayer_registration"));
        assert_eq!(
            cfg.legal.terms_url_for("v1"),
            "https://www.maxplayer.ai/terms/v1"
        );
        assert_eq!(
            cfg.legal.privacy_url_for("v2"),
            "https://www.maxplayer.ai/privacy/v2"
        );
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(Config::from_toml("resend_api_key = \"re_x\"").is_err());
        assert!(Config::from_toml("[email]\napi_key = \"re_x\"").is_err());
    }

    #[test]
    fn bad_values_are_refused() {
        assert!(
            Config::from_toml("public_base_url = \"https://relay.maxplayer.ai/account\"").is_err()
        );
        assert!(Config::from_toml("public_base_url = \"ftp://x\"").is_err());
        assert!(Config::from_toml("[legal]\nterms_url = \"https://x/terms\"").is_err());
        assert!(Config::from_toml("[legal]\nterms_versions = []").is_err());
        assert!(Config::from_toml("[legal]\nterms_versions = [\"v1/../x\"]").is_err());
        assert!(Config::from_toml("[limits]\nmax_attempts = 0").is_err());
        assert!(Config::from_toml("[relay]\nrelay_pubkey = \"nope\"").is_err());
        assert!(Config::from_toml("[sync]\nkeep_members = [\"nope\"]").is_err());
    }

    #[test]
    fn auth_base_defaults_to_base_url() {
        let cfg = Config::from_toml("[relay]\nbase_url = \"http://127.0.0.1:3000/\"").unwrap();
        assert_eq!(cfg.relay.effective_auth_base(), "http://127.0.0.1:3000");
        let cfg = Config::from_toml(
            "[relay]\nbase_url = \"http://127.0.0.1:3000\"\nauth_base_url = \"https://relay.maxplayer.ai\"",
        )
        .unwrap();
        assert_eq!(
            cfg.relay.effective_auth_base(),
            "https://relay.maxplayer.ai"
        );
    }
}
