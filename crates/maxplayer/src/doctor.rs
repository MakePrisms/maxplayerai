//! `maxplayer doctor` — seller environment self-check.
//!
//! Runs a registry of independent checks, each printing `PASS`/`WARN`/`FAIL` plus a one-line fix
//! hint, and exits `0` when nothing FAILed, `1` when any check FAILed (a WARN never fails the exit).
//! Every check runs even when an earlier one fails, so one run surfaces the full picture.

use std::io::Write;

const SUCCESS: i32 = 0;
const FAILURE: i32 = 1;

/// One check outcome. `Warn` is advisory (does not fail the exit); `Fail` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

/// A single named check result plus an optional one-line fix hint (shown only when not `Pass`).
#[derive(Debug, Clone)]
struct Check {
    name: String,
    status: Status,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            name: name.to_owned(),
            status,
            detail: detail.into(),
            hint,
        }
    }

    fn pass(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Pass, detail, None)
    }

    fn warn(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, detail, Some(hint.into()))
    }

    fn fail(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail, Some(hint.into()))
    }

    fn render(&self) -> String {
        let base = format!("{:<4} {} — {}", self.status.label(), self.name, self.detail);
        match &self.hint {
            Some(hint) if self.status != Status::Pass => format!("{base} (fix: {hint})"),
            _ => base,
        }
    }
}

/// Run every check in order, collecting all results — a `Fail` NEVER short-circuits later checks.
fn run_checks(checks: Vec<Box<dyn FnOnce() -> Check>>) -> Vec<Check> {
    checks.into_iter().map(|check| check()).collect()
}

/// Exit code for a set of results: `1` if ANY check FAILed, else `0`. WARN never fails the exit.
fn exit_code(results: &[Check]) -> i32 {
    if results.iter().any(|c| c.status == Status::Fail) {
        FAILURE
    } else {
        SUCCESS
    }
}

/// Boot-readiness verdict over a completed check set: the seller may start only when NO check
/// FAILed. WARN is advisory and never blocks. The pure core of [`sell_readiness_gate`].
// Its only non-test caller is `sell_readiness_gate` (acp); the boot-gate unit tests below also use
// it, so keep it under `test` too — same shape as the seller surface it serves (#360).
#[cfg(any(feature = "acp", test))]
fn readiness_ok(results: &[Check]) -> bool {
    !results.iter().any(|c| c.status == Status::Fail)
}


#[cfg(feature = "wallet")]
mod checks {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::Duration;

    use maxplayer_core::doctor::{self, RelayProbe};
    use maxplayer_core::home::DEFAULT_MINIBITS_MINT_URL;
    use maxplayer_core::home::{AgentPresetConfig, SandboxConfig, SellerConfig, TelemetryConfig};
    use maxplayer_core::seller_exec::SandboxPolicy;
    use maxplayer_core::seller_git;

    use super::Check;
    use maxplayer_core::seller_agents::{self, AgentRegistry};

    const RELAY_TIMEOUT: Duration = Duration::from_secs(15);
    const MINT_TIMEOUT: Duration = Duration::from_secs(10);

    const CREDENTIAL_HELPER_CHECK: &str = "credential helper";
    const KEY_CHECK: &str = "seller key";
    const RELAY_CHECK: &str = "relay reachability";
    const MINT_CHECK: &str = "mint reachability";
    const AGENT_CHECK: &str = "agent preset";
    const TELEMETRY_CHECK: &str = "telemetry";
    const SANDBOX_CHECK: &str = "sandbox launcher";

    // Informational only: the seller signs NIP-98 in-process (libgit2 transport), so the
    // external `git-credential-nostr` helper is not required for delivery push / base fetch.
    // We still report whether it resolves (useful for anyone driving raw `git` by hand) but never
    // fail on its absence.
    pub(super) fn check_credential_helper() -> Check {
        match seller_git::resolve_git_credential_nostr() {
            Some(path) => Check::pass(
                CREDENTIAL_HELPER_CHECK,
                format!("git-credential-nostr at {} (optional — seller signs NIP-98 in-process)", path.display()),
            ),
            None => Check::pass(
                CREDENTIAL_HELPER_CHECK,
                "git-credential-nostr not found — OK, not required (seller signs NIP-98 in-process via libgit2)",
            ),
        }
    }

    // Blocking (issue #107): a seller can neither sign offers nor NIP-98-authenticate delivery
    // pushes without its key, so `maxplayer sell` refuses to boot when this FAILs. `present` is
    // `home::key_file_present` for the RESOLVED home, and `key_path` is that home's key file — the
    // detail names the file actually inspected rather than a hardcoded `~/.mobee/key`, so a
    // multi-home / `--home` run never reports about a home it did not read (issues #216, #265). The
    // key material itself is never read here and never appears in any Check detail.
    pub(super) fn check_seller_key(key_path: &Path, present: bool) -> Check {
        let path = key_path.display();
        if present {
            Check::pass(KEY_CHECK, format!("{path} present"))
        } else {
            Check::fail(
                KEY_CHECK,
                format!("{path} missing — seller has no signing key"),
                "ensure the seller key file exists and is readable (mode 0600) — it is auto-generated on first run",
            )
        }
    }

    pub(super) fn check_relay(relay_url: String, secret: Option<String>) -> Check {
        let Some(secret) = secret else {
            return Check::warn(
                RELAY_CHECK,
                format!("{relay_url}: seller key unreadable — cannot test NIP-42 auth"),
                "ensure ~/.mobee/key exists and is readable (mode 0600)",
            );
        };
        let outcome = match build_runtime() {
            Ok(runtime) => runtime.block_on(doctor::probe_relay(&relay_url, &secret, RELAY_TIMEOUT)),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(RelayProbe::Authenticated) => {
                Check::pass(RELAY_CHECK, format!("{relay_url}: connected + NIP-42 authenticated"))
            }
            Ok(RelayProbe::ConnectedNoChallenge) => Check::pass(
                RELAY_CHECK,
                format!("{relay_url}: connected (relay issued no NIP-42 challenge)"),
            ),
            Err(error) => Check::fail(
                RELAY_CHECK,
                format!("{relay_url}: {error}"),
                "check relay_url in config.toml and network/relay availability",
            ),
        }
    }

    /// Aggregate reachability across the seller's accept-policy mints. The boot question is "can
    /// this seller settle *anywhere*", so it BLOCKS (`Fail`) only when EVERY accepted mint is
    /// unreachable; a single mint down while another is reachable is an advisory `Warn`, never a
    /// boot-blocker. The pure verdict lives in [`fold_mint_reachability`].
    pub(super) fn check_mints(mint_urls: Vec<String>) -> Check {
        let probes: Vec<(String, Result<(), String>)> = mint_urls
            .into_iter()
            .map(|url| {
                let outcome = match build_runtime() {
                    Ok(runtime) => runtime.block_on(doctor::probe_mint(&url, MINT_TIMEOUT)),
                    Err(error) => Err(error),
                };
                (url, outcome.map_err(|error| error.to_string()))
            })
            .collect();
        fold_mint_reachability(&probes)
    }

    /// Pure "can I settle anywhere?" verdict over already-probed accept-policy mints (no I/O — the
    /// testable core of [`check_mints`]). All reachable ⇒ `Pass`; some reachable ⇒ `Warn` (degraded
    /// but can still settle); none reachable (or none configured) ⇒ `Fail` (blocks boot).
    pub(super) fn fold_mint_reachability(probes: &[(String, Result<(), String>)]) -> Check {
        if probes.is_empty() {
            return Check::fail(
                MINT_CHECK,
                "no accepted mints configured — the seller cannot settle anywhere",
                format!(
                    "set [accepted_mints] in config.toml (it defaults to {DEFAULT_MINIBITS_MINT_URL}, a REAL mint)"
                ),
            );
        }
        let total = probes.len();
        let reachable = probes.iter().filter(|(_, result)| result.is_ok()).count();
        let down: Vec<String> = probes
            .iter()
            .filter_map(|(url, result)| result.as_ref().err().map(|error| format!("{url}: {error}")))
            .collect();
        if down.is_empty() {
            Check::pass(MINT_CHECK, format!("all {total} accepted mint(s) reachable"))
        } else if reachable == 0 {
            Check::fail(
                MINT_CHECK,
                format!("no accepted mint reachable — cannot settle anywhere ({})", down.join("; ")),
                "check the mint URLs in [accepted_mints] and network availability",
            )
        } else {
            Check::warn(
                MINT_CHECK,
                format!("{reachable}/{total} accepted mint(s) reachable; degraded: {}", down.join("; ")),
                "an accepted mint is unreachable but the seller can still settle at another — check the degraded mint(s)",
            )
        }
    }

    /// Resolve the seller's harness registry EXACTLY the way `SellerNodeRunner::boot` does — via
    /// [`seller_agents::resolve`] on this same config — and report ITS verdict, rather than
    /// re-deriving harness resolution through the PATH-based preset resolver (issue #217). Boot is
    /// the authority (#201): it refuses to advertise work it cannot run, and `--skip-doctor` can
    /// bypass this gate entirely, so the boot-side resolve is the only guaranteed check on a real
    /// launch. Because this lives in [`super::build_checks`], `maxplayer doctor` and the
    /// `sell_readiness_gate` share it, so a green doctor means "this seat boots with these
    /// harnesses" by construction.
    pub(super) fn check_agent_registry(
        seller: Option<SellerConfig>,
        presets: BTreeMap<String, AgentPresetConfig>,
    ) -> Check {
        let Some(seller) = seller else {
            return Check::warn(
                AGENT_CHECK,
                "no [seller] section configured",
                "run `maxplayer sell --agent <claude|cursor|codex> --rate-sats <n>` once to configure",
            );
        };
        match seller_agents::resolve(&seller, &presets) {
            // Fully resolved ⇒ PASS. A partial resolve still BOOTS (it serves with the remainder),
            // so it is an advisory WARN carrying the same loud degrade line boot would print — never
            // a boot-blocking FAIL, because boot does not refuse it.
            Ok(resolved) => match resolved.degrade_line() {
                None => Check::pass(AGENT_CHECK, describe_registry(&resolved.registry)),
                Some(degrade) => Check::warn(
                    AGENT_CHECK,
                    degrade,
                    "install the missing harness adapter(s) or fix [seller] agents / [agents]",
                ),
            },
            // Boot would REFUSE this config; doctor reports the identical refusal.
            Err(error) => Check::fail(
                AGENT_CHECK,
                error.to_string(),
                "set [seller] agents = [\"claude\", …] (or agent_command) and install the harness adapter",
            ),
        }
    }

    /// One-line PASS detail for a resolved registry: the advertised harnesses (in preference order)
    /// and the preferred entry's `argv0`. An unlabelled raw-`agent_command` hatch advertises nothing
    /// honest, so it is named as such.
    fn describe_registry(registry: &AgentRegistry) -> String {
        let argv0 = registry
            .entries()
            .first()
            .and_then(|entry| entry.argv.first())
            .cloned()
            .unwrap_or_default();
        let advertised = registry.advertised();
        if advertised.is_empty() {
            format!("registry resolves (raw agent_command hatch; argv0={argv0})")
        } else {
            format!("registry resolves: {} (preferred argv0={argv0})", advertised.join(", "))
        }
    }

    // Informational only: report the brain/episode telemetry channel's posture — armed?, sink
    // resolvable?, mirror configured? — and never FAIL on it (telemetry is diagnostic, best-effort;
    // a missing sink can never break selling). WARN only when a configured sink argv0 is unresolvable.
    pub(super) fn check_telemetry(telemetry: TelemetryConfig) -> Check {
        if !telemetry.enabled {
            return Check::pass(TELEMETRY_CHECK, "disabled ([telemetry] enabled = false)");
        }
        let mirror = telemetry
            .mirror_file
            .as_ref()
            .map(|p| format!(", mirror_file={}", p.display()))
            .unwrap_or_default();
        let Some(argv0) = telemetry.command.first().cloned() else {
            return Check::pass(
                TELEMETRY_CHECK,
                format!("armed, no sink command configured (episodes.jsonl still captured){mirror}"),
            );
        };
        if argv0_resolvable(&argv0) {
            Check::pass(TELEMETRY_CHECK, format!("armed, sink '{argv0}' resolvable{mirror}"))
        } else {
            Check::warn(
                TELEMETRY_CHECK,
                format!("armed, sink '{argv0}' not found{mirror}"),
                "install the sink command or fix [telemetry] command (telemetry is best-effort)",
            )
        }
    }

    /// The sandbox launcher (`[sandbox] launcher`) must resolve, or the seller advertises a capability
    /// it cannot deliver: a launcher that is neither on PATH nor an existing file makes EVERY awarded
    /// job — and the pre-advertise self-probe — die at spawn with ENOENT before any agent runs
    /// (#357/#358). A pass-through policy (no `[sandbox]` section) launches the agent directly, so
    /// there is nothing to resolve: a no-op Pass, never a spurious Fail.
    pub(super) fn check_sandbox_launcher(sandbox: Option<SandboxConfig>) -> Check {
        let policy = SandboxPolicy::from_config(sandbox.as_ref());
        match policy.launcher().first() {
            None => Check::pass(
                SANDBOX_CHECK,
                "no [sandbox] launcher configured — agent runs directly (unsandboxed)",
            ),
            Some(argv0) if argv0_resolvable(argv0) => {
                Check::pass(SANDBOX_CHECK, format!("launcher '{argv0}' resolvable"))
            }
            Some(argv0) => Check::fail(
                SANDBOX_CHECK,
                format!(
                    "launcher '{argv0}' is neither on PATH nor an existing file — every job and the \
                     pre-advertise self-probe would fail at spawn (ENOENT)"
                ),
                "install the launcher program or fix [sandbox] launcher (or remove [sandbox] to run unsandboxed)",
            ),
        }
    }

    fn build_runtime() -> Result<tokio::runtime::Runtime, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("tokio runtime: {error}"))
    }

    /// True when `argv0` names a runnable program: an existing file path, or a bare name found on
    /// PATH. Mirrors how the seller daemon would launch it.
    fn argv0_resolvable(argv0: &str) -> bool {
        if argv0.is_empty() {
            return false;
        }
        if Path::new(argv0).is_file() {
            return true;
        }
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| dir.join(argv0).is_file())
    }
}

/// Entry from `cli::run` for `maxplayer doctor`.
///
/// Honors `--home <dir>` (mirroring `maxplayer sell`) so an operator can diagnose a specific seat,
/// and REFUSES any other argument rather than silently dropping it — a discarded flag produced a
/// confident report about the wrong home, which is worse than an error (issue #216).
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    #[cfg(not(feature = "wallet"))]
    {
        let _ = out;
        let _ = args;
        let _ = writeln!(
            err,
            "maxplayer doctor requires the wallet feature (rebuild with default features)"
        );
        return FAILURE;
    }

    #[cfg(feature = "wallet")]
    {
        let home_override = match parse_doctor_args(args) {
            Ok(home_override) => home_override,
            Err(message) => {
                let _ = writeln!(err, "maxplayer doctor: {message}");
                return FAILURE;
            }
        };
        run_doctor(home_override, out, err)
    }
}

/// Parse `maxplayer doctor`'s argv (everything after the `doctor` subcommand). The only accepted
/// argument is `--home <dir>`; anything else is refused so the operator is told to use `--home` or
/// `MOBEE_HOME` rather than being silently answered about the default home (issue #216). Mirrors the
/// `--home` parse in `sell.rs`.
#[cfg(feature = "wallet")]
fn parse_doctor_args(args: &[String]) -> Result<Option<std::path::PathBuf>, String> {
    let mut home: Option<std::path::PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--home" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "missing value for --home".to_owned())?;
                home = Some(std::path::PathBuf::from(value));
            }
            other => {
                return Err(format!(
                    "unknown doctor option: {other} (doctor accepts only `--home <dir>`; \
                     the home may also be set via MOBEE_HOME)"
                ));
            }
        }
        index += 1;
    }
    Ok(home)
}

/// Build the full check registry from a bootstrapped home. Shared by `maxplayer doctor` and the
/// `maxplayer sell` boot-readiness gate (issue #107) so the two never drift and no check logic is
/// duplicated. The seller key is read once only to probe NIP-42 relay auth; it is NEVER placed in
/// any Check detail.
#[cfg(feature = "wallet")]
fn build_checks(home: &maxplayer_core::home::MobeeHome) -> Vec<Box<dyn FnOnce() -> Check>> {
    let relay_url = home.config.relay_url.clone();
    let secret = maxplayer_core::home::read_secret_key_hex(home).ok();
    let key_present = maxplayer_core::home::key_file_present(home);
    // The key file of the RESOLVED home, so the key check names what it read (#216/#265).
    let key_path = home.key_path.clone();
    // The seller accept-policy mints (`accepted_mints`) — the list this seller will settle at.
    // `extra_mints` is a BUYER wallet field (see `MobeeConfig` in home.rs) and has no place in a
    // seller boot gate, so it is deliberately NOT consulted here.
    let accepted_mints = home.config.accepted_mints.clone();
    let seller = home.config.seller.clone();
    let custom_agents = home.config.agents.clone();
    let telemetry = home.config.telemetry.clone();
    let sandbox = home.config.sandbox.clone();

    let mut checks: Vec<Box<dyn FnOnce() -> Check>> = vec![
        Box::new(checks::check_credential_helper),
        Box::new(move || checks::check_seller_key(&key_path, key_present)),
        Box::new(move || checks::check_relay(relay_url, secret)),
        // One aggregate mint check across the accept-policy: "can I settle anywhere?".
        Box::new(move || checks::check_mints(accepted_mints)),
    ];
    checks.push(Box::new(move || checks::check_agent_registry(seller, custom_agents)));
    checks.push(Box::new(move || checks::check_telemetry(telemetry)));
    // The seller boot gate blocks on this (issue #357): a launcher that cannot spawn would let the
    // node advertise and then fail every job. Bypassable, like every check, via --skip-doctor.
    checks.push(Box::new(move || checks::check_sandbox_launcher(sandbox)));
    checks
}

/// Resolve which home `maxplayer doctor` inspects and bootstrap it: the `--home <dir>` override when
/// given, else the default resolution (`MOBEE_HOME`, then `~/.mobee`). Threading the override here
/// is the fix for issue #216 — before it, `doctor` always bootstrapped the default home and
/// reported on a seat nobody asked about. No network I/O: `bootstrap` only touches the filesystem.
#[cfg(feature = "wallet")]
fn resolve_doctor_home(
    home_override: Option<std::path::PathBuf>,
) -> Result<maxplayer_core::home::MobeeHome, maxplayer_core::home::HomeError> {
    use maxplayer_core::home;
    let root = match home_override {
        Some(root) => root,
        None => home::default_home_dir()?,
    };
    home::bootstrap(&root)
}

#[cfg(feature = "wallet")]
fn run_doctor(
    home_override: Option<std::path::PathBuf>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let home = match resolve_doctor_home(home_override) {
        Ok(home) => home,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return FAILURE;
        }
    };

    let _ = writeln!(out, "maxplayer doctor — seller environment self-check (home={})", home.root.display());

    let results = run_checks(build_checks(&home));
    for result in &results {
        let _ = writeln!(out, "{}", result.render());
    }
    let code = exit_code(&results);
    let _ = writeln!(
        out,
        "\n{} check(s), exit {code}",
        results.len()
    );
    code
}

/// `maxplayer sell` startup readiness gate (issue #107 — auto-doctor, NOT a first-run wizard). Runs the
/// SAME registry as `maxplayer doctor` via [`build_checks`] and REFUSES to boot when any BLOCKING check
/// (`Status::Fail`) fails, echoing each failure's one-line fix hint. WARN checks are advisory: they
/// print but never block. Returns `Ok(())` when the box can sell, `Err(())` when it must not start.
///
/// The required (blocking) checks per the issue — agent adapter resolvable, at least one accepted
/// mint reachable, seller key present, relay reachable — each reports `Fail` on its own failure
/// path, so the gate needs no separate severity table. The mint check is deliberately aggregate:
/// it blocks only when EVERY accepted mint is unreachable (a single degraded mint is a `Warn`).
/// Non-critical checks (credential helper, telemetry) report `Pass`/`Warn` and never block.
// Gated with the seller surface it gates (#360): `sell` is the sole caller and is `acp`-only, so on
// a buyer-only (wallet, no-acp) build this is correctly absent rather than dead. Every shipped build
// carrying `acp` also carries `wallet`, so the wallet-gated `checks` it calls are present.
#[cfg(feature = "acp")]
pub fn sell_readiness_gate(
    home: &maxplayer_core::home::MobeeHome,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), ()> {
    let _ = writeln!(
        out,
        "maxplayer sell — startup readiness checks (auto-doctor; pass --skip-doctor to bypass)"
    );
    let results = run_checks(build_checks(home));
    for result in &results {
        let _ = writeln!(out, "{}", result.render());
    }
    if readiness_ok(&results) {
        let warns = results.iter().filter(|c| c.status == Status::Warn).count();
        let _ = writeln!(
            out,
            "readiness OK — {} check(s), {warns} warning(s); starting seller",
            results.len()
        );
        return Ok(());
    }
    let failures: Vec<&Check> = results.iter().filter(|c| c.status == Status::Fail).collect();
    let _ = writeln!(
        err,
        "\nmobee sell REFUSING to start: {} blocking readiness check(s) failed —",
        failures.len()
    );
    for failure in &failures {
        let _ = writeln!(err, "  {}", failure.render());
    }
    let _ = writeln!(
        err,
        "resolve the item(s) above, then re-run `maxplayer sell`. To bypass these checks (NOT recommended), pass --skip-doctor."
    );
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_runs_every_check_even_after_an_early_fail() {
        use std::cell::Cell;
        use std::rc::Rc;

        let ran = Rc::new(Cell::new(0usize));
        let checks: Vec<Box<dyn FnOnce() -> Check>> = vec![
            {
                let ran = Rc::clone(&ran);
                Box::new(move || {
                    ran.set(ran.get() + 1);
                    Check::fail("first", "boom", "fix")
                })
            },
            {
                let ran = Rc::clone(&ran);
                Box::new(move || {
                    ran.set(ran.get() + 1);
                    Check::warn("second", "meh", "fix")
                })
            },
            {
                let ran = Rc::clone(&ran);
                Box::new(move || {
                    ran.set(ran.get() + 1);
                    Check::pass("third", "ok")
                })
            },
        ];
        let results = run_checks(checks);
        assert_eq!(ran.get(), 3, "an early FAIL must not short-circuit later checks");
        assert_eq!(results.len(), 3);
        assert_eq!(exit_code(&results), FAILURE, "any FAIL ⇒ exit 1");
    }

    #[test]
    fn exit_code_zero_when_only_pass_and_warn() {
        let results = vec![
            Check::pass("a", "ok"),
            Check::warn("b", "meh", "fix"),
            Check::pass("c", "ok"),
        ];
        assert_eq!(exit_code(&results), SUCCESS, "WARN alone must not fail the exit");
    }

    #[test]
    fn render_shows_fix_hint_only_when_not_pass() {
        assert!(!Check::pass("x", "ok").render().contains("fix:"));
        assert!(Check::fail("x", "bad", "do this").render().contains("(fix: do this)"));
        assert!(Check::warn("x", "hmm", "do this").render().contains("(fix: do this)"));
    }

    // Issue #107: a missing seller key must BLOCK `maxplayer sell` (Fail, not Warn) and carry a fix hint.
    #[cfg(feature = "wallet")]
    #[test]
    fn seller_key_check_blocks_when_absent() {
        use std::path::Path;
        let key = Path::new("/some/seat/home/key");
        assert_eq!(checks::check_seller_key(key, true).status, Status::Pass);
        let missing = checks::check_seller_key(key, false);
        assert_eq!(missing.status, Status::Fail, "a missing key must block boot");
        assert!(missing.render().contains("fix:"), "must give a fix hint");
    }

    // Issue #216 / #265: the key check names the RESOLVED home's key file, never a hardcoded
    // `~/.mobee/key`, so a `--home`/multi-home run cannot report about a home it did not read.
    #[cfg(feature = "wallet")]
    #[test]
    fn seller_key_check_names_the_resolved_home_not_hardcoded_default() {
        use std::path::Path;
        let key = Path::new("/srv/forge/workspaces/.buzzwire-demo-seat/key");
        let detail = checks::check_seller_key(key, true).render();
        assert!(
            detail.contains("/srv/forge/workspaces/.buzzwire-demo-seat/key"),
            "key check must name the home it inspected: {detail}"
        );
        assert!(
            !detail.contains("~/.mobee"),
            "key check must not hardcode ~/.mobee: {detail}"
        );
    }

    // ---- Issue #216: `maxplayer doctor` must honor `--home` and refuse unknown flags ----

    #[cfg(feature = "wallet")]
    #[test]
    fn doctor_parses_home_and_refuses_unknown_flags() {
        use std::path::PathBuf;
        assert_eq!(parse_doctor_args(&[]).unwrap(), None, "no args ⇒ default home");
        assert_eq!(
            parse_doctor_args(&["--home".into(), "/srv/seat-b".into()]).unwrap(),
            Some(PathBuf::from("/srv/seat-b")),
            "--home must be parsed, not dropped"
        );
        assert!(
            parse_doctor_args(&["--home".into()]).is_err(),
            "--home with no value must error"
        );
        // The heart of #216: a flag doctor does not understand must be REFUSED, never silently
        // dropped and answered about the default home.
        let unknown = parse_doctor_args(&["--bogus".into()]).unwrap_err();
        assert!(unknown.contains("unknown doctor option"), "{unknown}");
        assert!(
            parse_doctor_args(&["/srv/seat-b".into()]).is_err(),
            "a bare positional must be refused too"
        );
    }

    // Issue #216 red-prove: `doctor --home <tmp>` must inspect THAT home. Drop the `--home` threading
    // (make `resolve_doctor_home` ignore its override) and `home.root` becomes the default home, so
    // this assertion goes red. No network: `bootstrap` is filesystem-only.
    #[cfg(feature = "wallet")]
    #[test]
    fn doctor_honors_home_override_and_inspects_that_home() {
        use std::path::PathBuf;
        let tmp: PathBuf = std::env::temp_dir().join(format!(
            "mobee-doctor-home-216-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let home = resolve_doctor_home(Some(tmp.clone())).expect("bootstrap the overridden home");
        assert_eq!(home.root, tmp, "doctor must bootstrap the --home dir, not the default");
        assert!(
            home.key_path.starts_with(&tmp),
            "the inspected key must live under the overridden home: {}",
            home.key_path.display()
        );

        // …and the key check reports about THAT home (ties #216 cosmetic / #265 residual).
        let present = maxplayer_core::home::key_file_present(&home);
        let detail = checks::check_seller_key(&home.key_path, present).render();
        assert!(
            detail.contains(&tmp.display().to_string()),
            "key check must name the overridden home: {detail}"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ---- Issue #357: the sandbox launcher must resolve before the seat advertises ----

    // The check is non-inert: a launcher that cannot spawn FAILs, a resolvable one PASSes, and an
    // unsandboxed seat (no launcher) is a no-op PASS rather than a spurious FAIL. No network / no
    // spawn — `argv0_resolvable` is a PATH/file lookup only.
    #[cfg(feature = "wallet")]
    #[test]
    fn sandbox_launcher_check_fails_only_on_an_unresolvable_launcher() {
        use maxplayer_core::home::SandboxConfig;

        let bogus = checks::check_sandbox_launcher(Some(SandboxConfig {
            launcher: vec!["definitely-not-a-real-binary-xyz".into()],
        }));
        assert_eq!(
            bogus.status,
            Status::Fail,
            "an unresolvable launcher must block boot: {}",
            bogus.render()
        );
        assert!(bogus.render().contains("fix:"), "a FAIL must carry a fix hint");

        // An existing file always resolves; the test binary itself is one, so this holds on any box.
        let real = std::env::current_exe()
            .expect("current exe")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            checks::check_sandbox_launcher(Some(SandboxConfig { launcher: vec![real] })).status,
            Status::Pass,
            "a resolvable launcher must PASS"
        );

        assert_eq!(
            checks::check_sandbox_launcher(None).status,
            Status::Pass,
            "an unsandboxed seat (no [sandbox]) must not FAIL"
        );
    }

    // RED-PROVE (wiring): the sandbox launcher check must be part of the seller boot gate registry,
    // or a box with an unresolvable launcher boots and then fails EVERY job. Drop the
    // `check_sandbox_launcher` push from `build_checks` and this goes red — no boot-gate result names
    // the bogus launcher. Network-free: `relay_url` is unparseable (add_relay fails fast) and no mints
    // are configured (mint reachability folds without I/O), so only the sandbox verdict is exercised.
    #[cfg(feature = "wallet")]
    #[test]
    fn sandbox_launcher_check_is_wired_into_the_boot_gate() {
        use maxplayer_core::home::SandboxConfig;

        let tmp = std::env::temp_dir().join(format!(
            "mobee-doctor-sandbox-357-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut home = resolve_doctor_home(Some(tmp.clone())).expect("bootstrap the home");
        home.config.sandbox = Some(SandboxConfig {
            launcher: vec!["definitely-not-a-real-binary-xyz".into()],
        });
        home.config.relay_url = "not-a-relay-url".into();
        home.config.accepted_mints = Vec::new();

        let results = run_checks(build_checks(&home));
        assert!(
            results
                .iter()
                .any(|c| c.status == Status::Fail
                    && c.detail.contains("definitely-not-a-real-binary-xyz")),
            "build_checks must run the sandbox launcher check and FAIL on a bogus launcher; got: {:?}",
            results.iter().map(Check::render).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ---- Issue #217: doctor's agent verdict must equal boot's registry verdict ----

    // A config where PATH-resolve (old doctor) and verbatim-resolve (boot) DISAGREE: an absolute
    // `agent_command` that boot uses verbatim (→ resolves), while the named preset's adapter is
    // absent from PATH (→ the old preset probe FAILed). Doctor must now report boot's PASS. Revert
    // to the preset/PATH check and doctor FAILs, so this assertion goes red.
    #[cfg(feature = "wallet")]
    #[test]
    fn doctor_agent_check_matches_boot_verdict_on_verbatim_command() {
        use maxplayer_core::home::{AgentPresetConfig, SellerConfig};
        use maxplayer_core::seller_agents;
        use std::collections::BTreeMap;

        // An existing file, used as an absolute agent_command — boot launches it verbatim.
        let existing = std::env::current_exe().expect("current exe exists");
        // A preset whose adapter is deliberately absent from PATH: the PATH-based resolver FAILs it.
        let mut presets = BTreeMap::new();
        presets.insert(
            "myabsent".to_owned(),
            AgentPresetConfig {
                argv: vec!["mobee-doctor-absent-adapter-x7q".to_owned()],
            },
        );

        let seller = SellerConfig {
            agent_command: vec![existing.to_string_lossy().into_owned()],
            rate_sats: 5,
            git_remote: "https://example.invalid/repo".into(),
            job_timeout_secs: None,
            agents: Vec::new(), // empty ⇒ boot uses fallback_registry (agent_command VERBATIM)
            claim_open_pool: false,
            offer_backfill_secs: 0,
            contribution_enabled: true,
            slots: 1,
            claim_award_timeout_secs: None,
        };

        // Boot's verdict: the registry the seller node actually boots with.
        let boot = seller_agents::resolve(&seller, &presets);
        assert!(boot.is_ok(), "boot resolves the verbatim agent_command");

        // Doctor must report the SAME verdict, not a FAIL derived from a PATH probe of the preset.
        let check = checks::check_agent_registry(Some(seller), presets);
        assert_eq!(
            check.status,
            Status::Pass,
            "doctor must converge on boot's registry verdict, not a PATH-based preset probe: {}",
            check.render()
        );
    }

    // The inverse hazard #217 names: a config that RESOLVES on PATH but whose registry REFUSES must
    // read as FAIL, matching boot's refusal — never a green doctor over a seat that cannot boot.
    #[cfg(feature = "wallet")]
    #[test]
    fn doctor_agent_check_fails_when_boot_registry_refuses() {
        use maxplayer_core::home::SellerConfig;
        use maxplayer_core::seller_agents::{self, RegistryError};
        use std::collections::BTreeMap;

        // `agents` lists a preset that is neither built-in nor configured ⇒ every listed preset
        // fails to resolve ⇒ resolve → AllFailed (boot refuses). Deterministic: an unknown preset
        // name errors without any PATH lookup.
        let presets = BTreeMap::new();
        let seller = SellerConfig {
            agent_command: vec!["ignored-when-agents-listed".to_owned()],
            rate_sats: 5,
            git_remote: "https://example.invalid/repo".into(),
            job_timeout_secs: None,
            agents: vec!["ghostxyz-not-a-preset".to_owned()],
            claim_open_pool: false,
            offer_backfill_secs: 0,
            contribution_enabled: true,
            slots: 1,
            claim_award_timeout_secs: None,
        };

        let boot = seller_agents::resolve(&seller, &presets);
        assert!(
            matches!(boot, Err(RegistryError::AllFailed(_))),
            "boot must refuse a registry with no launchable harness"
        );
        let check = checks::check_agent_registry(Some(seller), presets);
        assert_eq!(
            check.status,
            Status::Fail,
            "doctor must report boot's refusal as a FAIL: {}",
            check.render()
        );
    }

    // The mint gate answers "can I settle anywhere". It must Fail ONLY when every accepted mint is
    // down; a single degraded mint (with another reachable) is an advisory WARN, never a boot block.
    #[cfg(feature = "wallet")]
    #[test]
    fn mint_reachability_blocks_only_when_every_mint_is_down() {
        let ok = |u: &str| (u.to_owned(), Ok(()));
        let down = |u: &str| (u.to_owned(), Err("connection refused".to_owned()));

        // All reachable ⇒ Pass (boots).
        assert_eq!(
            checks::fold_mint_reachability(&[ok("https://a"), ok("https://b")]).status,
            Status::Pass,
        );
        // One of two down ⇒ Warn (degraded but can still settle — must NOT block boot).
        let partial = checks::fold_mint_reachability(&[ok("https://a"), down("https://b")]);
        assert_eq!(partial.status, Status::Warn, "single mint down must not block boot");
        assert!(partial.render().contains("https://b"), "names the degraded mint");
        // Every mint down ⇒ Fail (cannot settle anywhere — blocks boot).
        let all_down = checks::fold_mint_reachability(&[down("https://a"), down("https://b")]);
        assert_eq!(all_down.status, Status::Fail, "no reachable mint must block boot");
        // No accepted mints configured ⇒ Fail.
        assert_eq!(checks::fold_mint_reachability(&[]).status, Status::Fail);
    }

    // The boot gate refuses (readiness_ok == false) iff some check FAILed; WARN alone still boots.
    #[test]
    fn readiness_refuses_on_fail_and_boots_on_warn() {
        assert!(
            readiness_ok(&[Check::pass("a", "ok"), Check::warn("b", "meh", "fix")]),
            "only Pass/Warn ⇒ the seller may boot"
        );
        assert!(
            !readiness_ok(&[Check::pass("a", "ok"), Check::fail("b", "bad", "fix")]),
            "any Fail ⇒ the seller must be refused"
        );
    }
}
