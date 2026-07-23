//! `mobee sell` — seller daemon with good defaults.
//!
//! Required user choices: `--agent` (or `--agent-argv`) and `--rate-sats` on first run.
//! Everything else defaults (relay, mint, key 0600, relay-git delivery) and persists to
//! `config.toml` so subsequent launches are zero-prompt.
//!
//! Never accepts `--key` (key stays in `~/.mobee/key`; never argv).

use std::io::{self, Write};
use std::path::PathBuf;

use mobee_core::delivery_transport::is_relay_git_locator;
use mobee_core::home::{self, MobeeHome, SellerConfig, DEFAULT_MINT_URL, DEFAULT_RELAY_URL};
use mobee_core::profile::{self, SetProfileRequest};

use crate::agent_presets;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

#[derive(Debug, Default)]
struct SellOptions {
    /// Force fail-closed naming of missing fields (no TTY prompts).
    non_interactive: bool,
    /// Named preset: claude | cursor | codex.
    agent: Option<String>,
    /// Power-user escape hatch (repeatable).
    agent_argv: Vec<String>,
    rate_sats: Option<u64>,
    git_remote: Option<String>,
    job_timeout_secs: Option<u64>,
    /// Opt-in to claim untargeted/open offers (default OFF).
    claim_open_pool: Option<bool>,
    /// Open-pool offer-backfill window in seconds (default 1200 / 20 min; 0 = live-only).
    /// Targeted offers are unaffected (they always backfill in full).
    offer_backfill_secs: Option<u64>,
    name: Option<String>,
    home: Option<PathBuf>,
}

/// Entry from `cli::run` for `mobee sell ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let options = match SellOptions::parse(args) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            sell_usage(err);
            return USAGE_ERROR;
        }
    };

    #[cfg(not(feature = "wallet"))]
    {
        let _ = (options, out);
        let _ = writeln!(
            err,
            "mobee sell requires the wallet feature (rebuild with default features)"
        );
        return USAGE_ERROR;
    }

    #[cfg(feature = "wallet")]
    {
        match run_sell(options, out, err) {
            Ok(()) => SUCCESS,
            Err(code) => code,
        }
    }
}

#[cfg(feature = "wallet")]
fn run_sell(options: SellOptions, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), i32> {
    let root = match options.home.clone() {
        Some(path) => path,
        None => home::default_home_dir().map_err(|error| {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        })?,
    };
    let mut home = home::bootstrap(&root).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;

    // Explicit good defaults (never prompt for these). Persist through save_config's file-only
    // edit view so a MOBEE_* env override (which leaves the effective value non-empty) never gets
    // written back to config.toml.
    let needs_relay = home.config.relay_url.trim().is_empty();
    let needs_mints = home.config.accepted_mints.is_empty();
    if needs_relay || needs_mints {
        home::save_config(&mut home, |config| {
            if needs_relay {
                config.relay_url = DEFAULT_RELAY_URL.to_owned();
            }
            if needs_mints {
                config.accepted_mints = vec![DEFAULT_MINT_URL.to_owned()];
            }
        })
        .map_err(|error| {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        })?;
    }

    // Status must never echo the secret key.
    let _ = writeln!(
        err,
        "mobee sell home={} key_present={} mint={} relay={}",
        home.root.display(),
        home::key_file_present(&home),
        home.config.default_mint(),
        home.config.relay_url
    );

    ensure_seller_config(&mut home, &options, out, err)?;

    if let Some(name) = options.name.as_ref() {
        // set_profile publishes kind-0 over the relay (async); this sync CLI drives it on a
        // current-thread runtime. run_sell is never entered from inside a Tokio runtime (the seller
        // daemon builds its own later), so block_on cannot nest.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                let _ = writeln!(err, "{error}");
                RUNTIME_ERROR
            })?;
        runtime
            .block_on(profile::set_profile_async(
                &mut home,
                SetProfileRequest {
                    name: Some(name.clone()),
                    about: None,
                },
            ))
            .map_err(|error| {
                let _ = writeln!(err, "profile publish failed (fail-closed): {error}");
                RUNTIME_ERROR
            })?;
    }

    let seller = home.config.seller.clone().ok_or_else(|| {
        let _ = writeln!(err, "missing [seller] after ensure");
        RUNTIME_ERROR
    })?;

    // Relay-git: NIP-34 announce BEFORE any push (relay FORBIDs un-announced repos).
    // Relay `.names/<d>` is GLOBAL — collisions accept the event but skip seeding →
    // push 404s. Probe after announce so we never push into the void.
    if is_relay_git_locator(&seller.git_remote) {
        match profile::announce_seller_delivery_repo(&home, &seller.git_remote) {
            Ok(event_id) => {
                let _ = writeln!(
                    err,
                    "relay-git NIP-34 announce ok id={event_id} remote={}",
                    seller.git_remote
                );
            }
            Err(error) => {
                let _ = writeln!(
                    err,
                    "mobee-hosted delivery announce failed: {error}\n\
                     provide --git-remote <https-url> to use BYO delivery, or retry when relay-git is reachable"
                );
                return Err(RUNTIME_ERROR);
            }
        }
        if let Err(message) = probe_relay_git_seeded(&home, &seller.git_remote) {
            let _ = writeln!(err, "{message}");
            return Err(RUNTIME_ERROR);
        }
        let _ = writeln!(err, "relay-git seed probe ok (info/refs reachable)");
    }

    // Discoverability: clobber-safe kind-0 + idempotent NIP-89.
    let disco = profile::publish_seller_discoverability(&mut home).map_err(|error| {
        let _ = writeln!(
            err,
            "discoverability publish failed (fail-closed): {error}"
        );
        RUNTIME_ERROR
    })?;
    let _ = writeln!(
        err,
        "discoverable kind0={} nip89={} name={} pubkey={}",
        disco.kind0_event_id,
        disco.nip89_event_id,
        disco.name.as_deref().unwrap_or(""),
        disco.pubkey
    );

    let daemon = mobee_core::seller_daemon::SellerDaemon::open(home).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;
    let _ = writeln!(
        err,
        "seller starting pubkey={} agent={} rate_sats={} claim_open_pool={} git_remote={} (never-echo: key omitted)",
        daemon.seller_pubkey(),
        seller.agent.as_deref().unwrap_or("custom"),
        seller.rate_sats,
        seller.claim_open_pool,
        seller.git_remote
    );
    mobee_core::seller_daemon::run_forever_blocking(daemon).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;
    Ok(())
}

#[cfg(feature = "wallet")]
fn ensure_seller_config(
    home: &mut MobeeHome,
    options: &SellOptions,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), i32> {
    let existing = home.config.seller.clone();
    let existing_agent = existing.as_ref().and_then(|s| s.agent.clone());
    let steady_state = existing.is_some()
        && options.agent.is_none()
        && options.agent_argv.is_empty()
        && options.rate_sats.is_none()
        && options.git_remote.is_none();

    // Agent: preset | argv hatch | persisted config. Never re-prompt argv in steady state.
    let custom_agents = home.config.agents.clone();
    let (mut agent_label, mut agent_command) =
        resolve_agent(options, existing.as_ref(), &custom_agents, out, err)?;

    let mut rate_sats = options
        .rate_sats
        .or_else(|| existing.as_ref().map(|seller| seller.rate_sats));
    let mut git_remote = options.git_remote.clone().or_else(|| {
        existing
            .as_ref()
            .map(|seller| seller.git_remote.clone())
            .filter(|value| !value.trim().is_empty())
    });
    let job_timeout_secs = options
        .job_timeout_secs
        .or_else(|| existing.as_ref().and_then(|seller| seller.job_timeout_secs));
    let claim_open_pool = options
        .claim_open_pool
        .unwrap_or_else(|| existing.as_ref().map(|s| s.claim_open_pool).unwrap_or(false));
    // Offer backfill window: flag > existing config > serde default (1200s / 20 min).
    let offer_backfill_secs = options.offer_backfill_secs.unwrap_or_else(|| {
        existing
            .as_ref()
            .map(|s| s.offer_backfill_secs)
            .unwrap_or_else(home::default_offer_backfill_secs)
    });

    // Default delivery = relay-git (self-owned namespace).
    if git_remote.as_ref().map(|v| v.trim().is_empty()).unwrap_or(true) {
        let pubkey = home::public_key_hex(home).map_err(|error| {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        })?;
        git_remote = Some(home::default_relay_git_remote(&pubkey));
        let _ = writeln!(
            err,
            "git_remote defaulting to relay-git {}",
            git_remote.as_deref().unwrap_or("")
        );
    }

    let interactive = !options.non_interactive && !steady_state && atty_stderr();
    if options.non_interactive || steady_state {
        let mut missing = Vec::new();
        if agent_command.is_empty() {
            missing.push("agent (--agent claude|cursor|codex, or --agent-argv)");
        }
        if rate_sats.is_none() {
            missing.push("rate_sats (--rate-sats)");
        }
        if git_remote.as_ref().map(|v| v.trim().is_empty()).unwrap_or(true) {
            missing.push("git_remote");
        }
        if !missing.is_empty() {
            let _ = writeln!(
                err,
                "mobee sell missing required field(s): {}",
                missing.join(", ")
            );
            let available = agent_presets::detect_available_agents(&custom_agents);
            if !available.is_empty() {
                let _ = writeln!(err, "agents detected on PATH: {}", available.join(", "));
            }
            return Err(USAGE_ERROR);
        }
    } else if interactive {
        if agent_command.is_empty() {
            let available = agent_presets::detect_available_agents(&custom_agents);
            let suggestion = available.first().map(String::as_str).unwrap_or("claude");
            let detected = if available.is_empty() {
                "none".to_owned()
            } else {
                available.join(", ")
            };
            let _ = writeln!(
                out,
                "Pick an agent preset ({}). Detected: {detected}",
                agent_presets::preset_choices(&custom_agents)
            );
            let picked = prompt_line(out, err, "Agent", suggestion)?;
            let (label, argv) = agent_presets::resolve_agent_preset(&picked, &custom_agents)
                .map_err(|message| {
                    let _ = writeln!(err, "{message}");
                    USAGE_ERROR
                })?;
            agent_command = argv;
            agent_label = Some(label.clone());
            let _ = writeln!(err, "agent preset={label} argv0={}", agent_command[0]);
        }
        if rate_sats.is_none() {
            rate_sats = Some(prompt_u64(
                out,
                err,
                "Seller rate_sats (claim floor, sats)",
                2,
            )?);
        }
    } else if agent_command.is_empty() || rate_sats.is_none() {
        // Non-TTY first run without flags.
        let _ = writeln!(
            err,
            "mobee sell: pass --agent <claude|cursor|codex> --rate-sats <n> \
             (or run in a TTY for the guided wizard)"
        );
        return Err(USAGE_ERROR);
    }

    // Prefer the resolved preset label (the configured/normalized name) — it is the harness
    // identity reported in results.
    let agent = agent_label
        .or_else(|| options.agent.clone())
        .or(existing_agent);

    let seller = SellerConfig {
        agent_command,
        rate_sats: rate_sats.ok_or_else(|| {
            let _ = writeln!(err, "missing required field rate_sats (--rate-sats)");
            USAGE_ERROR
        })?,
        git_remote: git_remote.ok_or_else(|| {
            let _ = writeln!(err, "missing required field git_remote");
            USAGE_ERROR
        })?,
        job_timeout_secs,
        agent,
        claim_open_pool,
        offer_backfill_secs,
        // Piece-10: contribution (freelance-PR fork) support ON by default for CLI-configured
        // sellers. Operators disable it by editing `[seller] contribution_enabled = false`.
        contribution_enabled: true,
    };
    home::save_config(home, |config| {
        config.seller = Some(seller);
    })
    .map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;
    let _ = writeln!(
        err,
        "wrote [seller] to {}",
        home.root.join("config.toml").display()
    );
    Ok(())
}

#[cfg(feature = "wallet")]
fn resolve_agent(
    options: &SellOptions,
    existing: Option<&SellerConfig>,
    custom_agents: &std::collections::BTreeMap<String, home::AgentPresetConfig>,
    _out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(Option<String>, Vec<String>), i32> {
    if !options.agent_argv.is_empty() {
        if options.agent.is_some() {
            let _ = writeln!(err, "refused: pass either --agent or --agent-argv, not both");
            return Err(USAGE_ERROR);
        }
        return Ok((None, options.agent_argv.clone()));
    }
    if let Some(name) = options.agent.as_ref() {
        let (label, argv) =
            agent_presets::resolve_agent_preset(name, custom_agents).map_err(|message| {
                let _ = writeln!(err, "{message}");
                USAGE_ERROR
            })?;
        let _ = writeln!(err, "agent preset={label} argv0={}", argv[0]);
        return Ok((Some(label), argv));
    }
    if let Some(seller) = existing {
        return Ok((seller.agent.clone(), seller.agent_command.clone()));
    }
    Ok((None, Vec::new()))
}

fn atty_stderr() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// After NIP-34 announce, confirm the relay seeded the empty-manifest pointer.
///
/// Event-accept alone is insufficient: a global `.names/<d>` collision stores the
/// kind-30617 but skips seed → later push 404s ("repository not found").
#[cfg(feature = "wallet")]
fn probe_relay_git_seeded(home: &MobeeHome, remote_url: &str) -> Result<(), String> {
    // In-process libgit2 ls-remote (issue #55 — no system `git`). NIP-98 is signed from the seller
    // secret in-process for the relay-git upload-pack advertisement; the secret never hits argv/env.
    let secret = home::read_secret_key_hex(home).map_err(|e| e.to_string())?;
    let result = mobee_core::git_transport::ls_remote(remote_url, Some(&secret));
    drop(secret);
    match result {
        Ok(_) => Ok(()),
        Err(error) => {
            let message = error.to_string().to_lowercase();
            if message.contains("repository not found") || message.contains("404") {
                Err(format!(
                    "mobee-hosted delivery not seeded after NIP-34 announce (ls-remote 404).\n\
                     likely cause: relay-git global name collision on repo id, or seed side-effect failed.\n\
                     provide --git-remote <https-url> for BYO delivery, or pick a unique remote leaf.\n\
                     remote={remote_url}"
                ))
            } else {
                Err(format!(
                    "mobee-hosted delivery seed probe failed (in-process ls-remote): {error}.\n\
                     provide --git-remote <https-url> for BYO delivery.\n\
                     remote={remote_url}"
                ))
            }
        }
    }
}

fn prompt_line(
    out: &mut dyn Write,
    err: &mut dyn Write,
    label: &str,
    default: &str,
) -> Result<String, i32> {
    if default.is_empty() {
        let _ = write!(out, "{label}: ");
    } else {
        let _ = write!(out, "{label} [{default}]: ");
    }
    let _ = out.flush();
    let mut line = String::new();
    io::stdin().read_line(&mut line).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;
    let trimmed = line.trim().to_owned();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed)
    }
}

fn prompt_u64(
    out: &mut dyn Write,
    err: &mut dyn Write,
    label: &str,
    default: u64,
) -> Result<u64, i32> {
    let raw = prompt_line(out, err, label, &default.to_string())?;
    raw.parse().map_err(|_| {
        let _ = writeln!(err, "{label} must be a u64");
        USAGE_ERROR
    })
}

impl SellOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self::default();
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--non-interactive" => options.non_interactive = true,
                "--claim-open-pool" => options.claim_open_pool = Some(true),
                "--no-claim-open-pool" => options.claim_open_pool = Some(false),
                "--key" | "--secret-key" | "--private-key" => {
                    return Err(
                        "refused: --key / secret key argv is not allowed (key stays in home file)"
                            .into(),
                    );
                }
                "--agent" => {
                    index += 1;
                    let name = args
                        .get(index)
                        .ok_or_else(|| "missing value for --agent".to_owned())?;
                    options.agent = Some(name.clone());
                }
                "--agent-argv" => {
                    index += 1;
                    let part = args
                        .get(index)
                        .ok_or_else(|| "missing value for --agent-argv".to_owned())?;
                    if part.is_empty() {
                        return Err("--agent-argv entries must be non-empty".into());
                    }
                    options.agent_argv.push(part.clone());
                }
                "--rate-sats" => {
                    index += 1;
                    let raw = args
                        .get(index)
                        .ok_or_else(|| "missing value for --rate-sats".to_owned())?;
                    options.rate_sats = Some(
                        raw.parse()
                            .map_err(|_| format!("--rate-sats must be a u64, got {raw}"))?,
                    );
                }
                "--git-remote" => {
                    index += 1;
                    options.git_remote = Some(
                        args.get(index)
                            .ok_or_else(|| "missing value for --git-remote".to_owned())?
                            .clone(),
                    );
                }
                "--job-timeout-secs" => {
                    index += 1;
                    let raw = args
                        .get(index)
                        .ok_or_else(|| "missing value for --job-timeout-secs".to_owned())?;
                    options.job_timeout_secs = Some(
                        raw.parse()
                            .map_err(|_| format!("--job-timeout-secs must be a u64, got {raw}"))?,
                    );
                }
                "--offer-backfill-secs" => {
                    index += 1;
                    let raw = args
                        .get(index)
                        .ok_or_else(|| "missing value for --offer-backfill-secs".to_owned())?;
                    options.offer_backfill_secs = Some(
                        raw.parse()
                            .map_err(|_| format!("--offer-backfill-secs must be a u64, got {raw}"))?,
                    );
                }
                "--name" => {
                    index += 1;
                    options.name = Some(
                        args.get(index)
                            .ok_or_else(|| "missing value for --name".to_owned())?
                            .clone(),
                    );
                }
                "--home" => {
                    index += 1;
                    options.home = Some(PathBuf::from(
                        args.get(index)
                            .ok_or_else(|| "missing value for --home".to_owned())?,
                    ));
                }
                other => return Err(format!("unknown sell option: {other}")),
            }
            index += 1;
        }
        Ok(options)
    }
}

fn sell_usage(err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "Usage:\n  mobee sell --agent <claude|cursor|codex> --rate-sats <n> [--git-remote <url>] [--claim-open-pool] [--name <display>] [--home <dir>]\n  mobee sell   # zero-prompt relaunch from config.toml\n  mobee sell --agent-argv <prog> [--agent-argv <arg> ...] --rate-sats <n>   # power-user hatch\n\nNotes:\n  - required user choices: --agent (or --agent-argv) + --rate-sats (first run)\n  - defaults: relay=wss://mobee-relay.orveth.dev mint=testnut git-remote=relay-git key=0600 auto\n  - no --key (packaged key file only)\n  - open-pool claiming is OFF by default; pass --claim-open-pool to opt in\n  - --offer-backfill-secs <n>: see OPEN-POOL offers posted up to n seconds before startup (default 1200; 0 = live-only; targeted offers always backfill)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_key_argv() {
        let err = SellOptions::parse(&["--key".into(), "deadbeef".into()]).unwrap_err();
        assert!(err.contains("not allowed"));
    }

    #[test]
    fn parses_agent_preset_and_rate() {
        let options = SellOptions::parse(&[
            "--agent".into(),
            "claude".into(),
            "--rate-sats".into(),
            "2".into(),
            "--claim-open-pool".into(),
        ])
        .expect("parse");
        assert_eq!(options.agent.as_deref(), Some("claude"));
        assert_eq!(options.rate_sats, Some(2));
        assert_eq!(options.claim_open_pool, Some(true));
    }

    #[test]
    fn parses_agent_argv_array() {
        let options = SellOptions::parse(&[
            "--non-interactive".into(),
            "--agent-argv".into(),
            "cursor-agent".into(),
            "--agent-argv".into(),
            "acp".into(),
            "--rate-sats".into(),
            "21".into(),
            "--git-remote".into(),
            "https://example.invalid/repo.git".into(),
        ])
        .expect("parse");
        assert!(options.non_interactive);
        assert_eq!(
            options.agent_argv,
            vec!["cursor-agent".to_owned(), "acp".to_owned()]
        );
        assert_eq!(options.rate_sats, Some(21));
    }

    #[test]
    fn missing_required_names_agent_and_rate() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            &[
                "--non-interactive".into(),
                "--home".into(),
                std::env::temp_dir()
                    .join(format!("mobee-sell-miss-{}", std::process::id()))
                    .to_string_lossy()
                    .into_owned(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, USAGE_ERROR);
        let rendered = String::from_utf8_lossy(&err);
        assert!(
            rendered.contains("agent") && rendered.contains("rate_sats"),
            "stderr={rendered}"
        );
        assert!(!rendered.to_ascii_lowercase().contains("nsec"));
    }
}
