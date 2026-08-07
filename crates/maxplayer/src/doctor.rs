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
    /// Fault CLASS of a blocking `Fail`, distinct from `status` (which is outcome SEVERITY): `true`
    /// when the failure is a transient dependency blip (a relay/mint briefly unreachable) that a
    /// re-run could clear, `false` when it is an unrecoverable misconfiguration a retry cannot fix.
    /// Read ONLY by the seller boot gate's bounded transient-retry ([`run_readiness_with_retry`]), so
    /// it exists only in builds that carry that gate — a buyer-only (wallet, no-acp) build has none.
    #[cfg(any(feature = "acp", test))]
    transient: bool,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            name: name.to_owned(),
            status,
            detail: detail.into(),
            hint,
            // Default fault class is unrecoverable: only the two dependency-reachability checks
            // opt into transient via `fail_transient`, so every other blocking Fail refuses at once.
            #[cfg(any(feature = "acp", test))]
            transient: false,
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

    /// A blocking failure whose CAUSE is a transient dependency blip — a relay or mint briefly
    /// unreachable at boot — rather than an unrecoverable misconfiguration. Renders identically to
    /// [`Check::fail`]; the only difference is the `transient` marker the seller boot gate reads to
    /// decide whether a re-run could recover (see [`run_readiness_with_retry`]). In a buyer-only
    /// (wallet, no-acp) build there is no gate and no `transient` field, so this collapses to `fail`.
    fn fail_transient(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        let check = Self::fail(name, detail, hint);
        #[cfg(any(feature = "acp", test))]
        let check = Self { transient: true, ..check };
        check
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

/// Total readiness-gate evaluations before a still-failing box is refused: one initial pass plus up
/// to `READINESS_MAX_ATTEMPTS - 1` transient re-runs. Bounded so an unrecoverable box still exits (a
/// supervisor can then surface the fault) rather than looping forever.
#[cfg(any(feature = "acp", test))]
const READINESS_MAX_ATTEMPTS: u32 = 5;

/// Linear backoff base: the k-th retry waits `READINESS_BACKOFF_BASE * k`. Named and linear so the
/// wait is operator-visible and predictable rather than an opaque exponential curve.
#[cfg(any(feature = "acp", test))]
const READINESS_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(20);

/// Wait before the retry that follows a just-failed attempt `attempt` (1-based): `BASE * attempt`.
/// Over `READINESS_MAX_ATTEMPTS = 5` that is the four inter-attempt waits 20s + 40s + 60s + 80s =
/// 200s worst case added before a transient box is finally refused.
#[cfg(any(feature = "acp", test))]
fn readiness_backoff(attempt: u32) -> std::time::Duration {
    READINESS_BACKOFF_BASE * attempt
}

/// Whether a failed gate is worth retrying: `true` only when the set has at least one blocking `Fail`
/// AND every blocking `Fail` is `transient`. A single unrecoverable `Fail` (missing key, no mints
/// configured, unresolvable agent/launcher, containment, home perms) ⇒ `false` ⇒ refuse immediately,
/// because re-running cannot fix a misconfiguration and the seat would only burn the whole backoff
/// budget before the same refusal.
#[cfg(any(feature = "acp", test))]
fn transient_retry_worthwhile(results: &[Check]) -> bool {
    let mut saw_blocking_fail = false;
    for check in results.iter().filter(|c| c.status == Status::Fail) {
        saw_blocking_fail = true;
        if !check.transient {
            return false;
        }
    }
    saw_blocking_fail
}

/// Drive the readiness checks with bounded transient-retry, preserving fail-closed EXACTLY: the only
/// new behavior is that a gate whose every blocking failure is transient (a relay/mint blip) is
/// re-run up to `max_attempts` times before the same refuse verdict its caller maps to exit 2. A
/// single unrecoverable blocking failure refuses immediately (no retry, no sleep); exhausting the
/// transient retries refuses identically to a single-pass gate.
///
/// The retry knobs are injected — `run` re-runs the checks, `backoff` maps a just-failed attempt
/// number to a wait, `sleep` performs it — so the schedule and ceiling are unit-tested without real
/// sleeping (tests pass a recording `sleep` and a fake clock). Returns `Ok(())` when the box may
/// start, `Err(())` when it must not.
#[cfg(any(feature = "acp", test))]
fn run_readiness_with_retry(
    mut run: impl FnMut() -> Vec<Check>,
    max_attempts: u32,
    backoff: impl Fn(u32) -> std::time::Duration,
    mut sleep: impl FnMut(std::time::Duration),
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), ()> {
    let mut attempt: u32 = 1;
    loop {
        let results = run();
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
        // At least one blocking Fail. Re-run ONLY when every blocking Fail is transient and attempts
        // remain; anything else falls through to the unchanged fail-closed refusal below.
        if attempt < max_attempts && transient_retry_worthwhile(&results) {
            let wait = backoff(attempt);
            let _ = writeln!(
                out,
                "readiness: transient check(s) failed — retry {attempt}/{} in {}s…",
                max_attempts - 1,
                wait.as_secs()
            );
            sleep(wait);
            attempt += 1;
            continue;
        }
        let failures: Vec<&Check> = results.iter().filter(|c| c.status == Status::Fail).collect();
        let _ = writeln!(
            err,
            "\nmaxplayer seller REFUSING to start: {} blocking readiness check(s) failed —",
            failures.len()
        );
        for failure in &failures {
            let _ = writeln!(err, "  {}", failure.render());
        }
        let _ = writeln!(
            err,
            "resolve the item(s) above, then re-run `maxplayer seller`. To bypass these checks (NOT recommended), pass --skip-doctor."
        );
        return Err(());
    }
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

    use crate::sandbox_probe::Containment;
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
    /// Named apart from SANDBOX_CHECK on purpose: one says the launcher exists, the other says it
    /// confines, and a reader scanning the output must be able to tell which one passed.
    const CONTAINMENT_CHECK: &str = "sandbox containment";
    const HOME_PERMS_CHECK: &str = "home permissions";

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
    // pushes without its key, so `maxplayer seller` refuses to boot when this FAILs. `present` is
    // `home::key_file_present` for the RESOLVED home, and `key_path` is that home's key file — the
    // detail names the file actually inspected rather than a hardcoded `~/.maxplayer/key`, so a
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
                "ensure ~/.maxplayer/key exists and is readable (mode 0600)",
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
            // Transient: relay reachability is a live dependency, so a boot-time blip (relay
            // restart, momentary network loss) can clear on a re-run — the seller boot gate retries
            // this before refusing, unlike a misconfiguration such as a missing key.
            Err(error) => Check::fail_transient(
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
            // Transient: mints ARE configured but every one is unreachable right now — a dependency
            // blip the boot gate retries, distinct from the no-mints-configured Fail above, which is
            // a misconfiguration a re-run cannot fix and therefore stays a plain (unrecoverable) fail.
            Check::fail_transient(
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
    /// `sell_readiness_gate` share it.
    ///
    /// This is a RESOLUTION check, not an executability one, and the PASS says so out loud (#470). A
    /// harness that resolves can still fail to run: the rc.3 Bolty seat resolved its `claude` preset,
    /// passed doctor 8/8, then aborted the REAL prove-before-advertise probe because its runtime could
    /// not read /proc and /sys under the Landlock launcher. Resolvable ≠ executable — the same
    /// adjacency as #252's resolvable ≠ authorized — so executability is proven ONLY at the
    /// pre-advertise self-probe at boot (#357), never here, and the PASS detail is labelled so a green
    /// doctor cannot be read as a green probe. (Giving doctor a real exec leg was the alternative;
    /// rejected because `sell` runs this gate AND then the advertise probe, so it would double-probe at
    /// boot and stand up a second probe authority to keep in sync with the fail-closed gate forever.)
    pub(super) fn check_agent_registry(
        seller: Option<SellerConfig>,
        presets: BTreeMap<String, AgentPresetConfig>,
    ) -> Check {
        let Some(seller) = seller else {
            return Check::warn(
                AGENT_CHECK,
                "no [seller] section configured",
                "run `maxplayer seller --agent <claude|cursor|codex> --rate-sats <n>` once to configure",
            );
        };
        match seller_agents::resolve(&seller, &presets) {
            // Fully resolved ⇒ PASS. A partial resolve still BOOTS (it serves with the remainder),
            // so it is an advisory WARN carrying the same loud degrade line boot would print — never
            // a boot-blocking FAIL, because boot does not refuse it.
            Ok(resolved) => match resolved.degrade_line() {
                None => Check::pass(
                    AGENT_CHECK,
                    format!(
                        "{} — RESOLUTION ONLY: this proves the registry resolves the way boot does, \
                         NOT that any harness can deliver; executability is proven at the \
                         pre-advertise self-probe at boot, never here (a resolvable harness can still \
                         fail to run — #470/#252)",
                        describe_registry(&resolved.registry)
                    ),
                ),
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

    /// Containment, for a seat that serves the OPEN POOL — which means executing code posted by
    /// strangers. `check_sandbox_launcher` above answers "does the launcher resolve", a property
    /// one layer out from this one: bubblewrap resolves on Ubuntu 24.04 and then fails at spawn on
    /// the AppArmor unprivileged-userns restriction, so a resolvable launcher confined nothing on a
    /// live seat (#451). This runs it and reads what it did.
    ///
    /// A targeted-only seat gets the same probe reported as a WARN: it runs work from
    /// counterparties it chose, which is a different exposure from serving the open market.
    pub(super) fn check_sandbox_containment(
        sandbox: Option<SandboxConfig>,
        home_root: std::path::PathBuf,
        claims_open_pool: bool,
        unsafe_override: bool,
    ) -> Check {
        let policy = SandboxPolicy::from_config(sandbox.as_ref());
        let containment = crate::sandbox_probe::probe_containment(&policy, &home_root);

        if !claims_open_pool {
            return match containment {
                Containment::Contained => Check::pass(
                    CONTAINMENT_CHECK,
                    "launcher confines: a file outside the workdir was refused, the workdir was writable",
                ),
                other => Check::warn(
                    CONTAINMENT_CHECK,
                    format!("targeted-only seat, so advisory — {}", other.detail()),
                    "this seat only runs work from counterparties it accepts; configure a working [sandbox] launcher before serving the open pool",
                ),
            };
        }

        if unsafe_override {
            return Check::warn(
                CONTAINMENT_CHECK,
                match containment {
                    Containment::Contained => "--unsafe-no-sandbox passed, though the launcher does confine".to_owned(),
                    ref other => format!("--unsafe-no-sandbox passed: SERVING THE OPEN POOL UNCONTAINED — {}", other.detail()),
                },
                "remove --unsafe-no-sandbox and configure a [sandbox] launcher that passes the probe",
            );
        }

        match crate::sandbox_probe::open_pool_admission(true, &containment, false) {
            Ok(()) => Check::pass(
                CONTAINMENT_CHECK,
                "launcher confines: a file outside the workdir was refused, the workdir was writable",
            ),
            Err(detail) => Check::fail(
                CONTAINMENT_CHECK,
                format!("this seat claims OPEN-POOL jobs — arbitrary code from strangers — and {detail}"),
                "configure a [sandbox] launcher that passes `maxplayer sandbox-probe`, or drop open-pool claiming (--no-claim-open-pool), or accept the exposure deliberately with --unsafe-no-sandbox",
            ),
        }
    }

    /// The home and wallet CONTAINERS must be owner-only on disk. On a shared host, seller state — the
    /// key, mint proofs, config, job workdirs — IS the wallet, so a group/world-accessible dir lets any
    /// local user read money-bearing material (#473). `home::bootstrap` now chmods both `0700` at
    /// creation, so this check is the VERIFICATION half of that pairing: it catches a dir that drifted
    /// open AFTER bootstrap (an external chmod, a restored backup, a pre-#473 seat that never
    /// re-bootstrapped) rather than trusting the enforcement to be the only guard.
    ///
    /// Access-exposure is orthogonal to transaction value — testnut vs real changes nothing about who
    /// can read the key — so this never consults the mint. A too-open dir is a WARN for a targeted-only
    /// seat (single-user boxes are common, and there the exposure is nil) and a FAIL for an open-pool
    /// seat, whose higher exposure warrants the stricter posture. A no-op PASS where there is no POSIX
    /// mode to read (non-unix): the `too_open` list simply stays empty.
    pub(super) fn check_home_permissions(
        home_root: std::path::PathBuf,
        wallet_dir: std::path::PathBuf,
        claims_open_pool: bool,
    ) -> Check {
        let mut too_open: Vec<String> = Vec::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&home_root, &wallet_dir] {
                let metadata = match std::fs::metadata(dir) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        return Check::warn(
                            HOME_PERMS_CHECK,
                            format!("could not read {} permissions: {error}", dir.display()),
                            "check the seat home exists and is readable",
                        );
                    }
                };
                let mode = metadata.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    too_open.push(format!("{} ({mode:#o})", dir.display()));
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (&home_root, &wallet_dir);
        }

        if too_open.is_empty() {
            return Check::pass(HOME_PERMS_CHECK, "home and wallet are owner-only (0700)");
        }
        let detail = format!(
            "group/world-accessible: {} — another local user on this host can read this seat's key and wallet",
            too_open.join(", ")
        );
        let hint = "chmod 0700 the seat home and wallet/ (maxplayer re-tightens them on the next boot); on a shared host also set UMask=0077 on the service unit so harness state the binary does not own is owner-only too";
        if claims_open_pool {
            Check::fail(HOME_PERMS_CHECK, detail, hint)
        } else {
            Check::warn(HOME_PERMS_CHECK, detail, hint)
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
/// Honors `--home <dir>` (mirroring `maxplayer seller`) so an operator can diagnose a specific seat,
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
/// `MAXPLAYER_HOME` rather than being silently answered about the default home (issue #216). Mirrors the
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
                     the home may also be set via MAXPLAYER_HOME)"
                ));
            }
        }
        index += 1;
    }
    Ok(home)
}

/// Build the full check registry from a bootstrapped home. Shared by `maxplayer doctor` and the
/// `maxplayer seller` boot-readiness gate (issue #107) so the two never drift and no check logic is
/// duplicated. The seller key is read once only to probe NIP-42 relay auth; it is NEVER placed in
/// any Check detail.
#[cfg(feature = "wallet")]
fn build_checks(
    home: &maxplayer_core::home::MaxplayerHome,
    unsafe_no_sandbox: bool,
) -> Vec<Box<dyn FnOnce() -> Check>> {
    let relay_url = home.config.relay_url.clone();
    let secret = maxplayer_core::home::read_secret_key_hex(home).ok();
    let key_present = maxplayer_core::home::key_file_present(home);
    // The key file of the RESOLVED home, so the key check names what it read (#216/#265).
    let key_path = home.key_path.clone();
    // The seller accept-policy mints (`accepted_mints`) — the list this seller will settle at.
    // `extra_mints` is a BUYER wallet field (see `MaxplayerConfig` in home.rs) and has no place in a
    // seller boot gate, so it is deliberately NOT consulted here.
    let accepted_mints = home.config.accepted_mints.clone();
    let seller = home.config.seller.clone();
    let custom_agents = home.config.agents.clone();
    let telemetry = home.config.telemetry.clone();
    let sandbox = home.config.sandbox.clone();
    let sandbox_for_launcher = sandbox.clone();
    // The probe runs in the seat's OWN home, because that is where a launcher's config points.
    let home_root = home.root.clone();
    // Open-pool claiming is the exposure the containment gate is about: it is what makes this box
    // run code from a counterparty nobody chose. Off by default (#357), so an unconfigured seat is
    // targeted-only and stays advisory.
    let claims_open_pool = home
        .config
        .seller
        .as_ref()
        .is_some_and(|seller| seller.claim_open_pool);
    // Home/wallet perms are verified against the SAME resolved home the rest of the gate inspects.
    let perms_home_root = home.root.clone();
    let perms_wallet_dir = home.wallet_dir.clone();

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
    checks.push(Box::new(move || checks::check_sandbox_launcher(sandbox_for_launcher)));
    // Blocking for an open-pool seat (#451). Placed after the resolve check so that a launcher which
    // is not there reports as the missing file it is, rather than as a containment failure.
    checks.push(Box::new(move || {
        checks::check_sandbox_containment(sandbox, home_root, claims_open_pool, unsafe_no_sandbox)
    }));
    // Verifies the owner-only invariant `home::bootstrap` enforces at creation hasn't drifted (#473):
    // WARN for a targeted seat, FAIL for an open-pool one.
    checks.push(Box::new(move || {
        checks::check_home_permissions(perms_home_root, perms_wallet_dir, claims_open_pool)
    }));
    checks
}

/// Resolve which home `maxplayer doctor` inspects and bootstrap it: the `--home <dir>` override when
/// given, else the default resolution (`MAXPLAYER_HOME`, then `~/.maxplayer`). Threading the override here
/// is the fix for issue #216 — before it, `doctor` always bootstrapped the default home and
/// reported on a seat nobody asked about. No network I/O: `bootstrap` only touches the filesystem.
#[cfg(feature = "wallet")]
fn resolve_doctor_home(
    home_override: Option<std::path::PathBuf>,
) -> Result<maxplayer_core::home::MaxplayerHome, maxplayer_core::home::HomeError> {
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

    // `doctor` reports; it never boots a seller, so there is nothing here for an unsafe override to
    // waive. The containment check is read at its own severity.
    let results = run_checks(build_checks(&home, false));
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

/// `maxplayer seller` startup readiness gate (issue #107 — auto-doctor, NOT a first-run wizard). Runs the
/// SAME registry as `maxplayer doctor` via [`build_checks`] and REFUSES to boot when any BLOCKING check
/// (`Status::Fail`) fails, echoing each failure's one-line fix hint. WARN checks are advisory: they
/// print but never block. Returns `Ok(())` when the box can sell, `Err(())` when it must not start.
///
/// The required (blocking) checks per the issue — agent adapter resolvable, at least one accepted
/// mint reachable, seller key present, relay reachable — each reports `Fail` on its own failure
/// path, so the gate needs no separate severity table. The mint check is deliberately aggregate:
/// it blocks only when EVERY accepted mint is unreachable (a single degraded mint is a `Warn`).
/// Non-critical checks (credential helper, telemetry) report `Pass`/`Warn` and never block.
///
/// Fail-closed is preserved, but a boot-time dependency BLIP is no longer fatal (issue #553): when
/// EVERY blocking failure is transient (relay unreachable, or every accepted mint unreachable) the
/// gate re-runs the checks with linear backoff up to [`READINESS_MAX_ATTEMPTS`] before refusing, so
/// an unsupervised seat rides out a relay/mint restart instead of dying to it. A single UNRECOVERABLE
/// failure (missing key, no mints configured, unresolvable agent/launcher, containment, home perms)
/// still refuses immediately — a re-run cannot fix a misconfiguration — and exhausting the transient
/// retries refuses with the identical exit-2 verdict a single-pass gate produced. The retry loop and
/// schedule live in [`run_readiness_with_retry`]; this wrapper only supplies the real check runner,
/// backoff and `std::thread::sleep` (the gate runs on a plain thread, not inside a runtime).
// Gated with the seller surface it gates (#360): `sell` is the sole caller and is `acp`-only, so on
// a buyer-only (wallet, no-acp) build this is correctly absent rather than dead. Every shipped build
// carrying `acp` also carries `wallet`, so the wallet-gated `checks` it calls are present.
#[cfg(feature = "acp")]
pub fn sell_readiness_gate(
    home: &maxplayer_core::home::MaxplayerHome,
    unsafe_no_sandbox: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), ()> {
    let _ = writeln!(
        out,
        "maxplayer seller — startup readiness checks (auto-doctor; pass --skip-doctor to bypass)"
    );
    run_readiness_with_retry(
        || run_checks(build_checks(home, unsafe_no_sandbox)),
        READINESS_MAX_ATTEMPTS,
        readiness_backoff,
        std::thread::sleep,
        out,
        err,
    )
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

    // Issue #107: a missing seller key must BLOCK `maxplayer seller` (Fail, not Warn) and carry a fix hint.
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
    // `~/.maxplayer/key`, so a `--home`/multi-home run cannot report about a home it did not read.
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
            !detail.contains("~/.maxplayer"),
            "key check must not hardcode ~/.maxplayer: {detail}"
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
            "maxplayer-doctor-home-216-{}-{}",
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
            "maxplayer-doctor-sandbox-357-{}-{}",
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

        let results = run_checks(build_checks(&home, false));
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
                argv: vec!["maxplayer-doctor-absent-adapter-x7q".to_owned()],
            },
        );

        let seller = SellerConfig {
            agent_command: vec![existing.to_string_lossy().into_owned()],
            rate_sats: 5,
            git_remote: "https://example.invalid/repo".into(),
            job_timeout_secs: None,
            agents: Vec::new(), // empty ⇒ boot uses fallback_registry (agent_command VERBATIM)
            claim_open_pool: false,
            accept_offers_only_from: Vec::new(),
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

    // #470: doctor's agent leg is RESOLUTION-only, and its PASS must SAY so — a resolvable registry is
    // not a probe-verified one (rc.3 Bolty seat: doctor 8/8 PASS, then the real probe aborted under
    // Landlock). The loud label is the fix (Option B): executability is proven at the pre-advertise
    // probe, not here. Drop the disclaimer and a green doctor reads as a green probe again — this pins
    // it, so a future edit that silently restores the masquerade goes red.
    #[cfg(feature = "wallet")]
    #[test]
    fn doctor_agent_pass_is_labelled_resolution_only_not_probe_verified() {
        use maxplayer_core::home::SellerConfig;
        use std::collections::BTreeMap;

        // An existing file as an absolute agent_command resolves ⇒ a PASS with no degrade.
        let existing = std::env::current_exe().expect("current exe exists");
        let seller = SellerConfig {
            agent_command: vec![existing.to_string_lossy().into_owned()],
            rate_sats: 5,
            git_remote: "https://example.invalid/repo".into(),
            job_timeout_secs: None,
            agents: Vec::new(),
            claim_open_pool: false,
            accept_offers_only_from: Vec::new(),
            offer_backfill_secs: 0,
            contribution_enabled: true,
            slots: 1,
            claim_award_timeout_secs: None,
        };

        let check = checks::check_agent_registry(Some(seller), BTreeMap::new());
        assert_eq!(
            check.status,
            Status::Pass,
            "a resolvable registry passes: {}",
            check.render()
        );
        let detail = check.detail.to_lowercase();
        assert!(
            detail.contains("resolution only"),
            "the agent PASS must announce it is resolution-only, not probe-verified: {}",
            check.render()
        );
        assert!(
            detail.contains("pre-advertise") && detail.contains("self-probe"),
            "the agent PASS must point at the pre-advertise self-probe as the executability authority: {}",
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
            accept_offers_only_from: Vec::new(),
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

    // #473: the perms leg VERIFIES the owner-only invariant bootstrap enforces — PASS when owner-only,
    // WARN for a targeted seat that drifted open, FAIL for an open-pool one. Pure over two real dirs,
    // so no agent or network. Access-exposure is orthogonal to the mint (testnut vs real is irrelevant).
    #[cfg(all(unix, feature = "wallet"))]
    #[test]
    fn home_permissions_warns_targeted_and_fails_open_pool_on_a_loose_home() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mp-perms-{}", std::process::id()));
        let home = base.join("home");
        let wallet = home.join("wallet");
        std::fs::create_dir_all(&wallet).expect("mk dirs");

        // Owner-only ⇒ PASS, whatever the seat type.
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&wallet, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            checks::check_home_permissions(home.clone(), wallet.clone(), false).status,
            Status::Pass,
            "owner-only home and wallet must pass"
        );

        // Loosen the home to world-readable: targeted ⇒ WARN, open-pool ⇒ FAIL.
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            checks::check_home_permissions(home.clone(), wallet.clone(), false).status,
            Status::Warn,
            "a group/world-accessible home is a WARN for a targeted seat"
        );
        assert_eq!(
            checks::check_home_permissions(home.clone(), wallet.clone(), true).status,
            Status::Fail,
            "…and a FAIL for an open-pool seat"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- Issue #553: bounded transient-retry over the startup readiness gate ----
    //
    // These drive `run_readiness_with_retry` directly with SCRIPTED check-sets and an injected
    // recording `sleep` (the waits are captured, never actually slept), so the schedule and ceiling
    // are exercised in microseconds. They are plain `#[test]`s — the retry driver is feature-neutral
    // logic — so they run in BOTH the default-feature and the `acp` CI jobs (non-inert in each).

    /// Outcome of one scripted gate drive: the verdict, how many times the checks were re-run, the
    /// waits the gate asked to sleep (recorded, NOT slept — the real `readiness_backoff` is used, so
    /// this vector also pins the schedule), and the captured stdout/stderr.
    struct GateDrive {
        result: Result<(), ()>,
        attempts: usize,
        waits: Vec<std::time::Duration>,
        out: String,
        err: String,
    }

    fn drive_gate(
        max_attempts: u32,
        mut script: impl FnMut(usize) -> Vec<Check>,
    ) -> GateDrive {
        let attempts = std::cell::Cell::new(0usize);
        let waits = std::cell::RefCell::new(Vec::new());
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let result = run_readiness_with_retry(
            || {
                let n = attempts.get();
                attempts.set(n + 1);
                script(n)
            },
            max_attempts,
            readiness_backoff,
            |wait| waits.borrow_mut().push(wait),
            &mut out,
            &mut err,
        );
        GateDrive {
            result,
            attempts: attempts.get(),
            waits: waits.into_inner(),
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    // The retry PREDICATE: worthwhile only when there IS a blocking Fail and EVERY blocking Fail is
    // transient. This is the classification the whole fix keys on (fault CLASS, not outcome severity).
    #[test]
    fn transient_retry_worthwhile_requires_all_blocking_fails_transient() {
        assert!(
            !transient_retry_worthwhile(&[Check::pass("a", "ok"), Check::warn("b", "meh", "x")]),
            "no blocking Fail ⇒ nothing to retry"
        );
        assert!(
            transient_retry_worthwhile(&[Check::fail_transient("relay reachability", "down", "x")]),
            "a lone transient Fail is retry-worthwhile"
        );
        assert!(
            !transient_retry_worthwhile(&[Check::fail("seller key", "missing", "x")]),
            "a lone unrecoverable Fail is not"
        );
        assert!(
            !transient_retry_worthwhile(&[
                Check::fail_transient("relay reachability", "down", "x"),
                Check::fail("seller key", "missing", "x"),
            ]),
            "one unrecoverable among transients ⇒ refuse, not retry"
        );
        assert!(
            transient_retry_worthwhile(&[
                Check::warn("mint reachability", "degraded", "x"),
                Check::fail_transient("relay reachability", "down", "x"),
            ]),
            "a non-blocking WARN does not defeat retry"
        );
    }

    // (1) A transient blip that clears on re-run BOOTS the seller — the recovery #553 asks for.
    // RED-PROVE: delete the retry branch in `run_readiness_with_retry` (refuse on the first Fail) and
    // this goes red — attempt 1's transient Fail is treated as fatal and the box never re-runs.
    #[test]
    fn transient_failure_then_success_boots() {
        let run = drive_gate(READINESS_MAX_ATTEMPTS, |n| {
            if n == 0 {
                vec![Check::fail_transient("relay reachability", "relay down", "check relay")]
            } else {
                vec![Check::pass("relay reachability", "connected + NIP-42 authenticated")]
            }
        });
        assert_eq!(run.result, Ok(()), "a transient blip that clears must boot; stderr: {}", run.err);
        assert_eq!(run.attempts, 2, "exactly one retry after the blip");
        assert_eq!(
            run.waits,
            vec![readiness_backoff(1)],
            "exactly one backoff — the first-retry wait"
        );
        assert!(run.out.contains("retry 1/"), "operator must see the retry wait: {}", run.out);
        assert!(run.out.contains("starting seller"), "the recovered gate boots: {}", run.out);
    }

    // (2) An unrecoverable misconfiguration refuses on the FIRST pass — no retry, no sleep — because
    // re-running cannot conjure a missing key. RED-PROVE: make `transient_retry_worthwhile` ignore the
    // `transient` marker (retry on any Fail) and this goes red — `attempts` climbs past 1 and the gate
    // sleeps before the inevitable refusal.
    #[test]
    fn unrecoverable_failure_refuses_immediately_without_retry() {
        let run = drive_gate(READINESS_MAX_ATTEMPTS, |_| {
            vec![Check::fail("seller key", "key missing — seller has no signing key", "generate the key")]
        });
        assert_eq!(run.result, Err(()), "an unrecoverable failure must refuse");
        assert_eq!(run.attempts, 1, "no retry on an unrecoverable failure");
        assert!(run.waits.is_empty(), "must not sleep before refusing an unrecoverable failure");
        assert!(
            run.err.contains("REFUSING to start"),
            "the fail-closed refusal message is preserved: {}",
            run.err
        );
    }

    // (3) A transient failure that NEVER clears exhausts the bounded retries, then refuses — the
    // fail-closed CEILING. Pins the exact schedule (20s,40s,60s,80s = 200s) and that the box is
    // ultimately refused, not looping forever. RED-PROVE: dropping the `attempt < max_attempts` guard
    // (unbounded loop) hangs this test instead of returning Err.
    #[test]
    fn exhausted_transient_retries_still_refuse_fail_closed() {
        use std::time::Duration;
        let run = drive_gate(READINESS_MAX_ATTEMPTS, |_| {
            vec![Check::fail_transient("relay reachability", "relay down", "check relay")]
        });
        assert_eq!(run.result, Err(()), "exhausted transient retries must still refuse");
        assert_eq!(
            run.attempts,
            READINESS_MAX_ATTEMPTS as usize,
            "every attempt is consumed before refusing"
        );
        assert_eq!(
            run.waits,
            vec![
                readiness_backoff(1),
                readiness_backoff(2),
                readiness_backoff(3),
                readiness_backoff(4),
            ],
            "linear backoff across the four inter-attempt waits"
        );
        assert_eq!(
            run.waits.iter().sum::<Duration>(),
            Duration::from_secs(200),
            "documented 200s worst-case added wait over 5 attempts"
        );
        assert!(
            run.err.contains("REFUSING to start"),
            "the fail-closed refusal message is preserved: {}",
            run.err
        );
    }

    // (4) A mixed set — a transient Fail ALONGSIDE an unrecoverable one — refuses immediately: ANY
    // unrecoverable blocking failure defeats retry, so a genuinely misconfigured seat never burns the
    // backoff budget on a dependency that would not have saved it.
    #[test]
    fn any_unrecoverable_failure_defeats_transient_retry() {
        let run = drive_gate(READINESS_MAX_ATTEMPTS, |_| {
            vec![
                Check::fail_transient("relay reachability", "relay down", "check relay"),
                Check::fail("seller key", "key missing", "generate the key"),
            ]
        });
        assert_eq!(run.result, Err(()));
        assert_eq!(run.attempts, 1, "a single unrecoverable failure refuses at once");
        assert!(run.waits.is_empty(), "no sleep when an unrecoverable failure is present");
    }
}
