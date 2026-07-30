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
    fn new(name: &str, status: Status, detail: impl Into<String>, hint: Option<&str>) -> Self {
        Self {
            name: name.to_owned(),
            status,
            detail: detail.into(),
            hint: hint.map(str::to_owned),
        }
    }

    fn pass(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Pass, detail, None)
    }

    fn warn(name: &str, detail: impl Into<String>, hint: &str) -> Self {
        Self::new(name, Status::Warn, detail, Some(hint))
    }

    fn fail(name: &str, detail: impl Into<String>, hint: &str) -> Self {
        Self::new(name, Status::Fail, detail, Some(hint))
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
fn readiness_ok(results: &[Check]) -> bool {
    !results.iter().any(|c| c.status == Status::Fail)
}


#[cfg(feature = "wallet")]
mod checks {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::Duration;

    use mobee_core::doctor::{self, RelayProbe};
    use mobee_core::home::{AgentPresetConfig, SellerConfig, TelemetryConfig};
    use mobee_core::seller_git;

    use super::Check;
    use mobee_core::agent_presets;

    const RELAY_TIMEOUT: Duration = Duration::from_secs(15);
    const MINT_TIMEOUT: Duration = Duration::from_secs(10);

    const CREDENTIAL_HELPER_CHECK: &str = "credential helper";
    const KEY_CHECK: &str = "seller key";
    const RELAY_CHECK: &str = "relay reachability";
    const MINT_CHECK: &str = "mint reachability";
    const AGENT_CHECK: &str = "agent preset";
    const TELEMETRY_CHECK: &str = "telemetry";

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
    // `home::key_file_present` (the ~/.mobee/key file). The key material itself is never read here
    // and never appears in any Check detail.
    pub(super) fn check_seller_key(present: bool) -> Check {
        if present {
            Check::pass(KEY_CHECK, "~/.mobee/key present")
        } else {
            Check::fail(
                KEY_CHECK,
                "~/.mobee/key missing — seller has no signing key",
                "ensure ~/.mobee/key exists and is readable (mode 0600) — it is auto-generated on first run",
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
                "set [accepted_mints] in config.toml (it defaults to the testnut mint)",
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

    pub(super) fn check_agent_preset(
        seller: Option<SellerConfig>,
        custom: BTreeMap<String, AgentPresetConfig>,
    ) -> Check {
        let Some(seller) = seller else {
            return Check::warn(
                AGENT_CHECK,
                "no [seller] section configured",
                "run `maxplayer sell --agent <claude|cursor|codex> --rate-sats <n>` once to configure",
            );
        };
        let available = agent_presets::detect_available_agents(&custom);
        let (label, argv) = match seller.agent.as_deref() {
            Some(name) => match agent_presets::resolve_agent_preset(name, &custom) {
                Ok(pair) => pair,
                Err(message) => {
                    return Check::fail(
                        AGENT_CHECK,
                        message,
                        "set [seller] agent to claude|cursor|codex or a configured [agents] preset",
                    );
                }
            },
            None => ("custom".to_owned(), seller.agent_command.clone()),
        };
        let argv0 = argv.first().cloned().unwrap_or_default();
        if available.contains(&label) || argv0_resolvable(&argv0) {
            Check::pass(AGENT_CHECK, format!("agent '{label}' resolvable (argv0={argv0})"))
        } else {
            Check::fail(
                AGENT_CHECK,
                format!("agent '{label}' not found (argv0={argv0})"),
                "install the agent harness or fix [seller] agent / [agents]",
            )
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
pub fn run(_args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    #[cfg(not(feature = "wallet"))]
    {
        let _ = out;
        let _ = writeln!(
            err,
            "maxplayer doctor requires the wallet feature (rebuild with default features)"
        );
        return FAILURE;
    }

    #[cfg(feature = "wallet")]
    {
        run_doctor(out, err)
    }
}

/// Build the full check registry from a bootstrapped home. Shared by `maxplayer doctor` and the
/// `maxplayer sell` boot-readiness gate (issue #107) so the two never drift and no check logic is
/// duplicated. The seller key is read once only to probe NIP-42 relay auth; it is NEVER placed in
/// any Check detail.
#[cfg(feature = "wallet")]
fn build_checks(home: &mobee_core::home::MobeeHome) -> Vec<Box<dyn FnOnce() -> Check>> {
    let relay_url = home.config.relay_url.clone();
    let secret = mobee_core::home::read_secret_key_hex(home).ok();
    let key_present = mobee_core::home::key_file_present(home);
    // The seller accept-policy mints (`accepted_mints`) — the list this seller will settle at.
    // `extra_mints` is a BUYER wallet field (see `MobeeConfig` in home.rs) and has no place in a
    // seller boot gate, so it is deliberately NOT consulted here.
    let accepted_mints = home.config.accepted_mints.clone();
    let seller = home.config.seller.clone();
    let custom_agents = home.config.agents.clone();
    let telemetry = home.config.telemetry.clone();

    let mut checks: Vec<Box<dyn FnOnce() -> Check>> = vec![
        Box::new(checks::check_credential_helper),
        Box::new(move || checks::check_seller_key(key_present)),
        Box::new(move || checks::check_relay(relay_url, secret)),
        // One aggregate mint check across the accept-policy: "can I settle anywhere?".
        Box::new(move || checks::check_mints(accepted_mints)),
    ];
    checks.push(Box::new(move || checks::check_agent_preset(seller, custom_agents)));
    checks.push(Box::new(move || checks::check_telemetry(telemetry)));
    checks
}

#[cfg(feature = "wallet")]
fn run_doctor(out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use mobee_core::home;

    let root = match home::default_home_dir() {
        Ok(root) => root,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return FAILURE;
        }
    };
    let home = match home::bootstrap(&root) {
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
#[cfg(feature = "wallet")]
pub fn sell_readiness_gate(
    home: &mobee_core::home::MobeeHome,
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
        assert_eq!(checks::check_seller_key(true).status, Status::Pass);
        let missing = checks::check_seller_key(false);
        assert_eq!(missing.status, Status::Fail, "a missing key must block boot");
        assert!(missing.render().contains("fix:"), "must give a fix hint");
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
