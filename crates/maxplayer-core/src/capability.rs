//! The seat CAPABILITY vocabulary: what a seat can actually run (#784).
//!
//! A buyer may FILTER on capability, so this is a closed vocabulary rather than free text. The rule
//! the whole design rests on: every field a buyer can filter on is machine-sourced or enum-bound,
//! and normalisation happens at the SOURCE. Read-time fuzzy matching is the failure this module
//! exists to prevent — if `python3` and `python` can both reach the wire, then a buyer asking for
//! one silently misses seats advertising the other, and no amount of read-side cleverness is ever
//! complete.
//!
//! So the tokens are canonical BY PROVENANCE, not by a normalising function: the only thing that
//! emits them is [`probe_capabilities`], which yields entries of [`CAPABILITIES`] itself. There is
//! nothing to canonicalise because there is no path by which a hand-typed spelling can become an
//! advertised token — no config key, no operator string, no read-time rewriting.
//!
//! That also settles the READER side without any code: a buyer's requested token is compared
//! exactly, so an out-of-vocabulary spelling on the wire simply never matches. Out-of-enum is
//! unmatchable by construction, which is fail-closed and needs no normaliser to enforce.
//!
//! ⚠ WHAT A PROVEN TOKEN MEANS, AND WHAT IT DOES NOT. A token proves exactly one thing: the token's
//! probe binary RESOLVED and exited 0 inside the job execution policy, at probe time. It is a
//! statement about BINARY PRESENCE and nothing more. It does NOT prove that a build or run using that
//! binary will succeed (`cargo` resolving is necessary, not sufficient); it does NOT prove credential
//! forwarding, since the probe carries none; and it does NOT prove network reachability or job-network
//! parity — [`crate::seller_exec::probe_launch_argv`] renders with NO job netns, so the probe says
//! nothing about what a job's network can reach. A buyer filtering on a token is selecting seats where
//! the tool is present, not seats guaranteed to complete the work; that guarantee only ever comes from
//! probe-and-delivery, never from the advertisement.
//!
//! ⚠ THE TOKEN LIST IS NOT YET RATIFIED. These three are the ones issue #784 names verbatim
//! (`rust`, `node`, `python`). The spec (`docs/protocol-v1.md`) is the authority and does not carry
//! a capability vocabulary yet. Adding a token is a one-line change HERE plus a spec update — which
//! is the intended way new tokens land, per #784 ("new tokens land by a spec update"). Deliberately
//! minimal: a token invented here and shipped to the wire is far harder to withdraw than one added
//! later.

/// Every capability token a seat may advertise, sorted. A buyer's filter and a seller's emitter
/// share this one list, so a token that cannot be emitted also cannot be asked for.
pub const CAPABILITIES: [&str; 3] = ["node", "python", "rust"];

/// The command that PROVES a token, or `None` for a token outside the vocabulary.
///
/// **The probe command IS the token's definition.** Ratifying `rust` ratified "`cargo` resolves";
/// there is no separate prose meaning to drift from. A new token ships WITH its probe command or it
/// means nothing, which is why these live beside [`CAPABILITIES`] rather than in a config file.
///
/// `--version` rather than a shell `command -v`: it needs no shell in the execution environment (the
/// operator builds that image, and assuming `sh` is another capability we would be asserting without
/// probing), and a binary that is absent fails to spawn at all — the exact question being asked.
///
/// `python` probes `python3` specifically. A bare `python` still resolves to python2 on some images,
/// and advertising `python` for an interpreter that cannot run the code a buyer sends is the
/// unfalsifiable-claim failure this whole field was redesigned to avoid.
pub fn capability_probe_command(token: &str) -> Option<[&'static str; 2]> {
    match token {
        "node" => Some(["node", "--version"]),
        "python" => Some(["python3", "--version"]),
        "rust" => Some(["cargo", "--version"]),
        _ => None,
    }
}

/// The capabilities this seat can PROVE, by running each token's probe command in the JOB execution
/// environment.
///
/// ⚠ **The render happens INSIDE this function, deliberately.** The execution environment is
/// operator-built and a launcher may put jobs in a different filesystem entirely, so a probe that ran
/// beside the seat process would answer for the wrong machine — the right predicate in the wrong
/// environment, which is #358's own shape one level down. Taking the policy (rather than a
/// pre-rendered argv) is what makes bypassing the launcher unrepresentable at a call site.
///
/// Note this is NOT "never touch the host": under a pass-through policy the render is the identity,
/// and then the host genuinely IS the job environment. The invariant is that the launcher is never
/// bypassed, and one implementation satisfies it under every configuration.
///
/// **A DOCKER SEAT IS PROVEN THE SAME WAY EVERY OTHER SEAT IS**, because rendering goes through
/// [`crate::seller_exec::probe_launch_argv`], which is built on
/// [`crate::seller_exec::SandboxPolicy::launch`] and is therefore total over all three executors.
///
/// The alternative — [`crate::seller_exec::SandboxPolicy::wrap`] — cannot do this job. It yields no
/// argv under docker, because a container launch is not expressible as a bare host argv: it needs the
/// per-job mount, uid and env. A probe built on `wrap` reaches only host seats, and the ONLY safe
/// reading of its absence is "not proven", because running the bare command instead would execute it
/// on the HOST while jobs run inside the container, advertising a capability the job will not have.
/// So the choice is not between `wrap` and a fallback; it is between `launch` and advertising nothing
/// on exactly the seats containment is for.
///
/// `workdir` must be an existing throwaway directory, and the caller owns creating and removing it —
/// the same contract [`crate::seller_exec::probe_launch_argv`] states, kept here rather than hidden
/// so a probe never quietly invents a directory a real job would not get. A missing one is REFUSED
/// rather than passed through: docker would create the bind source as root, and the container,
/// running as the job's uid, could then not write its own workdir. That failure is SILENT for
/// `--version` probes, which write nothing and still exit 0 — the probe would answer correctly while
/// standing somewhere no job ever stands.
///
/// A render failure is "not proven", never a fallback. It is the same fail-closed reading as a token
/// with no probe command: an unstated capability loses a match, a false one attracts a job the seat
/// cannot do, and this field is one buyers commit sats against.
///
/// `resolves` runs one already-rendered argv and reports whether it succeeded, injected so the
/// decision is testable without spawning anything.
///
/// Gated on `wallet` because it takes a [`crate::seller_exec::SandboxPolicy`], and `seller_exec` is
/// itself `wallet`-gated. The VOCABULARY above is deliberately not gated: a build that emits or reads
/// a filterable field must be able to name the token set that field is bound to, and only the
/// *probing* needs an executor.
/// A capability could not be MEASURED, so boot has no honest answer and must refuse to advertise
/// rather than publish a shorter set (#784).
///
/// This is the fail-closed direction that separates "checked, and the binary is not here" (an ordinary
/// omitted token) from "could not check at all". Only the latter reaches this type. A buyer commits
/// sats on this field, so an unmeasured capability must never be indistinguishable from an absent one.
#[cfg(feature = "wallet")]
#[derive(Debug)]
pub enum CapabilityProbeError {
    /// The probe argv could not be rendered for the job environment (missing workdir, launcher
    /// misconfiguration). The measurement never started.
    Render(crate::seller_exec::ExecError),
    /// The probe was rendered but could not be run to a verdict (launcher unspawnable, timeout).
    Run(crate::seller_exec::ProbeRunError),
}

#[cfg(feature = "wallet")]
impl std::fmt::Display for CapabilityProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Render(error) => write!(f, "capability probe could not be rendered: {error}"),
            Self::Run(error) => write!(f, "{error}"),
        }
    }
}

#[cfg(feature = "wallet")]
impl std::error::Error for CapabilityProbeError {}

/// The capabilities this seat can PROVE, by running each token's probe command in the JOB execution
/// environment. Returns the proven set, or refuses if any token could not be measured.
///
/// `resolves` runs one already-rendered argv and reports its outcome:
/// - `Ok(true)` proven, `Ok(false)` ran and not present (omit the token),
/// - `Err(_)` could not be measured — which aborts the whole probe with [`CapabilityProbeError::Run`].
///
/// A render failure is likewise fatal ([`CapabilityProbeError::Render`]), never a bare-argv fallback:
/// running the bare command would execute it on the HOST while jobs run in the container, advertising a
/// capability the job will not have. The only two honest outcomes for a token are "proven" and "ran and
/// absent"; everything else fails boot.
#[cfg(feature = "wallet")]
pub fn probe_capabilities(
    policy: &crate::seller_exec::SandboxPolicy,
    workdir: &std::path::Path,
    resolves: impl Fn(&[String]) -> Result<bool, crate::seller_exec::ProbeRunError>,
) -> Result<Vec<String>, CapabilityProbeError> {
    let mut proven = Vec::new();
    for token in CAPABILITIES {
        // A token with no probe command cannot be proven, so it is never advertised. Unreachable while
        // the vocabulary and the command table agree — and a test holds them to that — but fail-closed.
        let Some(argv) = capability_probe_command(token) else {
            continue;
        };
        let argv: Vec<String> = argv.iter().map(|part| (*part).to_owned()).collect();
        let rendered = crate::seller_exec::probe_launch_argv(policy, &argv, workdir)
            .map_err(CapabilityProbeError::Render)?;
        if resolves(&rendered).map_err(CapabilityProbeError::Run)? {
            proven.push(token.to_owned());
        }
    }
    Ok(proven)
}

/// Probe this seat's capabilities for real: render through `policy`, run each in `workdir` under the
/// standard wall-clock bound, and force-remove any docker probe container afterward. The production
/// entry point [`crate::seller_node`] boot calls; the injectable [`probe_capabilities`] is what tests
/// drive.
#[cfg(feature = "wallet")]
pub fn probe_seat_capabilities(
    policy: &crate::seller_exec::SandboxPolicy,
    workdir: &std::path::Path,
) -> Result<Vec<String>, CapabilityProbeError> {
    let container = crate::seller_exec::probe_container_name(policy, workdir);
    probe_capabilities(policy, workdir, |argv| {
        crate::seller_exec::probe_command_outcome(
            argv,
            workdir,
            crate::seller_exec::CAPABILITY_PROBE_TIMEOUT,
            container.as_deref(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_in_the_vocabulary_has_a_probe_command() {
        // The vocabulary and the command table are two lists that must agree. A token added to one
        // and not the other would be silently unadvertisable — it would simply never appear, with no
        // error anywhere. Enumerated from CAPABILITIES rather than hand-listed, so adding a token
        // fails this test instead of quietly passing it.
        assert_eq!(CAPABILITIES.len(), 3, "denominator for the loop below");
        for token in CAPABILITIES {
            assert!(
                capability_probe_command(token).is_some(),
                "vocabulary token {token:?} has no probe command, so it can never be proven"
            );
        }
        assert_eq!(capability_probe_command("not-a-token"), None);
    }

    /// The probing tests, gated exactly as [`probe_capabilities`] is.
    ///
    /// ⚠ TWO OPPOSING CONTROLS, and one row alone cannot tell a working gate from a test deleted
    /// everywhere: without `wallet` these must be ABSENT (the function they call does not exist), and
    /// with `wallet` they must be PRESENT. The vocabulary tests OUTSIDE this module are the other
    /// row — they run in both configurations, so a default build still proves the token set and its
    /// probe-command table agree.
    #[cfg(feature = "wallet")]
    mod probing {
        use super::*;

    /// A REAL throwaway directory, because `probe_launch_argv` refuses one that does not exist — and
    /// that refusal is the point, so these tests must satisfy it rather than route around it.
    struct ProbeDir(std::path::PathBuf);

    impl ProbeDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "maxplayer-capability-probe-{label}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create the probe workdir");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ProbeDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_probe_runs_through_the_sandbox_launcher() {
        // The load-bearing property: under a wrapped policy every probe argv must be prefixed by the
        // launcher. A probe that skipped it would answer for the seat host while jobs run somewhere
        // else, and advertise a capability the job will not have.
        let policy = crate::seller_exec::SandboxPolicy::wrapped(vec![
            "bwrap".to_owned(),
            "--ro-bind".to_owned(),
        ]);
        let dir = ProbeDir::new("launcher");
        let seen = std::cell::RefCell::new(Vec::new());
        let proven = probe_capabilities(&policy, dir.path(), |argv| {
            seen.borrow_mut().push(argv.to_vec());
            Ok(true)
        })
        .expect("an injected probe that always resolves cannot fail to be measured");

        let seen = seen.into_inner();
        assert_eq!(seen.len(), 3, "one spawn per token: {seen:?}");
        for argv in &seen {
            assert_eq!(
                &argv[..2],
                &["bwrap".to_owned(), "--ro-bind".to_owned()],
                "a probe that bypasses the launcher answers for the wrong environment: {argv:?}"
            );
        }
        assert_eq!(proven, vec!["node", "python", "rust"]);
    }

    #[test]
    fn a_pass_through_policy_probes_the_bare_command() {
        // Under pass-through the host IS the job environment, so the bare command is correct — the
        // rule is "never bypass the launcher", not "never touch the host".
        let dir = ProbeDir::new("passthrough");
        let seen = std::cell::RefCell::new(Vec::new());
        probe_capabilities(&crate::seller_exec::SandboxPolicy::passthrough(), dir.path(), |argv| {
            seen.borrow_mut().push(argv.to_vec());
            Ok(false)
        })
        .expect("an injected probe that always resolves-false cannot fail to be measured");
        assert_eq!(
            seen.into_inner(),
            vec![
                vec!["node".to_owned(), "--version".to_owned()],
                vec!["python3".to_owned(), "--version".to_owned()],
                vec!["cargo".to_owned(), "--version".to_owned()],
            ]
        );
    }

    #[test]
    fn a_docker_policy_probes_inside_the_container_and_never_bare_on_the_host() {
        // #221 made docker a real executor, and the seat this whole field exists for is a contained
        // one. Rendering through `probe_launch_argv` reaches it; the `wrap` this once used could not,
        // and a docker seat advertised NOTHING as a result.
        //
        // ⚠ THE DANGEROUS OUTCOME IS NOT "nothing" — IT IS THE BARE COMMAND. Probing `node
        // --version` on the host while jobs run in a container proves a capability in the wrong
        // environment, on a field buyers commit sats against, and it looks entirely plausible when it
        // does it. So this asserts the argv is a CONTAINER launch, not merely that something ran:
        // a count alone cannot tell a container probe from a host one.
        //
        // Built through `from_config`, the production path, so this exercises the policy a real
        // docker seat resolves rather than one assembled for the test.
        // Names the two fields this fixture cares about and SPREADS the rest. `SandboxConfig` derives
        // `Default` for exactly this (`home.rs`), and an exhaustive literal here breaks on every field
        // that struct gains — it has gained three (`network`, `proxy_port_range`, `file_credentials`)
        // in the time this test existed, each an `E0063` in a `#[cfg(test)]` block that needs BOTH
        // `--all-targets` and `wallet` to surface at all.
        let policy = crate::seller_exec::SandboxPolicy::from_config(Some(
            &crate::home::SandboxConfig {
                mode: crate::home::SandboxMode::Docker,
                image: Some("maxplayer-sandbox:v1".to_owned()),
                ..Default::default()
            },
        ))
        .expect("a docker config naming an image resolves");
        assert!(policy.docker_image().is_some(), "the fixture must really be a docker policy");

        let dir = ProbeDir::new("docker");
        let seen = std::cell::RefCell::new(Vec::new());
        let proven = probe_capabilities(&policy, dir.path(), |argv| {
            seen.borrow_mut().push(argv.to_vec());
            Ok(true)
        })
        .expect("an injected probe that always resolves cannot fail to be measured");

        let seen = seen.into_inner();
        assert_eq!(seen.len(), 3, "one render per token on a docker seat: {seen:?}");
        for argv in &seen {
            assert_eq!(
                argv.first().map(String::as_str),
                Some("docker"),
                "a docker seat must be probed THROUGH docker; a bare argv here would prove the \
                 host's capability and advertise it as the container's: {argv:?}"
            );
            assert!(
                argv.iter().any(|part| part == "maxplayer-sandbox:v1"),
                "the probe must run in the seat's OWN image, not merely in some container: {argv:?}"
            );
        }
        // The regression this replaces: a docker seat used to advertise nothing at all.
        assert_eq!(
            proven,
            vec!["node", "python", "rust"],
            "a contained seat whose image has the toolchain must now be able to prove it"
        );
    }

    #[test]
    fn only_the_tokens_that_actually_resolved_are_advertised() {
        // The direction that decides whether this field is honest: a token whose command did not
        // resolve must not appear. Asserted per-token, so a probe that returned everything or
        // nothing cannot pass by accident.
        let dir = ProbeDir::new("only-resolved");
        let proven = probe_capabilities(
            &crate::seller_exec::SandboxPolicy::passthrough(),
            dir.path(),
            |argv| Ok(argv.first().is_some_and(|program| program == "node")),
        )
        .expect("an injected probe cannot fail to be measured");
        assert_eq!(proven, vec!["node"]);
    }

    #[test]
    fn a_stock_image_with_no_toolchain_advertises_nothing() {
        // #358's shipped runtime installs ca-certificates and tini and copies one binary — no cargo,
        // no node, no python. A seat there must advertise NO capabilities.
        //
        // ⚠ THE CONTROL THAT MAKES THIS MEAN ANYTHING is the assertion below it. Before the probe
        // existed, `capabilities` was populated by nothing, so `count == 0` passed because the
        // FEATURE was absent — a test green for the wrong reason. Asserting the same probe returns
        // all three when the environment DOES have them is what separates "correctly empty" from
        // "not wired up".
        let dir = ProbeDir::new("stock-image");
        let nothing_resolves = probe_capabilities(
            &crate::seller_exec::SandboxPolicy::passthrough(),
            dir.path(),
            |_| Ok(false),
        )
        .expect("an injected probe cannot fail to be measured");
        assert_eq!(
            nothing_resolves.len(),
            0,
            "a seat with no toolchain must advertise no capabilities: {nothing_resolves:?}"
        );

        let everything_resolves = probe_capabilities(
            &crate::seller_exec::SandboxPolicy::passthrough(),
            dir.path(),
            |_| Ok(true),
        )
        .expect("an injected probe cannot fail to be measured");
        assert_eq!(
            everything_resolves,
            vec!["node", "python", "rust"],
            "POSITIVE CONTROL: the same probe must return the SPECIFIC tokens when they DO resolve. \
             A count alone would not separate a working probe from one returning the wrong set, and \
             without this direction the zero above is also exactly what a probe that never ran prints"
        );
    }

    // Point ③ of #784's required shape: an UNMEASURABLE probe fails closed. A command that ran and was
    // absent omits its token (tested above); a probe that could not be RUN AT ALL must abort the whole
    // set with an error, never quietly shorten it — a buyer commits sats on this field, so "could not
    // check" must not read as "checked, and no".
    #[test]
    fn a_probe_that_cannot_be_run_fails_the_whole_set() {
        let dir = ProbeDir::new("unmeasurable");
        let result = probe_capabilities(
            &crate::seller_exec::SandboxPolicy::passthrough(),
            dir.path(),
            |_| Err(crate::seller_exec::ProbeRunError::TimedOut { after: std::time::Duration::from_secs(1) }),
        );
        assert!(
            matches!(result, Err(CapabilityProbeError::Run(_))),
            "a probe that timed out must fail the set, not return a silently shorter one: {result:?}"
        );
    }

    // The other unmeasurable direction: a RENDER that fails is fatal too, never a bare-host fallback.
    // A docker policy handed a workdir that does not exist cannot render a probe launch, and running
    // the bare command instead would prove the HOST's capability and advertise it as the container's.
    #[test]
    fn a_probe_that_cannot_be_rendered_fails_the_whole_set() {
        let policy = crate::seller_exec::SandboxPolicy::from_config(Some(&crate::home::SandboxConfig {
            mode: crate::home::SandboxMode::Docker,
            image: Some("maxplayer-sandbox:v1".to_owned()),
            ..Default::default()
        }))
        .expect("a docker config naming an image resolves");
        let missing = std::path::Path::new("/maxplayer/no/such/probe/workdir");

        let result = probe_capabilities(&policy, missing, |_| Ok(true));
        assert!(
            matches!(result, Err(CapabilityProbeError::Render(_))),
            "a render failure must fail the set, never fall back to the bare host command: {result:?}"
        );
    }
    }

    #[test]
    fn the_vocabulary_is_sorted_and_free_of_duplicates() {
        // Sorted so the list reads as a set rather than an accretion order, and duplicate-free so a
        // token cannot be probed twice. Asserted rather than assumed
        // because both properties are invisible at the point where someone appends a token.
        let mut sorted = CAPABILITIES.to_vec();
        sorted.sort_unstable();
        assert_eq!(CAPABILITIES.to_vec(), sorted, "CAPABILITIES must stay sorted");
        sorted.dedup();
        assert_eq!(sorted.len(), CAPABILITIES.len(), "CAPABILITIES must be duplicate-free");
    }

}
