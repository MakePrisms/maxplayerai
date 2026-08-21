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
    /// KIND of the check this result came from, distinct from both outcome SEVERITY (`status`) and
    /// fault CLASS (`transient`): `true` for an ENVIRONMENT requirement — a check that asks "can
    /// this box EVER do the work" (nix, #745) — which `--skip-doctor` never bypasses; `false` for a
    /// READINESS check — "is this box ready RIGHT NOW" — which the blanket bypass may skip. Read
    /// only by the boot gate's refusal message, so it is cfg-gated exactly like `transient`.
    #[cfg(any(feature = "acp", test))]
    skip_doctor_exempt: bool,
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
            // Default kind is READINESS: only the nix check opts into the environment kind via
            // `fail_environment`, so every other blocking Fail stays --skip-doctor-bypassable.
            #[cfg(any(feature = "acp", test))]
            skip_doctor_exempt: false,
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

    /// A blocking failure of an ENVIRONMENT requirement (#745): a check that asks "can this box
    /// EVER do the work", where every readiness check asks "is this box ready RIGHT NOW". A
    /// different KIND, and the difference is load-bearing twice over: the failure is unrecoverable
    /// (no retry, no sleep — `transient` stays `false`), and `--skip-doctor` does NOT bypass it —
    /// #745 rules out any escape hatch, and a blanket bypass that swept this up would be that
    /// escape hatch wearing an existing flag. Renders identically to [`Check::fail`]; in a
    /// buyer-only (wallet, no-acp) build there is no gate and no marker, so this collapses to
    /// `fail` — same shape as [`Check::fail_transient`].
    fn fail_environment(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        let check = Self::fail(name, detail, hint);
        #[cfg(any(feature = "acp", test))]
        let check = Self { skip_doctor_exempt: true, ..check };
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
        // The bypass epilogue is per-KIND (#745). A readiness failure may be bypassed with
        // --skip-doctor; an environment failure (nix) may not, and the refusal says WHY the kinds
        // differ so the asymmetry reads as the design it is, not as an inconsistency for a later
        // edit to "fix" by sweeping the environment check back into the blanket bypass.
        let exempt: Vec<&str> = failures
            .iter()
            .filter(|c| c.skip_doctor_exempt)
            .map(|c| c.name.as_str())
            .collect();
        if exempt.is_empty() {
            let _ = writeln!(
                err,
                "resolve the item(s) above, then re-run `maxplayer seller`. To bypass these checks (NOT recommended), pass --skip-doctor."
            );
        } else {
            let names = exempt.join(", ");
            let _ = writeln!(err, "resolve the item(s) above, then re-run `maxplayer seller`.");
            if failures.len() > exempt.len() {
                let _ = writeln!(
                    err,
                    "--skip-doctor can bypass the readiness failure(s) (NOT recommended) — but not {names}."
                );
            }
            let _ = writeln!(
                err,
                "{names} is not bypassable — not by --skip-doctor, not by any flag (#745): a readiness check asks whether this box is ready RIGHT NOW; {names} asks whether it can EVER do the work — a different kind of requirement, with no warn-and-serve mode and no escape hatch."
            );
        }
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

    use crate::sandbox_probe::{Containment, ContainmentModel};
    use maxplayer_core::seller_git;

    use super::Check;
    use maxplayer_core::agent_presets::AdapterHost;
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
    /// Named apart from SANDBOX_CHECK: that one says `docker` resolves, this one says the docker IMAGE
    /// the seat will run jobs in is actually available (present locally or pullable).
    const SANDBOX_IMAGE_CHECK: &str = "sandbox image";
    /// Named apart from SANDBOX_CHECK on purpose: one says the launcher exists, the other says it
    /// confines, and a reader scanning the output must be able to tell which one passed.
    const CONTAINMENT_CHECK: &str = "sandbox containment";
    const HOME_PERMS_CHECK: &str = "home permissions";
    /// Named apart from HOME_PERMS_CHECK: that one is the seat home/wallet; this one is the
    /// harness credential directory the cage cannot exclude (#689/#715).
    const HARNESS_CREDS_CHECK: &str = "harness credential permissions";
    const NIX_CHECK: &str = "nix";

    /// Determinate Systems' documented install one-liner — the fix for a box where nix truly is
    /// not installed. The same installer `install.sh` chains (#745's shell half).
    const DETERMINATE_NIX_INSTALL: &str =
        "curl -fsSL https://install.determinate.systems/nix | sh -s -- install";

    /// #745: a working nix, or `maxplayer seller` refuses to boot. This is an ENVIRONMENT
    /// requirement — every other check in the registry asks "is this box ready RIGHT NOW"
    /// (transient, operator-overridable); this one asks "can this box EVER do the work". A
    /// different KIND, hence [`Check::fail_environment`]: the failure is unrecoverable (a re-run
    /// cannot install nix, so the gate refuses immediately — no retry, no sleep) and it survives
    /// `--skip-doctor` — a nix-less seat serving the pool is exactly the state the requirement
    /// exists to prevent, so a blanket bypass that swept this check up would be #745's ruled-out
    /// escape hatch wearing an existing flag. If an operator override is ever wanted it must be
    /// its own explicitly named flag (the `--unsafe-no-sandbox` precedent, which waives exactly
    /// one check by name); we deliberately ship none.
    ///
    /// The probe EXECUTES `nix --version` rather than looking nix up on PATH — a resolvable but
    /// broken shim must not pass (same reason install.sh's probe runs it). When PATH fails, the
    /// well-known install locations are probed too, so an INSTALLED box whose service environment
    /// merely lacks nix on PATH (a systemd unit does not source a login shell) is reported as the
    /// PATH-skew case it is, never misread as an uninstalled one.
    pub(super) fn check_nix(user_home: Option<std::path::PathBuf>) -> Check {
        if nix_runs("nix") {
            return fold_nix(true, None);
        }
        fold_nix(false, off_path_nix(user_home.as_deref()))
    }

    /// Pure verdict over the two probe legs — the testable core of [`check_nix`]. `on_path` is
    /// "`nix --version` ran from this process's PATH"; `off_path` is a runnable nix found at a
    /// well-known install location despite that. Either failure carries BOTH remedies — the
    /// Determinate one-liner AND the PATH-skew fix — because a refusing gate cannot always tell an
    /// uninstalled box from a skewed one; when it CAN (the off-path leg found a runnable nix) the
    /// message leads with "installed, do NOT reinstall" so the box is never misread as uninstalled.
    pub(super) fn fold_nix(on_path: bool, off_path: Option<std::path::PathBuf>) -> Check {
        if on_path {
            return Check::pass(
                NIX_CHECK,
                "working nix on PATH (`nix --version` ran in this process's environment)",
            );
        }
        match off_path {
            Some(nix) => {
                let bin_dir = nix
                    .parent()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_else(|| "/nix/var/nix/profiles/default/bin".to_owned());
                Check::fail_environment(
                    NIX_CHECK,
                    format!(
                        "nix IS installed ({} runs) but `nix` is absent from this process's PATH — \
                         the PATH-skew case, not an uninstalled box: a systemd unit does not source \
                         a login shell, so a nix that works in your login shell never reaches the \
                         service environment",
                        nix.display()
                    ),
                    format!(
                        "do NOT reinstall — put the nix bin dir on the service PATH: `systemctl edit <unit>` \
                         and add `[Service]` `Environment=\"PATH={bin_dir}:/usr/local/bin:/usr/bin:/bin\"`, \
                         then restart the unit (only a box with no nix at all would instead need the \
                         installer: `{DETERMINATE_NIX_INSTALL}`)"
                    ),
                )
            }
            None => Check::fail_environment(
                NIX_CHECK,
                "no working nix: `nix --version` did not run from PATH, and no runnable nix was \
                 found at the well-known install locations (/nix/var/nix/profiles/default/bin, \
                 /run/current-system/sw/bin, ~/.nix-profile/bin) — a box without nix can never do \
                 the work, so the seller must not serve the pool from it",
                format!(
                    "install nix: `{DETERMINATE_NIX_INSTALL}` — or, if nix already works in your \
                     login shell, this is the PATH-skew case (a systemd unit does not source a \
                     login shell, so the login-shell PATH never reaches the service environment): \
                     add the nix bin dir to the unit's PATH, e.g. `systemctl edit <unit>` with \
                     `Environment=\"PATH=/nix/var/nix/profiles/default/bin:/usr/local/bin:/usr/bin:/bin\"`"
                ),
            ),
        }
    }

    /// A runnable nix at a well-known install prefix, probed only after the PATH leg failed. Each
    /// candidate is executed, not merely stat'ed — the point is a nix that would work if PATH
    /// carried it, and a broken shim at a known path proves nothing.
    fn off_path_nix(user_home: Option<&Path>) -> Option<std::path::PathBuf> {
        let mut candidates = vec![
            // Multi-user install (the Determinate installer's default profile).
            std::path::PathBuf::from("/nix/var/nix/profiles/default/bin/nix"),
            // NixOS system profile.
            std::path::PathBuf::from("/run/current-system/sw/bin/nix"),
        ];
        if let Some(home) = user_home {
            // Single-user install profile.
            candidates.push(home.join(".nix-profile/bin/nix"));
        }
        candidates
            .into_iter()
            .find(|nix| nix.is_file() && nix_runs(nix))
    }

    /// True when `program --version` spawns and exits 0, with every stdio stream nulled so the
    /// probe can never block on or pollute the gate's output.
    fn nix_runs(program: impl AsRef<std::ffi::OsStr>) -> bool {
        std::process::Command::new(program)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

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
                    "set [accepted_mints] in config.toml (it defaults to {DEFAULT_MINIBITS_MINT_URL})"
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
        host: AdapterHost,
    ) -> Check {
        let Some(seller) = seller else {
            return Check::warn(
                AGENT_CHECK,
                "no [seller] section configured",
                "run `maxplayer seller --agent <claude|cursor|codex> --rate-sats <n>` once to configure",
            );
        };
        match seller_agents::resolve(&seller, &presets, host) {
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
        check_sandbox_launcher_in(sandbox, container_marker())
    }

    /// The marker a container runtime plants in every container it starts — `Some(path)` when THIS
    /// process is itself containerized. Parsing cgroups is not an alternative: under cgroup v2 a
    /// container's `/proc/1/cgroup` reads `0::/`, byte-identical to the host's.
    fn container_marker() -> Option<&'static str> {
        ["/.dockerenv", "/run/.containerenv"]
            .into_iter()
            .find(|marker| std::path::Path::new(marker).exists())
    }

    /// [`check_sandbox_launcher`] over an injected container marker, so both sides of the
    /// docker-in-docker refusal are testable on a host that is not itself in a container.
    pub(super) fn check_sandbox_launcher_in(
        sandbox: Option<SandboxConfig>,
        container: Option<&str>,
    ) -> Check {
        let policy = match SandboxPolicy::from_config(sandbox.as_ref()) {
            Ok(policy) => policy,
            Err(error) => {
                return Check::fail(
                    SANDBOX_CHECK,
                    format!("[sandbox] does not resolve into an executor: {error}"),
                    "fix the [sandbox] section (or remove it to run unsandboxed)",
                )
            }
        };
        // Under `mode = "docker"` there is no launcher argv; the spawn is `docker run …`, so the
        // same "resolves before it can ENOENT every job" property is asked of `docker` itself.
        if let Some(image) = policy.docker_image() {
            // Docker-in-docker. `docker run -v <host path>:/work` is resolved by the HOST daemon, so
            // a seller that is itself containerized would name a workdir that exists only inside its
            // own filesystem (under compose: `/data/seller-jobs/…` on a named volume). Docker CREATES
            // a missing bind source as an empty directory rather than refusing — so the agent would
            // work in a phantom `/work`, the delivery snapshot would find the seller's real workdir
            // untouched, and the buyer would be charged for an EMPTY tree.
            //
            // Every other docker misconfiguration fails loudly at spawn. This one does not, which is
            // why it is refused here rather than left to be discovered by a paying buyer.
            if let Some(marker) = container {
                return Check::fail(
                    SANDBOX_CHECK,
                    format!(
                        "[sandbox] mode=docker (image '{image}'), but this seller is ITSELF running \
                         in a container ({marker}) — the per-job bind mount would resolve against \
                         the host filesystem, not this one, and a job would deliver an EMPTY tree"
                    ),
                    "run the seller directly on the host to use mode=docker, or switch [sandbox] to a \
                     launcher that confines from inside a container",
                );
            }
            return if argv0_resolvable("docker") {
                Check::pass(
                    SANDBOX_CHECK,
                    format!("[sandbox] mode=docker, image '{image}' — docker resolvable"),
                )
            } else {
                Check::fail(
                    SANDBOX_CHECK,
                    "[sandbox] mode=docker but 'docker' is neither on PATH nor an existing file — \
                     every job would fail at spawn (ENOENT)"
                        .to_owned(),
                    "install docker or switch [sandbox] mode (or remove [sandbox] to run unsandboxed)",
                )
            };
        }
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

    const EGRESS_CHECK: &str = "sandbox egress";

    /// Whether this seat can contain a job's egress when it launches one.
    ///
    /// Containment lives in the job's own network namespace (#797), installed before the job process
    /// exists and gone when it exits. So the state this check used to hunt for — rules configured but
    /// not in force — is no longer representable: a job either starts contained or does not start.
    /// What remains checkable BEFORE a job arrives is whether the namespace can be built at all, and
    /// the answer is the docker network the holder joins.
    pub(super) fn check_sandbox_egress(sandbox: Option<SandboxConfig>) -> Check {
        check_sandbox_egress_in(sandbox, read_sandbox_network)
    }

    /// Whether the configured docker network exists. `Ok(true)`/`Ok(false)` when we could look; `Err`
    /// when we could not (no docker on PATH, daemon unreachable).
    ///
    /// The `Err` arm is deliberately distinct from `Ok(false)`: "the network is absent" and "I could
    /// not ask" are different facts, and reporting the second as the first would send an operator to
    /// create a network that already exists.
    fn read_sandbox_network(network: &str) -> Result<bool, ()> {
        match std::process::Command::new("docker")
            .args(["network", "inspect", network])
            .output()
        {
            Ok(output) if output.status.success() => Ok(true),
            // The daemon distinguishes the two cases in its message, not its exit code: a missing
            // network is reported as such, while a daemon we cannot talk to is an instrument failure.
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                if stderr.contains("not found") || stderr.contains("no such network") {
                    Ok(false)
                } else {
                    Err(())
                }
            }
            Err(_) => Err(()),
        }
    }

    /// [`check_sandbox_egress`] over an injected network probe, so every branch is testable on a host
    /// with no docker daemon at all.
    pub(super) fn check_sandbox_egress_in(
        sandbox: Option<SandboxConfig>,
        network_exists: impl Fn(&str) -> Result<bool, ()>,
    ) -> Check {
        let policy = match SandboxPolicy::from_config(sandbox.as_ref()) {
            // A config that does not resolve is already FAILed by the launcher check; do not double-report.
            Err(_) => return Check::pass(EGRESS_CHECK, "no resolvable docker executor"),
            Ok(policy) => policy,
        };
        // A host executor has no container to contain. Never a spurious warning.
        if policy.docker_image().is_none() {
            return Check::pass(EGRESS_CHECK, "not a docker executor; no container egress to contain");
        }
        // ADVISORY for every docker seat: this check reports, it does not block. `Status::Warn` is
        // deliberate and is the whole policy of this check — see `boot_gate` and its doc comment,
        // where any `Fail` refuses to start and `Warn` does not.
        //
        // Why it reports rather than blocks, ruled by petar 2026-08-18: this release SHIPS THE WAY
        // to contain a docker seat and does not yet REQUIRE it. Installing the rules is still a
        // manual root command, and there is no packaging that survives a reboot, so a gate here
        // would fail every existing docker seat for a condition it has no automated way to satisfy.
        // Requiring containment is gated on that automation landing first.
        //
        // This block escalated to `Fail` earlier the same day and was returned here, so the reasoning
        // on both sides is kept rather than deleted. The escalation's premise stands and is not
        // withdrawn: a job is a stranger's code whether or not the seat chose its counterparty, so
        // when this does become blocking it applies to EVERY docker seat with no targeted-vs-open-pool
        // distinction. What changed is the ORDER — automate first, then require.
        //
        // Both messages name the exact command and the exact config key, so an operator can act on
        // the warning without reading #797.
        let Some(network) = policy.sandbox_network() else {
            return Check::warn(
                EGRESS_CHECK,
                "docker jobs run on the default bridge with no containment, so a job can reach this \
                 host's services and the seller's LAN (#797)",
                "set `[sandbox] network` to a dedicated docker network; jobs are then contained \
                 automatically at launch, with no root command to run and nothing to reinstall after \
                 a reboot",
            );
        };
        // What this can and cannot answer, stated because the old version of this check answered a
        // different question: the rules are installed per job, into that job's own namespace, so
        // there is no persistent ruleset to read and no drift to detect. A PASS here means "a job
        // launched now CAN be contained", never "a job is contained" — the only thing that
        // establishes containment is a launch, and a launch that cannot establish it FAILS the job.
        match network_exists(network) {
            Ok(true) => {
                // Also assert the policy is not vacuously empty. An empty plan would apply perfectly
                // and contain nothing, which is the failure the sidecar's exit 4 exists to refuse.
                let rules = maxplayer_core::sandbox_net::NetPolicy {
                    // A placeholder: the real address is measured per launch. Only the COUNT is read
                    // here, and no rule's presence depends on which address this is.
                    gateway: "172.17.0.1".into(),
                    proxy_ports: policy.proxy_ports(),
                    log_connections: true,
                }
                .install_plan()
                .len();
                Check::pass(
                    EGRESS_CHECK,
                    format!(
                        "network '{network}' exists and the policy renders {rules} rules; each job \
                         gets them installed in its own network namespace before it starts"
                    ),
                )
            }
            // Not "uncontained" — the opposite. Containment fails closed, so a missing network means
            // every docker job FAILS at launch rather than running unprotected. Worth its own message
            // because the operator's symptom is "no job runs", not "a job escaped".
            Ok(false) => Check::warn(
                EGRESS_CHECK,
                format!(
                    "`[sandbox] network` names '{network}', but no such docker network exists — \
                     containment cannot be established, so every docker job will FAIL at launch \
                     rather than run uncontained"
                ),
                format!("create it: `docker network create {network}`"),
            ),
            // An INSTRUMENT limit, not a measured unsafe state: the arms above are things we looked
            // at, this is a thing we could not look at. Kept separate for the day this check becomes
            // blocking — those arms should block then and this one still should not, or a doctor run
            // on a box whose daemon is merely asleep goes red on a correctly configured seat, and a
            // gate that is red on the normal case is a gate that gets skipped.
            Err(()) => Check::warn(
                EGRESS_CHECK,
                format!(
                    "`[sandbox] network` names '{network}', but docker could not be asked whether it \
                     exists, so this is UNVERIFIED rather than ready"
                ),
                "check the docker daemon is running and reachable: `docker network ls`",
            ),
        }
    }

    const CREDENTIAL_CONTAINMENT_CHECK: &str = "sandbox credential containment";

    /// The #647 credential proxy keeps out of a docker container every model-credential variable named
    /// in EITHER registry: the built-in table, and the operator's `[sandbox] file_credentials`. What it
    /// cannot contain is an operator-added `[sandbox] forward_env` variable the daemon does not
    /// recognize — a `MY_AGENT_TOKEN` may be a credential, and the daemon has no way to know, so it
    /// still crosses raw. This surfaces that as a WARN. Advisory — it never blocks boot, because the
    /// operator chose to forward the variable; the fix is theirs.
    ///
    /// The pass string names those two registries rather than claiming "every known credential",
    /// because the scope of this check is exactly what they list — a completeness claim wider than the
    /// registries it enumerates would go quietly false the first time a third source is added.
    pub(super) fn check_sandbox_credential_containment(sandbox: Option<SandboxConfig>) -> Check {
        check_sandbox_credential_containment_in(sandbox, |key| std::env::var(key).ok())
    }

    /// [`check_sandbox_credential_containment`] over an injected environment, so both the contained
    /// and the leaking case are testable without mutating the process environment.
    pub(super) fn check_sandbox_credential_containment_in(
        sandbox: Option<SandboxConfig>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Check {
        let policy = match SandboxPolicy::from_config(sandbox.as_ref()) {
            Ok(policy) => policy,
            // A config that does not resolve is already FAILed by the launcher check; do not double-report.
            Err(_) => return Check::pass(CREDENTIAL_CONTAINMENT_CHECK, "no resolvable docker executor"),
        };
        let uncontained =
            maxplayer_core::seller_exec::uncontained_forwarded_credentials(&policy, lookup);
        if uncontained.is_empty() {
            return Check::pass(
                CREDENTIAL_CONTAINMENT_CHECK,
                "every credential in the built-in table and in [sandbox] file_credentials is \
                 contained; no unrecognized forward_env var is set",
            );
        }
        let names = uncontained.join(", ");
        Check::warn(
            CREDENTIAL_CONTAINMENT_CHECK,
            format!(
                "[sandbox] forward_env carries {names} into the container UNCONTAINED — the proxy \
                 contains only known model-credential variables, so if {names} is a secret a \
                 stranger's job can read and reuse it"
            ),
            "remove it from [sandbox] forward_env, or treat that credential as compromised and \
             spend-cap it at the provider",
        )
    }

    /// Whether the docker sandbox image is on hand for the first job.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum ImageAvailability {
        /// `docker image inspect <ref>` succeeded — the image is already pulled locally.
        Present,
        /// Not local, but `docker manifest inspect <ref>` reached it in the registry — `docker run`
        /// will auto-pull it on the first job. Pre-pulling avoids that first-job latency.
        Pullable,
        /// Neither local nor reachable in the registry (not yet published, wrong ref, auth, or no
        /// network). The operator has to act before a job can run.
        Absent,
    }

    /// The docker sandbox image the seat runs jobs in must be present locally or pullable, or the
    /// FIRST awarded job stalls (or fails) on an image `docker run` cannot find. `check_sandbox_launcher`
    /// one layer out only proves `docker` itself resolves; the image is a separate object with its own
    /// failure mode. Under a non-docker policy there is no image, so this is a no-op Pass.
    ///
    /// On absence the check prints the EXACT `docker pull <ref>` command, so the operator acts without
    /// reading source — and running it surfaces the real reason (unpublished, auth, offline) directly.
    pub(super) fn check_sandbox_image(sandbox: Option<SandboxConfig>) -> Check {
        let policy = match SandboxPolicy::from_config(sandbox.as_ref()) {
            Ok(policy) => policy,
            // The launcher check already reports an unresolvable [sandbox]; don't double-fail here.
            Err(_) => return Check::pass(SANDBOX_IMAGE_CHECK, "no docker image to check"),
        };
        let Some(image) = policy.docker_image() else {
            return Check::pass(SANDBOX_IMAGE_CHECK, "no docker image to check (not mode=docker)");
        };
        // Only probe the image once docker itself resolves — otherwise the inspect would ENOENT and be
        // misread as an absent image. A missing docker is the launcher check's verdict, not this one's.
        if !argv0_resolvable("docker") {
            return Check::pass(
                SANDBOX_IMAGE_CHECK,
                format!("docker not resolvable; image '{image}' unchecked (see sandbox launcher)"),
            );
        }
        fold_sandbox_image(image, probe_image_availability(image))
    }

    /// Turn a probed [`ImageAvailability`] into a Check. Pure, so the verdict wording — and the exact
    /// `docker pull` hint on absence — is testable without a docker daemon.
    pub(super) fn fold_sandbox_image(image: &str, availability: ImageAvailability) -> Check {
        let pull = format!("docker pull {image}");
        match availability {
            ImageAvailability::Present => {
                Check::pass(SANDBOX_IMAGE_CHECK, format!("image '{image}' present locally"))
            }
            ImageAvailability::Pullable => Check::warn(
                SANDBOX_IMAGE_CHECK,
                format!(
                    "image '{image}' is not present locally but is pullable — docker will fetch it on \
                     the first job"
                ),
                format!("pre-pull to avoid a first-job delay: {pull}"),
            ),
            ImageAvailability::Absent => Check::fail(
                SANDBOX_IMAGE_CHECK,
                format!(
                    "image '{image}' is not present locally and could not be reached in the registry — \
                     the first awarded job would fail to start"
                ),
                pull,
            ),
        }
    }

    /// Probe whether `image` is present locally or pullable, executing docker with every stdio stream
    /// nulled (same discipline as [`nix_runs`]) so the gate's output is never polluted or blocked.
    /// `docker image inspect` is a purely-local lookup; `docker manifest inspect` is a registry HEAD
    /// that pulls no layers.
    fn probe_image_availability(image: &str) -> ImageAvailability {
        if docker_probe_ok(&["image", "inspect", image]) {
            ImageAvailability::Present
        } else if docker_probe_ok(&["manifest", "inspect", image]) {
            ImageAvailability::Pullable
        } else {
            ImageAvailability::Absent
        }
    }

    /// True when `docker <args>` spawns and exits 0, all stdio nulled.
    fn docker_probe_ok(args: &[&str]) -> bool {
        std::process::Command::new("docker")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    const SANDBOX_ENGINE_CHECK: &str = "sandbox engine floor";

    /// The first Docker Engine whose default seccomp profile BLOCKS the `io_uring` family.
    ///
    /// Derived from moby itself rather than from a changelog summary, because the obvious reading is
    /// wrong in the clearing direction. [moby/moby#46762] ("seccomp: block io_uring_* syscalls in
    /// default profile", milestone 25.0.0, merged 2023-11-02) removes exactly three entries —
    /// `io_uring_enter`, `io_uring_register`, `io_uring_setup` — from the allowlist in
    /// `profiles/seccomp/default_linux.go`. The profile's `defaultAction` is `SCMP_ACT_ERRNO`, so
    /// removal from the allowlist IS the block. The shipped `profiles/seccomp/default.json` carries
    /// three `io_uring` occurrences at v20.10.24, v23.0.0, v24.0.0 and v24.0.9, and zero at v25.0.0
    /// and v26.0.0; the commit is not on the v24 branch, so there is no 24.0.x backport to admit.
    ///
    /// The ADR calls this "a 2023 hardening". True of the COMMIT (2023-11-02), not of the release —
    /// it first ships in 25.0.0 (January 2024). A seat reasoning "2023, so Docker 24 (May 2023) has
    /// it" lands one major version too low, and that error CLEARS an exposed seat.
    ///
    /// Sharper than "older Engines merely lack the block": [moby/moby#39415] ADDED these three
    /// syscalls to the allowlist in 2019 and the revert ([moby/moby#41223]) was closed UNMERGED, so
    /// every Engine from 2019 through 24.x explicitly PERMITS io_uring. Below the floor the profile
    /// is not silent about io_uring — it allows it.
    const DOCKER_ENGINE_FLOOR: EngineVersion = EngineVersion { major: 25, minor: 0, patch: 0 };

    /// A Docker Engine version, ordered by (major, minor, patch) — derived `Ord` over the fields in
    /// declaration order is exactly that comparison.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub(super) struct EngineVersion {
        major: u32,
        minor: u32,
        patch: u32,
    }

    impl EngineVersion {
        /// Parse the leading `MAJOR[.MINOR[.PATCH]]` of a reported version.
        ///
        /// Lenient about what FOLLOWS the digits (`24.0.7-ce`, `25.0.3+dfsg1`, a vendor build suffix)
        /// and strict about the digits themselves: a string with no leading integer is `None`, never a
        /// silent `0.0.0`. A `0.0.0` would sort BELOW the floor and report an unreadable version as a
        /// confident "too old" — a wrong answer where the honest one is "unknown".
        pub(super) fn parse(reported: &str) -> Option<Self> {
            let mut fields = reported.trim().trim_start_matches('v').split('.');
            Some(Self {
                major: leading_number(fields.next()?)?,
                minor: fields.next().and_then(leading_number).unwrap_or(0),
                patch: fields.next().and_then(leading_number).unwrap_or(0),
            })
        }
    }

    impl std::fmt::Display for EngineVersion {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
        }
    }

    /// The leading run of ASCII digits in `field`, so `7-ce` reads as 7. `None` when the field opens
    /// with no digit — never 0, so "unparseable" stays distinguishable from "zero".
    fn leading_number(field: &str) -> Option<u32> {
        let digits: String =
            field.trim().chars().take_while(|character| character.is_ascii_digit()).collect();
        digits.parse().ok()
    }

    /// What `docker version` reported for the SERVER, or why no version could be read.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum EngineProbe {
        /// The daemon answered with a version that parsed.
        Reported(EngineVersion),
        /// The daemon was unreachable, or answered something no version parses from. Carries the
        /// operator-facing reason, so the WARN says which of the two happened.
        Unreadable(String),
    }

    /// The Docker Engine must be new enough that its DEFAULT seccomp profile blocks `io_uring`.
    ///
    /// `docs/SANDBOXING.md` §4 declines to hand-write a seccomp profile on purpose — dev toolchains
    /// touch a broad, shifting syscall surface — which leaves the Engine default as the ONLY filter
    /// standing on a non-gVisor seat. That reasoning silently assumes an Engine at or above
    /// [`DOCKER_ENGINE_FLOOR`]; below it the seat runs strangers' code with a recurring
    /// kernel-exploit surface explicitly allowed, and until this check nothing said so.
    ///
    /// WARN for a targeted-only seat, FAIL for an open-pool one — the same exposure split
    /// [`check_home_permissions`] and [`check_sandbox_containment`] already apply, for the same
    /// reason: serving the open pool means executing code posted by strangers.
    pub(super) fn check_sandbox_engine_floor(
        sandbox: Option<SandboxConfig>,
        claims_open_pool: bool,
    ) -> Check {
        let runtime = sandbox.as_ref().and_then(|sandbox| sandbox.runtime.clone());
        let policy = match SandboxPolicy::from_config(sandbox.as_ref()) {
            Ok(policy) => policy,
            // An unresolvable [sandbox] is already FAILed by the launcher check; don't double-report.
            Err(_) => {
                return Check::pass(SANDBOX_ENGINE_CHECK, "no docker executor to version-check")
            }
        };
        if policy.docker_image().is_none() {
            return Check::pass(SANDBOX_ENGINE_CHECK, "no Engine floor to check (not mode=docker)");
        }
        // Same ordering rule as check_sandbox_image: probe only once docker itself resolves, or the
        // spawn would ENOENT and be misreported as an unreadable Engine version. A missing docker is
        // the launcher check's verdict, not this one's.
        if !argv0_resolvable("docker") {
            return Check::pass(
                SANDBOX_ENGINE_CHECK,
                "docker not resolvable; Engine version unchecked (see sandbox launcher)",
            );
        }
        fold_sandbox_engine_floor(&probe_engine_version(), runtime.as_deref(), claims_open_pool)
    }

    /// Turn a probed Engine version into a Check. Pure, so every verdict — including the below-floor
    /// wording an exposed operator actually has to read — is testable with no docker daemon present.
    pub(super) fn fold_sandbox_engine_floor(
        probe: &EngineProbe,
        runtime: Option<&str>,
        claims_open_pool: bool,
    ) -> Check {
        let floor = DOCKER_ENGINE_FLOOR;
        // gVisor filters syscalls in the Sentry and does not apply the OCI seccomp profile at all
        // unless `--oci-seccomp` is set, so on a runsc seat the Engine default governs nothing and no
        // Engine version could change this verdict. Answered before the probe, not after it.
        if runtime == Some("runsc") {
            return Check::pass(
                SANDBOX_ENGINE_CHECK,
                format!(
                    "runtime = \"runsc\" — gVisor filters syscalls itself and ignores the OCI seccomp \
                     profile, so the Engine {floor} io_uring floor does not govern this seat"
                ),
            );
        }
        match probe {
            // Unknown is reported, never assumed. A silent Pass here would make the check quietest
            // exactly when it has learned least — and an operator whose daemon is down would read a
            // clean gate as "my Engine is fine".
            EngineProbe::Unreadable(reason) => Check::warn(
                SANDBOX_ENGINE_CHECK,
                format!(
                    "could not read the Docker Engine version ({reason}) — cannot confirm this seat \
                     has the io_uring seccomp block that first shipped in Engine {floor}"
                ),
                "start the docker daemon, then re-run `maxplayer doctor`; or read it by hand with \
                 `docker version --format '{{.Server.Version}}'`",
            ),
            EngineProbe::Reported(version) if *version >= floor => Check::pass(
                SANDBOX_ENGINE_CHECK,
                format!(
                    "Docker Engine {version} ≥ {floor} — the default seccomp profile blocks \
                     io_uring_setup/io_uring_enter/io_uring_register"
                ),
            ),
            EngineProbe::Reported(version) => {
                let detail = format!(
                    "Docker Engine {version} is BELOW the {floor} floor — its default seccomp profile \
                     still ALLOWS io_uring_setup/io_uring_enter/io_uring_register (allowlisted since \
                     2019, blocked only in {floor}), so a job reaches a recurring kernel-exploit \
                     surface docs/SANDBOXING.md §4 assumes is closed"
                );
                let hint = format!(
                    "upgrade the Docker Engine to {floor} or newer; on Linux, [sandbox] runtime = \
                     \"runsc\" (gVisor) also removes the exposure"
                );
                if claims_open_pool {
                    Check::fail(SANDBOX_ENGINE_CHECK, detail, hint)
                } else {
                    Check::warn(SANDBOX_ENGINE_CHECK, detail, hint)
                }
            }
        }
    }

    /// Ask the daemon for the SERVER version — the Engine that applies the seccomp profile.
    ///
    /// `{{.Server.Version}}`, never `{{.Client.Version}}`: the client is a separate program that can
    /// be a different version, and against a remote `DOCKER_HOST` it describes the wrong machine
    /// entirely. The client version is a value sitting NEXT TO the property this check is about.
    fn probe_engine_version() -> EngineProbe {
        let probe = std::process::Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let probe = match probe {
            Ok(probe) => probe,
            Err(error) => return EngineProbe::Unreadable(format!("could not run docker: {error}")),
        };
        // `docker version` prints the CLIENT block and then exits non-zero when the daemon does not
        // answer, so a populated stdout is not evidence of a server reply. The status is the signal.
        if !probe.status.success() {
            return EngineProbe::Unreadable(
                "docker daemon did not answer `docker version` (not running, or not permitted)"
                    .to_owned(),
            );
        }
        let reported = String::from_utf8_lossy(&probe.stdout).trim().to_owned();
        match EngineVersion::parse(&reported) {
            Some(version) => EngineProbe::Reported(version),
            None => EngineProbe::Unreadable(format!("unrecognized version string '{reported}'")),
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
        check_sandbox_containment_with_model(
            sandbox,
            home_root,
            claims_open_pool,
            unsafe_override,
            ContainmentModel::assumed_for_platform(),
        )
    }

    pub(super) fn check_sandbox_containment_with_model(
        sandbox: Option<SandboxConfig>,
        home_root: std::path::PathBuf,
        claims_open_pool: bool,
        unsafe_override: bool,
        model: ContainmentModel,
    ) -> Check {
        let policy = match SandboxPolicy::from_config(sandbox.as_ref()) {
            Ok(policy) => policy,
            Err(error) => {
                return Check::fail(
                    CONTAINMENT_CHECK,
                    format!("[sandbox] does not resolve into an executor: {error}"),
                    "fix the [sandbox] section so the containment probe has an executor to measure",
                )
            }
        };
        let containment = crate::sandbox_probe::probe_containment(&policy, &home_root);

        if !claims_open_pool {
            return match containment {
                Containment::Contained => Check::pass(
                    CONTAINMENT_CHECK,
                    format!(
                        "launcher confines: a file outside the workdir was refused, the workdir was writable ({})",
                        model.guarantee_clause()
                    ),
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
                    Containment::Contained => format!(
                        "--unsafe-no-sandbox passed, though the launcher does confine ({})",
                        model.guarantee_clause()
                    ),
                    ref other => format!("--unsafe-no-sandbox passed: SERVING THE OPEN POOL UNCONTAINED — {}", other.detail()),
                },
                "remove --unsafe-no-sandbox and configure a [sandbox] launcher that passes the probe",
            );
        }

        match crate::sandbox_probe::open_pool_admission(true, &containment, false) {
            Ok(()) => Check::pass(
                CONTAINMENT_CHECK,
                format!(
                    "launcher confines: a file outside the workdir was refused, the workdir was writable ({})",
                    model.guarantee_clause()
                ),
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

    /// The configured harness's credential directory, for loose permissions. Sibling of
    /// [`check_home_permissions`]: same unix-guard / metadata-error-is-warn / empty-`too_open`-is-pass
    /// shape, but a different directory and a different finding.
    ///
    /// Since #689 the seller quickstart tells operators the harness credential lives inside the cage
    /// and is readable by whatever the agent can read. That names it in the threat model; this check
    /// is the inspection half that was missing (#715). It does not change permissions, widen or
    /// narrow the sandbox, or revisit #689's conclusion that a subscription credential cannot be
    /// excluded from the cage.
    ///
    /// Resolution is [`seller_agents::harness_credential_dir`] over the same registry boot uses.
    /// An unlabelled `--agent-argv` hatch and any non-built-in label cannot be resolved: the check
    /// SAYS so and inspects nothing. Guessing a default harness's directory (or sniffing argv) would
    /// inspect the wrong path and pass — worse than no check.
    ///
    /// **Mask (0o022, not 0o077).** The home/wallet check flags any group/other access because those
    /// directories contain the key and proofs — a read is money-bearing. The harness credential FILE
    /// is already owner-only on a measured seat; group-read of the directory lists names, it does not
    /// leak the secret. The finding is group/other WRITABILITY of the directory and of `settings.json`,
    /// which STEERS the harness: write access is a configuration-injection surface, not merely an
    /// information leak. Flagging 0o077 would warn on a typical 0755 directory that is not that threat.
    ///
    /// **Always Warn, never Fail.** Open-pool vs targeted is about stranger code inside the cage
    /// reading the credential (#689) — a different axis from local group-write. Group-write is inert
    /// on a single-account host (the group has no other members) and only becomes a problem if the
    /// host grows a second account. Failing boot for a measured 0775 `~/.claude` would alarm an
    /// operator who is not currently exposed. Copy must not imply the seat is compromised.
    pub(super) fn check_harness_credential_permissions(
        seller: Option<SellerConfig>,
        presets: BTreeMap<String, AgentPresetConfig>,
        user_home: Option<std::path::PathBuf>,
    ) -> Check {
        let Some(seller) = seller else {
            return Check::warn(
                HARNESS_CREDS_CHECK,
                "no [seller] section configured — cannot resolve a harness credential directory",
                "run `maxplayer seller --agent <claude|cursor|codex> --rate-sats <n>` once to configure; doctor will not guess a harness",
            );
        };
        let resolved = match seller_agents::resolve(&seller, &presets, AdapterHost::Host) {
            Ok(resolved) => resolved,
            Err(_) => {
                return Check::warn(
                    HARNESS_CREDS_CHECK,
                    "harness registry did not resolve — cannot inspect a credential directory",
                    "fix the agent preset check first; doctor will not guess a harness credential path",
                );
            }
        };
        let Some(user_home) = user_home else {
            return Check::warn(
                HARNESS_CREDS_CHECK,
                "HOME is unset — cannot resolve a harness credential directory",
                "set HOME so doctor can inspect $HOME/.claude (or .cursor / .codex); doctor will not guess a path",
            );
        };

        let mut unresolvable: Vec<String> = Vec::new();
        let mut inspect: Vec<std::path::PathBuf> = Vec::new();
        for agent in resolved.registry.entries() {
            match seller_agents::harness_credential_dir(agent, &user_home) {
                Some(dir) => inspect.push(dir),
                None => match &agent.name {
                    None => unresolvable
                        .push("raw --agent-argv hatch (no preset label)".to_owned()),
                    Some(name) => unresolvable
                        .push(format!("harness {name} (no known credential directory)")),
                },
            }
        }
        if inspect.is_empty() && unresolvable.is_empty() {
            return Check::warn(
                HARNESS_CREDS_CHECK,
                "no harness to inspect — cannot resolve a credential directory",
                "doctor will not guess a harness credential path",
            );
        }

        let mut too_open: Vec<String> = Vec::new();
        #[cfg(unix)]
        {
            use std::io::ErrorKind;
            use std::os::unix::fs::PermissionsExt;

            let consider = |path: &Path, missing_ok: bool, too_open: &mut Vec<String>| -> Option<Check> {
                let metadata = match std::fs::metadata(path) {
                    Ok(metadata) => metadata,
                    Err(error) if missing_ok && error.kind() == ErrorKind::NotFound => {
                        return None;
                    }
                    Err(error) => {
                        return Some(Check::warn(
                            HARNESS_CREDS_CHECK,
                            format!("could not read {} permissions: {error}", path.display()),
                            "check the harness credential path exists and is readable",
                        ));
                    }
                };
                let mode = metadata.permissions().mode() & 0o777;
                // Group/other WRITE only. See the check's doc comment for why not 0o077.
                if mode & 0o022 != 0 {
                    too_open.push(format!("{} ({mode:#o})", path.display()));
                }
                None
            };

            for dir in &inspect {
                if let Some(check) = consider(dir, false, &mut too_open) {
                    return check;
                }
                // settings.json STEERS the harness when present; absence is not a permissions
                // problem (the file is optional) so NotFound is skipped, never a silent skip of
                // a metadata error on a file that does exist.
                if let Some(check) = consider(&dir.join("settings.json"), true, &mut too_open) {
                    return check;
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = &inspect;
        }

        if too_open.is_empty() && unresolvable.is_empty() {
            return Check::pass(
                HARNESS_CREDS_CHECK,
                format!(
                    "not group/world-writable: {}",
                    inspect
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        if too_open.is_empty() {
            return Check::warn(
                HARNESS_CREDS_CHECK,
                format!(
                    "cannot resolve a credential directory for {} — not inspected (doctor will not guess a path)",
                    unresolvable.join(", ")
                ),
                "use a named preset (claude|cursor|codex) to inspect that harness's credential directory; a raw --agent-argv hatch and unknown labels have no known path",
            );
        }
        let mut detail = format!(
            "group/world-writable: {} — another local account in this group could write harness settings (configuration injection, not just an information leak). On a single-account host this is inert; it stops being inert if this host grows a second account",
            too_open.join(", ")
        );
        if !unresolvable.is_empty() {
            detail.push_str(&format!(
                "; also cannot resolve a credential directory for {} — not inspected (doctor will not guess a path)",
                unresolvable.join(", ")
            ));
        }
        Check::warn(
            HARNESS_CREDS_CHECK,
            detail,
            "chmod go-w the named path(s); doctor reports, it does not change permissions",
        )
    }

    fn build_runtime() -> Result<tokio::runtime::Runtime, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("tokio runtime: {error}"))
    }

    /// True when `argv0` names a runnable program: an existing file path, or a bare name found on
    /// PATH. Mirrors how the seller daemon would launch it.
    pub(super) fn argv0_resolvable(argv0: &str) -> bool {
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

/// Help that was asked for goes to stdout and succeeds. Feature-independent: `doctor --help` works
/// on every build, before any home bootstrap or check runs.
fn write_usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n  maxplayer doctor [--home <dir>]   # seller environment self-check (nix, credential helper, seller key, relay, mint, agent, sandbox, home permissions, harness credential permissions)\n\nExit codes: 0 all checks passed, 1 a blocking check FAILed"
    );
}

/// Entry from `cli::run` for `maxplayer doctor`.
///
/// Honors `--home <dir>` (mirroring `maxplayer seller`) so an operator can diagnose a specific seat,
/// and REFUSES any other argument rather than silently dropping it — a discarded flag produced a
/// confident report about the wrong home, which is worse than an error (issue #216).
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` prints usage to STDOUT and exits 0 on every build, before any home
    // bootstrap or check runs.
    if crate::cli::is_help_request(args) {
        write_usage(out);
        return SUCCESS;
    }

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
    let seller_for_creds = home.config.seller.clone();
    let custom_agents_for_creds = home.config.agents.clone();
    let telemetry = home.config.telemetry.clone();
    let sandbox = home.config.sandbox.clone();
    let sandbox_for_launcher = sandbox.clone();
    let sandbox_for_creds = sandbox.clone();
    let sandbox_for_image = sandbox.clone();
    let sandbox_for_engine = sandbox.clone();
    let sandbox_for_egress = sandbox.clone();
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
    // Harness credentials live under the operator $HOME, not the seat home. Empty HOME is
    // carried as None — never guessed as a relative `.claude`.
    let user_home = user_home_dir();

    // Environment requirements (#745) head the ONE registry — build_checks stays the single
    // source both `maxplayer doctor` and the boot gate run, never a second list to keep in sync.
    let mut checks: Vec<Box<dyn FnOnce() -> Check>> = build_environment_checks();
    checks.push(Box::new(checks::check_credential_helper));
    checks.push(Box::new(move || checks::check_seller_key(&key_path, key_present)));
    checks.push(Box::new(move || checks::check_relay(relay_url, secret)));
    // One aggregate mint check across the accept-policy: "can I settle anywhere?".
    checks.push(Box::new(move || checks::check_mints(accepted_mints)));
    let agent_host = maxplayer_core::agent_presets::AdapterHost::for_sandbox(sandbox.as_ref());
    checks.push(Box::new(move || {
        checks::check_agent_registry(seller, custom_agents, agent_host)
    }));
    checks.push(Box::new(move || checks::check_telemetry(telemetry)));
    // The seller boot gate blocks on this (issue #357): a launcher that cannot spawn would let the
    // node advertise and then fail every job. Bypassable, like every check, via --skip-doctor.
    checks.push(Box::new(move || checks::check_sandbox_launcher(sandbox_for_launcher)));
    // #647 P2: the credential proxy contains every KNOWN model-credential variable. What it cannot
    // recognize is an operator-added forward_env var (which may itself be a credential), so this WARNs
    // when one is set. Advisory — never blocks boot; the operator chose to forward it.
    checks.push(Box::new(move || {
        checks::check_sandbox_credential_containment(sandbox_for_creds)
    }));
    // #797: a docker job's network containment is host-side firewall rules, and a seat with them
    // missing is indistinguishable from a contained one FROM INSIDE A JOB. The operator is the only
    // one who can be told, so this WARNs. Advisory — never blocks boot, because turning a working
    // docker seat red on upgrade is a behaviour change, not a doctor's call.
    checks.push(Box::new(move || checks::check_sandbox_egress(sandbox_for_egress)));
    // #792 phase 3: under mode=docker, the image the seat runs jobs in must be present or pullable,
    // or the first awarded job stalls. On absence this prints the exact `docker pull` command. A
    // non-docker policy is a no-op Pass. Placed after the launcher (docker-resolves) check.
    checks.push(Box::new(move || checks::check_sandbox_image(sandbox_for_image)));
    // #796: under mode=docker the Engine's DEFAULT seccomp profile is the only syscall filter standing
    // on a non-gVisor seat — docs/SANDBOXING.md §4 declines to hand-write one on purpose. The io_uring
    // block first ships in Engine 25.0.0, and every Engine from 2019 through 24.x explicitly ALLOWS
    // that family, so an older Engine is a materially weaker posture than the ADR assumes and nothing
    // said so before this check. WARN for a targeted seat, FAIL for an open-pool one — the same
    // exposure split check_home_permissions and check_sandbox_containment already use.
    checks.push(Box::new(move || {
        checks::check_sandbox_engine_floor(sandbox_for_engine, claims_open_pool)
    }));
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
    // #715: inspect the configured harness's credential directory. Advisory WARN; never blocks
    // boot (group-write is inert on a single-account host).
    checks.push(Box::new(move || {
        checks::check_harness_credential_permissions(
            seller_for_creds,
            custom_agents_for_creds,
            user_home,
        )
    }));
    checks
}

/// The ENVIRONMENT requirements — checks that ask "can this box EVER do the work" rather than "is
/// it ready right now" — currently just nix (#745). These head [`build_checks`], so `maxplayer
/// doctor` and the full boot gate run them from the one shared registry; they are ALSO the entire
/// registry the boot gate still runs under `--skip-doctor`, because #745 rules out any escape
/// hatch for them — the blanket bypass narrows the gate to this subset, it never skips it. A
/// second gate beside [`sell_readiness_gate`] was the rejected alternative: that is the #728
/// keep-two-things-in-sync defect, committed prospectively.
#[cfg(feature = "wallet")]
fn build_environment_checks() -> Vec<Box<dyn FnOnce() -> Check>> {
    // The single-user nix profile lives under the operator $HOME (`~/.nix-profile`).
    let user_home = user_home_dir();
    vec![Box::new(move || checks::check_nix(user_home))]
}

/// The operator's `$HOME`, or `None` when unset or empty — never guessed as a relative path.
#[cfg(feature = "wallet")]
fn user_home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").and_then(|home| {
        if home.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(home))
        }
    })
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
///
/// `--skip-doctor` narrows the gate instead of skipping it (#745): the READINESS checks — the
/// "ready right now" kind — are bypassed, but the ENVIRONMENT requirement (nix) still runs,
/// because a box that can never do the work must not serve the pool and #745 rules out any escape
/// hatch for that check. Environment failures are never transient, so the narrowed gate refuses
/// immediately — no retry, no sleep.
#[cfg(feature = "acp")]
pub fn sell_readiness_gate(
    home: &maxplayer_core::home::MaxplayerHome,
    unsafe_no_sandbox: bool,
    skip_doctor: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), ()> {
    if skip_doctor {
        let _ = writeln!(
            err,
            "maxplayer seller --skip-doctor: startup readiness checks bypassed (box may be unable to sell); \
             the nix environment check still runs — it asks whether this box can EVER do the work, not \
             whether it is ready right now, and #745 rules out any bypass for it"
        );
        return run_readiness_with_retry(
            || run_checks(build_environment_checks()),
            READINESS_MAX_ATTEMPTS,
            readiness_backoff,
            std::thread::sleep,
            out,
            err,
        );
    }
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
        use maxplayer_core::home::{SandboxConfig, SandboxMode};

        let bogus = checks::check_sandbox_launcher(Some(SandboxConfig {
            mode: SandboxMode::Launcher,
            launcher: vec!["definitely-not-a-real-binary-xyz".into()],
            image: None,
            forward_env: Vec::new(),
            runtime: None,
            ..Default::default()
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
            checks::check_sandbox_launcher(Some(SandboxConfig {
                mode: SandboxMode::Launcher,
                launcher: vec![real],
                image: None,
                forward_env: Vec::new(),
                runtime: None,
                ..Default::default()
            }))
            .status,
            Status::Pass,
            "a resolvable launcher must PASS"
        );

        assert_eq!(
            checks::check_sandbox_launcher(None).status,
            Status::Pass,
            "an unsandboxed seat (no [sandbox]) must not FAIL"
        );
    }

    #[cfg(feature = "wallet")]
    fn contained_probe_launcher() -> maxplayer_core::home::SandboxConfig {
        maxplayer_core::home::SandboxConfig {
            mode: maxplayer_core::home::SandboxMode::Launcher,
            launcher: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'canary_read=denied\\nworkdir_write=ok\\n'".into(),
            ],
            image: None,
            forward_env: Vec::new(),
            runtime: None,
            ..Default::default()
        }
    }

    #[cfg(feature = "wallet")]
    fn containment_test_home(label: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "maxplayer-doctor-containment-{label}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn containment_pass_names_the_assumed_platform_model() {
        let home = containment_test_home("assumed-model");
        let check = checks::check_sandbox_containment(
            Some(contained_probe_launcher()),
            home.clone(),
            false,
            false,
        );

        assert_eq!(check.status, Status::Pass, "{}", check.render());
        #[cfg(target_os = "macos")]
        assert!(
            check
                .detail
                .contains("assumed deny-list for this platform"),
            "macOS PASS must name the assumed deny-list model:\n{}",
            check.detail
        );
        #[cfg(not(target_os = "macos"))]
        assert!(
            check
                .detail
                .contains("assumed allow-list for this platform"),
            "non-macOS PASS must name the assumed allow-list model:\n{}",
            check.detail
        );

        std::fs::remove_dir_all(home).ok();
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn containment_messages_name_each_model_on_every_contained_branch() {
        use crate::sandbox_probe::ContainmentModel;

        for (model, clause) in [
            (
                ContainmentModel::AllowList,
                "assumed allow-list for this platform: unprobed paths fail closed",
            ),
            (
                ContainmentModel::DenyList,
                "assumed deny-list for this platform: probed paths only — unlisted paths remain reachable",
            ),
        ] {
            let home = containment_test_home(match model {
                ContainmentModel::AllowList => "allow-list",
                ContainmentModel::DenyList => "deny-list",
            });
            let expected_pass = format!(
                "launcher confines: a file outside the workdir was refused, the workdir was writable ({clause})"
            );
            let targeted = checks::check_sandbox_containment_with_model(
                Some(contained_probe_launcher()),
                home.clone(),
                false,
                false,
                model,
            );
            assert_eq!(targeted.status, Status::Pass, "{}", targeted.render());
            assert_eq!(targeted.detail, expected_pass);

            let open_pool = checks::check_sandbox_containment_with_model(
                Some(contained_probe_launcher()),
                home.clone(),
                true,
                false,
                model,
            );
            assert_eq!(open_pool.status, Status::Pass, "{}", open_pool.render());
            assert_eq!(open_pool.detail, expected_pass);

            let overridden = checks::check_sandbox_containment_with_model(
                Some(contained_probe_launcher()),
                home.clone(),
                true,
                true,
                model,
            );
            assert_eq!(overridden.status, Status::Warn, "{}", overridden.render());
            assert_eq!(
                overridden.detail,
                format!(
                    "--unsafe-no-sandbox passed, though the launcher does confine ({clause})"
                )
            );

            std::fs::remove_dir_all(home).ok();
        }
    }

    #[test]
    fn containment_model_guarantee_clauses_are_the_specified_wording() {
        use crate::sandbox_probe::ContainmentModel;

        assert_eq!(
            ContainmentModel::AllowList.guarantee_clause(),
            "assumed allow-list for this platform: unprobed paths fail closed"
        );
        assert_eq!(
            ContainmentModel::DenyList.guarantee_clause(),
            "assumed deny-list for this platform: probed paths only — unlisted paths remain reachable"
        );
    }

    // Docker-in-docker is the ONE docker misconfiguration that does not fail at spawn: the per-job
    // bind mount is resolved by the host daemon, docker creates the missing source as an empty dir,
    // and the job delivers an empty tree a buyer has already paid for. Every other docker fault
    // ENOENTs loudly, so this is the one the gate has to catch by reasoning rather than by trying.
    // Delete the `container` branch in `check_sandbox_launcher_in` and this goes red.
    #[cfg(feature = "wallet")]
    #[test]
    fn docker_mode_is_refused_when_the_seller_is_itself_containerized() {
        use maxplayer_core::home::{SandboxConfig, SandboxMode};

        let docker = || {
            Some(SandboxConfig {
                mode: SandboxMode::Docker,
                launcher: Vec::new(),
                image: Some("maxplayer-sandbox:latest".into()),
                forward_env: Vec::new(),
                runtime: None,
                ..Default::default()
            })
        };

        let inside = checks::check_sandbox_launcher_in(docker(), Some("/.dockerenv"));
        assert_eq!(
            inside.status,
            Status::Fail,
            "a containerized seller must not advertise mode=docker: {}",
            inside.render()
        );
        assert!(
            inside.detail.contains("EMPTY"),
            "the refusal must name the SILENT outcome (an empty delivery), not just a bad mount: {}",
            inside.detail
        );
        assert!(
            inside.render().contains("fix:"),
            "a FAIL must carry a fix hint"
        );

        // Off the host-in-a-container path the same config is judged only on whether docker resolves.
        // Without this, a check that always failed would satisfy the assertions above.
        let outside = checks::check_sandbox_launcher_in(docker(), None);
        assert!(
            !outside.detail.contains("ITSELF running"),
            "a seller on the host must never be refused for being containerized: {}",
            outside.detail
        );
    }

    // #647 P2: every KNOWN credential var is contained, so an OAuth/api-key seat passes; only an
    // operator-added forward_env var the daemon cannot recognize is WARNed as possibly-uncontained.
    #[cfg(feature = "wallet")]
    #[test]
    fn docker_operator_forwarded_var_warns_that_it_is_uncontained() {
        use maxplayer_core::home::{SandboxConfig, SandboxMode};
        let docker = |forward_env: Vec<String>| {
            Some(SandboxConfig {
                mode: SandboxMode::Docker,
                launcher: Vec::new(),
                image: Some("maxplayer-sandbox:latest".into()),
                forward_env,
                runtime: None,
                ..Default::default()
            })
        };
        // An operator-added var that is set: named, advisory (never a boot-block).
        let cfg = docker(vec!["MY_AGENT_TOKEN".into()]);
        let set = |key: &str| (key == "MY_AGENT_TOKEN").then(|| "operator-secret".to_owned());
        let warned = checks::check_sandbox_credential_containment_in(cfg, set);
        assert_eq!(warned.status, Status::Warn, "{}", warned.render());
        assert_ne!(warned.status, Status::Fail, "advisory: {}", warned.render());
        assert!(
            warned.detail.contains("MY_AGENT_TOKEN") && warned.detail.contains("UNCONTAINED"),
            "must name the operator var: {}",
            warned.detail
        );

        // A known credential (now contained) is NOT flagged, even when set.
        let contained_env = |key: &str| (key == "CLAUDE_CODE_OAUTH_TOKEN").then(|| "oauth-real".to_owned());
        assert_eq!(
            checks::check_sandbox_credential_containment_in(docker(Vec::new()), contained_env).status,
            Status::Pass,
            "a contained credential must not be flagged",
        );
        // A non-docker seat forwards nothing into a container ⇒ Pass regardless of the environment.
        assert_eq!(
            checks::check_sandbox_credential_containment_in(None, |_| Some("set".to_owned())).status,
            Status::Pass,
        );
    }

    // #797: the three states a docker seat can be in are indistinguishable from inside a job, so the
    // operator is the only one who can be told. Every branch asserted, including the one that must
    // NOT claim containment: a configured network denies nothing on its own.
    #[test]
    fn doctor_sandbox_egress_warns_until_the_rules_are_actually_installed() {
        use maxplayer_core::home::{SandboxConfig, SandboxMode};
        let docker = |network: Option<&str>| {
            Some(SandboxConfig {
                mode: SandboxMode::Docker,
                image: Some("maxplayer-sandbox:latest".into()),
                network: network.map(str::to_owned),
                ..Default::default()
            })
        };

        // No dedicated network ⇒ the job runs on the default bridge and reaches the LAN. Reported,
        // not blocked: requiring it would turn every existing docker seat red on upgrade.
        let bare = checks::check_sandbox_egress_in(docker(None), |_| Ok(true));
        assert_eq!(bare.status, Status::Warn, "{}", bare.render());
        assert!(
            bare.detail.contains("LAN") && bare.render().contains("[sandbox] network"),
            "must name the exposure and the config key that fixes it: {}",
            bare.render()
        );
        assert!(
            !bare.render().contains("sandbox-net"),
            "there is no host-side command to run any more; a remedy naming one sends the operator \
             to a subcommand that does not exist: {}",
            bare.render()
        );

        // Network NAMED but absent. Note the direction: containment fails closed, so this is not "a
        // job escapes", it is "no job starts". The message must not read as an exposure.
        let missing = checks::check_sandbox_egress_in(docker(Some("sbx")), |_| Ok(false));
        assert_eq!(missing.status, Status::Warn, "{}", missing.render());
        assert!(
            missing.detail.contains("no such docker network") && missing.detail.contains("FAIL"),
            "must say jobs will fail, not that they run uncontained: {}",
            missing.detail
        );
        assert!(
            missing.render().contains("docker network create sbx"),
            "the remedy must name the network it wants created: {}",
            missing.render()
        );

        // Could not ask docker — distinct from "absent", and deliberately NOT a Fail. An instrument
        // limit rather than a measured unsafe state; failing it would make a doctor run red whenever
        // the daemon is merely asleep and teach operators to skip the row.
        let unreadable = checks::check_sandbox_egress_in(docker(Some("sbx")), |_| Err(()));
        assert_eq!(unreadable.status, Status::Warn, "{}", unreadable.render());
        assert!(
            unreadable.detail.contains("could not be asked") && unreadable.detail.contains("UNVERIFIED"),
            "must say it could not look, not that nothing is there: {}",
            unreadable.detail
        );

        // Network present ⇒ Pass, and the wording must stay a capability claim. Containment is
        // established by a LAUNCH; nothing this check can see proves a job is contained right now.
        let ready = checks::check_sandbox_egress_in(docker(Some("sbx")), |_| Ok(true));
        assert_eq!(ready.status, Status::Pass, "{}", ready.render());
        assert!(
            ready.detail.contains("network namespace") && ready.detail.contains("rules"),
            "a pass must name where the rules go and that there are some: {}",
            ready.detail
        );
        assert!(
            !ready.detail.contains("0 rules"),
            "an empty plan applies perfectly and contains nothing — it must never read as a pass: {}",
            ready.detail
        );

        // The probe is asked about the CONFIGURED network, not some other string. Without this the
        // check could report on a network the seat does not use.
        let asked = std::cell::RefCell::new(Vec::new());
        let _ = checks::check_sandbox_egress_in(docker(Some("sbx")), |name| {
            asked.borrow_mut().push(name.to_owned());
            Ok(true)
        });
        assert_eq!(asked.into_inner(), vec!["sbx".to_owned()]);

        // A host executor has no container to contain ⇒ no spurious warning, whatever docker says.
        // Without this the check would nag every launcher-mode seat forever.
        assert_eq!(
            checks::check_sandbox_egress_in(None, |_| Ok(false)).status,
            Status::Pass,
        );
        assert_eq!(
            checks::check_sandbox_egress_in(Some(contained_probe_launcher()), |_| Ok(false)).status,
            Status::Pass,
        );
        // ADVISORY, NEVER BLOCKING — asserted as its own property below, not left implied by the
        // severity map, because "does this refuse to boot" is the thing that was ruled on twice and
        // it is the thing a future edit is most likely to change by accident.
        //
        // History kept deliberately, because both rulings were reasoned and a reader deserves both.
        // This block originally asserted advisory ("turning a working docker seat red on upgrade is a
        // behaviour change, not a doctor's call"). It was escalated to Fail for every docker seat on
        // 2026-08-18, on the premise that a job is a stranger's code whether or not the seat chose its
        // counterparty — that premise STILL STANDS and is not withdrawn. It was returned to advisory
        // the same day because the rules were installed by a manual root command with no packaging
        // that survived a reboot: a gate cannot demand a condition the operator has no automated way
        // to hold. So the order was automate first, then require.
        //
        // ⚠ THAT PRECONDITION HAS NOW CHANGED, AND THE DECISION SHOULD BE REVISITED DELIBERATELY
        // RATHER THAN DRIFT: containment is established automatically at launch and needs no root
        // command and nothing that survives a reboot, so "the operator cannot hold this condition" no
        // longer applies. What remains is only upgrade compatibility — an existing seat with no
        // `[sandbox] network` would go red. Requiring it is a behaviour change and therefore petar's
        // call, not something to be smuggled in by editing a severity.
        //
        // The whole severity map stays in one place so the policy is readable without tracing three
        // branches.
        for (state, expected) in [
            (Ok(true), Status::Pass),
            (Ok(false), Status::Warn),
            (Err(()), Status::Warn),
        ] {
            let check = checks::check_sandbox_egress_in(docker(Some("sbx")), |_| state);
            assert_eq!(
                check.status, expected,
                "docker seat, network probe returned {state:?}",
            );
            assert_ne!(
                check.status,
                Status::Fail,
                "this check must not block boot while containment is opt-in — network probe \
                 returned {state:?}: {}",
                check.render(),
            );
        }
        // And the same property on the arm with no dedicated network at all, which is the state every
        // existing docker seat is in on upgrade.
        assert_ne!(
            checks::check_sandbox_egress_in(docker(None), |_| Ok(true)).status,
            Status::Fail,
            "an unconfigured docker seat must not be blocked from booting",
        );
    }

    // RED-PROVE (#792 phase 3): an absent docker sandbox image is flagged with the ACTIONABLE
    // `docker pull <ref>` command, not a raw failure — the operator can act without reading source.
    // A present image passes; a pullable one warns and still prints the pre-pull command.
    #[test]
    fn doctor_sandbox_image_flags_absence_with_pull_command() {
        use checks::ImageAvailability;
        let image = "ghcr.io/makeprisms/maxplayer-sandbox:v9.9.9";
        let pull = format!("docker pull {image}");

        let absent = checks::fold_sandbox_image(image, ImageAvailability::Absent);
        assert_eq!(absent.status, Status::Fail, "an absent image must FAIL: {}", absent.render());
        assert!(
            absent.render().contains(&pull),
            "an absent image must print the exact `{pull}` command: {}",
            absent.render()
        );

        let present = checks::fold_sandbox_image(image, ImageAvailability::Present);
        assert_eq!(present.status, Status::Pass, "a present image must PASS: {}", present.render());

        let pullable = checks::fold_sandbox_image(image, ImageAvailability::Pullable);
        assert_eq!(pullable.status, Status::Warn, "a pullable image WARNs: {}", pullable.render());
        assert!(
            pullable.render().contains(&pull),
            "a pullable image must still offer the pre-pull command: {}",
            pullable.render()
        );
    }

    // RED-PROVE (#796): the below-floor path must SPEAK. A version check whose failure mode is silence
    // is worse than no check at all, so this asserts the TEXT an exposed operator actually reads — the
    // version, the floor, the syscalls and the fix — never merely a status enum.
    #[test]
    fn doctor_engine_floor_below_the_floor_names_the_exposure_and_the_fix() {
        use checks::{EngineProbe, EngineVersion};
        let below =
            EngineProbe::Reported(EngineVersion::parse("24.0.9").expect("'24.0.9' must parse"));

        // An open-pool seat executes code posted by strangers, so the finding blocks boot — the same
        // exposure split check_home_permissions and check_sandbox_containment already apply.
        let open_pool = checks::fold_sandbox_engine_floor(&below, None, true);
        let rendered = open_pool.render();
        assert_eq!(
            open_pool.status,
            Status::Fail,
            "an open-pool seat below the floor must FAIL: {rendered}"
        );
        for needle in ["24.0.9", "25.0.0", "io_uring_setup", "BELOW"] {
            assert!(rendered.contains(needle), "below-floor text must name '{needle}': {rendered}");
        }
        assert!(
            rendered.contains("upgrade the Docker Engine to 25.0.0"),
            "below-floor text must carry the actionable fix, not just the finding: {rendered}"
        );

        // A targeted-only seat chose its counterparties, so the same finding is advisory there.
        let targeted = checks::fold_sandbox_engine_floor(&below, None, false);
        assert_eq!(
            targeted.status,
            Status::Warn,
            "a targeted seat below the floor WARNs: {}",
            targeted.render()
        );
    }

    // The comparison is `>=`, so the floor itself must PASS. An off-by-one here would flag every
    // correctly-upgraded seat and teach operators to ignore this check — the failure mode that makes a
    // real warning invisible later.
    #[test]
    fn doctor_engine_floor_passes_at_and_above_the_floor() {
        use checks::{EngineProbe, EngineVersion};
        for version in ["25.0.0", "25.0.3", "26.1.4", "28.0.1"] {
            let probe =
                EngineProbe::Reported(EngineVersion::parse(version).expect("version must parse"));
            let check = checks::fold_sandbox_engine_floor(&probe, None, true);
            assert_eq!(
                check.status,
                Status::Pass,
                "Engine {version} is at or above the floor and must PASS: {}",
                check.render()
            );
        }
        // The nearest version below the floor still fails, so the boundary is exact in both directions.
        let just_below =
            EngineProbe::Reported(EngineVersion::parse("24.9.9").expect("'24.9.9' must parse"));
        assert_eq!(
            checks::fold_sandbox_engine_floor(&just_below, None, true).status,
            Status::Fail,
            "24.9.9 is below 25.0.0 and must still FAIL an open-pool seat"
        );
    }

    // An unreadable version must produce a VISIBLE unknown, never a quiet Pass. A silent Pass would
    // make this check quietest exactly when it has learned least, and an operator whose daemon is down
    // would read a clean gate as "my Engine is fine". It still never BLOCKS: an unreachable daemon is
    // the launcher and image checks' verdict, not this one's.
    #[test]
    fn doctor_engine_floor_reports_an_unreadable_version_instead_of_passing_silently() {
        let unknown = checks::EngineProbe::Unreadable("docker daemon did not answer".to_owned());
        let check = checks::fold_sandbox_engine_floor(&unknown, None, true);
        let rendered = check.render();
        assert_eq!(
            check.status,
            Status::Warn,
            "an unknown Engine version must WARN, never Pass: {rendered}"
        );
        assert!(
            rendered.contains("could not read the Docker Engine version"),
            "the WARN must say the version is unknown: {rendered}"
        );
        assert!(
            rendered.contains("docker daemon did not answer"),
            "the WARN must carry WHY it is unknown, so the operator knows what to fix: {rendered}"
        );
    }

    // Under gVisor the OCI seccomp profile is not applied at all, so the Engine default governs
    // nothing on a runsc seat. A controlled comparison: the SAME below-floor probe that FAILs above,
    // with the runtime as the only variable — so this proves the RUNTIME changed the verdict, not the
    // version.
    #[test]
    fn doctor_engine_floor_is_moot_under_runsc() {
        use checks::{EngineProbe, EngineVersion};
        let below =
            EngineProbe::Reported(EngineVersion::parse("24.0.9").expect("'24.0.9' must parse"));
        assert_eq!(
            checks::fold_sandbox_engine_floor(&below, None, true).status,
            Status::Fail,
            "control: the same Engine FAILs an open-pool seat under the default runtime"
        );

        let under_gvisor = checks::fold_sandbox_engine_floor(&below, Some("runsc"), true);
        assert_eq!(
            under_gvisor.status,
            Status::Pass,
            "runsc ignores the OCI seccomp profile, so the Engine floor does not govern: {}",
            under_gvisor.render()
        );

        // Only gVisor replaces the syscall filter. Sysbox and friends still take the Engine's OCI
        // profile, so naming any non-default runtime must NOT clear the finding.
        assert_eq!(
            checks::fold_sandbox_engine_floor(&below, Some("sysbox-runc"), true).status,
            Status::Fail,
            "a non-gVisor runtime still applies the OCI profile and stays exposed"
        );
    }

    // `0.0.0` sorts BELOW the floor, so parsing an unrecognized string into it would report an
    // unreadable version as a confident "too old" — a wrong answer where the honest one is "unknown".
    #[test]
    fn doctor_engine_version_parse_keeps_unknown_distinct_from_zero() {
        use checks::EngineVersion;
        assert!(EngineVersion::parse("not-a-version").is_none(), "garbage must be unknown, not 0.0.0");
        assert!(EngineVersion::parse("").is_none(), "an empty report must be unknown");
        assert!(EngineVersion::parse("   ").is_none(), "a blank report must be unknown");

        // Vendor, distro and pre-release suffixes are common in the wild and must not defeat the
        // comparison: `24.0.7-ce` is still 24.0.7.
        assert_eq!(EngineVersion::parse("24.0.7-ce"), EngineVersion::parse("24.0.7"));
        assert_eq!(EngineVersion::parse("v25.0.3"), EngineVersion::parse("25.0.3"));
        assert_eq!(EngineVersion::parse("26.1.4+dfsg1"), EngineVersion::parse("26.1.4"));

        // A truncated report still orders correctly rather than collapsing to unknown.
        assert_eq!(EngineVersion::parse("25"), EngineVersion::parse("25.0.0"));
        assert!(EngineVersion::parse("25") > EngineVersion::parse("24.0.9"));
    }

    // RED-PROVE (wiring, #796): the Engine floor check must sit in the ONE registry that both
    // `maxplayer doctor` and the seller boot gate run — drop the `check_sandbox_engine_floor` push
    // from `build_checks` and this goes red. It asserts on the check's NAME, which is present under
    // every verdict (including "docker not resolvable"), so the test is deterministic on a box with no
    // docker rather than depending on whatever Engine this host happens to run.
    #[cfg(feature = "wallet")]
    #[test]
    fn sandbox_engine_floor_check_is_wired_into_the_boot_gate() {
        use maxplayer_core::home::{SandboxConfig, SandboxMode};
        let tmp = std::env::temp_dir().join(format!(
            "maxplayer-doctor-engine-796-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut home = resolve_doctor_home(Some(tmp.clone())).expect("bootstrap the home");
        home.config.sandbox = Some(SandboxConfig {
            mode: SandboxMode::Docker,
            launcher: Vec::new(),
            image: Some("maxplayer-sandbox:test".into()),
            forward_env: Vec::new(),
            runtime: None,
            // This check is about the engine floor, not egress: no dedicated network and no proxy
            // port range. Written out rather than `..Default::default()` so that adding another
            // sandbox field breaks this test and makes someone decide what it should be here.
            network: None,
            proxy_port_range: None,
            // Decision for this test, per the note above: none. It asserts the engine-version floor,
            // and a file-sourced credential is a containment concern that would only add a second
            // reason for the check to move.
            file_credentials: Vec::new(),
        });
        home.config.relay_url = "not-a-relay-url".into();
        home.config.accepted_mints = Vec::new();

        let results = run_checks(build_checks(&home, false));
        assert!(
            results.iter().any(|check| check.name == "sandbox engine floor"),
            "build_checks must run the sandbox engine floor check; got: {:?}",
            results.iter().map(Check::render).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    // A non-docker (or absent) [sandbox] has no image to check: a no-op Pass, never a spurious fault.
    #[test]
    fn doctor_sandbox_image_is_a_noop_without_docker_mode() {
        use maxplayer_core::home::{SandboxConfig, SandboxMode};
        assert_eq!(checks::check_sandbox_image(None).status, Status::Pass);
        let launcher = Some(SandboxConfig {
            mode: SandboxMode::Launcher,
            launcher: vec!["bwrap".into()],
            image: None,
            forward_env: Vec::new(),
            runtime: None,
            ..Default::default()
        });
        assert_eq!(checks::check_sandbox_image(launcher).status, Status::Pass);
    }

    // RED-PROVE (wiring): the sandbox IMAGE check must be part of the boot-gate registry, or a docker
    // seat boots with an unavailable image and stalls the first job. A docker seat with an image that
    // cannot exist locally or in the registry (bogus registry host) must surface a "sandbox image"
    // FAIL naming the pull command. Drop the `check_sandbox_image` push from `build_checks` → red.
    // Network-touch is bounded: the ref points at an unresolvable host, so `docker manifest inspect`
    // fails fast; the test is skipped where docker is not installed (nothing to probe).
    #[cfg(feature = "wallet")]
    #[test]
    fn sandbox_image_check_is_wired_into_the_boot_gate() {
        use maxplayer_core::home::{SandboxConfig, SandboxMode};
        if !checks::argv0_resolvable("docker") {
            return; // no docker on this host; the image probe is a no-op Pass — nothing to assert
        }
        let tmp = std::env::temp_dir().join(format!(
            "maxplayer-doctor-image-792-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut home = resolve_doctor_home(Some(tmp.clone())).expect("bootstrap the home");
        home.config.sandbox = Some(SandboxConfig {
            mode: SandboxMode::Docker,
            launcher: Vec::new(),
            image: Some("no-such-registry.invalid/nope:v0".into()),
            forward_env: Vec::new(),
            runtime: None,
            ..Default::default()
        });
        home.config.relay_url = "not-a-relay-url".into();
        home.config.accepted_mints = Vec::new();

        let results = run_checks(build_checks(&home, false));
        assert!(
            results.iter().any(|c| c.status == Status::Fail
                && c.detail.contains("no-such-registry.invalid/nope:v0")
                && c.render().contains("docker pull no-such-registry.invalid/nope:v0")),
            "build_checks must run the sandbox image check and FAIL with the pull command; got: {:?}",
            results.iter().map(Check::render).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&tmp).ok();
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
            mode: maxplayer_core::home::SandboxMode::Launcher,
            launcher: vec!["definitely-not-a-real-binary-xyz".into()],
            image: None,
            forward_env: Vec::new(),
            runtime: None,
            ..Default::default()
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
        let boot =
            seller_agents::resolve(&seller, &presets, maxplayer_core::agent_presets::AdapterHost::Host);
        assert!(boot.is_ok(), "boot resolves the verbatim agent_command");

        // Doctor must report the SAME verdict, not a FAIL derived from a PATH probe of the preset.
        let check = checks::check_agent_registry(
            Some(seller),
            presets,
            maxplayer_core::agent_presets::AdapterHost::Host,
        );
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

        let check = checks::check_agent_registry(
            Some(seller),
            BTreeMap::new(),
            maxplayer_core::agent_presets::AdapterHost::Host,
        );
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

        let boot =
            seller_agents::resolve(&seller, &presets, maxplayer_core::agent_presets::AdapterHost::Host);
        assert!(
            matches!(boot, Err(RegistryError::AllFailed(_))),
            "boot must refuse a registry with no launchable harness"
        );
        let check = checks::check_agent_registry(
            Some(seller),
            presets,
            maxplayer_core::agent_presets::AdapterHost::Host,
        );
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
        let none = checks::fold_mint_reachability(&[]);
        assert_eq!(none.status, Status::Fail);
        // #595 (mints-are-mints): the remedy hint names the default mint but must not fork classes
        // or say "real". RED-ON-REVERT: re-adding ", a REAL mint" reds this.
        let hint = none.render();
        assert!(
            !hint.contains("REAL") && !hint.to_lowercase().contains("real mint"),
            "mint remedy hint must not fork mint classes / say 'real': {hint}"
        );
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

    // ---- Issue #715: doctor inspects the configured harness's credential directory ----

    #[cfg(feature = "wallet")]
    fn seller_for_agents(agents: Vec<String>, agent_command: Vec<String>) -> maxplayer_core::home::SellerConfig {
        maxplayer_core::home::SellerConfig {
            agent_command,
            rate_sats: 5,
            git_remote: "https://example.invalid/repo".into(),
            job_timeout_secs: None,
            agents,
            claim_open_pool: false,
            accept_offers_only_from: Vec::new(),
            offer_backfill_secs: 0,
            contribution_enabled: true,
            slots: 1,
            claim_award_timeout_secs: None,
        }
    }

    #[cfg(feature = "wallet")]
    fn claude_preset_table() -> std::collections::BTreeMap<String, maxplayer_core::home::AgentPresetConfig> {
        let existing = std::env::current_exe()
            .expect("current exe")
            .to_string_lossy()
            .into_owned();
        let mut presets = std::collections::BTreeMap::new();
        presets.insert(
            "claude".to_owned(),
            maxplayer_core::home::AgentPresetConfig {
                argv: vec![existing],
            },
        );
        presets
    }

    /// RED-PROVE: a raw `--agent-argv` hatch must SAY it cannot resolve, and must not name a
    /// guessed path. Fall back to `~/.claude` (or sniff argv) and this goes red — the detail
    /// would contain `.claude` and the status might even Pass.
    #[cfg(feature = "wallet")]
    #[test]
    fn harness_credential_check_unresolvable_hatch_says_so_and_does_not_guess() {
        let existing = std::env::current_exe()
            .expect("current exe")
            .to_string_lossy()
            .into_owned();
        let seller = seller_for_agents(Vec::new(), vec![existing, "claude-agent-acp".into()]);
        let check = checks::check_harness_credential_permissions(
            Some(seller),
            std::collections::BTreeMap::new(),
            Some(std::path::PathBuf::from("/home/seat")),
        );
        assert_eq!(
            check.status,
            Status::Warn,
            "an unresolvable hatch must be a WARN, not a silent pass: {}",
            check.render()
        );
        let rendered = check.render();
        assert!(
            rendered.contains("cannot resolve") || rendered.contains("will not guess"),
            "must say it cannot resolve: {rendered}"
        );
        assert!(
            !rendered.contains(".claude"),
            "must not guess a default harness directory: {rendered}"
        );
        assert_ne!(
            check.status,
            Status::Fail,
            "unresolvable is advisory, not a boot-blocker: {rendered}"
        );
    }

    /// An unknown / custom label is the same case as the hatch: say so, never fall back.
    #[cfg(feature = "wallet")]
    #[test]
    fn harness_credential_check_unknown_label_does_not_fall_back_to_a_builtin() {
        let existing = std::env::current_exe()
            .expect("current exe")
            .to_string_lossy()
            .into_owned();
        let mut presets = std::collections::BTreeMap::new();
        presets.insert(
            "grok".to_owned(),
            maxplayer_core::home::AgentPresetConfig {
                argv: vec![existing],
            },
        );
        let seller = seller_for_agents(vec!["grok".into()], vec!["ignored".into()]);
        let check = checks::check_harness_credential_permissions(
            Some(seller),
            presets,
            Some(std::path::PathBuf::from("/home/seat")),
        );
        assert_eq!(check.status, Status::Warn, "{}", check.render());
        let rendered = check.render();
        assert!(rendered.contains("grok"), "must name the unresolvable harness: {rendered}");
        assert!(
            !rendered.contains(".claude") && !rendered.contains(".cursor") && !rendered.contains(".codex"),
            "must not fall back to a built-in directory: {rendered}"
        );
    }

    /// No [seller] / no HOME / registry refusal: each is a named cannot-resolve WARN, never a
    /// guessed path and never a silent skip.
    #[cfg(feature = "wallet")]
    #[test]
    fn harness_credential_check_carries_absence_rather_than_inventing_a_path() {
        let none = checks::check_harness_credential_permissions(
            None,
            std::collections::BTreeMap::new(),
            Some(std::path::PathBuf::from("/home/seat")),
        );
        assert_eq!(none.status, Status::Warn, "{}", none.render());
        assert!(none.detail.contains("cannot resolve"), "{}", none.render());

        let seller = seller_for_agents(vec!["claude".into()], vec!["ignored".into()]);
        let no_home = checks::check_harness_credential_permissions(
            Some(seller.clone()),
            claude_preset_table(),
            None,
        );
        assert_eq!(no_home.status, Status::Warn, "{}", no_home.render());
        assert!(
            no_home.detail.contains("HOME") && no_home.detail.contains("cannot resolve"),
            "{}",
            no_home.render()
        );

        let refused = checks::check_harness_credential_permissions(
            Some(seller_for_agents(
                vec!["ghostxyz-not-a-preset".into()],
                vec!["ignored".into()],
            )),
            std::collections::BTreeMap::new(),
            Some(std::path::PathBuf::from("/home/seat")),
        );
        assert_eq!(refused.status, Status::Warn, "{}", refused.render());
        assert!(
            refused.detail.contains("cannot inspect") || refused.detail.contains("did not resolve"),
            "{}",
            refused.render()
        );
    }

    // The mask is 0o022 (group/other WRITE), not 0o077 (any group/other access). A 0755 directory
    // is the typical umask leftover that is NOT the issue's finding; a 0775 directory and a 0664
    // settings.json are. RED-PROVE: switching the mask to 0o077 reds the 0755 Pass.
    #[cfg(all(unix, feature = "wallet"))]
    #[test]
    fn harness_credential_check_flags_group_write_not_group_read() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!(
            "mp-harness-creds-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let claude_dir = base.join(".claude");
        std::fs::create_dir_all(&claude_dir).expect("mk .claude");
        let seller = seller_for_agents(vec!["claude".into()], vec!["ignored".into()]);
        let presets = claude_preset_table();
        let run = |user_home: &std::path::PathBuf| {
            checks::check_harness_credential_permissions(
                Some(seller.clone()),
                presets.clone(),
                Some(user_home.clone()),
            )
        };

        // Owner-only directory, no settings.json ⇒ Pass.
        std::fs::set_permissions(&claude_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let tight = run(&base);
        assert_eq!(tight.status, Status::Pass, "owner-only dir must pass: {}", tight.render());
        assert!(tight.detail.contains(&claude_dir.display().to_string()), "{}", tight.render());

        // 0755 = group/other read+execute, not write. 0o077 would flag this; 0o022 must not.
        std::fs::set_permissions(&claude_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let readable = run(&base);
        assert_eq!(
            readable.status,
            Status::Pass,
            "group-readable (0755) is not the write-injection finding: {}",
            readable.render()
        );

        // 0775 = group-writable directory — the issue's measured dir mode.
        std::fs::set_permissions(&claude_dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        let writable = run(&base);
        assert_eq!(
            writable.status,
            Status::Warn,
            "a group-writable credential dir must WARN: {}",
            writable.render()
        );
        assert_ne!(writable.status, Status::Fail, "advisory, not an alarm: {}", writable.render());
        assert!(
            writable.detail.contains("inert") && writable.detail.contains("second account"),
            "copy must carry the single-account bound: {}",
            writable.render()
        );
        assert!(
            !writable.detail.to_lowercase().contains("compromis"),
            "must not imply the operator is currently compromised: {}",
            writable.render()
        );

        // Directory tight again; settings.json group-writable — the sharper half (steers the harness).
        std::fs::set_permissions(&claude_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let settings = claude_dir.join("settings.json");
        std::fs::write(&settings, "{}\n").unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o664)).unwrap();
        let steered = run(&base);
        assert_eq!(
            steered.status,
            Status::Warn,
            "group-writable settings.json must WARN: {}",
            steered.render()
        );
        assert!(
            steered.detail.contains("settings.json"),
            "must name the steering file: {}",
            steered.render()
        );

        // 0644 settings.json is group-readable, not writable — not the injection surface.
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o644)).unwrap();
        let readable_settings = run(&base);
        assert_eq!(
            readable_settings.status,
            Status::Pass,
            "group-readable settings.json is not the write finding: {}",
            readable_settings.render()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A metadata read error is a WARN, never a silent skip — same tooth as the home-perms check.
    /// RED-PROVE: `continue` on a missing dir and a 0755-passing mask would Pass this.
    #[cfg(all(unix, feature = "wallet"))]
    #[test]
    fn harness_credential_check_metadata_error_is_warn_not_silent_skip() {
        let base = std::env::temp_dir().join(format!(
            "mp-harness-creds-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("mk user home");
        // No `.claude` directory — metadata of the credential dir fails.
        let seller = seller_for_agents(vec!["claude".into()], vec!["ignored".into()]);
        let check = checks::check_harness_credential_permissions(
            Some(seller),
            claude_preset_table(),
            Some(base.clone()),
        );
        assert_eq!(
            check.status,
            Status::Warn,
            "a missing credential dir must WARN, not skip: {}",
            check.render()
        );
        assert!(
            check.detail.contains("could not read") && check.detail.contains(".claude"),
            "must name the path it failed to stat: {}",
            check.render()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // RED-PROVE (wiring): drop the `check_harness_credential_permissions` push from `build_checks`
    // and this goes red — no boot-gate result names the unlabelled hatch as unresolvable.
    #[cfg(feature = "wallet")]
    #[test]
    fn harness_credential_check_is_wired_into_the_boot_gate() {
        let tmp = std::env::temp_dir().join(format!(
            "maxplayer-doctor-harness-creds-715-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut home = resolve_doctor_home(Some(tmp.clone())).expect("bootstrap the home");
        let existing = std::env::current_exe()
            .expect("current exe")
            .to_string_lossy()
            .into_owned();
        home.config.seller = Some(seller_for_agents(Vec::new(), vec![existing]));
        home.config.relay_url = "not-a-relay-url".into();
        home.config.accepted_mints = Vec::new();

        let results = run_checks(build_checks(&home, false));
        assert!(
            results.iter().any(|c| {
                c.name == "harness credential permissions"
                    && c.status == Status::Warn
                    && (c.detail.contains("cannot resolve") || c.detail.contains("will not guess") || c.detail.contains("no preset label"))
            }),
            "build_checks must run the harness credential check and WARN on an unlabelled hatch; got: {:?}",
            results.iter().map(Check::render).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&tmp).ok();
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

    // ---- Issue #745: `maxplayer seller` refuses to start without a working nix ----

    #[cfg(feature = "wallet")]
    #[test]
    fn nix_check_passes_when_nix_runs_from_path() {
        let check = checks::fold_nix(true, None);
        assert_eq!(check.status, Status::Pass, "{}", check.render());
    }

    // Not installed anywhere ⇒ FAIL, and the ONE message carries BOTH remedies: the Determinate
    // one-liner AND the PATH-skew case by name (a systemd unit does not source a login shell, so
    // an installed nix can be absent from the service PATH) with the systemd fix — the gate
    // cannot always tell the two apart, so the operator must be able to.
    #[cfg(feature = "wallet")]
    #[test]
    fn nix_check_failure_carries_installer_one_liner_and_names_path_skew() {
        let check = checks::fold_nix(false, None);
        assert_eq!(check.status, Status::Fail, "no working nix must block boot: {}", check.render());
        let rendered = check.render();
        assert!(
            rendered
                .contains("curl -fsSL https://install.determinate.systems/nix | sh -s -- install"),
            "must carry the Determinate one-liner: {rendered}"
        );
        assert!(
            rendered.contains("login shell") && rendered.contains("systemd"),
            "must name the PATH-skew case: {rendered}"
        );
        assert!(
            rendered.contains("systemctl edit"),
            "must carry the systemd fix, not just name the skew: {rendered}"
        );
        assert!(
            !check.transient,
            "a missing nix is UNRECOVERABLE — refuse immediately, no retry, no sleep"
        );
        assert!(
            check.skip_doctor_exempt,
            "the nix check must survive --skip-doctor (#745: no escape hatch)"
        );
    }

    // The skew DETECTED: nix runs from a well-known install location but is off this process's
    // PATH. The gate must not misread the installed box as an uninstalled one — the message says
    // installed, names where, gives the systemd fix, and does not tell the operator to reinstall.
    #[cfg(feature = "wallet")]
    #[test]
    fn nix_check_reports_an_installed_off_path_nix_as_path_skew_not_uninstalled() {
        let check = checks::fold_nix(
            false,
            Some(std::path::PathBuf::from("/nix/var/nix/profiles/default/bin/nix")),
        );
        assert_eq!(check.status, Status::Fail, "{}", check.render());
        let rendered = check.render();
        assert!(
            rendered.contains("IS installed")
                && rendered.contains("/nix/var/nix/profiles/default/bin"),
            "must say nix is installed and name where: {rendered}"
        );
        assert!(
            rendered.contains("login shell") && rendered.contains("systemctl edit"),
            "must name the skew and carry the systemd fix: {rendered}"
        );
        // One message still carries BOTH remedies, but the reinstall is explicitly marked as the
        // wrong move for THIS box — never a bare "install nix" over an installed one.
        assert!(
            rendered.contains("do NOT reinstall"),
            "must not read as telling an installed box to reinstall: {rendered}"
        );
        assert!(
            rendered
                .contains("curl -fsSL https://install.determinate.systems/nix | sh -s -- install"),
            "the one message carries both remedies: {rendered}"
        );
        assert!(!check.transient && check.skip_doctor_exempt);
    }

    // RED-PROVE (wiring): nix is part of the ONE registry both `maxplayer doctor` and the boot
    // gate run — drop the `build_environment_checks` head from `build_checks` and this goes red.
    // The nix result's status is machine-dependent (the CI box has nix, a bare box does not), so
    // only presence is asserted. Network-free: relay_url is unparseable and no mints are configured.
    #[cfg(feature = "wallet")]
    #[test]
    fn nix_check_is_wired_into_the_shared_registry() {
        let tmp = std::env::temp_dir().join(format!(
            "maxplayer-doctor-nix-745-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut home = resolve_doctor_home(Some(tmp.clone())).expect("bootstrap the home");
        home.config.relay_url = "not-a-relay-url".into();
        home.config.accepted_mints = Vec::new();

        let results = run_checks(build_checks(&home, false));
        assert!(
            results.iter().any(|c| c.name == "nix"),
            "build_checks must carry the nix check; got: {:?}",
            results.iter().map(Check::render).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    // The registry `--skip-doctor` cannot narrow past: exactly the environment checks, i.e. nix.
    #[cfg(feature = "wallet")]
    #[test]
    fn skip_doctor_registry_is_exactly_the_environment_checks() {
        let results = run_checks(build_environment_checks());
        assert_eq!(
            results.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["nix"],
            "the --skip-doctor gate runs the environment subset of the one registry — only nix today"
        );
    }

    // #745's ruled-out bypass, pinned: the refusal for an environment failure must NOT advertise
    // --skip-doctor as a way past it (that flag does not bypass it), and must state the KIND
    // distinction so the asymmetry reads as design. Also unrecoverable: one attempt, no sleep.
    #[test]
    fn environment_failure_refusal_does_not_advertise_skip_doctor() {
        let run = drive_gate(READINESS_MAX_ATTEMPTS, |_| {
            vec![Check::fail_environment("nix", "no working nix", "install nix")]
        });
        assert_eq!(run.result, Err(()), "an environment failure must refuse");
        assert_eq!(run.attempts, 1, "unrecoverable: no retry");
        assert!(run.waits.is_empty(), "unrecoverable: no sleep");
        assert!(run.err.contains("REFUSING to start"), "{}", run.err);
        assert!(
            !run.err.contains("pass --skip-doctor"),
            "must not offer --skip-doctor past an environment failure: {}",
            run.err
        );
        assert!(
            run.err.contains("RIGHT NOW") && run.err.contains("EVER do the work"),
            "must state the readiness-vs-environment kind distinction: {}",
            run.err
        );
        assert!(
            run.err.contains("not bypassable"),
            "must say the check has no bypass: {}",
            run.err
        );
    }

    // Mixed refusal: --skip-doctor is offered for the readiness failure but explicitly NOT for the
    // environment one — never a blanket "pass --skip-doctor" that reads as covering everything.
    #[test]
    fn mixed_refusal_offers_skip_doctor_only_for_the_readiness_failures() {
        let run = drive_gate(READINESS_MAX_ATTEMPTS, |_| {
            vec![
                Check::fail("seller key", "key missing", "generate the key"),
                Check::fail_environment("nix", "no working nix", "install nix"),
            ]
        });
        assert_eq!(run.result, Err(()));
        assert_eq!(run.attempts, 1, "the unrecoverable pair refuses at once");
        assert!(
            run.err.contains("--skip-doctor can bypass the readiness failure(s)"),
            "the readiness failure keeps its documented bypass: {}",
            run.err
        );
        assert!(
            run.err.contains("but not nix"),
            "…and the environment failure is named as NOT covered by it: {}",
            run.err
        );
    }
}
