//! Packaged buyer home under `~/.maxplayer` (or `MAXPLAYER_HOME`).
//!
//! First-run bootstrap writes working defaults: a REAL minibits mint, maxplayer-relay, budget caps,
//! autogen key (`0600`), and an empty `wallet/` dir. The secret key is never returned.
//!
//! # Layered configuration
//!
//! [`MaxplayerConfig`] resolves in three layers, later winning:
//!
//! 1. **built-in defaults** — [`MaxplayerConfig::default`].
//! 2. **file** — `~/.maxplayer/config.toml` (if present). Absent fields fall back to the defaults;
//!    unknown fields refuse (`deny_unknown_fields`). The single-mint legacy `mint_url = "…"` key
//!    folds into `accepted_mints`.
//! 3. **environment** — `MAXPLAYER_*` variables. Every field is reachable: uppercase the field path,
//!    prefix `MAXPLAYER_`, join nested fields with `__` (double underscore). Comma-separated for lists.
//!
//! The **entire `MAXPLAYER_` prefix is reserved** for this config: every `MAXPLAYER_*` variable must map
//! to a [`MaxplayerConfig`] field or be a known operational seam ([`RESERVED_ENV_VARS`], e.g.
//! `MAXPLAYER_HOME`). An unrecognized `MAXPLAYER_*` variable is refused fail-closed — never silently
//! ignored — so do not repurpose the prefix for unrelated environment variables. A single `_` where a
//! nested `__` was meant (`MAXPLAYER_SANDBOX_RO_PATHS` vs `MAXPLAYER_SANDBOX__RO_PATHS`) reads as an
//! unknown top-level field and refuses; the refusal names the offending variable and this rule.
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
//! | `relay_url` | `MAXPLAYER_RELAY_URL` |
//! | `accepted_mints` (list) | `MAXPLAYER_ACCEPTED_MINTS=a,b` |
//! | `per_job_budget_sats` | `MAXPLAYER_PER_JOB_BUDGET_SATS` |
//! | `extra_mints` (list) | `MAXPLAYER_EXTRA_MINTS=a,b` |
//! | `allow_real_mints` | `MAXPLAYER_ALLOW_REAL_MINTS` |
//! | `profile.name` | `MAXPLAYER_PROFILE__NAME` |
//! | `seller.rate_sats` | `MAXPLAYER_SELLER__RATE_SATS` |
//! | `seller.agent_command` (list) | `MAXPLAYER_SELLER__AGENT_COMMAND=claude,--flag` |
//! | `seller_announce.command` (list) | `MAXPLAYER_SELLER_ANNOUNCE__COMMAND=…` |
//! | `telemetry.mirror_file` | `MAXPLAYER_TELEMETRY__MIRROR_FILE` |
//! | `seller_heartbeat.interval_secs` | `MAXPLAYER_SELLER_HEARTBEAT__INTERVAL_SECS` |
//! | `seller_preflight.boot_push_preflight` | `MAXPLAYER_SELLER_PREFLIGHT__BOOT_PUSH_PREFLIGHT` |
//! | `buyer.hop_fee_buffer_multiplier` | `MAXPLAYER_BUYER__HOP_FEE_BUFFER_MULTIPLIER` |
//! | `contribution.allowed_paths` (list) | `MAXPLAYER_CONTRIBUTION__ALLOWED_PATHS=…` |
//!
//! List fields comma-split only for the paths in [`LIST_ENV_KEYS`]. The `agents` map is file-only
//! via env: its keys are dynamic, so a nested `argv` list path cannot be pre-registered for
//! splitting. `MAXPLAYER_`-prefixed operational/test seams ([`RESERVED_ENV_VARS`], e.g. `MAXPLAYER_HOME`)
//! are excluded from the config layer.
//!
//! ## Minimal env-only boot (file-less container)
//!
//! With no `config.toml`, the built-in defaults already boot a **buyer** (real minibits mint, maxplayer-relay,
//! budget caps). A **seller** additionally needs the seller table, whose minimal env set is:
//! `MAXPLAYER_SELLER__AGENT_COMMAND`, `MAXPLAYER_SELLER__RATE_SATS`, `MAXPLAYER_SELLER__GIT_REMOTE`. The key is
//! still auto-generated on bootstrap (or supplied out-of-band); `NOSTR_PRIVATE_KEY` handling is
//! unchanged and never read here.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Open-market relay — the maxplayer launch relay.
pub const DEFAULT_RELAY_URL: &str = "wss://relay.maxplayer.ai";
/// Standing CDK test mint — its bolt11 invoices auto-settle, so it moves no real money. Kept as the
/// testnut/dev allow-list anchor: `mint_allowed` admits exactly this when `allow_real_mints` is false.
pub const DEFAULT_MINT_URL: &str = "https://testnut.cashudevkit.org";
/// Shipped default seller mint (issue #378): a REAL minibits mint. Fresh configs accept real sats here
/// by default — paired with `allow_real_mints = true`, without which the fence would refuse this mint.
pub const DEFAULT_MINIBITS_MINT_URL: &str = "https://mint.minibits.cash/Bitcoin";
/// Dead testnut host — bootstrap migrates config.toml away from this.
pub const DEAD_TESTNUT_MINT_HOST: &str = "testnut.cashu.space";
/// Empty-market per-job spend fallback (sats): the cap applied when no market-rate signal exists.
/// Market-rate derivation is a follow-up (#378); until then every fresh config ships this cap.
pub const DEFAULT_PER_JOB_BUDGET_SATS: u64 = 30_000;
/// Suggested seller claim floor (sats) offered by every first-run seller setup path (#487).
/// 100 is the rate buyers are told to post at, so a fresh seller starts level with the market
/// instead of undercutting it. This is a suggested default only — nothing rejects a lower
/// configured `rate_sats`, and the seller may set any value.
pub const DEFAULT_RATE_SATS: u64 = 100;

const CONFIG_FILE: &str = "config.toml";
const KEY_FILE: &str = "key";
const WALLET_DIR: &str = "wallet";

/// Failure while resolving or bootstrapping the packaged home.
#[derive(Debug)]
pub enum HomeError {
    Io(String),
    Config(String),
    Key(String),
    /// The default home moved (`~/.mobee` → `~/.maxplayer`) but the new path is absent while the old
    /// one exists. Refuse to boot rather than silently create an empty home and strand the old
    /// wallet/keys. Carries both paths so the message prints the exact `mv` fix.
    OldHomeNeedsMigration { old: PathBuf, new: PathBuf },
}

impl std::fmt::Display for HomeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "home io error: {message}"),
            Self::Config(message) => write!(formatter, "home config error: {message}"),
            Self::Key(message) => write!(formatter, "home key error: {message}"),
            Self::OldHomeNeedsMigration { old, new } => write!(
                formatter,
                "refusing to start: no home at {new} but a pre-rename home exists at {old}. The \
                 default home moved from ~/.mobee to ~/.maxplayer; move it into place with:\n\n    \
                 mv {old} {new}\n\n(or set MAXPLAYER_HOME to choose a location). Booting with an \
                 empty home would strand the funds and keys in {old}.",
                old = old.display(),
                new = new.display()
            ),
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
    /// Provenance of `about`. `Some(true)` means this is our generated default and may be
    /// regenerated from live config on every seller boot. `Some(false)` and `None` are protected;
    /// `None` includes pre-upgrade text of unknown provenance. To migrate a stale old default,
    /// remove both `about` and `about_generated` from `[profile]`, then restart the seller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about_generated: Option<bool>,
}

/// Deprecated, runtime-ignored schema compatibility for existing `[buzz]` tables. The buzz
/// persona was never wired into production and has been removed; retaining this shape prevents
/// an existing home from failing `deny_unknown_fields` config parsing on upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuzzConfig {
    /// The buzz relay websocket URL (e.g. `wss://buzzrelay.orveth.dev`).
    pub relay_url: String,
    /// Persona display name (kind-0 `name`).
    pub name: String,
    /// Optional human blurb, prepended to the assembled rate card in kind-0 `about`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// Advertised rate (sats/job) shown in the rate card. `None` ⇒ falls back to the `[seller]`
    /// `rate_sats` when a seller is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_sats: Option<u64>,
    /// Human-readable capability tags shown in the rate card (e.g. `["code", "test"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Mint label shown in the rate card, for an operator who wants their own wording. `None` ⇒ the
    /// seat's `accepted_mints` answer, which is what it will really settle in; with neither, the
    /// rate card omits the mint clause rather than guessing (#453).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mint: Option<String>,
    /// Presence heartbeat cadence (seconds). Default **30** (the deployed relay expires presence
    /// after ~90s of silence, so a 30s cadence keeps it live with margin).
    #[serde(default = "default_buzz_heartbeat_secs")]
    pub heartbeat_secs: u64,
}

/// Deprecated `[buzz]` schema default; retained only so old config files remain parse-tolerated.
pub fn default_buzz_heartbeat_secs() -> u64 {
    30
}

/// Default relay-git base (delivery), on the launch relay (`/git/<owner>/<repo>.git`) — the same
/// host the default `relay_url` announces to, so the kind-30617 announce seeds the exact git base
/// the seed probe then checks (#394: splitting announce-host from git-host bricked fresh sellers).
pub const DEFAULT_RELAY_GIT_BASE: &str = "https://relay.maxplayer.ai/git";
/// Shared leaf name — NOT used as default (relay name registry is global).
pub const DEFAULT_RELAY_GIT_REPO: &str = "maxplayer-seller";

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
    /// The harnesses this node enables, in preference order — the multi-harness registry
    /// ([`crate::seller_agents`]). Each entry is a preset name (`claude` | `cursor` | `codex`, or a
    /// custom `[agents.<name>]`). Empty ⇒ the node serves with the single `agent_command` alone. The
    /// first entry doubles as the seller's advertised harness label (rediscovery / status / NIP-89).
    ///
    /// The node advertises this list on its heartbeat and claims, and dispatches a job to the harness
    /// its offer requested. How many awarded jobs run at once is governed by the homogeneous
    /// [`SellerConfig::slots`] (every slot runs whichever harness the job asked for). Issue #378
    /// removed the singular `agent` label field and the per-entry `{ name, slots }` table: this is a
    /// plain list of harness names, and the top-level `slots` is the only concurrency knob.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Opt-in to claim untargeted/open offers. Default **false** (targeted-only).
    ///
    /// This flag decides whether the seat can LOSE a race, and that is what makes it more than a
    /// throughput knob. A targeted offer has exactly one eligible claimant — `rate_gate_allows`
    /// refuses any offer whose `p` tag is not this seat — so a targeted seat that claimed a job
    /// necessarily won it. Opt in and the seat claims offers other seats also claim, so it receives
    /// AWARD and ACCEPT events that select SOMEONE ELSE's claim (its award subscription is
    /// deliberately unscoped when open-pool, #456, so a loser still learns to free its slot).
    /// Binding local state from those events requires knowing they name OUR claim: `on_award` and
    /// `on_accept` each match the event's claim id against this node's published claim id before
    /// recording an award. Recording and claiming the offer proves only that the job is one of ours
    /// (#626). Any handler added to that subscription inherits the same obligation.
    #[serde(default)]
    pub claim_open_pool: bool,
    /// Allowlist of buyer pubkeys (64-hex) whose offers this seller will claim. **Empty/absent =
    /// accept-all** — every buyer is eligible, subject to the usual targeting/rate/harness gates
    /// (the pre-#482 behavior; existing sellers are unaffected). **Populated = a hard fence**: an
    /// offer whose author (the buyer) is not on the list is skipped with a named `NotAllowlisted`
    /// skip reason and NO buyer feedback — a private seller does not advertise why it declined a
    /// stranger. Consulted right after the lapsed-offer refusal, before the rate/harness gates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept_offers_only_from: Vec<String>,
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
    /// Homogeneous execution slots: the maximum number of awarded jobs this node runs
    /// concurrently. Default **3** (issue #378). A slot is RESERVED when the node claims an offer
    /// and released on the job's terminal outcome (delivery/failure), when the buyer awards another
    /// seller, or when a parked claim lapses unawarded. Reserve-at-claim is what makes a fully loaded
    /// node invisible to the market: with no free slot it simply does not claim. Every slot is
    /// identical and runs whichever harness the job asked for — there is no per-slot harness typing
    /// (issue #378 removed the per-entry `{ name, slots }` pool; this homogeneous count is the only
    /// concurrency knob).
    #[serde(default = "default_slots")]
    pub slots: usize,
    /// How long (seconds) a parked, unawarded claim may hold its reserved execution slot before the
    /// lapse sweep reclaims it. Deliberately separate from — and much shorter than — the claim's
    /// on-the-wire publish window, which stays long for relay resilience. `None` ⇒ the built-in
    /// default (see `DEFAULT_CLAIM_AWARD_TIMEOUT_SECS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_award_timeout_secs: Option<u64>,
}

/// Executor sandbox config (`[sandbox]` section): which executor the awarded agent command runs
/// under. Absent ⇒ pass-through — the command runs exactly as configured, byte-identical to no
/// sandbox. Present ⇒ [`SandboxMode`] selects the executor: `launcher` prepends a launcher argv so
/// the command runs inside an OS sandbox, `docker` runs it inside a container mounting only the
/// per-job workdir. Either way the run/exec path never learns which executor it got.
///
/// Top-level on `MaxplayerConfig`, not nested under `[seller]`: `SellerConfig`'s literal is built in
/// the money-path `seller.rs`, which this must not touch (same placement rationale as
/// [`SellerAnnounceConfig`]).
/// `Default` is derived so a caller naming the two or three fields it cares about can spread the
/// rest. Every field is already `#[serde(default)]` — a config file has always been able to omit
/// them — so the derive states in Rust what the deserializer already accepted, and adding a field
/// stops being a mechanical edit at every construction site.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Which executor runs the agent command. `launcher` (default) prepends a launcher argv;
    /// `docker` runs the command inside a container that mounts ONLY the per-job workdir.
    #[serde(default)]
    pub mode: SandboxMode,
    /// `launcher` mode: the launcher argv the agent command runs inside (no-shell argv array, same
    /// rule as `agent_command`: a bare string is refused, and a present array must be non-empty).
    /// Omitted ⇒ pass-through under `launcher` mode. Unused under `docker` mode.
    #[serde(
        default,
        deserialize_with = "deserialize_agent_command_argv",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub launcher: Vec<String>,
    /// `docker` mode: the container image carrying the agent runtime (node + ACP adapter + git +
    /// CA certs — see `docker/maxplayer-sandbox/Dockerfile`). OPTIONAL: omitted ⇒ the binary supplies
    /// the version-pinned default [`crate::seller_exec::DEFAULT_SANDBOX_IMAGE`], so a fresh seller who
    /// sets only `mode = "docker"` gets a working container. Set this ONLY to run a fully-custom image;
    /// it is NOT a version selector (the binary owns the version).
    ///
    /// LOCAL DEVELOPMENT: the default ref pins this build's version (`:v<CARGO_PKG_VERSION>`), which is
    /// NOT published for dev builds — set `image` to a locally-built tag (e.g. `maxplayer-sandbox:latest`)
    /// to override, or docker will fail to pull the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `docker` mode: EXTRA environment variables to carry from the daemon into the container, on
    /// top of the built-in agent-auth set (see `seller_exec::FORWARDED_AGENT_ENV`).
    ///
    /// A host executor inherits the daemon's environment wholesale; a container inherits nothing, so
    /// without this an agent CLI inside the container has no credential and every job fails an auth
    /// error. Named variables only — never the whole environment, which would hand a stranger's job
    /// every secret the daemon happens to hold.
    ///
    /// Only needed for a custom `[agents]` preset whose CLI reads a variable the built-in set does
    /// not name, or to carry a gateway base-URL. Unused under `launcher` mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forward_env: Vec<String>,
    /// `docker` mode: the container runtime to run the job under (`docker run --runtime <name>`).
    /// Omitted ⇒ the daemon's default runtime (`runc`). The v1 sandbox posture sets this to `runsc`
    /// on Linux, where the default container shares the host kernel and gVisor is the primary
    /// boundary; a Mac seat leaves it unset and relies on the platform VM plus the hardening flags
    /// (`--cap-drop=ALL`, `--security-opt no-new-privileges`). The named runtime must be registered
    /// with the daemon (`docker info` → Runtimes); an unregistered name fails the run at spawn.
    /// Unused under `launcher` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// `docker` mode: the dedicated docker network a job's container joins (`docker run --network`).
    /// Omitted ⇒ the daemon default (the shared `bridge` network).
    ///
    /// The network is what makes the #797 egress policy expressible AND enforceable, for two
    /// separate reasons:
    ///
    /// * **A stable interface to scope rules to.** Firewall rules that deny the LAN must match only
    ///   sandbox traffic. A named network has its own `br-*` interface, so every rule is
    ///   interface-scoped and cannot match a service the seller runs.
    /// * **DNS survives the host deny.** On a user-defined network a container resolves through
    ///   docker's embedded resolver at `127.0.0.11`, inside its own netns — no packet crosses the
    ///   bridge to a host or LAN resolver. On the shared default bridge docker instead copies the
    ///   host's `resolv.conf`, so if that names a LAN or host resolver, denying the LAN also denies
    ///   DNS and every job fails to resolve anything.
    ///
    /// A seat that sets this without installing the rules is NOT contained — see
    /// `maxplayer sandbox-net`, and [`crate::sandbox_net`] for what the rules are and why there are
    /// two chains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// `docker` mode: the TCP port range the per-job credential proxy (#647) binds inside, as
    /// `"<start>-<end>"` or a single `"<port>"`. Omitted ⇒ the shipped behaviour, an ephemeral port
    /// chosen by the kernel per job.
    ///
    /// This exists only so a static firewall rule can name the pinhole. The proxy's default bind is
    /// port 0 — a fresh random high port every job — and no `iptables` rule can express "whatever
    /// port the proxy happens to have got". Setting a range narrows the ports the proxy may use so
    /// the host deny can carry one matching exception; leaving it unset changes nothing about how
    /// the proxy binds today.
    ///
    /// The range must be at least as large as the number of jobs that can run concurrently: each
    /// contained job holds its own listener for its lifetime, and a range that runs out fails the
    /// job rather than falling back to a random port (the containment path has no fallbacks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_port_range: Option<String>,
}

/// Which executor the `[sandbox]` section selects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// Prepend a launcher argv to the agent command (Phase-0 behavior; empty launcher = pass-through).
    #[default]
    Launcher,
    /// Run the agent command inside a container that mounts only the per-job workdir.
    Docker,
}

/// Persistent-seller-memory config (`[seller_memory]` section): the read-on-start +
/// retro-write-back knobs and the two plugin seams (prompt template paths). Every field has a
/// serde default so a config written before this section existed parses to the shipped defaults.
///
/// Placed top-level on `MaxplayerConfig` rather than nested under `[seller]`, so it needs no required
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
/// `MaxplayerConfig` (built only via `Default`) delivers the identical knob without touching any
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
/// maxplayer's brain: a pluggable sink command (one JSON event on stdin, same exec/timeout contract as
/// [`SellerAnnounceConfig`]) and/or an append-only JSONL mirror file. See [`crate::telemetry`].
///
/// **Feature ON by default** (`enabled = true`): the channel is armed. It only produces output
/// once a `command` and/or `mirror_file` is configured — with both unset, `enabled` alone emits
/// nowhere (and `episodes.jsonl` is unaffected either way). This is deliberate: telemetry is the
/// live wire over the top of the on-disk episode log, not a second copy of it — so the default
/// does not silently duplicate `episodes.jsonl` to a new file.
///
/// NOTE (same money-path build boundary as [`SellerMemoryConfig`] / [`SellerAnnounceConfig`]):
/// top-level on `MaxplayerConfig` (built only via `Default`) so no money-path file is touched.
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

/// `[buyer_reservation_floor]` — release a reservation nothing ever tried to pay, on the buyer's
/// own clock.
///
/// **Feature OFF by default.** Today a dead reservation is freed only when the relay stops
/// answering for its job, so the buyer's release of its own funds is gated on a third party's
/// retention policy. `reservations.created_at_unix` is the one clock that cannot become
/// unreachable: local, written at award, immutable.
///
/// The floor is deliberately narrow. It applies ONLY where the payment journal shows that nothing
/// was ever attempted ([`crate::buyer::lifecycle::PaymentProgress::None`]) — never to a debt that
/// was attempted and refused, which looks identical in the reservation row and is genuinely owed.
///
/// Enabling this is a money decision, not a tuning knob: a released reserve can fund a new award
/// while the old row later converts `Released → Spent`, so an over-eager floor spends past the
/// intended ceiling rather than losing funds outright. It ships off, and it stays off until
/// someone decides the grace is right for their deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuyerReservationFloorConfig {
    /// Release unattempted reservations past the grace. Default **false**.
    #[serde(default)]
    pub enabled: bool,
    /// Seconds past a reservation's creation before the floor may release it. Default **21600**
    /// (6h) — comfortably beyond the 3600 s default job deadline plus reconcile cadence, so a job
    /// still inside its own lifetime is never a candidate.
    #[serde(default = "default_reservation_floor_grace_secs")]
    pub grace_secs: u64,
}

impl Default for BuyerReservationFloorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            grace_secs: default_reservation_floor_grace_secs(),
        }
    }
}

impl BuyerReservationFloorConfig {
    /// True when every field is at its shipped default, so `config.toml` stays clean.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// serde default for [`BuyerReservationFloorConfig::grace_secs`] — 6 hours.
pub fn default_reservation_floor_grace_secs() -> u64 {
    21_600
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
    /// Relay-stall watchdog threshold, in **missed heartbeat intervals**. Default **3**. When no
    /// own heartbeat has round-tripped on the live subscription for this many intervals, the daemon
    /// treats the subscription as silently dead and reconnects + resubscribes (issue #142). The
    /// watchdog rides the heartbeat mechanism, so it is inert when `enabled` is false.
    #[serde(default = "default_heartbeat_stall_missed_intervals")]
    pub stall_missed_intervals: u32,
}

impl Default for SellerHeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: default_heartbeat_enabled(),
            interval_secs: default_heartbeat_interval_secs(),
            stall_missed_intervals: default_heartbeat_stall_missed_intervals(),
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

/// serde default for [`SellerHeartbeatConfig::stall_missed_intervals`] — 3 missed intervals.
pub fn default_heartbeat_stall_missed_intervals() -> u32 {
    3
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
/// force editing that literal. Placing it top-level as `[seller_preflight]` on `MaxplayerConfig` (built
/// only via `Default`) delivers the identical knob without touching any money file. Cosmetic only;
/// the probe is diagnostic — it NEVER feeds the pay gate, journal, or receipt bind, and NEVER
/// refuses boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SellerPreflightConfig {
    /// Run the boot-time dry-run push probe. Default **true**. Set false (or the env override
    /// `MAXPLAYER_SELLER_BOOT_PUSH_PREFLIGHT=0`) to skip — e.g. tests, or air-gapped first boots.
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

/// serde default for [`SellerConfig::slots`]: 3 (issue #378). A `[seller]` block that does not set
/// `slots` runs three concurrent execution slots. (Per-entry `AgentSlotConfig.slots` was removed as
/// dead weight; this homogeneous top-level count is the only concurrency knob.)
pub fn default_slots() -> usize {
    3
}

/// serde default for [`SellerConfig::offer_backfill_secs`]: 1200s (20 min). A `[seller]` block
/// written before this field existed parses to this default; `0` must be set explicitly.
pub fn default_offer_backfill_secs() -> u64 {
    1200
}

/// Per-seller NIP-34 `d` / path leaf. Relay `.names/` registry is GLOBAL across
/// owners — a shared constant like `maxplayer-seller` collides and seeds fail silently.
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
                "argv must be an array, not a string/shell value",
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
                return Err(de::Error::custom("argv must be non-empty"));
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

/// `[buyer]` hop planner knobs.
///
/// The fee-buffer multiplier is applied at hop **plan time** to the record's authorized cost
/// (quoted cost + multiplier × the source mint's own melt fee reserve). Recovery still compares
/// a replacement quote against that recorded ceiling unchanged — the tolerance lives in the
/// record, not in the comparison (issue #760). Default **2**. **0** restores exact quoted-cost
/// authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuyerConfig {
    /// Multiplier on the source mint's melt fee reserve, added to the hop record's authorized
    /// cost at plan time. Default **2**. **0** restores exact quoted-cost authorization.
    #[serde(default = "default_hop_fee_buffer_multiplier")]
    pub hop_fee_buffer_multiplier: u64,
}

impl Default for BuyerConfig {
    fn default() -> Self {
        Self {
            hop_fee_buffer_multiplier: default_hop_fee_buffer_multiplier(),
        }
    }
}

impl BuyerConfig {
    /// True when every field is at its shipped default (so config.toml stays clean — the section
    /// only serializes once an operator sets a non-default knob).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// serde default for [`BuyerConfig::hop_fee_buffer_multiplier`] — 2× the mint's melt fee reserve.
pub fn default_hop_fee_buffer_multiplier() -> u64 {
    2
}

/// Buyer-facing packaged config (`~/.maxplayer/config.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaxplayerConfig {
    /// Open-market relay. Absent in the file ⇒ the built-in [`DEFAULT_RELAY_URL`].
    #[serde(default = "default_relay_url")]
    pub relay_url: String,
    /// Seller-side accept policy: the mints this seller will accept payment at. The first
    /// entry is the mint the seller advertises first and also the buyer-side wallet default
    /// mint (read via [`MaxplayerConfig::default_mint`]). Defaults to `[DEFAULT_MINT_URL]`.
    ///
    /// NOTE: distinct from `extra_mints`. `accepted_mints` is the SELLER accept-policy list;
    /// `extra_mints` is the BUYER wallet's *additional allowed* mints. They are separate
    /// fields with separate meanings and are never merged or repurposed for one another.
    #[serde(default = "default_accepted_mints")]
    pub accepted_mints: Vec<String>,
    /// Per-job spend cap (sats) — the standing spend bound on the money path. Absent ⇒ the built-in
    /// [`DEFAULT_PER_JOB_BUDGET_SATS`]. Issue #378 removed the rolling/total cap: the durable
    /// `spent.jsonl` ledger still records every spend (audit + retry-idempotency), but this per-job cap
    /// is the only gate — every posted and paid job is bounded by this one number.
    #[serde(default = "default_per_job_budget_sats")]
    pub per_job_budget_sats: u64,
    /// Opt-in additional mints for the BUYER wallet (`maxplayer wallet mints add`). The buyer's
    /// default mint stays the first `accepted_mints` entry ([`MaxplayerConfig::default_mint`]);
    /// never invents spendable credit by itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_mints: Vec<String>,
    /// REAL-MONEY SWITCH (issue #49). When `true` (issue #378 made this the DEFAULT, because the
    /// shipped `accepted_mints` default is a real minibits mint) the seller `accepted_mints` boot fence
    /// and the buyer pay-path mint resolution admit any well-formed `https://` mint URL — real sats
    /// move. When `false` (explicit opt-OUT) only the testnut/dev allow-list ([`DEFAULT_MINT_URL`]) is
    /// admitted; a real mint is refused fail-closed. It flips ONLY the allow-list check; every other
    /// money gate (creq membership, redeem guard token==payload mint, dust guard, per-job budget cap,
    /// co-signatures) is unchanged — the per-job cap is the standing spend bound on the real path.
    #[serde(default = "default_allow_real_mints")]
    pub allow_real_mints: bool,
    /// Optional `[profile] name / about`. Skipped when absent so fresh homes stay unnamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileConfig>,
    /// Optional `[seller]` daemon config. Absent until `maxplayer seller` setup writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seller: Option<SellerConfig>,
    /// Deprecated compatibility field for the removed `[buzz]` persona. Parsed and preserved so
    /// existing homes keep booting, but ignored by all runtime behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buzz: Option<BuzzConfig>,
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
    /// `[buyer_reservation_floor]` local-clock release of an unattempted reservation.
    /// Defaults (feature OFF) when absent.
    #[serde(default, skip_serializing_if = "BuyerReservationFloorConfig::is_default")]
    pub buyer_reservation_floor: BuyerReservationFloorConfig,
    /// `[buyer]` hop planner knobs. Defaults (fee-buffer multiplier 2) when absent.
    #[serde(default, skip_serializing_if = "BuyerConfig::is_default")]
    pub buyer: BuyerConfig,
    /// Optional buyer-side contribution content policy (the content-policy hook). Absent
    /// ⇒ the FLOOR (refuse only empty diffs). Present ⇒ tighten pre-pay with a path allowlist /
    /// forbidden paths / max diff size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<ContributionPolicyConfig>,
    /// Optional `[sandbox]` executor config. Absent ⇒ the agent command runs pass-through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxConfig>,
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

/// Serde/default seed for [`MaxplayerConfig::accepted_mints`] (issue #378): a single REAL minibits mint.
/// A fresh config accepts real sats here by default — which is why [`MaxplayerConfig::default`] also flips
/// `allow_real_mints` true, without which `mint_allowed` would refuse this very default. Mint VARIETY
/// (multiple real mints) lives in the market-mode loop, not the shipped default pool.
fn default_accepted_mints() -> Vec<String> {
    vec![DEFAULT_MINIBITS_MINT_URL.to_owned()]
}

/// Serde default for [`MaxplayerConfig::relay_url`] — the built-in [`DEFAULT_RELAY_URL`].
fn default_relay_url() -> String {
    DEFAULT_RELAY_URL.to_owned()
}

/// Serde default for [`MaxplayerConfig::per_job_budget_sats`] — [`DEFAULT_PER_JOB_BUDGET_SATS`].
fn default_per_job_budget_sats() -> u64 {
    DEFAULT_PER_JOB_BUDGET_SATS
}

/// Serde default for [`MaxplayerConfig::allow_real_mints`] — `true` (issue #378). The shipped
/// `accepted_mints` default is a real mint, so the fence must admit it; `false` here would make the
/// default config refuse its own default mint. Set `allow_real_mints = false` to force testnut-only.
fn default_allow_real_mints() -> bool {
    true
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

impl MaxplayerConfig {
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

impl Default for MaxplayerConfig {
    fn default() -> Self {
        Self {
            relay_url: DEFAULT_RELAY_URL.to_owned(),
            accepted_mints: default_accepted_mints(),
            per_job_budget_sats: DEFAULT_PER_JOB_BUDGET_SATS,
            extra_mints: Vec::new(),
            allow_real_mints: true,
            profile: None,
            seller: None,
            buzz: None,
            agents: BTreeMap::new(),
            seller_memory: SellerMemoryConfig::default(),
            seller_announce: SellerAnnounceConfig::default(),
            telemetry: TelemetryConfig::default(),
            seller_heartbeat: SellerHeartbeatConfig::default(),
            seller_preflight: SellerPreflightConfig::default(),
            buyer_reservation_floor: BuyerReservationFloorConfig::default(),
            buyer: BuyerConfig::default(),
            contribution: None,
            sandbox: None,
        }
    }
}

/// Resolved packaged home after bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaxplayerHome {
    pub root: PathBuf,
    pub config: MaxplayerConfig,
    pub key_path: PathBuf,
    pub wallet_dir: PathBuf,
    /// True when this bootstrap call created the key file.
    pub key_created: bool,
}

/// Default home root: `MAXPLAYER_HOME` if set, else `~/.maxplayer`.
pub fn default_home_dir() -> Result<PathBuf, HomeError> {
    if let Ok(override_dir) = std::env::var("MAXPLAYER_HOME") {
        let path = PathBuf::from(override_dir);
        if path.as_os_str().is_empty() {
            return Err(HomeError::Io("MAXPLAYER_HOME is empty".into()));
        }
        return Ok(path);
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| HomeError::Io("HOME is unset and MAXPLAYER_HOME was not provided".into()))?;
    default_home_under(Path::new(&home))
}

/// Resolve the default home under `home_base` (`$HOME`) and apply the mobee→maxplayer migration
/// guard. Split out from [`default_home_dir`] so the guard is unit-testable against a temp dir
/// without mutating the process `$HOME`.
///
/// The default home moved from `~/.mobee` to `~/.maxplayer`. If the new path is absent but the old
/// one exists, REFUSE to boot: booting would bootstrap a fresh empty `~/.maxplayer` and leave the
/// wallet, keys, and state stranded in `~/.mobee`. No read-fallback and no auto-move (a move can
/// race a running daemon or cross filesystems); the operator runs the printed one-liner. A fresh
/// box — neither directory present — is NOT caught: it falls through and `bootstrap` creates
/// `~/.maxplayer` normally.
fn default_home_under(home_base: &Path) -> Result<PathBuf, HomeError> {
    let new_default = home_base.join(".maxplayer");
    let old_default = home_base.join(".mobee");
    if !new_default.exists() && old_default.exists() {
        return Err(HomeError::OldHomeNeedsMigration {
            old: old_default,
            new: new_default,
        });
    }
    Ok(new_default)
}

/// Whether `root` already holds an initialized maxplayer home — specifically, whether its key
/// file exists. This is the one fact [`bootstrap`] uses to decide "create a new identity" vs.
/// "use the existing one" (`key_path.exists()`), exposed so a caller that must NEVER silently
/// mint a new identity (e.g. `profile set`, a publish command) can check it BEFORE calling
/// `bootstrap`, rather than after.
pub fn is_initialized(root: impl AsRef<Path>) -> bool {
    root.as_ref().join(KEY_FILE).exists()
}

/// Ensure `root` exists with config, key (`0600`), and `wallet/` dir.
///
/// Idempotent: existing config/key are left in place except dead-mint migration
/// (`testnut.cashu.space` → [`DEFAULT_MINT_URL`]). The persisted `config.toml` is the file layer;
/// the returned [`MaxplayerHome::config`] additionally carries the `MAXPLAYER_*` environment overlay (see
/// the module docs). Never returns the secret key.
pub fn bootstrap(root: impl AsRef<Path>) -> Result<MaxplayerHome, HomeError> {
    let root = root.as_ref().to_path_buf();
    fs::create_dir_all(&root).map_err(|error| HomeError::Io(error.to_string()))?;
    // Owner-only BEFORE anything money-bearing is written inside: config, key, and wallet all land
    // under this dir, and a 0700 container fences them whatever the operator's umask was (#473).
    ensure_dir_owner_only(&root)?;

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
        let config = MaxplayerConfig::default();
        // First run: write config.toml WITH short per-field doc comments (issue #376).
        write_config_documented(&config_path, &config)?;
        config
    };

    fs::create_dir_all(&wallet_dir).map_err(|error| HomeError::Io(error.to_string()))?;
    // wallet/ holds mint proofs — spendable ecash — so it is fenced owner-only in its own right, not
    // only by the home dir above (defense in depth if the home dir is later loosened).
    ensure_dir_owner_only(&wallet_dir)?;

    let key_created = if key_path.exists() {
        validate_existing_key(&key_path)?;
        false
    } else {
        write_new_key(&key_path)?;
        true
    };

    let config = apply_env_layer(&file_config, config_env_from_process())?;

    Ok(MaxplayerHome {
        root,
        config,
        key_path,
        wallet_dir,
        key_created,
    })
}

/// Rewrite dead `.cashu.space` testnut hosts to [`DEFAULT_MINT_URL`] across every
/// `accepted_mints` entry. Returns true when any entry changed.
pub fn migrate_dead_mint_url(config: &mut MaxplayerConfig) -> bool {
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

/// Issue #378 removed three config surfaces; fold a pre-#378 `config.toml` forward so it still parses
/// under `deny_unknown_fields` instead of bricking on the now-unknown keys (same read-time migration
/// seam as [`fold_legacy_mint_url`], applied before the typed parse). Each step is idempotent on an
/// already-migrated file:
///
/// - top-level `total_budget_sats` is dropped — the rolling cap is gone; the per-job cap and the
///   `spent.jsonl` ledger stay;
/// - `[seller].agent = "x"` folds into `[seller].agents = ["x"]` unless a non-empty `agents` list is
///   already present (never drops a configured harness label), then the key is removed;
/// - each `[seller].agents` entry written as a `{ name, slots }` table collapses to its bare `name`
///   (the per-entry `slots` knob was dead weight — refused above 1 — so nothing is lost).
fn fold_removed_config_fields(table: &mut toml::Table) {
    table.remove("total_budget_sats");

    let Some(seller) = table.get_mut("seller").and_then(toml::Value::as_table_mut) else {
        return;
    };

    // Singular `agent` label → a single-entry `agents` list, unless a real list is already present
    // (the list wins, mirroring `fold_legacy_mint_url`). The legacy key is always removed.
    let legacy_agent = seller.remove("agent");
    let agents_present = seller
        .get("agents")
        .and_then(toml::Value::as_array)
        .is_some_and(|list| !list.is_empty());
    if !agents_present {
        if let Some(label) = legacy_agent.as_ref().and_then(toml::Value::as_str) {
            seller.insert(
                "agents".to_owned(),
                toml::Value::Array(vec![toml::Value::String(label.to_owned())]),
            );
        }
    }

    // `{ name, slots }` table entries → bare name strings.
    if let Some(agents) = seller.get_mut("agents").and_then(toml::Value::as_array_mut) {
        for entry in agents.iter_mut() {
            let name = entry
                .as_table()
                .and_then(|table| table.get("name"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned);
            if let Some(name) = name {
                *entry = toml::Value::String(name);
            }
        }
    }
}

/// Hex-encode the secp256k1 x-only/public view is deferred; this returns the *public* key
/// only when a caller supplies a derived pubkey. For bootstrap status we expose whether a
/// key file exists — use [`read_secret_key_hex`] only inside trusted surfaces that never log it.
pub fn key_file_present(home: &MaxplayerHome) -> bool {
    home.key_path.is_file()
}

/// Read the secret key hex from disk. Callers must not log, print, or put this in MCP tool output.
pub fn read_secret_key_hex(home: &MaxplayerHome) -> Result<String, HomeError> {
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
pub fn public_key_hex(home: &MaxplayerHome) -> Result<String, HomeError> {
    let secret = read_secret_key_hex(home)?;
    let keys = nostr_sdk::Keys::parse(&secret)
        .map_err(|error| HomeError::Key(format!("key parse for pubkey: {error}")))?;
    Ok(keys.public_key().to_hex())
}

/// The FILE layer: read `config.toml` into the typed [`MaxplayerConfig`]. Absent fields fall back to
/// the built-in defaults (so this is already the defaults→file merge); unknown fields refuse with
/// the offending key named. The legacy single `mint_url` folds into `accepted_mints` first.
fn load_config(path: &Path) -> Result<MaxplayerConfig, HomeError> {
    let raw = fs::read_to_string(path).map_err(|error| HomeError::Config(error.to_string()))?;
    parse_config_toml(&raw)
}

/// Parse a `config.toml` document into the file-layer [`MaxplayerConfig`]. Fold legacy `mint_url` and the
/// issue #378 removed fields, then typed-parse under `deny_unknown_fields` so any other unknown key
/// (at any depth) refuses. `pub(crate)` so sibling modules' tests can exercise the read-time migration.
pub(crate) fn parse_config_toml(raw: &str) -> Result<MaxplayerConfig, HomeError> {
    let mut table: toml::Table =
        toml::from_str(raw).map_err(|error| HomeError::Config(format!("config.toml: {error}")))?;
    fold_legacy_mint_url(&mut table);
    fold_removed_config_fields(&mut table);
    // LOAD-BEARING: Table -> try_into preserves dotted field-path attribution on value errors.
    // Do not replace it with toml::from_str::<MaxplayerConfig>: the document deserializer sets
    // span/raw context, suppressing the `in `<field>`` annotation the #381 test relies on.
    table
        .try_into()
        .map_err(|error| HomeError::Config(format!("config.toml: {error}")))
}

/// `MAXPLAYER_`-prefixed environment variables that are operational/test seams, **not**
/// [`MaxplayerConfig`] fields (home resolution and the daemon test overrides read these directly). They
/// are excluded from the env config layer so they neither collide with a field nor — under
/// `deny_unknown_fields` — refuse resolution. None of these collide with a real field's canonical
/// `MAXPLAYER_*` spelling, so excluding them costs no config coverage.
const RESERVED_ENV_VARS: &[&str] = &[
    "MAXPLAYER_HOME",
    "MAXPLAYER_HEARTBEAT_INTERVAL_SECS",
    "MAXPLAYER_HEARTBEAT_ENABLED",
    "MAXPLAYER_HEARTBEAT_STALL_MISSED_INTERVALS",
    "MAXPLAYER_WRAP_BACKFILL_INTERVAL_SECS",
    "MAXPLAYER_AWARD_SWEEP_INTERVAL_SECS",
    "MAXPLAYER_SELLER_BOOT_PUSH_PREFLIGHT",
    "MAXPLAYER_GIT_CREDENTIAL_NOSTR",
    "MAXPLAYER_ACP_SMOKE",
    "MAXPLAYER_ACP_SMOKE_CMD",
    "MAXPLAYER_EVALS_SNAPSHOT_DIR",
];

/// [`MaxplayerConfig`] fields whose env value is a comma-separated list. The env source must be told
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

/// The process environment's config-layer variables: every `MAXPLAYER_`-prefixed var that is not a
/// reserved operational seam ([`RESERVED_ENV_VARS`]).
fn config_env_from_process() -> HashMap<String, String> {
    std::env::vars()
        .filter(|(key, _)| key.starts_with("MAXPLAYER_") && !RESERVED_ENV_VARS.contains(&key.as_str()))
        .collect()
}

/// Overlay the ENV layer on a resolved defaults/file [`MaxplayerConfig`]. `env` is the pre-filtered
/// `MAXPLAYER_*` map ([`config_env_from_process`] in production; tests inject one). A malformed value
/// (wrong type) or an unknown `MAXPLAYER_<FIELD>` refuses fail-closed, naming the offending key.
fn apply_env_layer(base: &MaxplayerConfig, env: HashMap<String, String>) -> Result<MaxplayerConfig, HomeError> {
    // The `config` crate lowercases and strips the prefix, so its `deny_unknown_fields` refusal names
    // the derived FIELD (e.g. `sandbox_ro_paths`), not the `MAXPLAYER_*` variable the operator set.
    // Keep the caller's keys so [`reserved_namespace_hint`] can re-point the refusal at that variable.
    let env_keys: Vec<String> = env.keys().cloned().collect();
    let mut environment = config::Environment::with_prefix("MAXPLAYER")
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
                HomeError::Config(format!("MAXPLAYER_* environment layer: {error}"))
            })?,
        )
        .add_source(environment)
        .build()
        .map_err(|error| HomeError::Config(format!("MAXPLAYER_* environment layer: {error}")))?
        .try_deserialize::<MaxplayerConfig>()
        .map_err(|error| {
            HomeError::Config(reserved_namespace_hint(
                format!("MAXPLAYER_* environment layer: {error}"),
                &env_keys,
            ))
        })
}

/// Turn a `config`-crate `deny_unknown_fields` refusal into one that names the `MAXPLAYER_*` variable
/// the operator actually set (not just the derived field) and states the reserved-namespace rule.
///
/// The crate reports the derived field name (lowercased leaf, e.g. `sandbox_ro_paths` for
/// `MAXPLAYER_SANDBOX_RO_PATHS`, or `bogus` under key `seller` for `MAXPLAYER_SELLER__BOGUS`). We match
/// that leaf back to `env_keys` — the exact variables handed in — so the message points at what the
/// operator typed. Only unknown-field refusals are enriched; malformed-value errors (which already name
/// their key) pass through untouched.
fn reserved_namespace_hint(message: String, env_keys: &[String]) -> String {
    let Some(leaf) = message
        .split("unknown field `")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
    else {
        return message;
    };
    // The offending variable(s): a MAXPLAYER_* key whose derived leaf (last `__`-segment, lowercased)
    // is the field the crate rejected. Usually exactly one; if none/ambiguous, keep the teaching text
    // without naming a specific variable rather than guess.
    let offenders: Vec<&str> = env_keys
        .iter()
        .filter(|key| {
            key.strip_prefix("MAXPLAYER_")
                .map(|suffix| suffix.to_ascii_lowercase())
                .and_then(|lowered| lowered.rsplit("__").next().map(str::to_owned))
                .as_deref()
                == Some(leaf)
        })
        .map(String::as_str)
        .collect();
    let named = match offenders.as_slice() {
        [one] => format!(" — from environment variable {one}"),
        [] => String::new(),
        many => format!(" — from one of these variables: {}", many.join(", ")),
    };
    format!(
        "{message}{named}. The whole MAXPLAYER_ prefix is reserved for maxplayer config: every \
         MAXPLAYER_* variable must map to a config field — nested fields join with a double \
         underscore, e.g. MAXPLAYER_SELLER__RATE_SATS — or be a known operational seam, e.g. \
         MAXPLAYER_HOME. An unrecognized MAXPLAYER_* variable is refused fail-closed, never ignored. \
         Fix the spelling (a single '_' where a nested '__' was meant reads as an unknown top-level \
         field) or unset the variable."
    )
}

fn write_config(path: &Path, config: &MaxplayerConfig) -> Result<(), HomeError> {
    let raw = toml::to_string_pretty(config)
        .map_err(|error| HomeError::Config(error.to_string()))?;
    // Crash-atomic rewrite: config.toml holds money-adjacent state (budget caps, accepted mints), so
    // a truncating write that dies mid-flush must never leave it empty/half-written. temp → sync →
    // rename → dir-fsync.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    crate::durable::write_atomic(dir, path, raw.as_bytes())
        .map_err(|error| HomeError::Io(error.to_string()))
}

/// First-run variant of [`write_config`] (issue #376): the same crash-atomic write, but the serialized
/// body is annotated with short per-field doc comments describing the money-relevant defaults. The
/// comments are TOML comments (ignored on parse), so the file still round-trips byte-for-byte through
/// [`parse_config_toml`]. Only bootstrap's fresh-config branch uses this; later `save_config` rewrites
/// are bare, so an operator's edited value is never re-annotated with a default they changed.
fn write_config_documented(path: &Path, config: &MaxplayerConfig) -> Result<(), HomeError> {
    let raw = documented_config_toml(config)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    crate::durable::write_atomic(dir, path, raw.as_bytes())
        .map_err(|error| HomeError::Io(error.to_string()))
}

/// Render `config` as TOML with a header block and short `#` doc comments above the money-relevant
/// default keys (issue #376). The body is the ordinary `toml::to_string_pretty` output, so the result
/// parses back to an equal config; the comments are onboarding for the operator who first opens the
/// file — most importantly, that the shipped defaults move REAL sats.
fn documented_config_toml(config: &MaxplayerConfig) -> Result<String, HomeError> {
    // Doc lines injected above the first serialized line whose key matches. Keyed by field name, so
    // order-independent; a key not present in the (skip-defaulted) output is simply not annotated.
    const FIELD_DOCS: &[(&str, &[&str])] = &[
        ("relay_url", &["Open-market relay this node publishes offers to and reads from."]),
        (
            "accepted_mints",
            &[
                "Mints this seller accepts; the first is also the buyer wallet's default mint.",
                "⚠ THE SHIPPED DEFAULT IS A REAL MINT (minibits) — a fresh node moves REAL sats.",
                "For testnut/dev only, set a test mint here AND allow_real_mints = false below.",
            ],
        ),
        (
            "per_job_budget_sats",
            &[
                "Per-job spend cap (sats): the standing bound on every job posted or paid. There is",
                "no total cap — the spent.jsonl ledger records every spend for audit. Raise with care.",
            ],
        ),
        (
            "allow_real_mints",
            &[
                "Real-money switch — TRUE by default so the fence admits the real default mint above.",
                "Set false to force testnut/dev-only (any real mint is then refused fail-closed).",
            ],
        ),
    ];

    let body =
        toml::to_string_pretty(config).map_err(|error| HomeError::Config(error.to_string()))?;
    let mut out = String::new();
    out.push_str("# maxplayer config.toml — written on first run.\n");
    out.push_str("# Edit freely. Comments are NOT preserved when the app rewrites this file\n");
    out.push_str("# (e.g. `maxplayer seller` setup or a wallet change).\n");
    out.push_str("# ⚠ This node's DEFAULTS accept and pay REAL sats — see accepted_mints.\n\n");
    for line in body.lines() {
        let key = line.split('=').next().unwrap_or("").trim();
        if let Some((_, docs)) = FIELD_DOCS.iter().find(|(name, _)| *name == key) {
            for doc in *docs {
                out.push_str("# ");
                out.push_str(doc);
                out.push('\n');
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    // Onboarding (issue #376, extended): a commented [sandbox] template. A default config has NO
    // [sandbox] section — pass-through — so there is no key to annotate; instead the options are
    // shown inline, including the one difference that bites: gVisor (`runtime = "runsc"`) is
    // Linux-only, and a Mac seat omits it. Every line is a comment, so the file still round-trips
    // through parse_config_toml unchanged (the test asserts this).
    out.push('\n');
    out.push_str(
        r#"# --- [sandbox]: how an awarded job runs. Uncomment ONE option below. ---
# With no [sandbox] section (the default here) the agent runs PASS-THROUGH:
# directly as this daemon, with no isolation. A seat that claims open-pool
# work is refused at the boot gate until a real sandbox is configured.
#
# Option A - launcher (an OS sandbox such as bubblewrap; also works inside a
# container). Full bwrap example: docs/DOCKER.md
#   [sandbox]
#   mode = "launcher"
#   launcher = ["bwrap", "--unshare-all", "--die-with-parent", "..."]
#
# Option B - docker on LINUX. gVisor (runsc) is the primary boundary. Install
# it first (docs/SANDBOXING.md), then confirm:  docker info | grep -i runsc
# The image is supplied by this binary (a version-pinned GHCR ref); leave it
# unset. `doctor` prints the exact `docker pull` command if it is not present.
# LOCAL DEV: the default ref is `:v<this build's version>`, which is NOT
# published for dev builds — set `image` to a locally-built tag (e.g.
# "maxplayer-sandbox:latest") to override, or the default will fail to pull.
#   [sandbox]
#   mode = "docker"
#   runtime = "runsc"                  # gVisor - LINUX ONLY
#   # image = "ghcr.io/you/custom:tag" # ONLY to run a fully-custom image; NOT a version pin
#   # image = "maxplayer-sandbox:latest" # LOCAL DEV: your locally-built tag (default GHCR ref is unpublished for dev)
#   # forward_env = ["MY_AGENT_TOKEN"] # extra env names, atop the built-in auth allowlist
#
# Option C - docker on macOS. Docker Desktop cannot load runsc, so OMIT the
# runtime line; the platform VM is the boundary. Otherwise identical to B.
#   [sandbox]
#   mode = "docker"
#   # (no runtime line on macOS; image defaulted by the binary as in Option B)
"#,
    );
    Ok(out)
}

/// Persist an explicit config change to `config.toml`, keeping `MAXPLAYER_*` overrides runtime-only.
///
/// The file is the durable layer; the `MAXPLAYER_*` environment is an overlay applied at load
/// ([`apply_env_layer`]) and never written back. `edit` receives the file-only view (defaults +
/// current file, no env) and applies the caller's explicit change; that view is written, so an
/// env-origin value the caller did not choose cannot leak into the file. `home.config` is then
/// refreshed through the full layer pipeline so the in-process struct still reflects env.
pub fn save_config(
    home: &mut MaxplayerHome,
    edit: impl FnOnce(&mut MaxplayerConfig),
) -> Result<(), HomeError> {
    let config_path = home.root.join(CONFIG_FILE);
    let mut file_config = if config_path.exists() {
        load_config(&config_path)?
    } else {
        MaxplayerConfig::default()
    };
    edit(&mut file_config);
    write_config(&config_path, &file_config)?;
    home.config = apply_env_layer(&file_config, config_env_from_process())?;
    Ok(())
}

/// Reload `config.toml` into `home.config` without touching the key file. Routes through the same
/// layer pipeline as [`bootstrap`]: file layer then `MAXPLAYER_*` environment overlay.
pub fn reload_config(home: &mut MaxplayerHome) -> Result<(), HomeError> {
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

/// A home/wallet directory must be owner-only (`0700`), so seller state — which on a shared host IS
/// the wallet (key, mint proofs, config, job workdirs) — cannot be read by another local user (#473).
///
/// Owner-only is made a property of the PRODUCT here rather than of the operator's `umask`: the rc.3
/// exposure was a seat whose state defaulted broader than owner-only until a systemd `UMask=0077`
/// happened to tighten it, so every operator who did not replicate that unit was exposed. A `0700`
/// container dir fences everything inside it (traversal is denied), so this is the load-bearing bind.
///
/// Mirrors [`ensure_key_permissions`]: idempotent (a dir already owner-only is untouched), it re-chmods
/// an existing too-open dir — the real upgrade case, a seat created by an older binary under a `0022`
/// umask is `0755` — and REFUSES rather than leaving money-bearing state group/world-accessible, the
/// same fail-closed posture the key file already takes. A no-op on non-unix (no POSIX mode to enforce).
fn ensure_dir_owner_only(path: &Path) -> Result<(), HomeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(path).map_err(|error| HomeError::Io(error.to_string()))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            return Ok(());
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).map_err(|error| {
            HomeError::Io(format!(
                "home directory {} permissions too open ({mode:#o}); re-chmod 0700 failed: {error}",
                path.display()
            ))
        })?;
        let after = fs::metadata(path)
            .map_err(|error| HomeError::Io(error.to_string()))?
            .permissions()
            .mode()
            & 0o777;
        if after & 0o077 != 0 {
            return Err(HomeError::Io(format!(
                "home directory {} permissions too open ({mode:#o}); refused to leave open (still {after:#o})",
                path.display()
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
            "maxplayer-home-{label}-{}-{id}",
            std::process::id()
        ))
    }

    /// #487: the suggested seller claim floor is the rate buyers are told to post at. A fresh
    /// seller that accepts the offered default must not land below it.
    #[test]
    fn default_rate_sats_is_the_market_floor() {
        assert_eq!(DEFAULT_RATE_SATS, 100);
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
        let reloaded: MaxplayerConfig = toml::from_str(&serialized).expect("reparse");
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
        assert_eq!(home.config, MaxplayerConfig::default());
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

    // #473: the home and wallet CONTAINERS must be owner-only, so seller state (key, mint proofs,
    // config) is not readable by another local user on a shared host. Property of the product, not the
    // operator's umask.
    #[cfg(unix)]
    #[test]
    fn bootstrap_makes_home_and_wallet_dirs_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_home("owner-only");
        let _ = fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        for dir in [&home.root, &home.wallet_dir] {
            let mode = fs::metadata(dir)
                .expect("dir metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode & 0o077,
                0,
                "{} must be owner-only (no group/world bits), got {mode:#o}",
                dir.display()
            );
        }
    }

    // The load-bearing (umask-independent) case: a seat created by an OLDER binary under a 0022 umask
    // is 0755, and re-bootstrapping must RE-CHMOD it owner-only rather than leaving the drift. Revert
    // `ensure_dir_owner_only` and this goes red regardless of the test host's umask.
    #[cfg(unix)]
    #[test]
    fn bootstrap_tightens_an_existing_too_open_home() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_home("tighten");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mk root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("loosen to 0755");
        let home = bootstrap(&root).expect("bootstrap tightens");
        let mode = fs::metadata(&home.root)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode & 0o077,
            0,
            "an existing too-open home must be tightened to 0700, got {mode:#o}"
        );
    }

    #[test]
    fn default_home_dir_honors_maxplayer_home() {
        let root = temp_home("env");
        // Safety: test process isolation — restore after.
        let previous = std::env::var_os("MAXPLAYER_HOME");
        unsafe { std::env::set_var("MAXPLAYER_HOME", &root) };
        let resolved = default_home_dir().expect("resolve");
        match previous {
            Some(value) => unsafe { std::env::set_var("MAXPLAYER_HOME", value) },
            None => unsafe { std::env::remove_var("MAXPLAYER_HOME") },
        }
        assert_eq!(resolved, root);
    }

    // Money hard-boot guard (mobee→maxplayer home migration). Pure over `default_home_under`, so it
    // exercises the real filesystem-existence logic without mutating the process `$HOME`.
    #[test]
    fn default_home_guard_refuses_when_only_old_home_exists() {
        let base = temp_home("migrate-old-only");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join(".mobee")).expect("mk ~/.mobee");
        // new (~/.maxplayer) absent, old (~/.mobee) present ⇒ MUST refuse (funds would be stranded).
        let err = default_home_under(&base).expect_err("must refuse when only ~/.mobee exists");
        assert!(
            matches!(err, HomeError::OldHomeNeedsMigration { .. }),
            "expected OldHomeNeedsMigration, got {err:?}"
        );
        // the refusal names the exact `mv` fix so the operator is not left going in circles.
        let msg = err.to_string();
        assert!(msg.contains("mv "), "refusal must print the mv fix: {msg}");
    }

    #[test]
    fn default_home_guard_does_not_false_positive() {
        // Fresh box: NEITHER dir ⇒ falls through to ~/.maxplayer (must not strand a brand-new user).
        let fresh = temp_home("migrate-fresh");
        let _ = fs::remove_dir_all(&fresh);
        assert_eq!(
            default_home_under(&fresh).expect("fresh box must resolve, not refuse"),
            fresh.join(".maxplayer")
        );
        // Already migrated (or normal): ~/.maxplayer present ⇒ OK even if a stale ~/.mobee lingers.
        let migrated = temp_home("migrate-done");
        let _ = fs::remove_dir_all(&migrated);
        fs::create_dir_all(migrated.join(".maxplayer")).expect("mk ~/.maxplayer");
        fs::create_dir_all(migrated.join(".mobee")).expect("mk stale ~/.mobee");
        assert_eq!(
            default_home_under(&migrated).expect("migrated home must resolve"),
            migrated.join(".maxplayer")
        );
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
        let stale = MaxplayerConfig {
            accepted_mints: vec![format!("https://{DEAD_TESTNUT_MINT_HOST}")],
            ..MaxplayerConfig::default()
        };
        write_config(&config_path, &stale).expect("write stale");
        let home = bootstrap(&root).expect("bootstrap migrates");
        assert_eq!(home.config.accepted_mints, vec![DEFAULT_MINT_URL.to_owned()]);
        let reloaded = load_config(&config_path).expect("reload");
        assert_eq!(reloaded.accepted_mints, vec![DEFAULT_MINT_URL.to_owned()]);
    }

    #[test]
    fn accepted_mints_default() {
        // Issue #378: a config that names no mint yields the shipped default — a single REAL minibits
        // mint (paired with allow_real_mints = true; see `default_allow_real_mints`).
        let config: MaxplayerConfig = toml::from_str(
            "relay_url = 'r'\nper_job_budget_sats = 1\n",
        )
        .expect("parse mint-less config");
        assert_eq!(config.accepted_mints, vec![DEFAULT_MINIBITS_MINT_URL.to_owned()]);
        assert_eq!(
            MaxplayerConfig::default().accepted_mints,
            vec![DEFAULT_MINIBITS_MINT_URL.to_owned()]
        );
    }

    #[test]
    fn documented_config_round_trips_and_warns_of_real_money() {
        // #376: the first-run config.toml carries `#` comments; they must not break parse, and the
        // file must round-trip to the exact default it serialized (comments are TOML comments).
        let rendered = documented_config_toml(&MaxplayerConfig::default()).expect("render documented");
        let reparsed = parse_config_toml(&rendered).expect("documented config must parse");
        assert_eq!(
            toml::to_string_pretty(&reparsed).expect("ser"),
            toml::to_string_pretty(&MaxplayerConfig::default()).expect("ser"),
            "documented first-run config must round-trip to the default"
        );
        assert!(rendered.contains("REAL sats"), "must warn the shipped defaults move real sats");
        assert!(rendered.contains("minibits"), "must name the real default mint");
        assert!(rendered.contains("Per-job spend cap"), "must document the per-job cap");
        // The commented [sandbox] onboarding template, including the Linux-vs-macOS runtime split.
        assert!(rendered.contains("[sandbox]"), "must carry the sandbox onboarding template");
        assert!(
            rendered.contains("runtime = \"runsc\""),
            "must show the Linux gVisor runtime line"
        );
        assert!(rendered.contains("macOS"), "must call out the macOS difference (omit runtime)");
    }

    #[test]
    fn shipped_defaults_are_real_money_and_the_fence_admits_them() {
        // #378 flipped fresh nodes real-money-capable. The whole default posture in one place; the
        // load-bearing part is that mint_allowed ADMITS the shipped default mint (it would REFUSE it
        // if allow_real_mints had stayed false, or if the mint reverted to testnut).
        let d = MaxplayerConfig::default();
        assert_eq!(d.accepted_mints, vec![DEFAULT_MINIBITS_MINT_URL.to_owned()]);
        assert!(d.allow_real_mints, "fresh nodes are real-money-capable by default");
        assert_eq!(d.per_job_budget_sats, 30_000);
        assert_eq!(default_slots(), 3, "seller default concurrency");
        assert!(
            mint_allowed(d.default_mint(), d.allow_real_mints),
            "the fence must admit the shipped default mint (breaks if either default reverts)"
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
    fn pre_upgrade_profile_without_about_provenance_still_parses() {
        let root = temp_home("profile-schema-backcompat");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        let config_path = root.join(CONFIG_FILE);
        fs::write(
            &config_path,
            "relay_url = 'wss://relay.example'\n\
             [profile]\nname = 'legacy-name'\nabout = 'legacy about'\n",
        )
        .expect("write legacy profile config");
        let config = load_config(&config_path).expect("legacy profile parses");
        let profile = config.profile.expect("profile present");
        assert_eq!(profile.name.as_deref(), Some("legacy-name"));
        assert_eq!(profile.about.as_deref(), Some("legacy about"));
        assert_eq!(profile.about_generated, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn deprecated_buzz_table_remains_parse_tolerated_but_ignored() {
        let root = temp_home("buzz-schema-backcompat");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        let config_path = root.join(CONFIG_FILE);
        fs::write(
            &config_path,
            "relay_url = 'wss://relay.example'\n\
             [buzz]\nrelay_url = 'wss://buzz.example'\nname = 'legacy-buzz'\n",
        )
        .expect("write legacy buzz config");
        let config = load_config(&config_path).expect("legacy buzz table parses");
        let buzz = config.buzz.expect("buzz table preserved");
        assert_eq!(buzz.relay_url, "wss://buzz.example");
        assert_eq!(buzz.name, "legacy-buzz");
        assert_eq!(buzz.heartbeat_secs, 30);
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
                about_generated: None,
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
        // A field whose value came only from a MAXPLAYER_* env override must stay runtime-only:
        // saving an UNRELATED field must not bake the env value into config.toml.
        let root = temp_home("save-env-noleak");
        let _ = fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");

        // Resolve an env override into the in-process config, as a live process would.
        let file_before = load_config(&home.root.join(CONFIG_FILE)).expect("file before");
        home.config = apply_env_layer(&file_before, env(&[("MAXPLAYER_RELAY_URL", "wss://from-env")]))
            .expect("env layer");
        assert_eq!(home.config.relay_url, "wss://from-env");

        // Save an unrelated field.
        save_config(&mut home, |config| {
            config.profile = Some(ProfileConfig {
                name: Some("buyer".into()),
                about: None,
                about_generated: None,
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
    fn shipped_default_relay_is_the_maxplayer_launch_relay() {
        // Revert-guard for the #375 flip: a fresh config (relay_url absent from the file) must
        // resolve to the maxplayer launch relay, not the previous default. Pins the VALUE so a bad
        // rebase onto the neighbouring DEFAULT_* const block reddens instead of silently restoring
        // the old relay URL.
        assert_eq!(default_relay_url(), "wss://relay.maxplayer.ai");
        assert_eq!(DEFAULT_RELAY_URL, "wss://relay.maxplayer.ai");
    }

    #[test]
    fn shipped_default_git_base_is_on_the_launch_relay() {
        // Revert-guard for the #390 repoint, and a coherence pin for #394: the default git base
        // must live on the SAME host the default relay_url announces to — the kind-30617 announce
        // seeds git on whatever relay ingests it, and the seed probe checks DEFAULT_RELAY_GIT_BASE.
        // Splitting the two hosts bricks every fresh seller at the seed probe.
        assert_eq!(DEFAULT_RELAY_GIT_BASE, "https://relay.maxplayer.ai/git");
        let relay_host = DEFAULT_RELAY_URL.trim_start_matches("wss://");
        assert!(
            DEFAULT_RELAY_GIT_BASE.contains(relay_host),
            "git base ({DEFAULT_RELAY_GIT_BASE}) must be on the default relay's host ({relay_host})"
        );
    }

    #[test]
    fn save_persists_explicitly_chosen_field() {
        // The guarantee is only that UNCHOSEN env values do not leak. A value the caller
        // explicitly saves is persisted even when an env var also covers that field.
        let root = temp_home("save-explicit");
        let _ = fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");

        let file_before = load_config(&home.root.join(CONFIG_FILE)).expect("file before");
        home.config = apply_env_layer(&file_before, env(&[("MAXPLAYER_RELAY_URL", "wss://from-env")]))
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
        assert!(file.allow_real_mints); // default stands (file did not set it)

        let resolved = apply_env_layer(
            &file,
            env(&[
                ("MAXPLAYER_RELAY_URL", "wss://from-env"),
                ("MAXPLAYER_PER_JOB_BUDGET_SATS", "7"),
                ("MAXPLAYER_ACCEPTED_MINTS", "https://env-a,https://env-b"),
            ]),
        )
        .expect("env layer");

        assert_eq!(resolved.relay_url, "wss://from-env"); // env over file
        assert_eq!(resolved.per_job_budget_sats, 7); // env over file
        assert_eq!(
            resolved.accepted_mints,
            vec!["https://env-a".to_owned(), "https://env-b".to_owned()]
        ); // env list over file list
        assert!(resolved.allow_real_mints); // untouched default survives
    }

    #[test]
    fn env_layer_overrides_nested_field() {
        let base = MaxplayerConfig::default();
        let resolved = apply_env_layer(
            &base,
            env(&[("MAXPLAYER_SELLER_HEARTBEAT__INTERVAL_SECS", "42")]),
        )
        .expect("nested env");
        assert_eq!(resolved.seller_heartbeat.interval_secs, 42);
        assert!(resolved.seller_heartbeat.enabled); // sibling default preserved
    }

    #[test]
    fn env_layer_refuses_malformed_value_naming_the_key() {
        let error = apply_env_layer(
            &MaxplayerConfig::default(),
            env(&[("MAXPLAYER_PER_JOB_BUDGET_SATS", "not-a-number")]),
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
        // A MAXPLAYER_-prefixed var that is neither a field nor a reserved seam fails closed, and the
        // refusal names the VARIABLE the operator set (not just the derived field) plus the rule.
        let message = apply_env_layer(
            &MaxplayerConfig::default(),
            env(&[("MAXPLAYER_NO_SUCH_FIELD", "x")]),
        )
        .expect_err("unknown env must refuse")
        .to_string();
        assert!(message.contains("environment"), "names the layer: {message}");
        assert!(
            message.contains("MAXPLAYER_NO_SUCH_FIELD"),
            "names the actual variable, not just the derived field: {message}"
        );
        assert!(
            message.contains("reserved"),
            "states the reserved-namespace rule: {message}"
        );
        assert!(
            message.contains("MAXPLAYER_SELLER__RATE_SATS"),
            "shows the `__` nesting form: {message}"
        );
    }

    #[test]
    fn env_layer_refusal_names_the_single_underscore_footgun() {
        // The issue's case: an operator means the nested `sandbox.ro_paths` but writes a single `_`, so
        // the crate reads an unknown top-level field `sandbox_ro_paths`. The refusal must name THEIR
        // variable and the `_` vs `__` fix — not leave them staring at a derived field they never typed.
        let message = apply_env_layer(
            &MaxplayerConfig::default(),
            env(&[("MAXPLAYER_SANDBOX_RO_PATHS", "/x")]),
        )
        .expect_err("single-underscore nesting must refuse")
        .to_string();
        assert!(
            message.contains("MAXPLAYER_SANDBOX_RO_PATHS"),
            "names the operator's variable: {message}"
        );
        assert!(
            message.contains("double underscore") && message.contains("single '_'"),
            "explains the `_` vs `__` nesting fix: {message}"
        );
    }

    #[test]
    fn env_layer_refusal_names_a_nested_variable() {
        // A genuinely nested unknown (`seller.bogus`) resolves back to its full `__` variable, not the
        // bare leaf `bogus` the crate reports "for key `seller`".
        let message = apply_env_layer(
            &MaxplayerConfig::default(),
            env(&[("MAXPLAYER_SELLER__BOGUS", "x")]),
        )
        .expect_err("unknown nested env must refuse")
        .to_string();
        assert!(
            message.contains("MAXPLAYER_SELLER__BOGUS"),
            "names the full nested variable: {message}"
        );
    }

    #[test]
    fn env_layer_refusal_names_only_the_offender_beside_valid_vars() {
        // With a valid variable set alongside the unknown one, only the unknown is named — the leaf
        // match must not misattribute the refusal to a well-formed neighbour.
        let message = apply_env_layer(
            &MaxplayerConfig::default(),
            env(&[
                ("MAXPLAYER_RELAY_URL", "wss://valid"),
                ("MAXPLAYER_SANDBOX_RO_PATHS", "/x"),
            ]),
        )
        .expect_err("unknown env beside a valid one must still refuse")
        .to_string();
        assert!(
            message.contains("MAXPLAYER_SANDBOX_RO_PATHS"),
            "names the offender: {message}"
        );
        assert!(
            !message.contains("variable MAXPLAYER_RELAY_URL"),
            "must not blame the valid neighbour: {message}"
        );
    }

    #[test]
    fn reserved_env_seams_never_reach_the_config_layer() {
        // MAXPLAYER_HOME (and the daemon test seams) map to no field; excluding them is what keeps
        // resolution from refusing when they are set. The filtered map must drop them.
        let raw = env(&[
            ("MAXPLAYER_HOME", "/tmp/x"),
            ("MAXPLAYER_HEARTBEAT_INTERVAL_SECS", "9"),
            ("MAXPLAYER_RELAY_URL", "wss://kept"),
        ]);
        let kept: HashMap<String, String> = raw
            .into_iter()
            .filter(|(key, _)| key.starts_with("MAXPLAYER_") && !RESERVED_ENV_VARS.contains(&key.as_str()))
            .collect();
        assert_eq!(kept.len(), 1);
        assert!(kept.contains_key("MAXPLAYER_RELAY_URL"));
        // And resolution succeeds precisely because the reserved seams were dropped.
        let resolved = apply_env_layer(&MaxplayerConfig::default(), kept).expect("resolve");
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
    fn sandbox_launcher_empty_argv_error_names_sandbox_field_not_agent_command() {
        // #381: an empty `[sandbox] launcher = []` must not misdirect debugging toward
        // `agent_command` — the shared argv validator's message must be field-agnostic and
        // let the per-call-site path (which `toml` already attaches) do the naming.
        let message = parse_config_toml("[sandbox]\nlauncher = []\n")
            .expect_err("empty sandbox.launcher must refuse")
            .to_string();
        assert!(
            message.contains("sandbox.launcher"),
            "names the actual field that failed: {message}"
        );
        assert!(
            !message.contains("agent_command"),
            "must not misdirect toward agent_command: {message}"
        );

        // The bare-string variant of the same field must get the same treatment.
        let string_message = parse_config_toml("[sandbox]\nlauncher = 'x'\n")
            .expect_err("string sandbox.launcher must refuse")
            .to_string();
        assert!(
            string_message.contains("sandbox.launcher"),
            "names the actual field that failed: {string_message}"
        );
        assert!(
            !string_message.contains("agent_command"),
            "must not misdirect toward agent_command: {string_message}"
        );

        // Guard the OTHER direction: `agent_command` itself must still be named when IT is
        // the field that actually failed (regression guard for the shared deserializer).
        let seller_message = parse_config_toml(
            "[seller]\nagent_command = []\nrate_sats = 1\ngit_remote = 'https://relay.example/git/x/y.git'\n",
        )
        .expect_err("empty seller.agent_command must refuse")
        .to_string();
        assert!(
            seller_message.contains("seller.agent_command"),
            "still names agent_command when it's the actual offender: {seller_message}"
        );
    }

    #[test]
    fn env_only_boots_buyer_and_seller_without_a_file() {
        // File-less container: defaults alone already boot a BUYER (mint, relay, budget caps).
        let buyer = apply_env_layer(&MaxplayerConfig::default(), HashMap::new()).expect("buyer");
        assert!(!buyer.relay_url.is_empty());
        assert!(!buyer.default_mint().is_empty());
        assert!(buyer.per_job_budget_sats > 0);
        assert!(buyer.seller.is_none());

        // A SELLER needs only the seller table's required fields via env.
        let seller = apply_env_layer(
            &MaxplayerConfig::default(),
            env(&[
                ("MAXPLAYER_SELLER__AGENT_COMMAND", "claude,--headless"),
                ("MAXPLAYER_SELLER__RATE_SATS", "3"),
                ("MAXPLAYER_SELLER__GIT_REMOTE", "https://relay.example/git/x/y.git"),
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

    #[test]
    fn hop_fee_buffer_multiplier_defaults_to_two_and_zero_is_exact() {
        assert_eq!(default_hop_fee_buffer_multiplier(), 2);
        assert_eq!(
            MaxplayerConfig::default().buyer.hop_fee_buffer_multiplier,
            2
        );

        let absent = parse_config_toml("relay_url = 'r'\nper_job_budget_sats = 1\n")
            .expect("absent [buyer] parses");
        assert_eq!(absent.buyer.hop_fee_buffer_multiplier, 2);
        assert!(
            !toml::to_string_pretty(&absent)
                .expect("ser")
                .contains("[buyer]"),
            "default multiplier must not invent a [buyer] section"
        );

        let zero = parse_config_toml(
            "relay_url = 'r'\nper_job_budget_sats = 1\n[buyer]\nhop_fee_buffer_multiplier = 0\n",
        )
        .expect("zero multiplier parses");
        assert_eq!(zero.buyer.hop_fee_buffer_multiplier, 0);
        assert!(
            toml::to_string_pretty(&zero)
                .expect("ser")
                .contains("hop_fee_buffer_multiplier = 0"),
            "a non-default multiplier must serialize so an operator can see it"
        );

        let empty_table = parse_config_toml("relay_url = 'r'\nper_job_budget_sats = 1\n[buyer]\n")
            .expect("empty [buyer] parses");
        assert_eq!(empty_table.buyer.hop_fee_buffer_multiplier, 2);

        let from_env = apply_env_layer(
            &MaxplayerConfig::default(),
            env(&[("MAXPLAYER_BUYER__HOP_FEE_BUFFER_MULTIPLIER", "0")]),
        )
        .expect("nested env");
        assert_eq!(from_env.buyer.hop_fee_buffer_multiplier, 0);
    }
}
