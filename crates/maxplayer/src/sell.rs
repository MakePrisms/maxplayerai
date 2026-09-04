//! `maxplayer seller` — seller daemon with good defaults.
//!
//! Required user choices: `--agent` (or `--agent-argv`) and `--rate-sats` on first run.
//! Everything else defaults (relay, mint, key 0600, relay-git delivery) and persists to
//! `config.toml` so subsequent launches are zero-prompt.
//!
//! On startup it runs the `doctor` readiness gate (issue #107) and REFUSES to boot when a blocking
//! check fails (no working nix, agent unresolvable, no mint reachable, seller key missing, relay
//! unreachable), printing a per-failure fix hint. Pass `--skip-doctor` to bypass the readiness
//! checks (default: checks-on); the nix ENVIRONMENT check (#745) still runs — it has no bypass.
//!
//! Never accepts `--key` (key stays in `~/.maxplayer/key`; never argv).

use std::io::{self, Write};
use std::path::PathBuf;

use maxplayer_core::delivery_transport::is_relay_git_locator;
use maxplayer_core::home::{
    self, MaxplayerHome, SellerConfig, DEFAULT_MINT_URL, DEFAULT_RATE_SATS, DEFAULT_RELAY_URL,
};
use maxplayer_core::profile::{self, SetProfileRequest};

use maxplayer_core::agent_presets;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

/// Decide whether `maxplayer seller` may proceed past the startup gate. The gate ALWAYS runs
/// (#745): `--skip-doctor` narrows it to the environment requirement (nix) instead of skipping it,
/// so there is deliberately no "gate never ran" arm here — that arm was the escape hatch #745
/// rules out. A failed gate aborts startup. Pure so the run_sell wiring is unit-tested both ways.
fn readiness_decision(gate: Result<(), ()>) -> Result<(), i32> {
    gate.map_err(|()| RUNTIME_ERROR)
}

#[derive(Debug, Default)]
struct SellOptions {
    /// Force fail-closed naming of missing fields (no TTY prompts).
    non_interactive: bool,
    /// The preferred named preset — the first `--agent`.
    agent: Option<String>,
    /// Every `--agent` in the order given: the harness registry, preference first. One entry is
    /// the single-harness case and writes the config a single-harness seller has always had.
    agents: Vec<String>,
    /// Power-user escape hatch (repeatable).
    agent_argv: Vec<String>,
    rate_sats: Option<u64>,
    git_remote: Option<String>,
    job_timeout_secs: Option<u64>,
    /// Opt-in to claim untargeted/open offers (default OFF).
    claim_open_pool: Option<bool>,
    /// Opt-in to accept targeted offers from buyers this seat has not named (default OFF).
    /// A separate surface from `claim_open_pool` — see `SellerConfig::accept_open_targeted`.
    accept_open_targeted: Option<bool>,
    /// Open-pool offer-backfill window in seconds (default 1200 / 20 min; 0 = live-only).
    /// Targeted offers are unaffected (they always backfill in full).
    offer_backfill_secs: Option<u64>,
    name: Option<String>,
    home: Option<PathBuf>,
    /// Bypass the startup doctor readiness gate (issue #107). Default is checks-ON; this is a
    /// documented escape hatch for operators who knowingly want to boot without passing checks.
    skip_doctor: bool,
    /// Serve a STRANGER-FACING surface with no working sandbox, deliberately (#451). Either open
    /// surface counts — claiming the open pool, or accepting targeted offers from buyers this seat
    /// never named — because both put code written by someone the operator did not choose on this
    /// box, and the containment finding blocks on both. Narrower than
    /// `--skip-doctor`, and that is the point: this waives ONE finding and leaves every other check
    /// blocking, so an operator who accepts the code-execution exposure does not also switch off the
    /// relay, mint and key gates to get past it.
    unsafe_no_sandbox: bool,
}

/// Entry from `cli::run` for `maxplayer seller ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // Help that was asked for goes to stdout and succeeds (mirrors `cli::write_usage`): `--help`
    // must print the seller usage, not reach the parser's catch-all and be reported as an unknown
    // option (empty stdout, exit 1 — the pre-existing regression the rename never carried). Shares
    // the crate-wide sole-help predicate (issue #570): a bare `--help` succeeds, while a `--help`
    // that follows a flag (e.g. an `--agent-argv --help` value) still reaches the parser unchanged.
    if crate::cli::is_help_request(args) {
        sell_usage(out);
        return SUCCESS;
    }
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
            "maxplayer seller requires the wallet feature (rebuild with default features)"
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
    // edit view so a MAXPLAYER_* env override (which leaves the effective value non-empty) never gets
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
        "maxplayer seller home={} key_present={} mint={} relay={}",
        home.root.display(),
        home::key_file_present(&home),
        home.config.default_mint(),
        home.config.relay_url
    );

    ensure_seller_config(&mut home, &options, out, err)?;

    // Auto-doctor (issue #107): refuse to boot a box that cannot sell. Runs the SAME readiness
    // checks as `maxplayer doctor` and blocks only on FAILs (agent unresolvable, no accepted mint
    // reachable, seller key missing, relay unreachable); WARNs are advisory. Runs AFTER
    // ensure_seller_config so the agent-preset check sees the just-resolved [seller], and BEFORE
    // any network mutation (NIP-34 announce / discoverability publish) so we fail fast without
    // side effects. Still fail-closed, but a purely TRANSIENT failure (relay or all mints briefly
    // unreachable) is retried with backoff before refusing, so an unsupervised seat rides out a
    // boot-time dependency blip instead of dying to it (issue #553); an unrecoverable failure still
    // refuses at once. `--skip-doctor` bypasses the READINESS checks only — the gate itself always
    // runs, narrowed to the nix ENVIRONMENT check (#745): nix asks whether this box can EVER do
    // the work, not whether it is ready right now, and #745 rules out any escape hatch for it.
    let gate = crate::doctor::sell_readiness_gate(
        &home,
        options.unsafe_no_sandbox,
        options.skip_doctor,
        out,
        err,
    );
    readiness_decision(gate)?;

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
                    "maxplayer-hosted delivery announce failed: {error}\n\
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

    // Boot the durable seller node (sqlite store + outbox + reconcile_on_start) as the seller path.
    // run_sell is synchronous, so it owns a runtime here and block_on's the async boot + run loop.
    //
    // MULTI-THREAD is deliberate. On a current-thread runtime the single thread drives futures, the
    // I/O driver AND the timer wheel, so any blocking or stalled call stops *time* — the heartbeat
    // tick, the relay-stall watchdog and every relay notification die together, at 0% CPU, silently
    // (#173). Blocking git work is now handed to `spawn_blocking` and the signer round-trips are
    // bounded, which is the real fix; a worker pool is the complement that keeps one stuck task from
    // taking the timer with it. Two workers is enough for a node whose concurrency is one job at a
    // time — the point is that the timer is never the only thing waiting on a busy thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| {
            let _ = writeln!(err, "tokio runtime: {error}");
            RUNTIME_ERROR
        })?;

    // Prove-before-advertise (#357): probe each configured harness once, THEN publish the
    // clobber-safe kind-0 identity and boot serving ONLY the harnesses that proved
    // they can deliver a probe artifact. If NONE prove out, advertise nothing and refuse to start
    // (fail loud) — a seat that cannot deliver must never appear on the market, because under
    // award-is-payment a buyer commits the sats at award. The probe is local compute only (no
    // sats/mint); the gate + roster narrowing live in maxplayer-core so the kind-30340 heartbeat is
    // honest for free.
    let runner = runtime
        .block_on(async {
            let verdicts = maxplayer_core::seller_node::run::probe_configured_harnesses(&home).await?;
            maxplayer_core::seller_node::run::boot_advertising_only_proven(home, verdicts).await
        })
        .map_err(|error| {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        })?;
    let _ = writeln!(
        err,
        // Both open surfaces AND the size of the allowlist, because the seat's reachability is now a
        // three-knob answer and any one of them alone reads as the whole posture. The allowlist is
        // reported as a COUNT, never its contents: a private seller's buyer list is not boot-log
        // material, and the count is what an operator needs to tell "I named someone" from "I did
        // not" — the distinction that decides whether an empty list leaves them reachable.
        "seller node starting pubkey={} agent={} rate_sats={} claim_open_pool={} accept_open_targeted={} accept_offers_only_from={} git_remote={} (never-echo: key omitted)",
        runner.seller_pubkey(),
        seller.agents.first().map(String::as_str).unwrap_or("custom"),
        seller.rate_sats,
        seller.claim_open_pool,
        seller.accept_open_targeted,
        seller.accept_offers_only_from.len(),
        seller.git_remote
    );
    // #747: SIGTERM/SIGINT must reach the run loop rather than the kernel's default disposition. A
    // seat's kind-30340 announcement is addressable, so whatever it published last is its permanent
    // public answer — a process killed outright leaves `accepting=y` standing forever, and because
    // the kind is replaceable no later event ever corrects it. Listening lets the loop exit through
    // its own path and publish the terminal `accepting=n` beat first. This covers `Ctrl-C`,
    // `systemctl stop`, `docker stop` and a pod delete; it CANNOT cover SIGKILL, a panic, an OOM
    // kill or a power cut, which run no code at all — a reader must still treat an old announcement
    // as stale.
    runtime
        .block_on(async {
            maxplayer_core::seller_node::shutdown::spawn_os_signal_listener(
                runner.shutdown_handle(),
            );
            runner.run().await
        })
        .map_err(|error| {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        })?;
    Ok(())
}

#[cfg(feature = "wallet")]
fn ensure_seller_config(
    home: &mut MaxplayerHome,
    options: &SellOptions,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), i32> {
    let existing = home.config.seller.clone();
    // Advertised label of an existing config = its first `agents` entry (issue #378 removed the
    // singular `agent` field).
    let existing_agent = existing.as_ref().and_then(|s| s.agents.first().cloned());
    let steady_state = existing.is_some()
        && options.agent.is_none()
        && options.agent_argv.is_empty()
        && options.rate_sats.is_none()
        && options.git_remote.is_none();

    // Agent: preset | argv hatch | persisted config. Never re-prompt argv in steady state.
    // Docker mode runs the adapter INSIDE the image, so resolution must not consult the host PATH
    // (the image bakes the adapter in); every other executor runs it on the host.
    let custom_agents = home.config.agents.clone();
    let adapter_host = agent_presets::AdapterHost::for_sandbox(home.config.sandbox.as_ref());
    let (mut agent_label, mut agent_command) =
        resolve_agent(options, existing.as_ref(), &custom_agents, adapter_host, out, err)?;

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
    // Same precedence as the pool flag: explicit flag > existing config > CLOSED. Carrying the
    // existing value is what stops a plain relaunch from silently closing a seat its operator had
    // opened — the #369-class clobber, in the direction that takes a working seller off the market.
    let accept_open_targeted = options
        .accept_open_targeted
        .unwrap_or_else(|| existing.as_ref().map(|s| s.accept_open_targeted).unwrap_or(false));
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
                "maxplayer seller missing required field(s): {}",
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
            let (label, argv) = agent_presets::resolve_agent_preset_in(&picked, &custom_agents, adapter_host)
                .map_err(|message| {
                    let _ = writeln!(err, "{message}");
                    USAGE_ERROR
                })?;
            agent_command = argv;
            agent_label = Some(label.clone());
            report_agent_preset(err, &label, &agent_command[0], &custom_agents);
        }
        if rate_sats.is_none() {
            rate_sats = Some(prompt_u64(
                out,
                err,
                "Seller rate_sats (claim floor, sats)",
                DEFAULT_RATE_SATS,
            )?);
        }
    } else if agent_command.is_empty() || rate_sats.is_none() {
        // Non-TTY first run without flags.
        let _ = writeln!(
            err,
            "maxplayer seller: pass --agent <claude|cursor|codex> --rate-sats <n> \
             (or run in a TTY for the guided wizard)"
        );
        return Err(USAGE_ERROR);
    }

    // The advertised harness label. Prefer the resolved preset label (the configured/normalized
    // name); issue #378 removed the singular `agent` field, so this label lives in `agents` below —
    // the wire name is `agents.first()`.
    let agent_label = agent_label
        .or_else(|| options.agent.clone())
        .or(existing_agent);

    // The harness registry (`Vec<String>` of preset names since #378). Every named preset is resolved
    // here so a typo or a missing adapter is a config-time refusal, not a boot-time degrade. A bare
    // relaunch preserves the existing registry IN FULL; explicit `--agent`s rebuild it.
    let agents = if options.agents.len() > 1 {
        // Multiple `--agent`s: resolve each named preset in preference order (dedup, order kept).
        let mut resolved: Vec<String> = Vec::with_capacity(options.agents.len());
        for name in &options.agents {
            let (label, _argv) = agent_presets::resolve_agent_preset_in(name, &custom_agents, adapter_host)
                .map_err(|message| {
                    let _ = writeln!(err, "{message}");
                    USAGE_ERROR
                })?;
            if !resolved.iter().any(|existing| existing == &label) {
                resolved.push(label);
            }
        }
        let _ = writeln!(err, "agent registry: {}", resolved.join(", "));
        resolved
    } else if options.agents.is_empty()
        && options.agent.is_none()
        && options.agent_argv.is_empty()
    {
        // No explicit agent input this run (a bare relaunch): carry the existing registry IN FULL so a
        // multi-harness `agents` list is never truncated to its first entry (#369 clobber class — the
        // member `agents` itself, alongside slots/contribution/claim-timeout). With no existing registry
        // to preserve, fall through to the freshly-resolved single label (first-time wizard) or nothing.
        match existing.as_ref().map(|seller| seller.agents.clone()) {
            Some(list) if !list.is_empty() => list,
            _ => agent_label.map(|label| vec![label]).unwrap_or_default(),
        }
    } else if let Some(label) = agent_label {
        // A single explicit `--agent` (or wizard pick): a one-entry registry keeping its advertised name
        // (the exact shape a pre-#378 `agent = "x"` migrates to).
        vec![label]
    } else {
        // Raw-argv hatch: no label — serves unlabelled through the `agent_command` fallback.
        Vec::new()
    };

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
        agents,
        claim_open_pool,
        accept_open_targeted,
        // Free-lane opt-in (spec §2.1/§4.1): carried from an existing config so a relaunch never
        // clobbers it (#369-class), defaulting to false on a fresh config. There is deliberately no
        // CLI flag — `takes_no_payment` is only valid with `rate_sats = 0` and hands the operator's
        // only remaining control to admission, so it is an edit an operator makes deliberately in
        // `[seller]`, not one a wizard can set in passing.
        takes_no_payment: existing.as_ref().map(|s| s.takes_no_payment).unwrap_or(false),
        // Buyer allowlist (#482): carried from an existing config so a relaunch never clobbers an
        // operator's private-seller fence (#369-class); a fresh config defaults empty, which now
        // means "no buyer named" rather than accept-all — reachability is decided by the two
        // surface flags. Operators edit it via `[seller] accept_offers_only_from`.
        accept_offers_only_from: existing
            .as_ref()
            .map(|s| s.accept_offers_only_from.clone())
            .unwrap_or_default(),
        offer_backfill_secs,
        // Contribution (freelance-PR fork) support: carried from an existing config so a relaunch
        // never clobbers an operator's `contribution_enabled = false` back to true (#369-class); a
        // fresh config defaults ON. Operators toggle it by editing `[seller] contribution_enabled`.
        contribution_enabled: existing
            .as_ref()
            .map(|s| s.contribution_enabled)
            .unwrap_or(true),
        // Concurrency inherits an existing config, else the built-in default (3 since #378);
        // operators tune it by editing `[seller] slots = N`.
        slots: existing.as_ref().map(|s| s.slots).unwrap_or_else(home::default_slots),
        claim_award_timeout_secs: existing.as_ref().and_then(|s| s.claim_award_timeout_secs),
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
    host: agent_presets::AdapterHost,
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
            agent_presets::resolve_agent_preset_in(name, custom_agents, host).map_err(|message| {
                let _ = writeln!(err, "{message}");
                USAGE_ERROR
            })?;
        report_agent_preset(err, &label, &argv[0], custom_agents);
        return Ok((Some(label), argv));
    }
    if let Some(seller) = existing {
        return Ok((seller.agents.first().cloned(), seller.agent_command.clone()));
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
fn probe_relay_git_seeded(home: &MaxplayerHome, remote_url: &str) -> Result<(), String> {
    // In-process libgit2 ls-remote (issue #55 — no system `git`). NIP-98 is signed from the seller
    // secret in-process for the relay-git upload-pack advertisement; the secret never hits argv/env.
    let secret = home::read_secret_key_hex(home).map_err(|e| e.to_string())?;
    let result = maxplayer_core::git_transport::ls_remote(remote_url, Some(&secret));
    drop(secret);
    match result {
        Ok(_) => Ok(()),
        Err(error) => {
            let message = error.to_string().to_lowercase();
            if message.contains("repository not found") || message.contains("404") {
                Err(format!(
                    "maxplayer-hosted delivery not seeded after NIP-34 announce (ls-remote 404).\n\
                     likely cause: relay-git global name collision on repo id, or seed side-effect failed.\n\
                     provide --git-remote <https-url> for BYO delivery, or pick a unique remote leaf.\n\
                     remote={remote_url}"
                ))
            } else {
                Err(format!(
                    "maxplayer-hosted delivery seed probe failed (in-process ls-remote): {error}.\n\
                     provide --git-remote <https-url> for BYO delivery.\n\
                     remote={remote_url}"
                ))
            }
        }
    }
}

/// Report a resolved preset, and — for a built-in — the underlying agent CLI it still needs (#488).
///
/// Both preset paths (the guided wizard and the `--agent` flag) report through here, so a seller
/// learns the prerequisite whichever way they picked the preset. Before this, the only thing that
/// ever mentioned the underlying CLI was the probe's `-32000 Authentication required` failure,
/// which arrives after every readiness check has already printed PASS.
/// A `[agents]` entry may override a built-in NAME while launching something else entirely, and
/// that config wins in `resolve_agent_preset`. Its prerequisites are then the operator's, not the
/// built-in's — so an overridden name reports no prerequisite rather than a confidently wrong one.
fn report_agent_preset(
    err: &mut dyn Write,
    label: &str,
    argv0: &str,
    custom_agents: &std::collections::BTreeMap<String, home::AgentPresetConfig>,
) {
    let _ = writeln!(err, "agent preset={label} argv0={argv0}");
    if custom_agents.contains_key(label) {
        return;
    }
    if let Some(prerequisite) = agent_presets::preset_prerequisite(label) {
        let _ = writeln!(err, "  {label} also requires {prerequisite}");
        let _ = writeln!(err, "  {}", agent_presets::PREREQUISITE_ENFORCEMENT);
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
                "--skip-doctor" => options.skip_doctor = true,
                "--unsafe-no-sandbox" => options.unsafe_no_sandbox = true,
                "--claim-open-pool" => options.claim_open_pool = Some(true),
                "--no-claim-open-pool" => options.claim_open_pool = Some(false),
                // Both open surfaces are opt-IN, so the bare flag TURNS ON and the `--no-` form is
                // the explicit restatement of the default. A `--no-accept-open-targeted` that had to
                // be passed to get the safe posture would put the flag set out of step with the
                // defaults, which is how an operator ends up believing they closed something.
                "--accept-open-targeted" => options.accept_open_targeted = Some(true),
                "--no-accept-open-targeted" => options.accept_open_targeted = Some(false),
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
                    // Repeatable: each occurrence enables one more harness, first = preferred.
                    options.agents.push(name.clone());
                    options.agent.get_or_insert_with(|| name.clone());
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
                other => return Err(format!("unknown seller option: {other}")),
            }
            index += 1;
        }
        Ok(options)
    }
}

fn sell_usage(w: &mut dyn Write) {
    let _ = writeln!(
        w,
        "Usage:\n  maxplayer seller --agent <claude|cursor|codex> --rate-sats <n> [--git-remote <url>] [--claim-open-pool] [--accept-open-targeted] [--name <display>] [--home <dir>] [--skip-doctor]\n  maxplayer seller   # zero-prompt relaunch from config.toml\n  maxplayer seller --agent-argv <prog> [--agent-argv <arg> ...] --rate-sats <n>   # power-user hatch\n\nNotes:\n  - required user choices: --agent (or --agent-argv) + --rate-sats (first run)\n  - defaults: relay=wss://relay.maxplayer.ai mint=mint.minibits.cash git-remote=relay-git key=0600 auto\n  - no --key (packaged key file only)\n  - startup runs the doctor readiness gate and REFUSES to boot on a blocking failure (no working nix, agent unresolvable, no mint reachable, seller key missing, relay unreachable), each with a fix hint\n  - --skip-doctor: bypass the startup readiness checks (default: checks-on; not recommended). The nix check still runs — it is an environment requirement (#745) with no bypass\n  - --unsafe-no-sandbox: serve a STRANGER-FACING surface with no working sandbox (either open surface) — this box then runs code written by strangers with no containment (waives only that one check)\n  - BOTH open surfaces are OFF by default, and they are separate: --claim-open-pool opts in to untargeted pool offers, --accept-open-targeted opts in to targeted offers from buyers you have not named\n  - with neither set and no [seller] accept_offers_only_from, this seat claims NOTHING and says so at boot\n  - --offer-backfill-secs <n>: see OPEN-POOL offers posted up to n seconds before startup (default 1200; 0 = live-only; targeted offers always backfill)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // #595 (mints-are-mints): the seller `--help` names the shipped default mint but must not fork
    // mint classes or say "real" — a REAL label on the normal path is a constant column (zero
    // information; testnut never appears in normal use). This also binds the string to a test: the
    // #447/#595 root bug was a money string nothing checked, which let the copy drift from the mint
    // the code ships (this help had already drifted from its SELLER-QUICKSTART.md mirror).
    // RED-ON-REVERT: re-adding "(a REAL mint — jobs settle in real sats)" reds this.
    #[test]
    fn sell_usage_does_not_fork_mint_classes_or_say_real() {
        let mut buf = Vec::new();
        sell_usage(&mut buf);
        let help = String::from_utf8_lossy(&buf);
        assert!(
            !help.contains("REAL")
                && !help.to_lowercase().contains("real sats")
                && !help.to_lowercase().contains("real mint"),
            "seller --help must not fork mint classes or say 'real':\n{help}"
        );
    }

    /// #488: picking a preset must surface the underlying CLI's auth requirement, so the seller
    /// reads it while choosing rather than discovering it via the probe's -32000 minutes later.
    #[test]
    fn reporting_a_builtin_preset_names_its_auth_prerequisite() {
        let mut err = Vec::new();
        let no_custom = std::collections::BTreeMap::new();
        report_agent_preset(&mut err, "codex", "/usr/local/bin/codex-acp", &no_custom);
        let printed = String::from_utf8(err).expect("utf8");

        // The pre-existing line is preserved — this adds to the report, it does not replace it.
        assert!(printed.contains("agent preset=codex argv0=/usr/local/bin/codex-acp"));
        // The load-bearing addition: the CLI behind the adapter, and how to authenticate it.
        assert!(printed.contains("codex login"), "no auth step named: {printed}");
        assert!(
            printed.contains("refuses to advertise"),
            "consequence not stated: {printed}"
        );
    }

    /// A custom `[agents]` preset has no prerequisite we could know, so the report stays exactly
    /// as it was — no invented advice, and no blank bullet.
    #[test]
    fn reporting_a_custom_preset_adds_no_prerequisite() {
        let mut err = Vec::new();
        let no_custom = std::collections::BTreeMap::new();
        report_agent_preset(&mut err, "my-own-agent", "/opt/mine", &no_custom);
        let printed = String::from_utf8(err).expect("utf8");

        assert!(printed.contains("agent preset=my-own-agent argv0=/opt/mine"));
        assert_eq!(
            printed.lines().count(),
            1,
            "expected only the preset line: {printed}"
        );
    }

    /// A `[agents]` entry that OVERRIDES a built-in name launches the operator's argv, not the
    /// built-in's CLI — so the built-in's login instructions would be confidently wrong. The name
    /// alone must not be enough to earn a prerequisite line.
    #[test]
    fn an_overridden_builtin_name_reports_no_prerequisite() {
        let mut custom = std::collections::BTreeMap::new();
        custom.insert(
            "codex".to_owned(),
            home::AgentPresetConfig {
                argv: vec!["/opt/not-codex".to_owned()],
            },
        );

        let mut err = Vec::new();
        report_agent_preset(&mut err, "codex", "/opt/not-codex", &custom);
        let printed = String::from_utf8(err).expect("utf8");

        assert!(printed.contains("agent preset=codex argv0=/opt/not-codex"));
        assert!(
            !printed.contains("codex login"),
            "invented an auth step for someone else's argv: {printed}"
        );
        assert_eq!(printed.lines().count(), 1, "expected only the preset line: {printed}");
    }

    /// The wiring, not just the helper: the real `--agent` flag path must report through
    /// `report_agent_preset`. A custom entry pointed at a file that exists resolves on any machine,
    /// so this never depends on an adapter being installed and never passes vacuously.
    #[cfg(feature = "wallet")]
    #[test]
    fn the_agent_flag_path_reports_through_the_shared_reporter() {
        let argv0 = std::env::current_exe().expect("test binary path is a file that exists");
        let argv0 = argv0.to_string_lossy().into_owned();
        let mut custom = std::collections::BTreeMap::new();
        custom.insert(
            "mine".to_owned(),
            home::AgentPresetConfig {
                argv: vec![argv0.clone()],
            },
        );

        let options = SellOptions::parse(&["--agent".into(), "mine".into()]).expect("parse");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (label, resolved) =
            resolve_agent(
                &options,
                None,
                &custom,
                maxplayer_core::agent_presets::AdapterHost::Host,
                &mut out,
                &mut err,
            )
            .expect("resolves");

        assert_eq!(label.as_deref(), Some("mine"));
        assert_eq!(resolved, vec![argv0.clone()]);
        let printed = String::from_utf8(err).expect("utf8");
        assert!(
            printed.contains(&format!("agent preset=mine argv0={argv0}")),
            "flag path did not report the preset: {printed}"
        );
    }

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
    fn skip_doctor_defaults_off_and_parses_on() {
        let default = SellOptions::parse(&["--agent".into(), "claude".into(), "--rate-sats".into(), "2".into()])
            .expect("parse");
        assert!(!default.skip_doctor, "doctor gate is on by default");
        let skipped = SellOptions::parse(&[
            "--agent".into(),
            "claude".into(),
            "--rate-sats".into(),
            "2".into(),
            "--skip-doctor".into(),
        ])
        .expect("parse");
        assert!(
            skipped.skip_doctor,
            "--skip-doctor bypasses the readiness checks (the nix environment check still runs)"
        );
    }

    // The gate wiring run_sell uses: a failed gate refuses startup; a passed gate proceeds. There
    // is deliberately NO "gate never ran" arm anymore (#745): --skip-doctor narrows the gate to
    // the nix environment check instead of skipping it, so even a --skip-doctor boot flows a real
    // gate verdict through here — the type no longer admits a bypassed-entirely state.
    #[test]
    fn readiness_decision_refuses_on_fail_and_proceeds_on_pass() {
        assert_eq!(
            readiness_decision(Err(())),
            Err(RUNTIME_ERROR),
            "a failed gate must refuse run_sell — including the narrowed --skip-doctor gate"
        );
        assert_eq!(readiness_decision(Ok(())), Ok(()), "a passed gate proceeds");
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
                    .join(format!("maxplayer-sell-miss-{}", std::process::id()))
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

    // Red-prove for #369. A steady-state `maxplayer seller` relaunch (an existing `[seller]` on disk,
    // no CLI flags) MUST carry the operator's `slots` and `claim_award_timeout_secs` through the
    // config write-back — the same way `agents` is already carried from `existing`. Pre-fix
    // `ensure_seller_config` hardcoded `slots: home::default_slots()` (=1) and
    // `claim_award_timeout_secs: None`, so a `slots = 2` an operator set was reset to 1 on every
    // boot, before the seller node read it. The assertion is on the PERSISTED (reloaded-from-disk)
    // config because that disk value is exactly what the next boot reads for capacity
    // (`maxplayer_core::seller_node::run` derives it from `config.seller.slots` with no ceiling clamp).
    #[test]
    fn sell_writeback_preserves_operator_multi_harness_agents() {
        // #369-class (seller-orch comment 5173135097): a bare relaunch must carry a multi-harness
        // `agents` list IN FULL, never truncate it to the first entry. RED-PROVES the carry — reverting
        // to the pre-fix `else if let Some(label) { vec![label] }` reloads ["claude"] and reddens both
        // asserts below.
        let root = std::env::temp_dir().join(format!(
            "maxplayer-369-multi-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap temp home");
        home::save_config(&mut home, |config| {
            config.seller = Some(SellerConfig {
                agent_command: vec!["claude".to_owned()],
                rate_sats: 5,
                takes_no_payment: false,
                git_remote: "https://example.invalid/seller.git".to_owned(),
                job_timeout_secs: None,
                agents: vec!["claude".to_owned(), "codex".to_owned()],
                claim_open_pool: false,
                accept_open_targeted: false,
                accept_offers_only_from: Vec::new(),
                offer_backfill_secs: home::default_offer_backfill_secs(),
                contribution_enabled: true,
                slots: 3,
                claim_award_timeout_secs: None,
            });
        })
        .expect("seed multi-harness [seller]");

        let options = SellOptions::default();
        let mut out = Vec::new();
        let mut err = Vec::new();
        ensure_seller_config(&mut home, &options, &mut out, &mut err).unwrap_or_else(|code| {
            panic!(
                "ensure_seller_config failed code={code} err={}",
                String::from_utf8_lossy(&err)
            )
        });

        // Reload from DISK exactly as the next boot loads it.
        let reloaded = home::bootstrap(&root).expect("reload persisted config");
        let seller = reloaded.config.seller.expect("[seller] persisted");
        assert_eq!(
            seller.agents,
            vec!["claude".to_owned(), "codex".to_owned()],
            "a bare relaunch must carry the full multi-harness registry, not truncate to first"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sell_writeback_preserves_operator_contribution_disabled_and_agent_label() {
        // #369-class: a steady-state relaunch must NOT clobber contribution_enabled=false back to
        // true, and must keep a single-harness wire label in `agents`.
        let root = std::env::temp_dir().join(format!(
            "maxplayer-369-contrib-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap temp home");
        home::save_config(&mut home, |config| {
            config.seller = Some(SellerConfig {
                agent_command: vec!["claude".to_owned()],
                rate_sats: 5,
                takes_no_payment: false,
                git_remote: "https://example.invalid/seller.git".to_owned(),
                job_timeout_secs: None,
                agents: vec!["claude".to_owned()],
                claim_open_pool: false,
                accept_open_targeted: false,
                accept_offers_only_from: Vec::new(),
                offer_backfill_secs: home::default_offer_backfill_secs(),
                contribution_enabled: false,
                slots: 3,
                claim_award_timeout_secs: None,
            });
        })
        .expect("seed [seller] with contribution disabled");

        let options = SellOptions::default();
        let mut out = Vec::new();
        let mut err = Vec::new();
        ensure_seller_config(&mut home, &options, &mut out, &mut err).unwrap_or_else(|code| {
            panic!(
                "ensure_seller_config failed code={code} err={}",
                String::from_utf8_lossy(&err)
            )
        });

        let reloaded = home::bootstrap(&root).expect("reload persisted config");
        let seller = reloaded.config.seller.expect("[seller] persisted");
        assert!(
            !seller.contribution_enabled,
            "operator contribution_enabled=false must survive relaunch (pre-fix clobbered to true)"
        );
        assert_eq!(
            seller.agents,
            vec!["claude".to_owned()],
            "single-harness wire label must survive relaunch"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sell_writeback_preserves_operator_accept_offers_only_from() {
        // #482 / #369-class: a steady-state relaunch must NOT clobber an operator's buyer allowlist —
        // a private seller must stay private across a bare `sell` relaunch. Seed a populated
        // allowlist, relaunch, and assert it survives the write-back. (Clobbering it no longer means
        // "back to accept-all": since the three-knob change an empty list means no buyer named, so the
        // damage is a seat that claims NOTHING rather than one that claims from everyone. Still a
        // clobber, opposite direction — see the accept_open_targeted twin below.)
        let root = std::env::temp_dir().join(format!(
            "maxplayer-482-allowlist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap temp home");
        home::save_config(&mut home, |config| {
            config.seller = Some(SellerConfig {
                agent_command: vec!["claude".to_owned()],
                rate_sats: 5,
                takes_no_payment: false,
                git_remote: "https://example.invalid/seller.git".to_owned(),
                job_timeout_secs: None,
                agents: vec!["claude".to_owned()],
                claim_open_pool: false,
                accept_open_targeted: false,
                accept_offers_only_from: vec!["buyer-abc".to_owned(), "buyer-def".to_owned()],
                offer_backfill_secs: home::default_offer_backfill_secs(),
                contribution_enabled: true,
                slots: 3,
                claim_award_timeout_secs: None,
            });
        })
        .expect("seed [seller] with a populated allowlist");

        let options = SellOptions::default();
        let mut out = Vec::new();
        let mut err = Vec::new();
        ensure_seller_config(&mut home, &options, &mut out, &mut err).unwrap_or_else(|code| {
            panic!(
                "ensure_seller_config failed code={code} err={}",
                String::from_utf8_lossy(&err)
            )
        });

        let reloaded = home::bootstrap(&root).expect("reload persisted config");
        let seller = reloaded.config.seller.expect("[seller] persisted");
        assert_eq!(
            seller.accept_offers_only_from,
            vec!["buyer-abc".to_owned(), "buyer-def".to_owned()],
            "operator accept_offers_only_from must survive relaunch (a relaunch must never open a private seller)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sell_writeback_preserves_operator_accept_open_targeted() {
        // The #369-class clobber in the direction that COSTS AN OPERATOR MONEY. The allowlist test
        // above guards a relaunch silently OPENING a private seller; this guards the mirror — a bare
        // relaunch silently CLOSING a seat the operator had opened, which takes a working seller off
        // the market with no error and no output. The flag defaults false, so a write-back that
        // forgets to carry it reconstructs the default and looks entirely correct.
        let root = std::env::temp_dir().join(format!(
            "maxplayer-open-targeted-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap temp home");
        home::save_config(&mut home, |config| {
            config.seller = Some(SellerConfig {
                agent_command: vec!["claude".to_owned()],
                rate_sats: 5,
                takes_no_payment: false,
                git_remote: "https://example.invalid/seller.git".to_owned(),
                job_timeout_secs: None,
                agents: vec!["claude".to_owned()],
                claim_open_pool: false,
                accept_open_targeted: true,
                accept_offers_only_from: Vec::new(),
                offer_backfill_secs: home::default_offer_backfill_secs(),
                contribution_enabled: true,
                slots: 3,
                claim_award_timeout_secs: None,
            });
        })
        .expect("seed [seller] with an opened targeted surface");

        let options = SellOptions::default();
        let mut out = Vec::new();
        let mut err = Vec::new();
        ensure_seller_config(&mut home, &options, &mut out, &mut err).unwrap_or_else(|code| {
            panic!(
                "ensure_seller_config failed code={code} err={}",
                String::from_utf8_lossy(&err)
            )
        });

        let reloaded = home::bootstrap(&root).expect("reload persisted config");
        let seller = reloaded.config.seller.expect("[seller] persisted");
        assert!(
            seller.accept_open_targeted,
            "operator accept_open_targeted must survive a bare relaunch (a relaunch must never close an open seat)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The flag set matches the defaults: the bare flag OPENS, and its `--no-` form restates the
    // default. Both are asserted because an opt-in whose parser silently ignored it would leave the
    // seat closed and give the operator no signal at all — the same silence this change exists to end.
    #[test]
    fn accept_open_targeted_flags_are_opt_in_shaped() {
        assert_eq!(
            SellOptions::parse(&["--accept-open-targeted".to_owned()])
                .expect("flag parses")
                .accept_open_targeted,
            Some(true),
            "the bare flag must OPEN the targeted surface"
        );
        assert_eq!(
            SellOptions::parse(&["--no-accept-open-targeted".to_owned()])
                .expect("flag parses")
                .accept_open_targeted,
            Some(false),
            "the --no- form must explicitly close it"
        );
        assert_eq!(
            SellOptions::default().accept_open_targeted,
            None,
            "absent from argv must stay None so the existing config decides, not the flag"
        );
    }

    #[test]
    fn sell_writeback_preserves_operator_slots_and_claim_timeout() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-369-slots-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap temp home");

        // The pre-relaunch on-disk state: an operator-configured `[seller]` with slots = 2 and a set
        // claim-award timeout, plus the fields a steady-state relaunch reads back for every choice.
        home::save_config(&mut home, |config| {
            config.seller = Some(SellerConfig {
                agent_command: vec!["claude".to_owned()],
                rate_sats: 5,
                takes_no_payment: false,
                git_remote: "https://example.invalid/seller.git".to_owned(),
                job_timeout_secs: None,
                agents: Vec::new(),
                claim_open_pool: false,
                accept_open_targeted: false,
                accept_offers_only_from: Vec::new(),
                offer_backfill_secs: home::default_offer_backfill_secs(),
                contribution_enabled: true,
                slots: 2,
                claim_award_timeout_secs: Some(777),
            });
        })
        .expect("seed existing [seller]");

        // Guard: the seed truly reached disk with slots = 2 before the relaunch runs.
        let seeded = std::fs::read_to_string(root.join("config.toml")).expect("read seeded config");
        assert!(
            seeded.contains("slots = 2"),
            "seed must persist slots = 2, got:\n{seeded}"
        );

        // Steady-state relaunch: `SellOptions::default()` (no --agent / --rate-sats / --git-remote)
        // ⇒ `existing.is_some()` drives every field from the loaded config. This is the exact
        // zero-prompt `maxplayer seller` write-back path.
        let options = SellOptions::default();
        let mut out = Vec::new();
        let mut err = Vec::new();
        ensure_seller_config(&mut home, &options, &mut out, &mut err).unwrap_or_else(|code| {
            panic!(
                "ensure_seller_config failed code={code} err={}",
                String::from_utf8_lossy(&err)
            )
        });

        // Assert the PERSISTED config, reloaded from disk exactly as the next boot loads it.
        let reloaded = home::bootstrap(&root).expect("reload persisted config");
        let seller = reloaded.config.seller.expect("[seller] persisted after write-back");
        assert_eq!(
            seller.slots, 2,
            "operator slots must survive the write-back; pre-#369 clobbered it to \
             default_slots()={}",
            home::default_slots()
        );
        assert_eq!(
            seller.claim_award_timeout_secs,
            Some(777),
            "operator claim_award_timeout_secs must survive the write-back"
        );

        // Disk-content proof, independent of the deserializer round-trip.
        let persisted =
            std::fs::read_to_string(root.join("config.toml")).expect("read persisted config");
        assert!(
            persisted.contains("slots = 2"),
            "config.toml must still carry slots = 2 after the write-back, got:\n{persisted}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
