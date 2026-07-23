//! Packaged buyer home under `~/.mobee` (or `MOBEE_HOME`).
//!
//! First-run bootstrap writes working defaults: testnut mint, mobee-relay, budget caps,
//! autogen key (`0600`), and an empty `wallet/` dir. The secret key is never returned.
//!
//! # Layered configuration
//!
//! [`MobeeConfig`] resolves in three layers, later winning:
//!
//! 1. **built-in defaults** — [`MobeeConfig::default`].
//! 2. **file** — `~/.mobee/config.toml` (if present). Absent fields fall back to the defaults;
//!    unknown fields refuse (`deny_unknown_fields`). The single-mint legacy `mint_url = "…"` key
//!    folds into `accepted_mints`.
//! 3. **environment** — `MOBEE_*` variables. Every field is reachable: uppercase the field path,
//!    prefix `MOBEE_`, join nested fields with `__` (double underscore). Comma-separated for lists.
//!
//! The typed struct is the single in-process representation — only its *construction* is layered
//! (the one seam is [`bootstrap`] / [`reload_config`], both routed through the env overlay). Every
//! layer fails closed: an unknown or malformed key refuses with the offending key named, never a
//! silent default.
//!
//! ## Env mapping
//!
//! | Field | Variable |
//! |-------|----------|
//! | `relay_url` | `MOBEE_RELAY_URL` |
//! | `accepted_mints` (list) | `MOBEE_ACCEPTED_MINTS=a,b` |
//! | `per_job_budget_sats` | `MOBEE_PER_JOB_BUDGET_SATS` |
//! | `total_budget_sats` | `MOBEE_TOTAL_BUDGET_SATS` |
//! | `extra_mints` (list) | `MOBEE_EXTRA_MINTS=a,b` |
//! | `allow_real_mints` | `MOBEE_ALLOW_REAL_MINTS` |
//! | `profile.name` | `MOBEE_PROFILE__NAME` |
//! | `seller.rate_sats` | `MOBEE_SELLER__RATE_SATS` |
//! | `seller.agent_command` (list) | `MOBEE_SELLER__AGENT_COMMAND=claude,--flag` |
//! | `seller_announce.command` (list) | `MOBEE_SELLER_ANNOUNCE__COMMAND=…` |
//! | `telemetry.mirror_file` | `MOBEE_TELEMETRY__MIRROR_FILE` |
//! | `seller_heartbeat.interval_secs` | `MOBEE_SELLER_HEARTBEAT__INTERVAL_SECS` |
//! | `seller_preflight.boot_push_preflight` | `MOBEE_SELLER_PREFLIGHT__BOOT_PUSH_PREFLIGHT` |
//! | `contribution.allowed_paths` (list) | `MOBEE_CONTRIBUTION__ALLOWED_PATHS=…` |
//!
//! List fields comma-split only for the paths in [`LIST_ENV_KEYS`]. The `agents` map is file-only
//! via env: its keys are dynamic, so a nested `argv` list path cannot be pre-registered for
//! splitting. `MOBEE_`-prefixed operational/test seams ([`RESERVED_ENV_VARS`], e.g. `MOBEE_HOME`)
//! are excluded from the config layer.
//!
//! ## Minimal env-only boot (file-less container)
//!
//! With no `config.toml`, the built-in defaults already boot a **buyer** (testnut mint, mobee-relay,
//! budget caps). A **seller** additionally needs the seller table, whose minimal env set is:
//! `MOBEE_SELLER__AGENT_COMMAND`, `MOBEE_SELLER__RATE_SATS`, `MOBEE_SELLER__GIT_REMOTE`. The key is
//! still auto-generated on bootstrap (or supplied out-of-band); `NOSTR_PRIVATE_KEY` handling is
//! unchanged and never read here.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Open-market demo relay.
pub const DEFAULT_RELAY_URL: &str = "wss://mobee-relay.orveth.dev";
/// Standing CDK test mint — its bolt11 invoices auto-settle, so the default moves no real money.
/// The specific host may change; the load-bearing rule is the class: the default is a test/dev mint.
pub const DEFAULT_MINT_URL: &str = "https://testnut.cashudevkit.org";
/// Dead testnut host — bootstrap migrates config.toml away from this.
pub const DEAD_TESTNUT_MINT_HOST: &str = "testnut.cashu.space";
/// Conservative per-job spend cap (sats) until config is tuned.
pub const DEFAULT_PER_JOB_BUDGET_SATS: u64 = 21;
/// Conservative rolling/session total spend cap (sats).
pub const DEFAULT_TOTAL_BUDGET_SATS: u64 = 100;

const CONFIG_FILE: &str = "config.toml";
const KEY_FILE: &str = "key";
const WALLET_DIR: &str = "wallet";

/// Failure while resolving or bootstrapping the packaged home.
#[derive(Debug)]
pub enum HomeError {
    Io(String),
    Config(String),
    Key(String),
}

impl std::fmt::Display for HomeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "home io error: {message}"),
            Self::Config(message) => write!(formatter, "home config error: {message}"),
            Self::Key(message) => write!(formatter, "home key error: {message}"),
        }
    }
}

impl std::error::Error for HomeError {}

/// Optional buyer identity metadata (`[profile]` in config.toml).
///
/// Absent by default — fresh bootstrap does **not** invent a name. Kind-0 names are
/// untrusted display metadata only; decision paths must key on hex pubkey alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
}

/// Default relay-git base (delivery). Live on mobee-relay (`/git/<owner>/<repo>.git`).
pub const DEFAULT_RELAY_GIT_BASE: &str = "https://mobee-relay.orveth.dev/git";
/// Shared leaf name — NOT used as default (relay name registry is global).
pub const DEFAULT_RELAY_GIT_REPO: &str = "mobee-seller";

/// Seller daemon config (`[seller]` in config.toml). Key never lives here.
///
/// `agent_command` MUST be an argv array — a TOML string/shell value is refused at parse
/// (no-shell by construction).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SellerConfig {
    #[serde(deserialize_with = "deserialize_agent_command_argv")]
    pub agent_command: Vec<String>,
    pub rate_sats: u64,
    pub git_remote: String,
    /// Job deadline override (seconds). Default: offer `deadline_unix`, else ~600s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_timeout_secs: Option<u64>,
    /// Optional preset label (`claude` | `cursor` | `codex`) for rediscovery / status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Opt-in to claim untargeted/open offers. Default **false** (targeted-only).
    #[serde(default)]
    pub claim_open_pool: bool,
    /// Backfill window (seconds) for the seller's UNTARGETED (open-pool) offer-kind offer
    /// filter. On (re)subscribe the open-pool filter requests stored offers dated at/after
    /// `now - this`, so a daemon started AFTER an open-pool offer was posted still SEES it
    /// (and claims it iff every money-safety guard passes: not deadline-expired, clears the
    /// rate floor, not already delivered/settled, not live-claimed by another seller).
    /// Default **1200** (20 min). **`0` = live-only** — byte-identical pre-backfill shape
    /// (`since(now)` + `limit(0)`): no stored open-pool offers, only ones posted while the
    /// daemon runs. The TARGETED (`#p==self`) filter is NOT affected by this knob — it keeps
    /// its original full-history backfill at all values (stored targeted offers are addressed
    /// to this seller); the classify-level deadline-expiry refusal is the staleness guard on
    /// both paths.
    #[serde(default = "default_offer_backfill_secs")]
    pub offer_backfill_secs: u64,
    /// Opt-in to the contribution (freelance-PR fork) path. Default **true**. When
    /// **false** the daemon behaves as a seller WITHOUT contribution support: it feedback-kind
    /// `status=error`s a `job-class=contribution` offer instead of running it as from-scratch
    /// (interop courtesy — NOT a security control; buyer refusal is the boundary).
    #[serde(default = "default_contribution_enabled")]
    pub contribution_enabled: bool,
}

/// Persistent-seller-memory config (`[seller_memory]` section): the read-on-start +
/// retro-write-back knobs and the two plugin seams (prompt template paths). Every field has a
/// serde default so a config written before this section existed parses to the shipped defaults.
///
/// Placed top-level on `MobeeConfig` rather than nested under `[seller]`, so it needs no required
/// field on `SellerConfig`; the knobs and seams are identical either way.
///
/// This is **diagnostic/economic** context only. Nothing here ever feeds the pay gate, the
/// journal, or the receipt bind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SellerMemoryConfig {
    /// Inline the distilled `MEMORY.md` index into the agent's job prompt at start. Default
    /// **on**; when **false** the composed prompt is byte-identical to the memory-off output.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,
    /// Run one best-effort retro agent turn after a delivered-**paid** job to update memory.
    /// Default **on**; gated separately from `memory_enabled` (the read path is cheap, the retro
    /// turn costs a model call). Never blocks or affects the money path.
    #[serde(default = "default_retro_enabled")]
    pub retro_enabled: bool,
    /// Plugin seam: template for the retro/distiller prompt. Unset ⇒ the in-repo default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retro_prompt_path: Option<PathBuf>,
    /// Plugin seam: template framing how `MEMORY.md` is inlined at job start. Unset ⇒ in-repo
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_on_start_template_path: Option<PathBuf>,
}

impl Default for SellerMemoryConfig {
    fn default() -> Self {
        Self {
            memory_enabled: default_memory_enabled(),
            retro_enabled: default_retro_enabled(),
            retro_prompt_path: None,
            read_on_start_template_path: None,
        }
    }
}

/// Seller lifecycle **announce** config (`[seller_announce]` section). Wires the daemon's
/// structured lifecycle events (online/claimed/delivered/collected/refused/reconcile-released/
/// job-failed) to a pluggable external sink command that receives one JSON event on stdin.
///
/// NOTE (same build judgment call as [`SellerMemoryConfig`]): the natural spelling would nest
/// this under `[seller]`, but `SellerConfig`'s literal is constructed in `seller.rs` — a money-
/// path file the gateway build must not touch. Placing it top-level as `[seller_announce]` on
/// `MobeeConfig` (built only via `Default`) delivers the identical knob without touching any
/// money file. Cosmetic nesting only; behavior is unchanged.
///
/// **Feature OFF by default**: an absent section (or an empty `command`) means the daemon emits
/// nothing and spawns no process — byte-identical behavior to before the feature existed. This is
/// diagnostic/observability context only; nothing here ever feeds the pay gate, journal, or
/// receipt bind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SellerAnnounceConfig {
    /// Sink command as an argv array (no-shell by construction, like `agent_command`). Empty ⇒
    /// feature OFF. Each lifecycle event spawns this command with the event JSON on stdin.
    #[serde(default)]
    pub command: Vec<String>,
    /// Upper bound (ms) the daemon waits for one sink invocation before killing it. Emission is
    /// always off the event loop (its own detached thread), so this bounds only that thread — the
    /// seller loop is never blocked regardless. Default **2000**.
    #[serde(default = "default_announce_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for SellerAnnounceConfig {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            timeout_ms: default_announce_timeout_ms(),
        }
    }
}

impl SellerAnnounceConfig {
    /// True when every field is at its shipped default (so config.toml stays clean — the section
    /// only serializes once an operator sets a sink command or a non-default bound).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// True when a sink command is configured (feature ON).
    pub fn is_enabled(&self) -> bool {
        !self.command.is_empty()
    }
}

/// serde default for [`SellerAnnounceConfig::timeout_ms`] — a 2s bound on one sink invocation.
pub fn default_announce_timeout_ms() -> u64 {
    2000
}

/// Seller **brain/episode telemetry** config (`[telemetry]` section). Wires every captured
/// [`Episode`](crate::episode::Episode) — the per-job reasoning + economics record already written
/// to `episodes.jsonl` — to a live stream so an operator can watch what is going on inside a
/// mobee's brain: a pluggable sink command (one JSON event on stdin, same exec/timeout contract as
/// [`SellerAnnounceConfig`]) and/or an append-only JSONL mirror file. See [`crate::telemetry`].
///
/// **Feature ON by default** (`enabled = true`): the channel is armed. It only produces output
/// once a `command` and/or `mirror_file` is configured — with both unset, `enabled` alone emits
/// nowhere (and `episodes.jsonl` is unaffected either way). This is deliberate: telemetry is the
/// live wire over the top of the on-disk episode log, not a second copy of it — so the default
/// does not silently duplicate `episodes.jsonl` to a new file.
///
/// NOTE (same money-path build boundary as [`SellerMemoryConfig`] / [`SellerAnnounceConfig`]):
/// top-level on `MobeeConfig` (built only via `Default`) so no money-path file is touched.
///
/// Diagnostic/observability only, sharing the episode's guarantees: an event NEVER carries a
/// token/key/proof-secret (it wraps an `Episode`, which holds none — see `episode.rs`), emission is
/// best-effort off the hot path, and a sink/mirror failure never blocks or loses the
/// `episodes.jsonl` append (the caller performs that FIRST). Nothing here ever feeds the pay gate,
/// journal, or receipt bind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Arm the telemetry channel. Default **true**. When false, no event is emitted or mirrored
    /// (episodes.jsonl is unaffected).
    #[serde(default = "default_telemetry_enabled")]
    pub enabled: bool,
    /// Sink command as an argv array (no-shell by construction, like `agent_command`). Empty ⇒ no
    /// sink process is spawned. Each episode spawns this command with the event JSON on stdin.
    #[serde(default)]
    pub command: Vec<String>,
    /// Upper bound (ms) the emitter waits for one sink invocation before killing it. Emission is
    /// off the hot path (its own detached thread), so this bounds only that thread — the seller
    /// loop and the episode append are never blocked regardless. Default **2000**.
    #[serde(default = "default_telemetry_timeout_ms")]
    pub timeout_ms: u64,
    /// Optional append-only JSONL mirror path. Unset ⇒ no mirror. When set, each event is durably
    /// appended to this file in addition to (or instead of) the sink command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_file: Option<PathBuf>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: default_telemetry_enabled(),
            command: Vec::new(),
            timeout_ms: default_telemetry_timeout_ms(),
            mirror_file: None,
        }
    }
}

impl TelemetryConfig {
    /// True when every field is at its shipped default (so config.toml stays clean — the section
    /// only serializes once an operator points it somewhere or changes the bound/enablement).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// True when the channel is armed AND has somewhere to emit (a sink command or a mirror file).
    /// `enabled` alone (no command, no mirror) is armed-but-unpointed and emits nowhere.
    pub fn is_active(&self) -> bool {
        self.enabled && (!self.command.is_empty() || self.mirror_file.is_some())
    }
}

/// serde default for [`TelemetryConfig::enabled`] — the brain-telemetry channel is ON by default.
pub fn default_telemetry_enabled() -> bool {
    true
}

/// serde default for [`TelemetryConfig::timeout_ms`] — a 2s bound on one sink invocation.
pub fn default_telemetry_timeout_ms() -> u64 {
    2000
}

/// `[seller_heartbeat]` — cadence + enablement for the addressable kind-30340 liveness event.
/// **Feature ON by default**: a running seller advertises liveness every
/// [`interval_secs`](SellerHeartbeatConfig::interval_secs) seconds. The heartbeat is
/// diagnostic/discovery context only — publish failures log-and-continue and it never blocks the
/// job loop, feeds the pay gate, or binds a receipt. Tests can override the cadence/enablement
/// via [`crate::heartbeat::HEARTBEAT_INTERVAL_ENV`] / [`crate::heartbeat::HEARTBEAT_ENABLED_ENV`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SellerHeartbeatConfig {
    /// Publish heartbeats while the daemon runs. Default **true**.
    #[serde(default = "default_heartbeat_enabled")]
    pub enabled: bool,
    /// Cadence in seconds. Default **300** (~5 min).
    #[serde(default = "default_heartbeat_interval_secs")]
    pub interval_secs: u64,
}

impl Default for SellerHeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: default_heartbeat_enabled(),
            interval_secs: default_heartbeat_interval_secs(),
        }
    }
}

impl SellerHeartbeatConfig {
    /// True when every field is at its shipped default (so config.toml stays clean — the section
    /// only serializes once an operator sets a non-default knob).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// serde default for [`SellerHeartbeatConfig::enabled`] — heartbeats ON.
pub fn default_heartbeat_enabled() -> bool {
    true
}

/// serde default for [`SellerHeartbeatConfig::interval_secs`] — 300s (~5 min).
pub fn default_heartbeat_interval_secs() -> u64 {
    300
}

/// Boot-time push-preflight config (`[seller_preflight]` section). Gates the seller daemon's
/// one-shot WRITE-auth probe at startup (a `git push --dry-run` against the seller's relay-git
/// canonical repo) so environment breakage — most notably git < 2.54 silently dropping the
/// Authorization credential on the git-receive-pack POST (reads work, pushes 401) — surfaces at
/// BOOT instead of mid-job.
///
/// NOTE (same money-path build boundary as [`SellerMemoryConfig`] / [`SellerAnnounceConfig`]): the
/// natural spelling would nest this under `[seller]`, but `SellerConfig`'s literal is constructed
/// in `seller.rs` — a money-path file this change must not touch. A new required field there would
/// force editing that literal. Placing it top-level as `[seller_preflight]` on `MobeeConfig` (built
/// only via `Default`) delivers the identical knob without touching any money file. Cosmetic only;
/// the probe is diagnostic — it NEVER feeds the pay gate, journal, or receipt bind, and NEVER
/// refuses boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SellerPreflightConfig {
    /// Run the boot-time dry-run push probe. Default **true**. Set false (or the env override
    /// `MOBEE_SELLER_BOOT_PUSH_PREFLIGHT=0`) to skip — e.g. tests, or air-gapped first boots.
    #[serde(default = "default_boot_push_preflight")]
    pub boot_push_preflight: bool,
}

impl Default for SellerPreflightConfig {
    fn default() -> Self {
        Self {
            boot_push_preflight: default_boot_push_preflight(),
        }
    }
}

impl SellerPreflightConfig {
    /// True when every field is at its shipped default (so config.toml stays clean — the section is
    /// only serialized once an operator sets a non-default knob).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// serde default for [`SellerPreflightConfig::boot_push_preflight`] — probe ON.
pub fn default_boot_push_preflight() -> bool {
    true
}

impl SellerMemoryConfig {
    /// True when every field is at its shipped default (so config.toml stays clean — the section
    /// is only serialized once an operator sets a non-default knob).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// serde default for [`SellerMemoryConfig::memory_enabled`] — read-on-start ON.
pub fn default_memory_enabled() -> bool {
    true
}

/// serde default for [`SellerMemoryConfig::retro_enabled`] — retro write-back ON.
pub fn default_retro_enabled() -> bool {
    true
}

/// Default for [`SellerConfig::contribution_enabled`] — contribution support ON.
pub fn default_contribution_enabled() -> bool {
    true
}

/// serde default for [`SellerConfig::offer_backfill_secs`]: 1200s (20 min). A `[seller]` block
/// written before this field existed parses to this default; `0` must be set explicitly.
pub fn default_offer_backfill_secs() -> u64 {
    1200
}

/// Per-seller NIP-34 `d` / path leaf. Relay `.names/` registry is GLOBAL across
/// owners — a shared constant like `mobee-seller` collides and seeds fail silently.
pub fn default_relay_git_repo_id(seller_pubkey_hex: &str) -> String {
    let pk = seller_pubkey_hex.trim().to_ascii_lowercase();
    let short = &pk[..16.min(pk.len())];
    format!("m{short}")
}

/// Build the default relay-git remote for a seller pubkey (self-owned namespace).
pub fn default_relay_git_remote(seller_pubkey_hex: &str) -> String {
    let pk = seller_pubkey_hex.trim().to_ascii_lowercase();
    let repo = default_relay_git_repo_id(&pk);
    format!("{DEFAULT_RELAY_GIT_BASE}/{pk}/{repo}.git")
}

/// Repo `d`-tag / path leaf for a relay-git remote (`…/git/<owner>/<repo>[.git]`).
pub fn relay_git_repo_id(remote_url: &str) -> Option<String> {
    let lower = remote_url.trim().to_ascii_lowercase();
    let idx = lower.find("/git/")?;
    let prefix_len = "/git/".len();
    let rest = remote_url.trim().get(idx + prefix_len..)?;
    let mut parts = rest.split('/').filter(|p| !p.is_empty());
    let _owner = parts.next()?;
    let mut repo = parts.next()?.to_owned();
    if let Some(stripped) = repo.strip_suffix(".git") {
        repo = stripped.to_owned();
    }
    if repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(repo)
}

fn deserialize_agent_command_argv<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, SeqAccess, Visitor};
    use std::fmt;

    struct ArgvVisitor;

    impl<'de> Visitor<'de> for ArgvVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("argv array (not a shell string)")
        }

        fn visit_str<E: de::Error>(self, _value: &str) -> Result<Self::Value, E> {
            Err(E::custom(
                "agent_command must be an argv array, not a string/shell value",
            ))
        }

        fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
            self.visit_str(&value)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                out.push(item);
            }
            if out.is_empty() {
                return Err(de::Error::custom("agent_command argv must be non-empty"));
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(ArgvVisitor)
}

/// One custom agent preset (`[agents.<name>] argv = [...]`). The argv is a launch command
/// for the seller ACP driver — same no-shell argv-array rule as `agent_command`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPresetConfig {
    #[serde(deserialize_with = "deserialize_agent_command_argv")]
    pub argv: Vec<String>,
}

/// Buyer-facing packaged config (`~/.mobee/config.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MobeeConfig {
    /// Open-market relay. Absent in the file ⇒ the built-in [`DEFAULT_RELAY_URL`].
    #[serde(default = "default_relay_url")]
    pub relay_url: String,
    /// Seller-side accept policy: the mints this seller will accept payment at. The first
    /// entry is the mint the seller advertises first and also the buyer-side wallet default
    /// mint (read via [`MobeeConfig::default_mint`]). Defaults to `[DEFAULT_MINT_URL]`.
    ///
    /// NOTE: distinct from `extra_mints`. `accepted_mints` is the SELLER accept-policy list;
    /// `extra_mints` is the BUYER wallet's *additional allowed* mints. They are separate
    /// fields with separate meanings and are never merged or repurposed for one another.
    #[serde(default = "default_accepted_mints")]
    pub accepted_mints: Vec<String>,
    /// Per-job spend cap (sats). Absent ⇒ the built-in [`DEFAULT_PER_JOB_BUDGET_SATS`].
    #[serde(default = "default_per_job_budget_sats")]
    pub per_job_budget_sats: u64,
    /// Rolling/session spend cap (sats). Absent ⇒ the built-in [`DEFAULT_TOTAL_BUDGET_SATS`].
    #[serde(default = "default_total_budget_sats")]
    pub total_budget_sats: u64,
    /// Opt-in additional mints for the BUYER wallet (`mobee wallet mints add`). The buyer's
    /// default mint stays the first `accepted_mints` entry ([`MobeeConfig::default_mint`]);
    /// never invents spendable credit by itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_mints: Vec<String>,
    /// REAL-MONEY SWITCH (issue #49). When `false` (default — the safety posture) the seller
    /// `accepted_mints` boot fence and the buyer pay-path mint resolution admit ONLY the
    /// testnut/dev allow-list ([`DEFAULT_MINT_URL`]); a real mint is refused fail-closed. When
    /// `true` (deliberate operator opt-in) any well-formed `https://` mint URL
    /// is admitted — this is the switch that lets real sats move. It flips ONLY the allow-list
    /// check; every other money gate (creq membership, redeem guard token==payload mint, dust
    /// guard, budget caps, co-signatures) is unchanged.
    #[serde(default)]
    pub allow_real_mints: bool,
    /// Optional `[profile] name / about`. Skipped when absent so fresh homes stay unnamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileConfig>,
    /// Optional `[seller]` daemon config. Absent until `mobee sell` setup writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seller: Option<SellerConfig>,
    /// Optional `[agents]` table of custom presets: name -> `{ argv = [...] }`. A custom
    /// entry named after a built-in preset (claude|cursor|codex) OVERRIDES that built-in.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, AgentPresetConfig>,
    /// `[seller_memory]` config (read-on-start + retro seams). Defaults when absent.
    #[serde(default, skip_serializing_if = "SellerMemoryConfig::is_default")]
    pub seller_memory: SellerMemoryConfig,
    /// `[seller_announce]` lifecycle-event sink config. Defaults (feature OFF) when absent.
    #[serde(default, skip_serializing_if = "SellerAnnounceConfig::is_default")]
    pub seller_announce: SellerAnnounceConfig,
    /// `[telemetry]` brain/episode stream config. Defaults (armed, no sink/mirror) when absent.
    #[serde(default, skip_serializing_if = "TelemetryConfig::is_default")]
    pub telemetry: TelemetryConfig,
    /// `[seller_heartbeat]` addressable kind-30340 liveness config. Defaults (ON, 300s) when absent.
    #[serde(default, skip_serializing_if = "SellerHeartbeatConfig::is_default")]
    pub seller_heartbeat: SellerHeartbeatConfig,
    /// `[seller_preflight]` boot push-probe config. Defaults (probe ON) when absent.
    #[serde(default, skip_serializing_if = "SellerPreflightConfig::is_default")]
    pub seller_preflight: SellerPreflightConfig,
    /// Optional buyer-side contribution content policy (the content-policy hook). Absent
    /// ⇒ the FLOOR (refuse only empty diffs). Present ⇒ tighten pre-pay with a path allowlist /
    /// forbidden paths / max diff size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<ContributionPolicyConfig>,
}

/// Buyer-side content policy for contribution verify (the content-policy hook). Maps 1:1
/// to `contribution::ContentPolicy`; kept as a plain config type so `home` need not depend on the
/// git-delivery feature. All fields default to the floor (allow all, forbid none, no cap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ContributionPolicyConfig {
    /// Non-empty ⇒ every changed path MUST lie under one of these prefixes (out-of-scope refuse).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<String>,
    /// A changed path under any of these prefixes is refused (checked before the allowlist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_paths: Vec<String>,
    /// Refuse when summed churn exceeds this many units. `None` ⇒ no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_diff_bytes: Option<u64>,
}

/// Serde/default seed for [`MobeeConfig::accepted_mints`]: exactly the current testnut
/// default, so an operator who configures nothing behaves identically to today.
fn default_accepted_mints() -> Vec<String> {
    vec![DEFAULT_MINT_URL.to_owned()]
}

/// Serde default for [`MobeeConfig::relay_url`] — the built-in [`DEFAULT_RELAY_URL`].
fn default_relay_url() -> String {
    DEFAULT_RELAY_URL.to_owned()
}

/// Serde default for [`MobeeConfig::per_job_budget_sats`] — [`DEFAULT_PER_JOB_BUDGET_SATS`].
fn default_per_job_budget_sats() -> u64 {
    DEFAULT_PER_JOB_BUDGET_SATS
}

/// Serde default for [`MobeeConfig::total_budget_sats`] — [`DEFAULT_TOTAL_BUDGET_SATS`].
fn default_total_budget_sats() -> u64 {
    DEFAULT_TOTAL_BUDGET_SATS
}

/// The single real-mint fence predicate (issue #49), shared by the seller `accepted_mints` boot
/// check and the buyer pay-path mint resolution so both sides gate on the SAME rule.
///
/// - `allow_real_mints == false` (default safety posture): only the testnut/dev allow-list — today
///   that is exactly [`DEFAULT_MINT_URL`].
/// - `allow_real_mints == true` (operator opt-in real-money switch): any well-formed `https://`
///   mint URL. Full URL validity is re-checked downstream (`MintUrl::from_str` / `Wallet::new`);
///   this predicate only decides the POLICY (the testnut/dev allow-list vs any-https).
pub fn mint_allowed(mint_url: &str, allow_real_mints: bool) -> bool {
    if allow_real_mints {
        mint_url
            .strip_prefix("https://")
            .is_some_and(|host| !host.is_empty())
    } else {
        mint_url == DEFAULT_MINT_URL
    }
}

impl MobeeConfig {
    /// Buyer-side default mint: the first accepted mint. Falls back to [`DEFAULT_MINT_URL`]
    /// only if the list is empty (boot validation refuses an empty list for sellers). Buyer
    /// wallet ops read a single default mint through this accessor; the seller accept policy
    /// is the full `accepted_mints` list.
    pub fn default_mint(&self) -> &str {
        self.accepted_mints
            .first()
            .map(String::as_str)
            .unwrap_or(DEFAULT_MINT_URL)
    }
}

impl Default for MobeeConfig {
    fn default() -> Self {
        Self {
            relay_url: DEFAULT_RELAY_URL.to_owned(),
            accepted_mints: default_accepted_mints(),
            per_job_budget_sats: DEFAULT_PER_JOB_BUDGET_SATS,
            total_budget_sats: DEFAULT_TOTAL_BUDGET_SATS,
            extra_mints: Vec::new(),
            allow_real_mints: false,
            profile: None,
            seller: None,
            agents: BTreeMap::new(),
            seller_memory: SellerMemoryConfig::default(),
            seller_announce: SellerAnnounceConfig::default(),
            telemetry: TelemetryConfig::default(),
            seller_heartbeat: SellerHeartbeatConfig::default(),
            seller_preflight: SellerPreflightConfig::default(),
            contribution: None,
        }
    }
}

/// Resolved packaged home after bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobeeHome {
    pub root: PathBuf,
    pub config: MobeeConfig,
    pub key_path: PathBuf,
    pub wallet_dir: PathBuf,
    /// True when this bootstrap call created the key file.
    pub key_created: bool,
}

/// Default home root: `MOBEE_HOME` if set, else `~/.mobee`.
pub fn default_home_dir() -> Result<PathBuf, HomeError> {
    if let Ok(override_dir) = std::env::var("MOBEE_HOME") {
        let path = PathBuf::from(override_dir);
        if path.as_os_str().is_empty() {
            return Err(HomeError::Io("MOBEE_HOME is empty".into()));
        }
        return Ok(path);
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| HomeError::Io("HOME is unset and MOBEE_HOME was not provided".into()))?;
    Ok(PathBuf::from(home).join(".mobee"))
}

/// Ensure `root` exists with config, key (`0600`), and `wallet/` dir.
///
/// Idempotent: existing config/key are left in place except dead-mint migration
/// (`testnut.cashu.space` → [`DEFAULT_MINT_URL`]). The persisted `config.toml` is the file layer;
/// the returned [`MobeeHome::config`] additionally carries the `MOBEE_*` environment overlay (see
/// the module docs). Never returns the secret key.
pub fn bootstrap(root: impl AsRef<Path>) -> Result<MobeeHome, HomeError> {
    let root = root.as_ref().to_path_buf();
    fs::create_dir_all(&root).map_err(|error| HomeError::Io(error.to_string()))?;

    let config_path = root.join(CONFIG_FILE);
    let key_path = root.join(KEY_FILE);
    let wallet_dir = root.join(WALLET_DIR);

    let file_config = if config_path.exists() {
        let mut config = load_config(&config_path)?;
        if migrate_dead_mint_url(&mut config) {
            write_config(&config_path, &config)?;
        }
        config
    } else {
        let config = MobeeConfig::default();
        write_config(&config_path, &config)?;
        config
    };

    fs::create_dir_all(&wallet_dir).map_err(|error| HomeError::Io(error.to_string()))?;

    let key_created = if key_path.exists() {
        validate_existing_key(&key_path)?;
        false
    } else {
        write_new_key(&key_path)?;
        true
    };

    let config = apply_env_layer(&file_config, config_env_from_process())?;

    Ok(MobeeHome {
        root,
        config,
        key_path,
        wallet_dir,
        key_created,
    })
}

/// Rewrite dead `.cashu.space` testnut hosts to [`DEFAULT_MINT_URL`] across every
/// `accepted_mints` entry. Returns true when any entry changed.
pub fn migrate_dead_mint_url(config: &mut MobeeConfig) -> bool {
    let mut changed = false;
    for mint in &mut config.accepted_mints {
        if mint.to_ascii_lowercase().contains(DEAD_TESTNUT_MINT_HOST) {
            *mint = DEFAULT_MINT_URL.to_owned();
            changed = true;
        }
    }
    changed
}

/// Back-compat: a legacy config carrying the single top-level `mint_url = "…"` (pre-
/// `accepted_mints`) folds into `accepted_mints = ["<that value>"]` when the file does not already
/// carry an `accepted_mints` key — the modern list wins when both are present. Never silently drops
/// a configured mint. The legacy key is removed from the table afterward so the typed parse (which
/// refuses unknown fields) never sees it.
fn fold_legacy_mint_url(table: &mut toml::Table) {
    let Some(legacy) = table.remove("mint_url") else {
        return;
    };
    if table.contains_key("accepted_mints") {
        return;
    }
    if let Some(mint) = legacy.as_str() {
        table.insert(
            "accepted_mints".to_owned(),
            toml::Value::Array(vec![toml::Value::String(mint.to_owned())]),
        );
    }
}

/// Hex-encode the secp256k1 x-only/public view is deferred; this returns the *public* key
/// only when a caller supplies a derived pubkey. For bootstrap status we expose whether a
/// key file exists — use [`read_secret_key_hex`] only inside trusted surfaces that never log it.
pub fn key_file_present(home: &MobeeHome) -> bool {
    home.key_path.is_file()
}

/// Read the secret key hex from disk. Callers must not log, print, or put this in MCP tool output.
pub fn read_secret_key_hex(home: &MobeeHome) -> Result<String, HomeError> {
    let mut file =
        File::open(&home.key_path).map_err(|error| HomeError::Key(error.to_string()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| HomeError::Key(error.to_string()))?;
    let secret = contents.trim().to_owned();
    validate_secret_hex(&secret)?;
    Ok(secret)
}

/// Hex-encode the buyer's nostr public key derived from the packaged secret.
/// Safe to return on MCP surfaces (not secret material).
#[cfg(feature = "wallet")]
pub fn public_key_hex(home: &MobeeHome) -> Result<String, HomeError> {
    let secret = read_secret_key_hex(home)?;
    let keys = nostr_sdk::Keys::parse(&secret)
        .map_err(|error| HomeError::Key(format!("key parse for pubkey: {error}")))?;
    Ok(keys.public_key().to_hex())
}

/// The FILE layer: read `config.toml` into the typed [`MobeeConfig`]. Absent fields fall back to
/// the built-in defaults (so this is already the defaults→file merge); unknown fields refuse with
/// the offending key named. The legacy single `mint_url` folds into `accepted_mints` first.
fn load_config(path: &Path) -> Result<MobeeConfig, HomeError> {
    let raw = fs::read_to_string(path).map_err(|error| HomeError::Config(error.to_string()))?;
    parse_config_toml(&raw)
}

/// Parse a `config.toml` document into the file-layer [`MobeeConfig`]. Fold legacy `mint_url`, then
/// typed-parse under `deny_unknown_fields` so any other unknown key (at any depth) refuses.
fn parse_config_toml(raw: &str) -> Result<MobeeConfig, HomeError> {
    let mut table: toml::Table =
        toml::from_str(raw).map_err(|error| HomeError::Config(format!("config.toml: {error}")))?;
    fold_legacy_mint_url(&mut table);
    table
        .try_into()
        .map_err(|error| HomeError::Config(format!("config.toml: {error}")))
}

/// `MOBEE_`-prefixed environment variables that are operational/test seams, **not**
/// [`MobeeConfig`] fields (home resolution and the daemon test overrides read these directly). They
/// are excluded from the env config layer so they neither collide with a field nor — under
/// `deny_unknown_fields` — refuse resolution. None of these collide with a real field's canonical
/// `MOBEE_*` spelling, so excluding them costs no config coverage.
const RESERVED_ENV_VARS: &[&str] = &[
    "MOBEE_HOME",
    "MOBEE_HEARTBEAT_INTERVAL_SECS",
    "MOBEE_HEARTBEAT_ENABLED",
    "MOBEE_WRAP_BACKFILL_INTERVAL_SECS",
    "MOBEE_AWARD_SWEEP_INTERVAL_SECS",
    "MOBEE_SELLER_BOOT_PUSH_PREFLIGHT",
    "MOBEE_GIT_CREDENTIAL_NOSTR",
    "MOBEE_ACP_SMOKE",
    "MOBEE_ACP_SMOKE_CMD",
    "MOBEE_EVALS_SNAPSHOT_DIR",
];

/// [`MobeeConfig`] fields whose env value is a comma-separated list. The env source must be told
/// which keys parse into a sequence — a scalar `String` field must not be split. Keyed by the
/// resolved (lowercase, `.`-nested) config path. `agents.<name>.argv` is intentionally absent: the
/// map keys are dynamic and cannot be pre-registered, so multi-token agent argv is file-only.
const LIST_ENV_KEYS: &[&str] = &[
    "accepted_mints",
    "extra_mints",
    "seller.agent_command",
    "seller_announce.command",
    "telemetry.command",
    "contribution.allowed_paths",
    "contribution.forbidden_paths",
];

/// The process environment's config-layer variables: every `MOBEE_`-prefixed var that is not a
/// reserved operational seam ([`RESERVED_ENV_VARS`]).
fn config_env_from_process() -> HashMap<String, String> {
    std::env::vars()
        .filter(|(key, _)| key.starts_with("MOBEE_") && !RESERVED_ENV_VARS.contains(&key.as_str()))
        .collect()
}

/// Overlay the ENV layer on a resolved defaults/file [`MobeeConfig`]. `env` is the pre-filtered
/// `MOBEE_*` map ([`config_env_from_process`] in production; tests inject one). A malformed value
/// (wrong type) or an unknown `MOBEE_<FIELD>` refuses fail-closed, naming the offending key.
fn apply_env_layer(base: &MobeeConfig, env: HashMap<String, String>) -> Result<MobeeConfig, HomeError> {
    let mut environment = config::Environment::with_prefix("MOBEE")
        .prefix_separator("_")
        .separator("__")
        .try_parsing(true)
        .list_separator(",")
        .ignore_empty(true)
        .source(Some(env));
    for key in LIST_ENV_KEYS {
        environment = environment.with_list_parse_key(key);
    }
    config::Config::builder()
        .add_source(
            config::Config::try_from(base).map_err(|error| {
                HomeError::Config(format!("MOBEE_* environment layer: {error}"))
            })?,
        )
        .add_source(environment)
        .build()
        .map_err(|error| HomeError::Config(format!("MOBEE_* environment layer: {error}")))?
        .try_deserialize::<MobeeConfig>()
        .map_err(|error| HomeError::Config(format!("MOBEE_* environment layer: {error}")))
}

fn write_config(path: &Path, config: &MobeeConfig) -> Result<(), HomeError> {
    let raw = toml::to_string_pretty(config)
        .map_err(|error| HomeError::Config(error.to_string()))?;
    // Crash-atomic rewrite: config.toml holds money-adjacent state (budget caps, accepted mints), so
    // a truncating write that dies mid-flush must never leave it empty/half-written. temp → sync →
    // rename → dir-fsync.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    crate::durable::write_atomic(dir, path, raw.as_bytes())
        .map_err(|error| HomeError::Io(error.to_string()))
}

/// Persist an explicit config change to `config.toml`, keeping `MOBEE_*` overrides runtime-only.
///
/// The file is the durable layer; the `MOBEE_*` environment is an overlay applied at load
/// ([`apply_env_layer`]) and never written back. `edit` receives the file-only view (defaults +
/// current file, no env) and applies the caller's explicit change; that view is written, so an
/// env-origin value the caller did not choose cannot leak into the file. `home.config` is then
/// refreshed through the full layer pipeline so the in-process struct still reflects env.
pub fn save_config(
    home: &mut MobeeHome,
    edit: impl FnOnce(&mut MobeeConfig),
) -> Result<(), HomeError> {
    let config_path = home.root.join(CONFIG_FILE);
    let mut file_config = if config_path.exists() {
        load_config(&config_path)?
    } else {
        MobeeConfig::default()
    };
    edit(&mut file_config);
    write_config(&config_path, &file_config)?;
    home.config = apply_env_layer(&file_config, config_env_from_process())?;
    Ok(())
}

/// Reload `config.toml` into `home.config` without touching the key file. Routes through the same
/// layer pipeline as [`bootstrap`]: file layer then `MOBEE_*` environment overlay.
pub fn reload_config(home: &mut MobeeHome) -> Result<(), HomeError> {
    let mut file_config = load_config(&home.root.join(CONFIG_FILE))?;
    if migrate_dead_mint_url(&mut file_config) {
        write_config(&home.root.join(CONFIG_FILE), &file_config)?;
    }
    home.config = apply_env_layer(&file_config, config_env_from_process())?;
    Ok(())
}

fn validate_existing_key(path: &Path) -> Result<(), HomeError> {
    ensure_key_permissions(path)?;
    let mut file = File::open(path).map_err(|error| HomeError::Key(error.to_string()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| HomeError::Key(error.to_string()))?;
    validate_secret_hex(contents.trim())
}

/// Existing keys must be `0600`. Too-open modes are re-chmod'd; if that fails, refuse.
fn ensure_key_permissions(path: &Path) -> Result<(), HomeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata =
            fs::metadata(path).map_err(|error| HomeError::Key(error.to_string()))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            return Ok(());
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)
            .map_err(|error| HomeError::Key(format!(
                "key file permissions too open ({mode:#o}); re-chmod 0600 failed: {error}"
            )))?;
        let after = fs::metadata(path)
            .map_err(|error| HomeError::Key(error.to_string()))?
            .permissions()
            .mode()
            & 0o777;
        if after & 0o077 != 0 {
            return Err(HomeError::Key(format!(
                "key file permissions too open ({mode:#o}); refused to leave open (still {after:#o})"
            )));
        }
    }
    Ok(())
}

fn validate_secret_hex(secret: &str) -> Result<(), HomeError> {
    if secret.len() != 64 {
        return Err(HomeError::Key(format!(
            "secret key must be 64 hex chars, got {}",
            secret.len()
        )));
    }
    if !secret.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(HomeError::Key("secret key must be hex".into()));
    }
    if secret.chars().all(|ch| ch == '0') {
        return Err(HomeError::Key("secret key must be non-zero".into()));
    }
    Ok(())
}

fn write_new_key(path: &Path) -> Result<(), HomeError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| HomeError::Key(error.to_string()))?;
    if bytes.iter().all(|&byte| byte == 0) {
        return Err(HomeError::Key("generated an all-zero key".into()));
    }
    let secret = hex::encode(bytes);

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| HomeError::Key(error.to_string()))?;
    file.write_all(secret.as_bytes())
        .map_err(|error| HomeError::Key(error.to_string()))?;
    file.write_all(b"\n")
        .map_err(|error| HomeError::Key(error.to_string()))?;
    file.sync_all()
        .map_err(|error| HomeError::Key(error.to_string()))?;
    // The key is written once and never rewritten, but its directory ENTRY must be fsync'd or a
    // power-loss right after creation can drop the only copy of the identity/spend key — locking
    // any funds already received. sync_all on the file alone does not make the new entry durable.
    if let Some(parent) = path.parent() {
        crate::durable::sync_dir(parent).map_err(|error| HomeError::Key(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-home-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn agents_table_parses_round_trips_and_refuses_string_argv() {
        // The legacy `mint_url` filler also exercises the fold in `parse_config_toml`.
        let raw = "relay_url = 'r'\nmint_url = 'm'\n\
                   per_job_budget_sats = 1\ntotal_budget_sats = 2\n\
                   [agents.grok]\nargv = ['grok', 'agent', 'stdio']\n";
        let config = parse_config_toml(raw).expect("parse [agents]");
        assert_eq!(
            config.agents.get("grok").map(|p| p.argv.clone()),
            Some(vec!["grok".into(), "agent".into(), "stdio".into()])
        );

        let serialized = toml::to_string_pretty(&config).expect("serialize");
        let reloaded: MobeeConfig = toml::from_str(&serialized).expect("reparse");
        assert_eq!(reloaded, config);

        // Same no-shell rule as `agent_command`: a string argv is refused at parse.
        let shelly = "relay_url = 'r'\nmint_url = 'm'\n\
                      per_job_budget_sats = 1\ntotal_budget_sats = 2\n\
                      [agents.grok]\nargv = 'grok agent stdio'\n";
        assert!(parse_config_toml(shelly).is_err());

        // Absent table stays absent (config.toml stays clean).
        let bare = parse_config_toml(
            "relay_url = 'r'\nmint_url = 'm'\nper_job_budget_sats = 1\ntotal_budget_sats = 2\n",
        )
        .expect("parse bare");
        assert!(bare.agents.is_empty());
        assert!(!toml::to_string_pretty(&bare).expect("ser").contains("[agents"));
    }

    #[test]
    fn bootstrap_writes_defaults_key_and_wallet_dir() {
        let root = temp_home("fresh");
        let _ = fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        assert!(home.key_created);
        assert_eq!(home.config, MobeeConfig::default());
        assert!(home.root.join(CONFIG_FILE).is_file());
        assert!(home.key_path.is_file());
        assert!(home.wallet_dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&home.key_path)
                .expect("key metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let secret = read_secret_key_hex(&home).expect("read key");
        assert_eq!(secret.len(), 64);
    }

    #[test]
    fn bootstrap_is_idempotent_and_preserves_key() {
        let root = temp_home("idempotent");
        let _ = fs::remove_dir_all(&root);
        let first = bootstrap(&root).expect("first");
        let secret = read_secret_key_hex(&first).expect("secret");
        let second = bootstrap(&root).expect("second");
        assert!(!second.key_created);
        assert_eq!(read_secret_key_hex(&second).expect("secret again"), secret);
        assert_eq!(second.config, first.config);
    }

    #[test]
    fn default_home_dir_honors_mobee_home() {
        let root = temp_home("env");
        // Safety: test process isolation — restore after.
        let previous = std::env::var_os("MOBEE_HOME");
        unsafe { std::env::set_var("MOBEE_HOME", &root) };
        let resolved = default_home_dir().expect("resolve");
        match previous {
            Some(value) => unsafe { std::env::set_var("MOBEE_HOME", value) },
            None => unsafe { std::env::remove_var("MOBEE_HOME") },
        }
        assert_eq!(resolved, root);
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_rechmods_too_open_existing_key() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_home("open-key");
        let _ = fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let secret = read_secret_key_hex(&home).expect("secret");

        let mut permissions = fs::metadata(&home.key_path)
            .expect("meta")
            .permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&home.key_path, permissions).expect("chmod 644");

        let again = bootstrap(&root).expect("re-bootstrap must re-chmod or refuse");
        let mode = fs::metadata(&again.key_path)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(read_secret_key_hex(&again).expect("secret again"), secret);
    }

    #[test]
    fn bootstrap_migrates_dead_cashu_space_mint() {
        let root = temp_home("dead-mint");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        let config_path = root.join(CONFIG_FILE);
        let stale = MobeeConfig {
            accepted_mints: vec![format!("https://{DEAD_TESTNUT_MINT_HOST}")],
            ..MobeeConfig::default()
        };
        write_config(&config_path, &stale).expect("write stale");
        let home = bootstrap(&root).expect("bootstrap migrates");
        assert_eq!(home.config.accepted_mints, vec![DEFAULT_MINT_URL.to_owned()]);
        let reloaded = load_config(&config_path).expect("reload");
        assert_eq!(reloaded.accepted_mints, vec![DEFAULT_MINT_URL.to_owned()]);
    }

    #[test]
    fn accepted_mints_default() {
        // A config that names no mint at all yields accepted_mints == [DEFAULT_MINT_URL].
        let config: MobeeConfig = toml::from_str(
            "relay_url = 'r'\nper_job_budget_sats = 1\ntotal_budget_sats = 2\n",
        )
        .expect("parse mint-less config");
        assert_eq!(config.accepted_mints, vec![DEFAULT_MINT_URL.to_owned()]);
        assert_eq!(
            MobeeConfig::default().accepted_mints,
            vec![DEFAULT_MINT_URL.to_owned()]
        );
    }

    #[test]
    fn legacy_mint_url_migrates() {
        // A legacy config carrying only the single `mint_url` loads as accepted_mints=[value].
        let root = temp_home("legacy-mint");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        let config_path = root.join(CONFIG_FILE);
        fs::write(
            &config_path,
            "relay_url = 'r'\nmint_url = 'https://legacy.example'\n\
             per_job_budget_sats = 1\ntotal_budget_sats = 2\n",
        )
        .expect("write legacy");
        let config = load_config(&config_path).expect("load legacy");
        assert_eq!(
            config.accepted_mints,
            vec!["https://legacy.example".to_owned()]
        );
    }

    #[test]
    fn bootstrap_does_not_invent_profile() {
        let root = temp_home("no-profile");
        let _ = fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        assert!(home.config.profile.is_none());
        let raw = fs::read_to_string(home.root.join(CONFIG_FILE)).expect("read");
        assert!(
            !raw.contains("[profile]"),
            "fresh bootstrap must not invent [profile]: {raw}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn save_and_reload_profile_round_trip() {
        let root = temp_home("profile-rt");
        let _ = fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        save_config(&mut home, |config| {
            config.profile = Some(ProfileConfig {
                name: Some("test-buyer".into()),
                about: Some("testnut only".into()),
            });
        })
        .expect("save");
        home.config.profile = None;
        reload_config(&mut home).expect("reload");
        let profile = home.config.profile.expect("profile present");
        assert_eq!(profile.name.as_deref(), Some("test-buyer"));
        assert_eq!(profile.about.as_deref(), Some("testnut only"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn save_does_not_persist_env_override_values() {
        // A field whose value came only from a MOBEE_* env override must stay runtime-only:
        // saving an UNRELATED field must not bake the env value into config.toml.
        let root = temp_home("save-env-noleak");
        let _ = fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");

        // Resolve an env override into the in-process config, as a live process would.
        let file_before = load_config(&home.root.join(CONFIG_FILE)).expect("file before");
        home.config = apply_env_layer(&file_before, env(&[("MOBEE_RELAY_URL", "wss://from-env")]))
            .expect("env layer");
        assert_eq!(home.config.relay_url, "wss://from-env");

        // Save an unrelated field.
        save_config(&mut home, |config| {
            config.profile = Some(ProfileConfig {
                name: Some("buyer".into()),
                about: None,
            });
        })
        .expect("save");

        // The env-origin relay_url is absent from the file; the explicit field is present.
        let raw = fs::read_to_string(home.root.join(CONFIG_FILE)).expect("read");
        assert!(
            !raw.contains("wss://from-env"),
            "env override leaked into config.toml: {raw}"
        );
        let on_disk = load_config(&home.root.join(CONFIG_FILE)).expect("reload file");
        assert_eq!(on_disk.relay_url, DEFAULT_RELAY_URL);
        assert_eq!(
            on_disk.profile.and_then(|profile| profile.name).as_deref(),
            Some("buyer")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn save_persists_explicitly_chosen_field() {
        // The guarantee is only that UNCHOSEN env values do not leak. A value the caller
        // explicitly saves is persisted even when an env var also covers that field.
        let root = temp_home("save-explicit");
        let _ = fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");

        let file_before = load_config(&home.root.join(CONFIG_FILE)).expect("file before");
        home.config = apply_env_layer(&file_before, env(&[("MOBEE_RELAY_URL", "wss://from-env")]))
            .expect("env layer");

        save_config(&mut home, |config| {
            config.relay_url = "wss://chosen".into();
        })
        .expect("save");

        let on_disk = load_config(&home.root.join(CONFIG_FILE)).expect("reload file");
        assert_eq!(
            on_disk.relay_url, "wss://chosen",
            "an explicitly chosen value is persisted"
        );
        assert_ne!(
            on_disk.relay_url, "wss://from-env",
            "the persisted value is the caller's choice, not the env override"
        );
        let _ = fs::remove_dir_all(&root);
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn env_layer_wins_over_file_and_defaults() {
        // FILE layer overrides defaults for one field; DEFAULT stands for another; ENV then wins
        // over both — across a scalar, a numeric, and a list (incl. the legacy-folded mint list).
        let file = parse_config_toml(
            "relay_url = 'wss://from-file'\nper_job_budget_sats = 50\n\
             accepted_mints = ['https://file-mint']\n",
        )
        .expect("file layer");
        // Sanity: defaults<file already merged by the file parse.
        assert_eq!(file.relay_url, "wss://from-file"); // file over default
        assert_eq!(file.total_budget_sats, DEFAULT_TOTAL_BUDGET_SATS); // default stands

        let resolved = apply_env_layer(
            &file,
            env(&[
                ("MOBEE_RELAY_URL", "wss://from-env"),
                ("MOBEE_PER_JOB_BUDGET_SATS", "7"),
                ("MOBEE_ACCEPTED_MINTS", "https://env-a,https://env-b"),
            ]),
        )
        .expect("env layer");

        assert_eq!(resolved.relay_url, "wss://from-env"); // env over file
        assert_eq!(resolved.per_job_budget_sats, 7); // env over file
        assert_eq!(
            resolved.accepted_mints,
            vec!["https://env-a".to_owned(), "https://env-b".to_owned()]
        ); // env list over file list
        assert_eq!(resolved.total_budget_sats, DEFAULT_TOTAL_BUDGET_SATS); // untouched default survives
    }

    #[test]
    fn env_layer_overrides_nested_field() {
        let base = MobeeConfig::default();
        let resolved = apply_env_layer(
            &base,
            env(&[("MOBEE_SELLER_HEARTBEAT__INTERVAL_SECS", "42")]),
        )
        .expect("nested env");
        assert_eq!(resolved.seller_heartbeat.interval_secs, 42);
        assert!(resolved.seller_heartbeat.enabled); // sibling default preserved
    }

    #[test]
    fn env_layer_refuses_malformed_value_naming_the_key() {
        let error = apply_env_layer(
            &MobeeConfig::default(),
            env(&[("MOBEE_PER_JOB_BUDGET_SATS", "not-a-number")]),
        )
        .expect_err("malformed env must refuse");
        let message = error.to_string();
        assert!(
            message.contains("per_job_budget_sats"),
            "error must name the offending key: {message}"
        );
    }

    #[test]
    fn env_layer_refuses_unknown_variable() {
        // A MOBEE_-prefixed var that is neither a field nor a reserved seam fails closed.
        let error = apply_env_layer(
            &MobeeConfig::default(),
            env(&[("MOBEE_NO_SUCH_FIELD", "x")]),
        )
        .expect_err("unknown env must refuse");
        assert!(error.to_string().contains("environment"));
    }

    #[test]
    fn reserved_env_seams_never_reach_the_config_layer() {
        // MOBEE_HOME (and the daemon test seams) map to no field; excluding them is what keeps
        // resolution from refusing when they are set. The filtered map must drop them.
        let raw = env(&[
            ("MOBEE_HOME", "/tmp/x"),
            ("MOBEE_HEARTBEAT_INTERVAL_SECS", "9"),
            ("MOBEE_RELAY_URL", "wss://kept"),
        ]);
        let kept: HashMap<String, String> = raw
            .into_iter()
            .filter(|(key, _)| key.starts_with("MOBEE_") && !RESERVED_ENV_VARS.contains(&key.as_str()))
            .collect();
        assert_eq!(kept.len(), 1);
        assert!(kept.contains_key("MOBEE_RELAY_URL"));
        // And resolution succeeds precisely because the reserved seams were dropped.
        let resolved = apply_env_layer(&MobeeConfig::default(), kept).expect("resolve");
        assert_eq!(resolved.relay_url, "wss://kept");
    }

    #[test]
    fn unknown_toml_field_refuses() {
        let error = parse_config_toml(
            "relay_url = 'r'\nper_job_budget_sats = 1\ntotal_budget_sats = 2\nbogus_field = 5\n",
        )
        .expect_err("unknown TOML field must refuse");
        let message = error.to_string();
        assert!(message.contains("config.toml"), "names the layer: {message}");
        assert!(message.contains("bogus_field"), "names the key: {message}");
    }

    #[test]
    fn env_only_boots_buyer_and_seller_without_a_file() {
        // File-less container: defaults alone already boot a BUYER (mint, relay, budget caps).
        let buyer = apply_env_layer(&MobeeConfig::default(), HashMap::new()).expect("buyer");
        assert!(!buyer.relay_url.is_empty());
        assert!(!buyer.default_mint().is_empty());
        assert!(buyer.per_job_budget_sats > 0);
        assert!(buyer.seller.is_none());

        // A SELLER needs only the seller table's required fields via env.
        let seller = apply_env_layer(
            &MobeeConfig::default(),
            env(&[
                ("MOBEE_SELLER__AGENT_COMMAND", "claude,--headless"),
                ("MOBEE_SELLER__RATE_SATS", "3"),
                ("MOBEE_SELLER__GIT_REMOTE", "https://relay.example/git/x/y.git"),
            ]),
        )
        .expect("seller boots from env alone");
        let seller_cfg = seller.seller.expect("seller table present");
        assert_eq!(
            seller_cfg.agent_command,
            vec!["claude".to_owned(), "--headless".to_owned()]
        );
        assert_eq!(seller_cfg.rate_sats, 3);
        assert_eq!(seller_cfg.git_remote, "https://relay.example/git/x/y.git");
    }
}
