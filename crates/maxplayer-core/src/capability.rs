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
/// ⚠ **The wrap happens INSIDE this function, deliberately.** The execution environment is
/// operator-built and a launcher may put jobs in a different filesystem entirely, so a probe that ran
/// beside the seat process would answer for the wrong machine — the right predicate in the wrong
/// environment, which is #358's own shape one level down. Taking the policy (rather than a
/// pre-wrapped argv) is what makes bypassing the launcher unrepresentable at a call site.
///
/// Note this is NOT "never touch the host": under a pass-through policy `wrap` is the identity, and
/// then the host genuinely IS the job environment. The invariant is that the launcher is never
/// bypassed, and one implementation satisfies it under both configurations.
///
/// ⛔ **A DOCKER POLICY PROVES NOTHING HERE, AND THAT IS DELIBERATE.**
/// [`crate::seller_exec::SandboxPolicy::wrap`] returns `None` under docker, because a docker launch
/// is not expressible as a bare host argv — it needs the per-job mount, uid and env. The ONLY safe
/// reading of that `None` is "not proven": running the bare command instead would execute it on the
/// HOST while jobs run inside a container, advertising a capability the job will not have. That is
/// the precise mistake this function was written to make unrepresentable, so the fallback is refused
/// rather than taken.
///
/// The cost is real and is the honest one: a docker seat currently advertises NO capabilities, even
/// when its image has all three. Absent beats wrong — an unstated capability loses a match, a false
/// one attracts a job the seat cannot do. Proving them under docker needs a throwaway workdir and a
/// [`crate::seller_exec::SandboxPolicy::launch`] the way the harness probe already builds one; that
/// is a separate change, not a fallback to be improvised here.
///
/// `resolves` runs one already-wrapped argv and reports whether it succeeded, injected so the
/// decision is testable without spawning anything.
///
/// Gated on `wallet` because it takes a [`crate::seller_exec::SandboxPolicy`], and `seller_exec` is
/// itself `wallet`-gated. The VOCABULARY above is deliberately not gated: a build that emits or reads
/// a filterable field must be able to name the token set that field is bound to, and only the
/// *probing* needs an executor.
#[cfg(feature = "wallet")]
pub fn probe_capabilities(
    policy: &crate::seller_exec::SandboxPolicy,
    resolves: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    CAPABILITIES
        .iter()
        .copied()
        .filter(|token| {
            // A token with no probe command cannot be proven, so it is never advertised. Unreachable
            // while the vocabulary and the command table agree — and a test holds them to that —
            // but fail-closed, because the failure of the other direction is an unprovable claim on
            // a field buyers commit sats against.
            let Some(argv) = capability_probe_command(token) else {
                return false;
            };
            let argv: Vec<String> = argv.iter().map(|part| (*part).to_owned()).collect();
            // `None` ⇒ this policy has no host argv (docker). Not proven. Never a bare-argv fallback.
            let Some(wrapped) = policy.wrap(&argv) else {
                return false;
            };
            resolves(&wrapped)
        })
        .map(str::to_owned)
        .collect()
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

    #[test]
    fn the_probe_runs_through_the_sandbox_launcher() {
        // The load-bearing property: under a wrapped policy every probe argv must be prefixed by the
        // launcher. A probe that skipped it would answer for the seat host while jobs run somewhere
        // else, and advertise a capability the job will not have.
        let policy = crate::seller_exec::SandboxPolicy::wrapped(vec![
            "bwrap".to_owned(),
            "--ro-bind".to_owned(),
        ]);
        let seen = std::cell::RefCell::new(Vec::new());
        let proven = probe_capabilities(&policy, |argv| {
            seen.borrow_mut().push(argv.to_vec());
            true
        });

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
        let seen = std::cell::RefCell::new(Vec::new());
        probe_capabilities(&crate::seller_exec::SandboxPolicy::passthrough(), |argv| {
            seen.borrow_mut().push(argv.to_vec());
            false
        });
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
    fn a_docker_policy_proves_nothing_and_never_falls_back_to_the_host() {
        // #221 made docker a real executor, and `wrap` returns None for it. The dangerous reading of
        // that None is "no wrapper needed, run it bare" — which would probe the HOST while jobs run
        // in a container: a capability proven in the wrong environment, on a field buyers commit
        // sats against. This asserts the safe reading, and asserts it by watching whether ANYTHING
        // was spawned at all, not merely by checking the returned list is empty.
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
        let spawned = std::cell::RefCell::new(0usize);
        let proven = probe_capabilities(&policy, |_| {
            *spawned.borrow_mut() += 1;
            true
        });

        assert_eq!(
            spawned.into_inner(),
            0,
            "a docker policy must spawn NOTHING on the host — a bare-argv fallback would probe the \
             wrong environment even when its result looks plausible"
        );
        assert!(proven.is_empty(), "unproven means unadvertised: {proven:?}");
    }

    #[test]
    fn only_the_tokens_that_actually_resolved_are_advertised() {
        // The direction that decides whether this field is honest: a token whose command did not
        // resolve must not appear. Asserted per-token, so a probe that returned everything or
        // nothing cannot pass by accident.
        let proven = probe_capabilities(
            &crate::seller_exec::SandboxPolicy::passthrough(),
            |argv| argv.first().is_some_and(|program| program == "node"),
        );
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
        let nothing_resolves = probe_capabilities(
            &crate::seller_exec::SandboxPolicy::passthrough(),
            |_| false,
        );
        assert_eq!(
            nothing_resolves.len(),
            0,
            "a seat with no toolchain must advertise no capabilities: {nothing_resolves:?}"
        );

        let everything_resolves =
            probe_capabilities(&crate::seller_exec::SandboxPolicy::passthrough(), |_| true);
        assert_eq!(
            everything_resolves,
            vec!["node", "python", "rust"],
            "POSITIVE CONTROL: the same probe must return the SPECIFIC tokens when they DO resolve. \
             A count alone would not separate a working probe from one returning the wrong set, and \
             without this direction the zero above is also exactly what a probe that never ran prints"
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
