//! Live containment tests: these run real containers and ask a real kernel what it holds.
//!
//! `#[ignore]`d because they need a docker daemon and the two first-party images, so they are not
//! part of an ordinary `cargo test` run. Run them with:
//!
//! ```text
//! docker build -t mx-netfilter:live docker/maxplayer-netfilter
//! MAXPLAYER_NETFILTER_IMAGE=mx-netfilter:live \
//!   cargo test -p maxplayer-core --features acp,wallet --test sandbox_netns_live -- --ignored
//! ```
//!
//! **Why these exist at all.** Every other test in this crate asserts what is *rendered*. That is
//! exactly the gap a measured, total failure went through: the policy rendered correctly and could not
//! be applied, because a `--log-prefix` containing a space arrives at iptables as two arguments. Rule
//! 1 of 24 was refused, the namespace ended up with no rules, and 1069 unit tests stayed green. A test
//! that never executes a plan cannot see that class of bug.
//!
//! They are also the only red-prove of the readback. A verifier that always returns `Ok` would pass
//! every unit test written against a captured fixture, so each case here breaks containment in a
//! specific way against a live namespace and requires the refusal to name it.

// Gated on BOTH features, because this file needs `establish` (behind `acp`) and `SandboxPolicy`
// (behind `wallet`). Gating on `acp` alone breaks the acp-only CI row, which has no `wallet`.
//
// The row that runs it is "the full shipped feature combo (acp + wallet)" — added deliberately,
// because `acp` and `wallet` are never both on in any other `cargo test` here and a test gated on both
// would otherwise be compiled out everywhere. As ci.yml puts it: a compiled-out test and a passing
// test produce the same green. Verify membership with `cargo test … -- --list`, never by a green tick.
#![cfg(all(feature = "acp", feature = "wallet"))]

use std::process::Command;

use maxplayer_core::sandbox_iface::{
    filter_readback_argv, iface_sidecar_argv, link_probe_argv, parse_links, select_egress_link,
    IfacePlan,
};
use maxplayer_core::sandbox_net::{Family, NetPolicy, PortRange};
use maxplayer_core::sandbox_netns::{plan_stdin, readback_argv};

/// The netfilter image to exercise. Deliberately required rather than defaulted: a default would let
/// this test silently measure a stale image, and its whole purpose is to measure the real one.
fn netfilter_image() -> String {
    std::env::var("MAXPLAYER_NETFILTER_IMAGE").expect(
        "set MAXPLAYER_NETFILTER_IMAGE (e.g. `docker build -t mx-netfilter:live \
         docker/maxplayer-netfilter`) — this test refuses to guess which image it is verifying",
    )
}

/// The holder only has to own a namespace and do nothing, so any small image serves. It is not the
/// subject of these tests.
fn holder_image() -> String {
    std::env::var("MAXPLAYER_HOLDER_IMAGE").unwrap_or_else(|_| "alpine".to_owned())
}

fn docker(args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("docker")
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker must be on PATH for a live containment test");
    if let Some(text) = stdin {
        child
            .stdin
            .as_mut()
            .expect("piped")
            .write_all(text.as_bytes())
            .expect("write plan");
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().expect("wait");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    )
}

// ── Fixture ownership ─────────────────────────────────────────────────────────────────────────
//
// Every resource these tests create is unique to this run and carries a label saying so, and
// nothing is ever removed unless that label is read back off it first.
//
// The rule exists because the alternative was in this file: fixtures named deterministically
// (`mx-runsc-net`, `mx-reap-idle`, …) and a `docker rm --force` of those names at setup, to clear
// whatever a previous run had left. That start-by-deleting step is indistinguishable from deleting
// somebody else's container — a concurrent run of this same suite, or an operator's box where the
// name happens to be taken — and it destroys the evidence of the leak it is papering over. A unique
// name needs no pre-delete, and an ownership check makes teardown provably ours.

/// The label every fixture resource carries, with this run's token as its value.
const FIXTURE_OWNER_LABEL: &str = "ai.maxplayer.live-fixture-owner";

/// This run's ownership token: pid plus process start-unique nanoseconds, so two concurrent runs on
/// one host — and a rerun after a crash — never share it.
fn owner_token() -> &'static str {
    static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 + d.as_secs().wrapping_mul(1_000_000_000))
            .unwrap_or(0);
        format!("{}-{nanos:x}", std::process::id())
    })
}

/// A resource name this run owns and nothing else can be using.
fn owned_name(kind: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("mx-live-{kind}-{}-{}", owner_token(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The `--label` argument that stamps this run's ownership.
fn owner_label() -> String {
    format!("{FIXTURE_OWNER_LABEL}={}", owner_token())
}

/// Whether a resource carries **this run's** ownership token. Read off the daemon, never assumed
/// from the name: the name is what a collision would reproduce, the label is what it would not.
fn owned_by_this_run(kind: &str, name: &str) -> bool {
    let format = format!("{{{{index .{} \"{FIXTURE_OWNER_LABEL}\"}}}}", match kind {
        "network" => "Labels",
        _ => "Config.Labels",
    });
    let args: Vec<&str> = match kind {
        "network" => vec!["network", "inspect", "--format", &format, name],
        _ => vec!["inspect", "--format", &format, name],
    };
    let (ok, out, _) = docker(&args, None);
    ok && out.trim() == owner_token()
}

/// Remove a container this run created, and only if it still says it is ours.
fn remove_owned_container(name: &str) {
    if owned_by_this_run("container", name) {
        docker(&["rm", "--force", "--volumes", name], None);
    }
}

/// Remove a network this run created, and only if it still says it is ours.
fn remove_owned_network(name: &str) {
    if owned_by_this_run("network", name) {
        docker(&["network", "rm", name], None);
    }
}

/// A namespace holder plus the network it sits on, torn down on drop however the test exits.
struct Fixture {
    network: String,
    holder: String,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let network = owned_name(&format!("net-{tag}"));
        let holder = owned_name(&format!("holder-{tag}"));
        // No pre-delete: the names above did not exist a microsecond ago, so there is nothing of
        // anyone's to clear, and a create that fails is a real failure rather than a stale leftover.
        let (ok, _, err) = docker(&["network", "create", "--label", &owner_label(), &network], None);
        assert!(ok, "could not create the test network: {err}");
        let (ok, _, err) = docker(
            &[
                "run",
                "--detach",
                "--name",
                &holder,
                "--label",
                &owner_label(),
                "--network",
                &network,
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--entrypoint",
                "sleep",
                &holder_image(),
                "infinity",
            ],
            None,
        );
        assert!(ok, "could not start the holder: {err}");
        Self { network, holder }
    }

    /// Apply `plan` through the real sidecar, returning the applier's own output.
    fn apply(&self, plan: &str) -> (bool, String, String) {
        docker(
            &[
                "run",
                "--rm",
                "--interactive",
                "--network",
                &format!("container:{}", self.holder),
                "--cap-drop",
                "ALL",
                "--cap-add",
                "NET_ADMIN",
                "--security-opt",
                "no-new-privileges",
                &netfilter_image(),
            ],
            Some(plan),
        )
    }

    /// Read the namespace back through the argv the daemon itself uses.
    fn readback(&self, family: Family) -> String {
        let argv = readback_argv(&self.holder, &netfilter_image(), family);
        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        let (ok, stdout, err) = docker(&args, None);
        assert!(ok, "readback failed: {err}");
        stdout
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        remove_owned_container(&self.holder);
        remove_owned_network(&self.network);
    }
}

fn policy(gateway: &str) -> NetPolicy {
    NetPolicy {
        gateway: gateway.to_owned(),
        proxy_ports: Some(PortRange::new(49200, 49299).expect("valid range")),
        log_connections: true,
        dns_resolvers: Vec::new(),
    }
}

/// The whole plan applies to a real kernel, and the readback of that kernel verifies.
///
/// This is the test the log-prefix bug would have failed: the applier refuses the first rule, so
/// `applied` never reaches the rendered count.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn a_rendered_policy_applies_to_a_live_namespace_and_verifies() {
    let fixture = Fixture::new("verify");
    let policy = policy("172.17.0.1");
    let (plan, expected) = plan_stdin(&policy);

    let (ok, applied, err) = fixture.apply(&plan);
    assert!(ok, "the sidecar refused the plan: {err}");
    assert_eq!(
        applied.parse::<usize>().expect("a count"),
        expected,
        "every rendered rule must reach the kernel"
    );

    for family in [Family::V4, Family::V6] {
        let readback = fixture.readback(family);
        assert_eq!(
            policy.verify_readback(family, &readback),
            Ok(()),
            "{} readback did not verify:\n{readback}",
            family.binary()
        );
    }
}

/// A partially applied policy must be refused, measured against a live kernel rather than a fixture.
///
/// The truncation is silent by construction: a short plan applies perfectly and the applier exits 0,
/// so nothing but comparing against the policy can notice.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn a_truncated_plan_leaves_a_namespace_the_readback_refuses() {
    let fixture = Fixture::new("truncated");
    let policy = policy("172.17.0.1");
    let (plan, expected) = plan_stdin(&policy);

    // The cut is expressed in terms of the v4 rule count, not as a fixed number of lines from the
    // end, and that is a correctness fix rather than a tidy-up. The plan is every v4 rule followed
    // by every v6 rule, so `expected - 4` silently meant "drop the v6 rules and one v4 rule" only
    // while v6 had three. The moment v6 grew the two neighbour-discovery exceptions, the same cut
    // removed four v6 lines and no v4 line at all: v4 verified, and the test that exists to catch a
    // partial install passed while installing a complete v4 policy.
    let v4_rules = policy.rule_count(Family::V4);
    let truncated_to = v4_rules - 1;
    let short: String = plan
        .lines()
        .take(truncated_to)
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(truncated_to < expected, "the plan must actually be truncated");
    let (ok, applied, err) = fixture.apply(&short);
    assert!(ok, "a short plan still applies cleanly, which is the point: {err}");
    assert_eq!(applied.parse::<usize>().expect("a count"), truncated_to);

    // Both families refuse, and for different reasons: v4 is short by exactly one rule, v6 was
    // never reached at all.
    for family in [Family::V4, Family::V6] {
        let readback = fixture.readback(family);
        let refusal = policy
            .verify_readback(family, &readback)
            .expect_err("a partially contained namespace must not verify");
        assert!(
            refusal.contains("rules in OUTPUT"),
            "the refusal must name the count it measured: {refusal}"
        );
    }
}

/// A namespace missing exactly the metadata drop must be refused, and the refusal must name it.
///
/// The count still matches, so this cannot be caught by counting — it is the test that the
/// destination checks are load-bearing rather than decorative.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn a_namespace_missing_only_the_metadata_drop_is_refused() {
    let fixture = Fixture::new("metadata");
    let policy = policy("172.17.0.1");
    let (plan, expected) = plan_stdin(&policy);

    // Drop the metadata DROP, and duplicate a later rule so the total is unchanged.
    let mut lines: Vec<String> = plan.lines().map(str::to_owned).collect();
    let metadata_at = lines
        .iter()
        .position(|line| line.contains("169.254.169.254/32") && line.ends_with("-j DROP"))
        .expect("the metadata drop is in the plan");
    lines.remove(metadata_at);
    lines.push("iptables -A OUTPUT -d 240.0.0.0/4 -j DROP".to_owned());
    let doctored: String = lines.iter().map(|line| format!("{line}\n")).collect();

    let (ok, applied, err) = fixture.apply(&doctored);
    assert!(ok, "the doctored plan applies: {err}");
    assert_eq!(
        applied.parse::<usize>().expect("a count"),
        expected,
        "the count is deliberately unchanged, so only a destination check can catch this"
    );

    let readback = fixture.readback(Family::V4);
    let refusal = policy
        .verify_readback(Family::V4, &readback)
        .expect_err("a namespace that does not drop metadata must not verify");
    assert!(
        refusal.contains("169.254.169.254/32"),
        "the refusal must name the endpoint left reachable: {refusal}"
    );
}

/// **Does containment actually contain?** Every other test here proves the rules are in the kernel.
/// That is not the same claim: a ruleset can be present and ineffective, and "the rules are installed"
/// standing in for "the packets stop" is exactly the substitution that hides a hole.
///
/// So this asks the namespace to open real TCP connections, and it is built to make the result
/// attributable rather than merely negative:
///
/// * two docker networks, one with a subnet **inside** a denied range and one **outside** every denied
///   range, both joined to the same namespace — so blocked-vs-reachable is decided by the policy and
///   not by the environment, with no dependency on internet access;
/// * a real listener on each, so a refusal cannot be confused with "nothing was listening" — the trap
///   that makes naive egress canaries meaningless, since a connect to an address with no listener fails
///   identically whether or not a DROP exists;
/// * **both destinations probed before the rules are applied**, which is the positive control. Without
///   it, a connect that fails for an environmental reason reads as proof of containment.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn a_contained_namespace_stops_a_denied_range_and_still_reaches_an_allowed_one() {
    // 198.18.7/24 sits inside the denied 198.18.0.0/15 (RFC 2544 benchmarking space, which the policy
    // denies and ordinary networks do not use). 203.0.113/24 (RFC 5737 documentation space) is in none
    // of the denied ranges, so it is an "allowed" destination that needs no internet access.
    //
    // Both are chosen to be unlikely to collide with a real network on the host. A 172.16/12 subnet
    // would have been the obvious denied choice and is the wrong one: docker's own default pools live
    // there, so the test would fight whatever else the box is running.
    let canary = Canary::new("203.0.113.0/24", "198.18.7.0/24");

    // Positive control: with no rules installed, BOTH are reachable. If this fails the test proves
    // nothing about the policy, so it must fail loudly here rather than pass later for the wrong
    // reason.
    assert!(
        canary.can_reach(&canary.allowed_ip),
        "control: the allowed listener at {} must be reachable before any rules exist",
        canary.allowed_ip
    );
    assert!(
        canary.can_reach(&canary.denied_ip),
        "control: the denied-range listener at {} must be reachable before any rules exist — \
         otherwise a later refusal is not attributable to the policy",
        canary.denied_ip
    );

    let policy = policy("172.17.0.1");
    let (plan, expected) = plan_stdin(&policy);
    let (ok, applied, err) = canary.fixture.apply(&plan);
    assert!(ok, "the sidecar refused the plan: {err}");
    assert_eq!(applied.parse::<usize>().expect("a count"), expected);

    // The measurement.
    assert!(
        !canary.can_reach(&canary.denied_ip),
        "a denied range is still reachable at {} after containment was installed and verified — the \
         rules are present and doing nothing",
        canary.denied_ip
    );
    // …and the refusal above was the policy, not a listener that quietly died between the control and
    // the measurement. Same address, same port, same moment, from outside the namespace.
    assert!(
        canary.can_reach_from_outside(&canary.denied_net.clone(), &canary.denied_ip),
        "the listener at {} is unreachable from outside the namespace too, so the refusal inside \
         proves nothing about the policy",
        canary.denied_ip
    );
    assert!(
        canary.can_reach(&canary.allowed_ip),
        "containment also blocked {}, which is in none of the denied ranges — an over-broad policy \
         breaks every job while looking like a working sandbox",
        canary.allowed_ip
    );
}

/// **The pinhole must actually open, and must open exactly one thing.**
///
/// This is the leg whose failure is silent and total: the proxy's address sits inside a denied range by
/// construction, so the ACCEPT has to override a DROP that would otherwise cover it. If that ordering
/// does not take effect, every job loses its model while every rule in the namespace reads correctly —
/// and the readback cannot tell, because the rule is present either way.
///
/// Deliberately container-to-container. Pointing the policy's `gateway` at a *container* rather than at
/// the host tests the property that can actually break — an ACCEPT winning over a range DROP at the
/// same address — without depending on how the host's firewall is configured. Two live ports on one
/// address, one inside the permitted range and one outside it, so the only thing separating reachable
/// from blocked is the policy.
///
/// **What this does not cover, stated rather than implied:** that a job reaches the credential proxy on
/// the *host*. That needs the host to accept container-to-host TCP on the proxy's port, which a host
/// firewall may refuse — measured here on NixOS with the firewall active, `172.17.0.1:22` is reachable
/// from a container and `:49250` is not, which is how the cause was identified as the firewall rather
/// than the design. Two further measurements show the design is sound: the `host-gateway` address **is**
/// reachable from a container on a *custom* network, and the probe run from there returns the
/// daemon-wide `172.17.0.1` rather than that network's own gateway — the exact trap
/// [`maxplayer_core::sandbox_netns::host_gateway_probe_argv`] exists to avoid. The final host hop is for
/// an end-to-end contained job to prove, not this test.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn the_pinhole_opens_one_port_and_the_rest_of_that_range_stays_denied() {
    let canary = Canary::new("203.0.113.0/24", "198.18.7.0/24");

    // Control: both ports on the denied-range listener answer before any rule exists.
    for port in [Canary::PORT, Canary::OTHER_PORT] {
        assert!(
            canary.can_reach_port(&canary.denied_ip, port),
            "control: {}:{port} must answer before the rules exist, or nothing below is attributable",
            canary.denied_ip
        );
    }

    // The pinhole names the listener's address and only the range containing PORT, so OTHER_PORT — at
    // the very same address — must stay covered by the range DROP.
    let port: u16 = Canary::PORT.parse().expect("a port");
    let policy = NetPolicy {
        gateway: canary.denied_ip.clone(),
        proxy_ports: Some(PortRange::new(port, port).expect("valid range")),
        log_connections: true,
        dns_resolvers: Vec::new(),
    };
    let (plan, expected) = plan_stdin(&policy);
    let (ok, applied, err) = canary.fixture.apply(&plan);
    assert!(ok, "the sidecar refused the plan: {err}");
    assert_eq!(applied.parse::<usize>().expect("a count"), expected);
    assert_eq!(
        policy.verify_readback(Family::V4, &canary.fixture.readback(Family::V4)),
        Ok(()),
        "the namespace must verify before its behaviour means anything"
    );

    assert!(
        canary.can_reach_port(&canary.denied_ip, Canary::PORT),
        "the pinhole is closed: {}:{} is the one destination this policy permits and it is \
         unreachable, so a job would lose its model while every rule looked right",
        canary.denied_ip,
        Canary::PORT
    );
    assert!(
        !canary.can_reach_port(&canary.denied_ip, Canary::OTHER_PORT),
        "the pinhole is not a pinhole: {}:{} is outside the permitted range and still reachable, so \
         the ACCEPT opened the whole address instead of one port",
        canary.denied_ip,
        Canary::OTHER_PORT
    );
}
/// Reaping removes this seat's unattached holder, **spares a busy one**, and **spares another seat's**.
///
/// The last two are the safety property and the reason this runs against real docker. Two seller
/// daemons share a host — VM1854 runs two earning seats, Server One runs three — and a boot that took
/// every labelled holder would strip the network namespace out from under another daemon's job. The
/// unit tests check the predicate; only this checks that docker reports labels and attachment the way
/// the predicate expects, which is the half a fixture can never prove.
///
/// Scoped to a synthetic seat key, so it reaps only what it planted. That is not politeness: it is the
/// behaviour under test.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn reaping_removes_an_unattached_holder_and_spares_a_busy_one_and_another_seats() {
    // Two synthetic seats. `MINE` boots and reaps; `FOREIGN` is a co-tenant that must be left alone.
    const MINE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const FOREIGN: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    // Unique per run, and stamped as ours. The seats below are synthetic, so no OTHER run of this
    // suite can be reaping them; that is exactly why these names must not be the same two runs
    // apart, and why nothing is force-removed here before it is created.
    let idle = owned_name("reap-idle");
    let busy = owned_name("reap-busy");
    let job = owned_name("reap-job");
    // Deliberately unattached, like a holder in its pre-attach window: the co-tenant case that the
    // host-wide reaper destroyed and attachment state cannot distinguish.
    let foreign = owned_name("reap-cotenant");
    let (idle, busy, job, foreign) =
        (idle.as_str(), busy.as_str(), job.as_str(), foreign.as_str());

    // Three holders carrying the real label — two mine, one another seat's — and a job joined to
    // exactly one of mine.
    for (name, seat) in [(idle, MINE), (busy, MINE), (foreign, FOREIGN)] {
        let (ok, _, err) = docker(
            &[
                "run",
                "--detach",
                "--name",
                name,
                "--label",
                &owner_label(),
                "--label",
                &format!("{}=jobfor-{name}", maxplayer_core::sandbox_netns::HOLDER_LABEL),
                "--label",
                &format!("{}={seat}", maxplayer_core::sandbox_netns::HOLDER_SEAT_LABEL),
                "--entrypoint",
                "sleep",
                &holder_image(),
                "infinity",
            ],
            None,
        );
        assert!(ok, "could not start holder {name}: {err}");
    }
    let (ok, _, err) = docker(
        &[
            "run",
            "--detach",
            "--name",
            job,
            "--label",
            &owner_label(),
            "--network",
            &format!("container:{busy}"),
            "--entrypoint",
            "sleep",
            &holder_image(),
            "infinity",
        ],
        None,
    );
    assert!(ok, "could not join a job to {busy}: {err}");

    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let reaped = runtime
        .block_on(maxplayer_core::sandbox_netns::reap_orphans(MINE))
        .expect("reaping must not fail");

    let still_there = |name: &str| {
        let (_, out, _) = docker(&["ps", "--all", "--quiet", "--filter", &format!("name={name}")], None);
        !out.is_empty()
    };
    let idle_survived = still_there(idle);
    let busy_survived = still_there(busy);
    let foreign_survived = still_there(foreign);

    // Clean up before asserting, so a failure does not leak containers. `idle` is expected to be
    // gone already — the reaper removed it — and the ownership check simply finds nothing to do.
    for name in [idle, busy, job, foreign] {
        remove_owned_container(name);
    }

    assert!(!idle_survived, "my own unattached holder should have been reaped; reaped={reaped:?}");
    assert!(
        busy_survived,
        "the holder with a job attached was reaped — on a shared host that strips the namespace out \
         from under another daemon's running job; reaped={reaped:?}"
    );
    assert!(
        foreign_survived,
        "another seat's unattached holder was reaped — that is the pre-attach window, and destroying \
         it fails a stranger's job that was about to start; reaped={reaped:?}"
    );
    assert_eq!(
        reaped.removed.len(),
        1,
        "exactly one holder was mine and idle; reaped={reaped:?}"
    );
    // #905: a removal docker refused is now carried back rather than printed and dropped, so against
    // a real daemon it is an assertion here instead of a line nobody reads.
    assert!(reaped.failed.is_empty(), "a real reap must not leave holders behind; reaped={reaped:?}");
}

/// **End to end: a job launched through the seller's own argv builder is contained.**
///
/// Everything else here contains a namespace and then joins a container to it by hand. That leaves the
/// last link untested — whether `SandboxPolicy::launch` actually puts the *job* in the holder's
/// namespace. It is the link where a `Some(holder)` that never reaches `--network` would leave every job
/// uncontained while `establish` reported success and the readback verified a namespace nothing runs in.
///
/// So the policy builds the argv, the argv is executed verbatim, and the job's own process is asked to
/// reach a denied address. The control is the same policy and the same argv with `netns: None`: that job
/// must reach it. One difference between the two runs, and it is the field under test.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn a_job_launched_through_the_policy_is_contained_and_an_uncontained_one_is_not() {
    use maxplayer_core::home::{SandboxConfig, SandboxMode};
    use maxplayer_core::seller_exec::{JobLaunch, SandboxPolicy};

    let canary = Canary::new("203.0.113.0/24", "198.18.7.0/24");
    let denied = canary.denied_ip.clone();

    // Resolved from config through the same call a booting seat makes, rather than by assembling the
    // policy directly — so this exercises the path an operator's `[sandbox]` section actually takes.
    //
    // The image carries `nc` and declares NO entrypoint, so the "agent" can be a single connection
    // attempt. Deliberately not the netfilter image: its entrypoint is the applier, which would swallow
    // the agent command as its own arguments and report "empty plan" while `nc` never ran — measured,
    // and it is why this test's control existed to catch it.
    let config = SandboxConfig {
        mode: SandboxMode::Docker,
        launcher: Vec::new(),
        image: Some(holder_image()),
        forward_env: Vec::new(),
        runtime: None,
        network: Some(canary.denied_net.clone()),
        proxy_port_range: None,
        // This test measures egress, so no file-sourced credential: one would add a second reason
        // for the contained launch to differ from its control.
        file_credentials: Vec::new(),
        // No resolver either: the launch under test is compared against a control, and a resolver
        // mount would differ between the two for a reason this test is not measuring.
        dns_servers: Vec::new(),
        codex_chatgpt: None,
        // ABSENT, as an operator's docker config has it — which since the default moved means the
        // container delivery path. Written as `None` rather than `Some(false)` so this fixture stays
        // the config a real seat has. It cannot change what this test measures: the delivery path
        // decides where git runs, and `SandboxPolicy::launch` (the only call below) never reads it.
        container_delivery: None,
        container_delivery_token: None,
        container_delivery_token_cap_secs: None,
    };
    let policy = SandboxPolicy::from_config(Some(&config)).expect("a docker policy");
    let agent_command: Vec<String> = ["nc", "-w", "2", denied.as_str(), Canary::PORT]
        .into_iter()
        .map(String::from)
        .collect();

    // One owned workdir per launch, and never `/tmp`.
    //
    // Production names the job container after the workdir's basename and does not pass `--rm`, so
    // a shared `/tmp` meant a single fixed name, `maxplayer-job-tmp`, for every job this file ever
    // launched. It worked once on a clean daemon and then collided forever: this control passed on
    // the first real run (2026-09-14) and failed on the next three with nothing changed, on
    // `docker: Error response from daemon: Conflict. The container name "/maxplayer-job-tmp" is
    // already in use`. Two workdirs, because the control and the contained job are two containers
    // and would otherwise collide with each other inside this one test.
    let control_workdir = std::env::temp_dir().join(owned_name("workdir-control"));
    let contained_workdir = std::env::temp_dir().join(owned_name("workdir-contained"));
    for dir in [&control_workdir, &contained_workdir] {
        std::fs::create_dir_all(dir).expect("a workdir");
    }

    // Control first, while the namespace has no rules: an UNCONTAINED job reaches the address. This also
    // proves the argv itself works — image, mount, user and all — so a later failure is attributable to
    // containment rather than to a malformed launch.
    let uncontained = policy
        .launch(
            &agent_command,
            &JobLaunch {
                workdir: &control_workdir,
                env: &[],
                uid: 0,
                gid: 0,
                netns: None,
                resolv_conf: None,
            },
        )
        .expect("the policy must build a launch");
    assert!(
        run_launch(&uncontained),
        "control: a job launched with netns: None must reach {denied} — if this fails the argv is \
         broken and the contained case below would pass for the wrong reason"
    );

    // Now contain the namespace and launch the same job into it.
    let (plan, expected) = plan_stdin(&policy_for(&denied));
    let (ok, applied, err) = canary.fixture.apply(&plan);
    assert!(ok, "the sidecar refused the plan: {err}");
    assert_eq!(applied.parse::<usize>().expect("a count"), expected);

    let contained = policy
        .launch(
            &agent_command,
            &JobLaunch {
                workdir: &contained_workdir,
                env: &[],
                uid: 0,
                gid: 0,
                netns: Some(&canary.fixture.holder),
                resolv_conf: None,
            },
        )
        .expect("the policy must build a launch");
    assert!(
        contained.args.iter().any(|arg| arg == &format!("container:{}", canary.fixture.holder)),
        "the launch must join the holder's namespace: {:?}",
        contained.args
    );
    assert!(
        !contained.args.iter().any(|arg| arg == &canary.denied_net),
        "a contained launch must not also name a network — docker takes the last --network and the \
         job would silently land outside the namespace the rules are in: {:?}",
        contained.args
    );
    assert!(
        !run_launch(&contained),
        "a job launched into the contained namespace still reached {denied} — the policy built an \
         argv that does not put the job where the rules are"
    );
}

/// A policy whose pinhole names `gateway`, matching what the canary's listener answers on.
fn policy_for(gateway: &str) -> NetPolicy {
    let port: u16 = Canary::PORT.parse().expect("a port");
    NetPolicy {
        gateway: gateway.to_owned(),
        // No pinhole: this test wants the denied address denied, not excepted.
        proxy_ports: Some(PortRange::new(port + 1, port + 1).expect("valid range")),
        log_connections: true,
        dns_resolvers: Vec::new(),
    }
}

/// Execute an `AgentLaunch` verbatim. `true` iff it exited zero.
fn run_launch(launch: &maxplayer_core::seller_exec::AgentLaunch) -> bool {
    Command::new(&launch.program)
        .args(&launch.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Attempt one TCP connection from a container on `network`. `true` iff it connected.
fn connect(network: &str, ip: &str, port: &str) -> bool {
    let (ok, _, _) = docker(
        &[
            "run",
            "--rm",
            "--network",
            network,
            "--entrypoint",
            "nc",
            &netfilter_image(),
            "-w",
            "2",
            ip,
            port,
        ],
        None,
    );
    ok
}

/// Two networks, two listeners, and a holder joined to both.
struct Canary {
    fixture: Fixture,
    allowed_net: String,
    denied_net: String,
    allowed_listener: String,
    denied_listener: String,
    allowed_ip: String,
    denied_ip: String,
}

impl Canary {
    const PORT: &'static str = "9999";
    /// A second live port on the same listeners, so the pinhole test can put one port inside the proxy
    /// range and one outside it at the same address.
    const OTHER_PORT: &'static str = "9998";

    fn new(allowed_subnet: &str, denied_subnet: &str) -> Self {
        // Unique per run and stamped as ours, so setup creates and never clears.
        //
        // These names used to be fixed, with a `docker rm --force` of each at the top to make a
        // rerun idempotent over the previous run's debris. "Nothing here can touch a name we did not
        // create" was wrong twice: a second concurrent run of this suite creates exactly these
        // names, and on any host an operator may already hold one. Deleting first also destroys the
        // leak it was hiding, so a fixture that leaks on panic now stays visible and attributable to
        // the run that leaked it.
        let allowed_net = owned_name("canary-allowed");
        let denied_net = owned_name("canary-denied");

        let fixture = Fixture::new("canary");
        for (net, subnet) in [(&allowed_net, allowed_subnet), (&denied_net, denied_subnet)] {
            let (ok, _, err) =
                docker(&["network", "create", "--label", &owner_label(), "--subnet", subnet, net], None);
            assert!(
                ok,
                "could not create {net} on {subnet}: {err}\n\
                 If this says the pool overlaps, another network on this host already holds that \
                 subnet — pick a free one inside the same policy range rather than widening the test."
            );
        }

        let allowed_listener = owned_name("canary-listener-allowed");
        let denied_listener = owned_name("canary-listener-denied");
        let mut ips = Vec::new();
        for (name, net) in [(&allowed_listener, &allowed_net), (&denied_listener, &denied_net)] {
            // Two ports, because the pinhole test needs one address that is reachable on one port and
            // denied on another — that pair is what separates "a pinhole" from "an open host".
            let (ok, _, err) = docker(
                &[
                    "run",
                    "--detach",
                    "--name",
                    name,
                    "--label",
                    &owner_label(),
                    "--network",
                    net,
                    "--entrypoint",
                    "sh",
                    &netfilter_image(),
                    "-c",
                    &format!(
                        "while :; do nc -l -p {} >/dev/null 2>&1; done & \
                         while :; do nc -l -p {} >/dev/null 2>&1; done",
                        Self::PORT,
                        Self::OTHER_PORT
                    ),
                ],
                None,
            );
            assert!(ok, "could not start listener {name}: {err}");
            // Measured, never assumed: docker's IPAM picks the address inside the subnet.
            // `index` rather than dot notation because a Go template cannot name a field containing
            // hyphens, and every network here has them.
            let (ok, ip, err) = docker(
                &[
                    "inspect",
                    "--format",
                    &format!("{{{{(index .NetworkSettings.Networks \"{net}\").IPAddress}}}}"),
                    name,
                ],
                None,
            );
            assert!(ok && !ip.is_empty(), "could not read {name}'s address: {err}");
            ips.push(ip);
        }

        // The holder joins BOTH networks, so the one namespace has a route to each listener. Without
        // this the denied listener would be unroutable rather than blocked, and an unroutable
        // destination fails exactly like a dropped one.
        for net in [&allowed_net, &denied_net] {
            let (ok, _, err) = docker(&["network", "connect", net, &fixture.holder], None);
            assert!(ok, "could not attach {net} to the holder: {err}");
        }

        let canary = Self {
            fixture,
            allowed_net,
            denied_net,
            allowed_listener,
            denied_listener,
            allowed_ip: ips[0].clone(),
            denied_ip: ips[1].clone(),
        };

        // Readiness, not assumption. `docker run --detach` returns once the container has *started*;
        // the shell inside still has to add its second address and reach `nc -l`. Three runs of this
        // file on the same daemon disagreed about the very first control for exactly that reason —
        // it passed on the first run and failed on the next two, with nothing changed. Probing from
        // outside the contained namespace is the same discriminator the legs use, so a listener that
        // never comes up still fails, here rather than as a false containment three asserts later.
        for (net, ip) in [
            (canary.allowed_net.clone(), canary.allowed_ip.clone()),
            (canary.denied_net.clone(), canary.denied_ip.clone()),
        ] {
            assert!(
                wait_until(20, || canary.can_reach_from_outside(&net, &ip)),
                "the listener on {ip} never answered from {net} — every leg below would have read \
                 that silence as containment"
            );
        }
        canary
    }

    /// Open a real TCP connection from inside the contained namespace. `true` iff it connected.
    fn can_reach(&self, ip: &str) -> bool {
        self.connect_from(&format!("container:{}", self.fixture.holder), ip)
    }

    /// The same connection attempted from a container on the network directly, **outside** the
    /// contained namespace.
    ///
    /// This is the discriminator for the one ambiguity the canary cannot resolve from inside: a failed
    /// connect looks identical whether the policy dropped the packet or the listener had died. From out
    /// here the policy does not apply, so a success proves the listener is alive and the refusal inside
    /// was the rules doing their job.
    fn can_reach_from_outside(&self, net: &str, ip: &str) -> bool {
        self.connect_from(net, ip)
    }

    fn connect_from(&self, network: &str, ip: &str) -> bool {
        connect(network, ip, Self::PORT)
    }

    /// Reach `ip` on an explicit port from inside the contained namespace.
    fn can_reach_port(&self, ip: &str, port: &str) -> bool {
        connect(&format!("container:{}", self.fixture.holder), ip, port)
    }
}

impl Drop for Canary {
    fn drop(&mut self) {
        // A network cannot be removed while a container is attached, and the holder is attached to
        // both. `Fixture`'s Drop runs after this one, so the holder has to go first here — its own
        // removal then finds nothing, which is harmless.
        remove_owned_container(&self.fixture.holder);
        for name in [&self.allowed_listener, &self.denied_listener] {
            remove_owned_container(name);
        }
        for net in [&self.allowed_net, &self.denied_net] {
            remove_owned_network(net);
        }
    }
}

/// `establish` end to end: it creates the holder, installs the policy, verifies it, and the holder it
/// hands back really is contained. Then dropping the containment removes the holder.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn establish_contains_a_namespace_and_tears_it_down_on_drop() {
    let network = owned_name("net-establish");
    let network = network.as_str();
    let (ok, _, err) = docker(&["network", "create", "--label", &owner_label(), network], None);
    assert!(ok, "could not create the test network: {err}");

    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let outcome = runtime.block_on(maxplayer_core::sandbox_netns::establish(
        network,
        &holder_image(),
        &netfilter_image(),
        "host.docker.internal",
        "live-establish",
        "3333333333333333333333333333333333333333333333333333333333333333",
        1000,
        1000,
        Some(PortRange::new(49200, 49299).expect("valid range")),
        true,
        // No resolver exception in this live case: the DNS pinholes are rendered and read back by
        // unit tests, and adding one here would open port 53 to an address this fixture never
        // measured.
        Vec::new(),
        2_000_000_000,
    ));

    let holder_name = match outcome {
        Ok(containment) => {
            let name = containment.holder.name().to_owned();
            // The namespace it hands back is contained: ask the kernel directly, not the return value.
            let argv = readback_argv(&name, &netfilter_image(), Family::V4);
            let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
            let (ok, readback, err) = docker(&args, None);
            assert!(ok, "readback failed: {err}");
            assert!(
                readback.contains("169.254.169.254/32"),
                "the namespace establish() blessed has no metadata drop:\n{readback}"
            );
            drop(containment);
            name
        }
        Err(error) => {
            remove_owned_network(network);
            panic!("establish failed: {error}");
        }
    };

    // The guard's Drop is synchronous, so by here the holder must be gone.
    let (_, listed, _) = docker(&["ps", "--all", "--quiet", "--filter", &format!("name={holder_name}")], None);
    remove_owned_network(network);
    assert!(
        listed.is_empty(),
        "dropping the containment must remove the holder, but {holder_name} is still listed"
    );
}

// ---------------------------------------------------------------------------------------------
// The interface layer: the filters on the veth the packets actually leave by
// ---------------------------------------------------------------------------------------------

/// The container runtime whose payloads do **not** traverse the host's `OUTPUT` chain, named by the
/// operator rather than guessed. Required, like [`netfilter_image`]: a default of `runsc` would let
/// this test quietly measure `runc` on a host without gVisor and report the leak as closed by rules
/// that never had to stop anything.
fn runsc_runtime() -> String {
    std::env::var("MAXPLAYER_RUNSC_RUNTIME").expect(
        "set MAXPLAYER_RUNSC_RUNTIME (e.g. `runsc`) — the whole point of this test is the runtime \
         that bypasses the OUTPUT chain, so it refuses to guess which one that is",
    )
}

/// What a probe launched inside the namespace actually established.
///
/// A bare boolean could not tell a DENIED packet from a probe that never ran. "the `docker run`
/// exited non-zero" is true when the filters dropped the packet, and equally true when the image is
/// missing, the runtime is not installed, or `nc` is not on the image — and a failure that never
/// reached the path proves nothing whatsoever about the path. Read as a denial, such a failure
/// reports containment that was never exercised.
#[derive(Debug)]
enum Reach {
    /// `nc` connected.
    Connected,
    /// `nc` ran, reached the path, and did not get through: a real denial.
    Denied,
    /// The probe never ran. A broken fixture, not a containment result.
    ToolFailure(String),
}

impl Reach {
    fn connected(&self) -> bool {
        matches!(self, Reach::Connected)
    }

    /// A denial that is REALLY a denial — or a loud failure. Never a silent "not connected".
    fn denied(&self, what: &str) -> bool {
        match self {
            Reach::Denied => true,
            Reach::Connected => false,
            Reach::ToolFailure(why) => panic!(
                "the probe for {what} never ran ({why}), so this run establishes nothing about \
                 containment: a tool failure is not a denial"
            ),
        }
    }
}

/// Run docker and report the child's EXIT CODE, not merely success.
///
/// The code is what separates "the packet was denied" from "the probe never ran": docker reserves
/// 125 for its own failure, 126 for a command it cannot execute and 127 for one it cannot find,
/// while any other non-zero code is the payload itself speaking.
fn docker_exit(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new("docker")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("docker must be on PATH for a live containment test");
    (out.status.code(), String::from_utf8_lossy(&out.stderr).trim().to_owned())
}

/// Connect from a container started under an explicit `--runtime`.
fn connect_under(runtime: &str, network: &str, ip: &str, port: &str) -> Reach {
    let image = netfilter_image();
    let (code, stderr) = docker_exit(&[
        "run",
        "--rm",
        "--runtime",
        runtime,
        "--network",
        network,
        "--entrypoint",
        "nc",
        &image,
        "-w",
        "2",
        ip,
        port,
    ]);
    match code {
        Some(0) => Reach::Connected,
        // docker's own reserved codes: the container never got as far as running the probe.
        Some(code @ (125 | 126 | 127)) => {
            Reach::ToolFailure(format!("docker exited {code}: {stderr}"))
        }
        // `nc` ran and reported that it could not connect.
        Some(_) => Reach::Denied,
        None => Reach::ToolFailure(format!("the probe was killed by a signal: {stderr}")),
    }
}

/// Run a daemon-built argv verbatim. Every helper below goes through this rather than assembling its
/// own docker command, so what the tests exercise is the argv the product ships.
fn run_argv(argv: &[String], stdin: Option<&str>) -> (bool, String, String) {
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    docker(&args, stdin)
}

/// The job's egress link, measured inside the namespace through the daemon's own probe argv.
fn egress_dev(holder: &str) -> String {
    let (ok, stdout, err) = run_argv(&link_probe_argv(holder, &netfilter_image()), None);
    assert!(ok, "could not enumerate the namespace's links: {err}");
    // `parse_links` refuses a record it cannot read rather than skipping it, so an unreadable line
    // fails the test here instead of silently shrinking the list the selector then judges.
    let links = parse_links(&stdout).unwrap_or_else(|error| {
        panic!("the namespace's link list could not be read: {error}\n{stdout}")
    });
    select_egress_link(&links)
        .expect("a job holder's namespace has exactly one non-loopback link")
        .name
}

fn iface_readback(holder: &str, dev: &str) -> String {
    let (ok, stdout, err) = run_argv(&filter_readback_argv(holder, &netfilter_image(), dev), None);
    assert!(ok, "reading the egress filters back from {dev} failed: {err}");
    stdout
}

/// `establish` installs the egress filters too, and the kernel is asked — not the return value.
///
/// The interface leg is inside `establish` rather than beside it deliberately: a test-only installer
/// would prove that these filters *can* be installed while every real job launched without them.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn establish_filters_the_veth_the_packets_actually_leave_by() {
    let network = owned_name("net-iface");
    let network = network.as_str();
    let (ok, _, err) = docker(&["network", "create", "--label", &owner_label(), network], None);
    assert!(ok, "could not create the test network: {err}");

    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let outcome = runtime.block_on(maxplayer_core::sandbox_netns::establish(
        network,
        &holder_image(),
        &netfilter_image(),
        "host.docker.internal",
        "live-iface",
        "4444444444444444444444444444444444444444444444444444444444444444",
        1000,
        1000,
        Some(PortRange::new(49200, 49299).expect("valid range")),
        true,
        // No resolver exception here either, for the same reason as the establish case above: this
        // test asserts the veth filters mirror the rendered policy exactly, so an exception the
        // fixture never measured would be an exception it cannot check.
        Vec::new(),
        2_000_000_000,
    ));

    let containment = match outcome {
        Ok(containment) => containment,
        Err(error) => {
            remove_owned_network(network);
            panic!("establish failed: {error}");
        }
    };

    // The device it filtered is a measured veth, not loopback and not a name anybody assumed.
    assert_ne!(containment.egress_dev, "lo", "establish filtered loopback, not the job's egress link");
    assert!(!containment.egress_dev.is_empty(), "establish named no egress device at all");

    // The plan the daemon must have installed, re-derived from the address IT measured, and checked
    // against what the kernel in that namespace actually holds.
    let policy = NetPolicy {
        gateway: containment.proxy_host.clone(),
        proxy_ports: Some(PortRange::new(49200, 49299).expect("valid range")),
        log_connections: true,
        // Matches the `Vec::new()` handed to `establish` above: this must be the policy the daemon
        // actually installed, so a resolver here that the call never passed would fail the
        // comparison for the wrong reason.
        dns_resolvers: Vec::new(),
    };
    let plan = IfacePlan::derive(&containment.egress_dev, &policy).expect("the plan renders");
    let readback = iface_readback(containment.holder.name(), &containment.egress_dev);
    assert_eq!(
        plan.verify_readback(&readback),
        Ok(()),
        "the namespace establish() blessed does not hold the egress filters:\n{readback}"
    );
    // Both families present as drops, stated here as well as inside the verifier: an unfiltered
    // address family is the cheapest bypass there is, and this assert fails by name if the verifier
    // is ever loosened.
    for protocol in ["ip", "ipv6"] {
        assert!(
            readback.lines().any(|line| line.contains(protocol)),
            "no {protocol} filter in the live readback:\n{readback}"
        );
    }

    let holder_name = containment.holder.name().to_owned();
    drop(containment);
    let (_, listed, _) =
        docker(&["ps", "--all", "--quiet", "--filter", &format!("name={holder_name}")], None);
    remove_owned_network(network);
    assert!(listed.is_empty(), "the holder {holder_name} outlived its containment");
}

/// The red-prove for the egress readback: remove ONE filter from a live namespace and the verifier
/// must refuse it.
///
/// Without this the verifier could `Ok(())` unconditionally and every other test here would still be
/// green — the failure mode that makes a readback worthless is the readback that cannot fail.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn a_namespace_missing_one_egress_filter_is_refused() {
    let fixture = Fixture::new("iface-missing");
    let policy = policy("172.17.0.1");
    let dev = egress_dev(&fixture.holder);
    let plan = IfacePlan::derive(&dev, &policy).expect("the plan renders");
    let (expected_stdin, expected) = maxplayer_core::sandbox_iface::plan_stdin(&plan);

    let (ok, applied, err) =
        run_argv(&iface_sidecar_argv(&fixture.holder, &netfilter_image()), Some(&expected_stdin));
    assert!(ok, "the interface applier refused the plan: {err}");
    assert_eq!(applied.parse::<usize>().expect("a count"), expected, "every step must reach the kernel");
    assert_eq!(
        plan.verify_readback(&iface_readback(&fixture.holder, &dev)),
        Ok(()),
        "control: the untouched namespace must verify, or the refusal below proves nothing"
    );

    // Delete the LAST filter, so what is left is a prefix of the plan: order, protocol and every
    // remaining match key are still perfect, and only the absence is wrong.
    let victim = plan.filters.last().expect("a plan has filters");
    let pref = victim.pref.to_string();
    let protocol = maxplayer_core::sandbox_iface::tc_protocol(victim.family);
    let (ok, _, err) = docker(
        &[
            "run",
            "--rm",
            "--network",
            &format!("container:{}", fixture.holder),
            "--cap-drop",
            "ALL",
            "--cap-add",
            "NET_ADMIN",
            "--entrypoint",
            "tc",
            &netfilter_image(),
            "filter",
            "del",
            "dev",
            &dev,
            // The hook spelling the daemon's own argv uses. `clsact egress` is what `filter show`
            // accepts and `filter del` does not: measured on iproute2 in the gvisor-repro VM, which
            // answers `Unknown filter "clsact", hence option "egress" is unparsable`.
            maxplayer_core::sandbox_iface::EGRESS_HOOK,
            "pref",
            &pref,
            "protocol",
            protocol,
        ],
        None,
    );
    assert!(ok, "could not remove a filter to break containment with: {err}");

    let broken = iface_readback(&fixture.holder, &dev);
    let refusal = plan
        .verify_readback(&broken)
        .expect_err("a namespace missing an egress filter must be refused");
    assert!(
        refusal.contains("expected"),
        "the refusal must say what is missing, got: {refusal}"
    );
}

/// **The regression gate.** The `OUTPUT` chain alone does not contain a `runsc` job; the veth filters
/// do. One namespace, one destination, one payload runtime — the only thing that changes between the
/// two measurements is whether the interface plan is installed.
///
/// Leg 1 is the leak, and it is asserted as a *success*: with the iptables policy installed and
/// verified, a job under gVisor still reaches a destination the policy denies, while a `runc` job in
/// an identically prepared namespace is refused. That pair is what makes this a runtime property
/// rather than a broken fixture.
///
/// Leg 2 installs the same rendered policy on the veth and the same connection is refused, with two
/// live positive controls so a refusal cannot be environmental: an allowed destination stays reachable
/// from inside, and the denied listener stays reachable from outside the namespace.
///
/// **One namespace per gVisor payload, always.** `runsc` claims the namespace's links when it starts,
/// and a SECOND joiner into the same namespace gets `ENETUNREACH` — a refusal no rule caused, which
/// reads exactly like containment. Measured twice: by the prototype matrix ("single-use namespace")
/// and here, where reusing one holder made a *control* fail before any rule existed. So every probe
/// below builds its own holder through [`Payload`], and the listeners are what persist.
#[test]
#[ignore = "needs docker, the netfilter image and a runsc runtime"]
fn the_output_chain_alone_lets_a_runsc_job_out_and_the_veth_filters_stop_it() {
    let runsc = runsc_runtime();
    let net = RunscNet::new();
    let policy = policy("172.17.0.1");

    // CONTROL — with no rules anywhere, a gVisor payload reaches both addresses. A "denied" address
    // that was never reachable proves nothing later.
    assert!(
        Payload::new(&net, "c1").reach(&runsc, RunscNet::DENIED_IP),
        "control: {} must be reachable under {runsc} before any rules exist",
        RunscNet::DENIED_IP
    );
    assert!(
        Payload::new(&net, "c2").reach(&runsc, &net.allowed_ip),
        "control: {} must be reachable under {runsc} before any rules exist",
        net.allowed_ip
    );

    // LEG 1 — the shipped iptables policy alone, installed and verified in each namespace.
    //
    // The discriminator first: the same policy, prepared the same way, contains a runc job. Without
    // it a leak below could just as well be a policy that never applied.
    assert!(
        !Payload::new(&net, "l1runc")
            .with_output_policy(&policy)
            .reach("runc", RunscNet::DENIED_IP),
        "control: the OUTPUT chain must contain a runc job, or leg 1 measures a broken policy rather \
         than a runtime bypass"
    );
    assert!(
        Payload::new(&net, "l1").with_output_policy(&policy).reach(&runsc, RunscNet::DENIED_IP),
        "THE LEAK this change exists to close did not reproduce: a {runsc} job failed to reach {} \
         with only the OUTPUT chain installed. Do not read that as containment — read it as this \
         gate no longer measuring what it claims.",
        RunscNet::DENIED_IP
    );

    // LEG 2 — the same rendered policy, also translated onto the veth.
    assert!(
        !Payload::new(&net, "l2")
            .with_output_policy(&policy)
            .with_veth_filters(&policy)
            .reach(&runsc, RunscNet::DENIED_IP),
        "a {runsc} job still reached the denied {} with the veth filters in force",
        RunscNet::DENIED_IP
    );
    assert!(
        Payload::new(&net, "l2ok")
            .with_output_policy(&policy)
            .with_veth_filters(&policy)
            .reach(&runsc, &net.allowed_ip),
        "positive control: the allowed destination {} must stay reachable under the veth filters — a \
         filter that denies everything is not containment",
        net.allowed_ip
    );
    assert!(
        net.reachable_from_outside(RunscNet::DENIED_IP),
        "positive control: the denied listener must still answer from outside every contained \
         namespace, or the refusal above was a dead listener"
    );
}

/// One network, one listener, two addresses — the topology a **contained job actually gets**.
///
/// The canary fixture above attaches its holder to three networks, which is right for the `OUTPUT`
/// chain and wrong here: a job holder in production is created on exactly one network, so it has
/// exactly one veth, and `select_egress_link` refuses to filter one of several links rather than
/// leave the others open. Measured: the three-network holder made this gate fail with *"expected
/// exactly one non-loopback link, found 3"*, which is the product being right and the fixture being
/// unrealistic.
///
/// So the denied destination is a **second address on the same listener**, inside a denied prefix,
/// with an on-link route added in each namespace. One veth, two destinations, and the only thing that
/// decides reachability is the policy.
struct RunscNet {
    network: String,
    listener: String,
    /// Measured, never assumed: docker's IPAM picks it inside 203.0.113.0/24, which no policy rule
    /// denies.
    allowed_ip: String,
}

impl RunscNet {
    /// Inside the denied 198.18.0.0/15 (RFC 2544 benchmarking space), which ordinary networks do not
    /// use and this repo's policy drops.
    const DENIED_IP: &'static str = "198.18.7.2";

    fn new() -> Self {
        // Unique per run and stamped as ours. The fixed `mx-runsc-net` / `mx-runsc-listener` pair
        // this replaces was force-removed at setup to clear a prior run, which is the same command
        // whether the name is a leftover of ours or a resource somebody else owns.
        let network = owned_name("runsc-net");
        let listener = owned_name("runsc-listener");
        let (ok, _, err) = docker(
            &["network", "create", "--label", &owner_label(), "--subnet", "203.0.113.0/24", &network],
            None,
        );
        assert!(
            ok,
            "could not create {network}: {err}\n\
             If this says the pool overlaps, another network on this host already holds \
             203.0.113.0/24 — pick a free prefix rather than deleting whatever holds it."
        );

        // One process, both addresses: `nc -l` binds every local address, so a refusal can never be
        // "that one was not listening".
        let (ok, _, err) = docker(
            &[
                "run",
                "--detach",
                "--name",
                &listener,
                "--label",
                &owner_label(),
                "--network",
                &network,
                "--cap-add",
                "NET_ADMIN",
                "--entrypoint",
                "sh",
                &netfilter_image(),
                "-c",
                &format!(
                    "ip addr add {}/32 dev eth0 && \
                     while :; do nc -l -p {} >/dev/null 2>&1; done & \
                     while :; do nc -l -p {} >/dev/null 2>&1; done",
                    Self::DENIED_IP,
                    Canary::OTHER_PORT,
                    Canary::PORT
                ),
            ],
            None,
        );
        assert!(ok, "could not start the listener: {err}");

        let (ok, allowed_ip, err) = docker(
            &[
                "inspect",
                "--format",
                &format!("{{{{(index .NetworkSettings.Networks \"{network}\").IPAddress}}}}"),
                &listener,
            ],
            None,
        );
        assert!(ok && !allowed_ip.is_empty(), "could not read the listener's address: {err}");
        let net = Self { network, listener, allowed_ip };
        net.await_listener();
        net
    }

    /// Block until the listener actually answers.
    ///
    /// `docker run --detach` returns when the container has *started*, not when the process inside
    /// it has added its second address and reached `nc -l`. Two runs of this file on the same
    /// daemon disagreed about the very first control for exactly that reason, so readiness is now
    /// the fixture's job rather than a race every leg re-runs. This probes the same destination the
    /// tests do: a listener that never comes up still fails, here instead of three legs later.
    fn await_listener(&self) {
        assert!(
            wait_until(20, || self.reachable_from_outside(Self::DENIED_IP)),
            "the listener never answered on {} — every leg below would have read that as \
             containment",
            Self::DENIED_IP
        );
        // The neighbouring port is a control, so it carries the same readiness bar as the first.
        // A leg that asserts the allowed destination stays reachable on OTHER_PORT is measuring
        // policy only if something is listening there; unchecked, a slow second `nc -l` would read
        // as a port-scoped denial and the control would pass for the wrong reason.
        assert!(
            wait_until(20, || self.reachable_on(&self.allowed_ip, Canary::OTHER_PORT)),
            "the listener never answered on {}:{} — the neighbouring-port control cannot tell a \
             policy denial from an absent listener",
            self.allowed_ip,
            Canary::OTHER_PORT
        );
    }

    /// The same destination, from a container on the network but **outside** every contained
    /// namespace. A success proves the listener is alive, which is the one thing a refusal inside
    /// cannot distinguish itself from.
    fn reachable_from_outside(&self, ip: &str) -> bool {
        self.reachable_on(ip, Canary::PORT)
    }

    /// The same probe, on a named port.
    ///
    /// Split out because the neighbouring-port control needs to establish that `OTHER_PORT` answers
    /// from outside every contained namespace — the only thing that makes a refusal *inside* one
    /// attributable to policy rather than to an absent listener.
    fn reachable_on(&self, ip: &str, port: &str) -> bool {
        let (ok, _, _) = docker(
            &[
                "run",
                "--rm",
                "--network",
                &self.network,
                "--cap-add",
                "NET_ADMIN",
                "--entrypoint",
                "sh",
                &netfilter_image(),
                "-c",
                &format!("ip route add {ip}/32 dev eth0 && nc -w 2 {ip} {port}"),
            ],
            None,
        );
        ok
    }
}

/// Can **this process**, on the VM host and in no namespace at all, reach `ip:port`?
///
/// Every other probe in this file runs inside a container. That is the right instrument for asking
/// what a contained job can do, and it is the wrong one for asking whether containment leaked into
/// the host: a job's rules could be installed host-globally, break the host's own egress, and every
/// container-side leg here would still read exactly the same.
///
/// So this one connects directly. A short timeout, because the failure being guarded against is a
/// DROP, which does not answer at all rather than refusing.
fn host_can_reach(ip: &str, port: &str) -> bool {
    let address = format!("{ip}:{port}");
    let Ok(mut addresses) = std::net::ToSocketAddrs::to_socket_addrs(&address) else {
        return false;
    };
    let Some(address) = addresses.next() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2)).is_ok()
}

/// Retry `probe` once a second until it holds, up to `attempts` times.
///
/// For fixture startup only — a container that has been *started* is not yet a container whose
/// process is listening. It never softens an assertion: the probe is the same one the caller would
/// have run once, and a destination that never answers still returns `false`.
fn wait_until(attempts: u32, probe: impl Fn() -> bool) -> bool {
    for attempt in 0..attempts {
        if probe() {
            return true;
        }
        if attempt + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    false
}

impl Drop for RunscNet {
    fn drop(&mut self) {
        remove_owned_container(&self.listener);
        remove_owned_network(&self.network);
    }
}

/// A namespace for **one** payload: its own holder on the one network, carrying whichever containment
/// layers the case under test installs. Removed on drop however the test exits.
///
/// It exists because a gVisor payload cannot share a namespace with an earlier one (see the gate
/// above), which is also why production gives every job a fresh holder.
struct Payload {
    holder: String,
}

impl Payload {
    fn new(net: &RunscNet, tag: &str) -> Self {
        let holder = owned_name(&format!("payload-{tag}"));
        let (ok, _, err) = docker(
            &[
                "run",
                "--detach",
                "--name",
                &holder,
                "--label",
                &owner_label(),
                "--network",
                &net.network,
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--entrypoint",
                "sleep",
                &holder_image(),
                "infinity",
            ],
            None,
        );
        assert!(ok, "could not start the payload holder {holder}: {err}");
        // The denied address has to be ROUTABLE from this namespace before any rule exists, or a
        // later refusal would be "no route" rather than "dropped" — and the two fail identically.
        // On-link on the job's own veth, which is the interface the filters go on.
        let dev = egress_dev(&holder);
        let (ok, _, err) = docker(
            &[
                "run",
                "--rm",
                "--network",
                &format!("container:{holder}"),
                "--cap-drop",
                "ALL",
                "--cap-add",
                "NET_ADMIN",
                "--entrypoint",
                "ip",
                &netfilter_image(),
                "route",
                "add",
                &format!("{}/32", RunscNet::DENIED_IP),
                "dev",
                &dev,
            ],
            None,
        );
        assert!(ok, "could not route {} into {holder}: {err}", RunscNet::DENIED_IP);
        Self { holder }
    }

    /// The shipped iptables policy, through the real sidecar, verified per family before any payload.
    fn with_output_policy(self, policy: &NetPolicy) -> Self {
        let (plan, expected) = plan_stdin(policy);
        let (ok, applied, err) = docker(
            &[
                "run",
                "--rm",
                "--interactive",
                "--network",
                &format!("container:{}", self.holder),
                "--cap-drop",
                "ALL",
                "--cap-add",
                "NET_ADMIN",
                "--security-opt",
                "no-new-privileges",
                &netfilter_image(),
            ],
            Some(&plan),
        );
        assert!(ok, "the sidecar refused the policy: {err}");
        assert_eq!(applied.parse::<usize>().expect("a count"), expected);
        for family in [Family::V4, Family::V6] {
            let argv = readback_argv(&self.holder, &netfilter_image(), family);
            let (ok, readback, err) = run_argv(&argv, None);
            assert!(ok, "policy readback failed: {err}");
            assert_eq!(
                policy.verify_readback(family, &readback),
                Ok(()),
                "{} policy readback did not verify:\n{readback}",
                family.binary()
            );
        }
        self
    }

    /// The same rendered policy on the veth, through the real applier, read back and verified.
    fn with_veth_filters(self, policy: &NetPolicy) -> Self {
        let dev = egress_dev(&self.holder);
        let iface = IfacePlan::derive(&dev, policy).expect("the plan renders");
        let (stdin, expected) = maxplayer_core::sandbox_iface::plan_stdin(&iface);
        let (ok, applied, err) =
            run_argv(&iface_sidecar_argv(&self.holder, &netfilter_image()), Some(&stdin));
        assert!(ok, "the interface applier refused the plan: {err}");
        assert_eq!(applied.parse::<usize>().expect("a count"), expected);
        let readback = iface_readback(&self.holder, &dev);
        assert_eq!(
            iface.verify_readback(&readback),
            Ok(()),
            "the egress filters did not verify on {dev}:\n{readback}"
        );
        self
    }

    /// Run the one payload this namespace gets, under `runtime`, and report whether it connected.
    ///
    /// A probe that never ran is raised here rather than folded into `false`: "did not connect"
    /// because the image is missing is not the same measurement as "did not connect" because the
    /// filters stopped it, and only one of them says anything about containment.
    fn reach(&self, runtime: &str, ip: &str) -> bool {
        let probe = connect_under(runtime, &format!("container:{}", self.holder), ip, Canary::PORT);
        if let Reach::ToolFailure(why) = &probe {
            panic!(
                "the canary probe never ran ({why}): a tool failure is not a containment result"
            );
        }
        probe.connected()
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        remove_owned_container(&self.holder);
    }
}

// =================================================================================================
// The integrated launch matrix
// =================================================================================================
//
// Everything above this line prepares a namespace by hand and then joins something to it. That
// proves these filters CAN be installed; it does not prove a real job is launched with them, and a
// gate that installs its own plan would stay green if `prepare_launch` stopped calling `establish`
// altogether.
//
// So this section goes through `seller_exec::with_prepared_launch`: the production `prepare_launch`
// establishes containment, the production `SandboxPolicy::launch` builds the argv, `netns` is wired
// from `holder_name` exactly as `run_agent_job_with_env` wires it, and the argv is executed
// verbatim. The separate OUTPUT-only reproduction above is kept deliberately — it is the baseline
// arm, and it must not be folded into this one.
//
// **The oracle.** A refusal and a launch that never happened are the same exit code, and reading
// docker's status alone lets a broken image, a missing mount or an OOM be scored as containment.
// Every payload here therefore prints a start marker before it tries anything and a result marker
// carrying the connection's own exit code, and the outcome is read from those markers. A payload
// that did not print the start marker is `NeverStarted` and is never counted as a denial.

/// Printed by the payload before it attempts anything, so "the job ran" is observable separately
/// from "the job's connection failed".
const STARTED_MARKER: &str = "MX-PAYLOAD-STARTED";

/// Printed after the connection attempt, carrying `nc`'s own exit code.
const RESULT_MARKER: &str = "MX-CONNECT-RC=";

/// What a payload actually did. The distinction between the last two variants is the whole point:
/// only `Refused` is evidence of containment.
#[derive(Debug, PartialEq, Eq)]
enum PayloadOutcome {
    /// The payload's own process never reached its first statement. A docker, image, mount or
    /// runtime failure — never containment evidence, in either direction.
    NeverStarted,
    /// The payload ran and its connection succeeded.
    Connected,
    /// The payload ran and its connection was refused or timed out.
    Refused,
    /// The payload ran, but what exited was the TOOL rather than the connection: `nc` missing,
    /// not executable, or rejecting its own arguments. Never containment evidence.
    ///
    /// Split out of `Refused`, which used to absorb every nonzero status. An image without `nc`
    /// exits 127 through `sh`, and 127 is not zero, so a leg measuring nothing at all scored as a
    /// denial -- the single most flattering way this matrix could be wrong, since a fixture that
    /// silently lost its payload would have reported perfect containment on every denied leg.
    ToolFailed(i32),
}

/// The agent command for one connection attempt, bracketed by markers.
///
/// `sh -c` rather than `nc` directly, because the markers have to come from the payload's own
/// process: a wrapper outside the container would print "started" for a container that never did.
fn payload_command(ip: &str, port: &str) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!("echo {STARTED_MARKER}; nc -w 4 {ip} {port} </dev/null >/dev/null 2>&1; echo {RESULT_MARKER}$?"),
    ]
}

/// Classify a payload's own output. Docker's exit status is deliberately not consulted.
fn classify_payload(stdout: &str, stderr: &str) -> PayloadOutcome {
    let combined = format!("{stdout}\n{stderr}");
    if !combined.contains(STARTED_MARKER) {
        return PayloadOutcome::NeverStarted;
    }
    match combined
        .lines()
        .find_map(|line| line.trim().strip_prefix(RESULT_MARKER))
        .and_then(|code| code.trim().parse::<i32>().ok())
    {
        Some(0) => PayloadOutcome::Connected,
        // BusyBox `nc`, which is what the Alpine fixture carries, exits 1 for a refused connection
        // and 1 for the `-w` timeout. Those are the two shapes containment takes here, and they
        // are the only statuses allowed to mean it.
        Some(1) => PayloadOutcome::Refused,
        // 127 not found, 126 not executable, 2 usage, 128+n killed by a signal. Every one of these
        // is the tool failing rather than the network answering.
        Some(other) => PayloadOutcome::ToolFailed(other),
        // Started, but never reported a result: killed mid-attempt. Not a denial.
        None => PayloadOutcome::NeverStarted,
    }
}

/// The denial oracle only calls a connection refused when the connection was refused.
///
/// Offline, because it is a property of the classifier rather than of any network, and because the
/// whole matrix rests on it: every denied leg in the saved record is this function's verdict. The
/// failure it guards against is the flattering one -- an image that lost `nc` exits 127, and while
/// any nonzero status counted as a denial, a leg that measured nothing reported containment.
#[test]
fn only_a_connection_failure_is_scored_as_a_denial() {
    let started = |code: &str| format!("{STARTED_MARKER}\n{RESULT_MARKER}{code}\n");

    assert_eq!(classify_payload(&started("0"), ""), PayloadOutcome::Connected);
    assert_eq!(
        classify_payload(&started("1"), ""),
        PayloadOutcome::Refused,
        "BusyBox nc exits 1 for a refused connection and for the -w timeout"
    );
    for broken in [127, 126, 2, 137] {
        assert_eq!(
            classify_payload(&started(&broken.to_string()), ""),
            PayloadOutcome::ToolFailed(broken),
            "exit {broken} is the tool failing, not the network refusing"
        );
    }
    // And the pre-existing boundaries still hold: no start marker, and a start with no result.
    assert_eq!(classify_payload("", ""), PayloadOutcome::NeverStarted);
    assert_eq!(classify_payload(STARTED_MARKER, ""), PayloadOutcome::NeverStarted);
}

/// Free the deterministic container name a launch is about to use.
///
/// Production names a job container from its workdir and does not pass `--rm`, so the container
/// survives its own exit and the *same* launch cannot be run twice: the second attempt dies on
/// `Conflict. The container name ... is already in use` before the payload exists, which arrives as
/// `NeverStarted` and looks exactly like containment. Only the legs that deliberately run one job
/// more than once call this, and only between attempts — it is the reaper's job done by hand, not a
/// change to what any assertion measures.
fn free_job_name(launch: &maxplayer_core::seller_exec::AgentLaunch) {
    if let Some(name) =
        launch.args.windows(2).find(|pair| pair[0] == "--name").map(|pair| pair[1].clone())
    {
        let _ = docker(&["rm", "-f", &name], None);
    }
}

/// Execute a production-built `AgentLaunch` verbatim and read the payload's own markers back.
fn run_launch_attributably(launch: &maxplayer_core::seller_exec::AgentLaunch) -> PayloadOutcome {
    let out = Command::new(&launch.program)
        .args(&launch.args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the launch program must be runnable");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let outcome = classify_payload(&stdout, &stderr);
    if matches!(outcome, PayloadOutcome::ToolFailed(_)) {
        // Loud for the same reason `NeverStarted` is loud, and louder in one respect: this leg's
        // payload DID start, so the leg looks like it measured something.
        eprintln!(
            "ToolFailed ({outcome:?}): the payload's connection tool failed rather than being \
             refused, so this leg measured nothing: {} {:?}\n  stdout: {}\n  stderr: {}",
            launch.program,
            launch.args,
            stdout.trim(),
            stderr.trim()
        );
    }
    if outcome == PayloadOutcome::NeverStarted {
        // "NeverStarted" is the one outcome that says nothing about containment and everything
        // about the launch, so it must not be silent: the first real run of this file reported it
        // for the sibling leg with no way to tell a refused `docker run` from a killed payload.
        eprintln!(
            "NeverStarted: {} {:?}\n  stdout: {}\n  stderr: {}",
            launch.program,
            launch.args,
            stdout.trim(),
            stderr.trim()
        );
    }
    outcome
}

/// The seat identity the gate launches as. Synthetic, and distinct from the reaper test's seats so a
/// concurrent reap cannot touch this run's holders.
fn gate_identity() -> maxplayer_core::seller_git::DeliveryAgentIdentity {
    maxplayer_core::seller_git::DeliveryAgentIdentity::for_seller(
        "5555555555555555555555555555555555555555555555555555555555555555",
    )
}

/// The resolver every gate in this file configures.
///
/// TEST-NET-1 (RFC 5737): reserved for documentation and routed nowhere. Named explicitly rather
/// than left empty, because empty does not mean "no DNS" -- `sandbox_dns::resolve` falls back to
/// the host's `resolv.conf` and then to `resolvectl`, and refuses a loopback address, which is
/// exactly what a systemd host presents at `127.0.0.53`. Left empty, these legs would depend on the
/// DNS configuration of whichever machine ran them.
///
/// **This address answers nothing, and nothing in this file proves DNS works.** It is a
/// deterministic fixture for *containment* legs: it makes the rendered rule set predictable so the
/// pinhole leg can check the SHAPE of what production installed. A rendered `--dport 53 -j ACCEPT`
/// and an enforced one produce the same green here.
///
/// Functioning resolution is a different claim and is measured elsewhere, against a resolver that
/// actually answers: [`crate::sandbox_dns_live`] runs a real dnsmasq fixture through the same
/// production path, covering v4/v6 resolution through the written `resolv.conf`, UDP truncation
/// falling back to TCP 53, host-stub discovery, and the denied-neighbour controls (another private
/// address, and a non-53 port on the resolver itself). Do not cite this constant, or this file's
/// readback, as evidence that a contained job can resolve a name.
const GATE_DNS_RESOLVER: &str = "192.0.2.53";

/// The `[sandbox]` section an operator writes, resolved through the same call a booting seat makes.
fn gate_config(network: &str) -> maxplayer_core::home::SandboxConfig {
    maxplayer_core::home::SandboxConfig {
        mode: maxplayer_core::home::SandboxMode::Docker,
        launcher: Vec::new(),
        // Carries `sh` and `nc`, and declares no entrypoint, so the agent command is the payload.
        image: Some(holder_image()),
        forward_env: Vec::new(),
        runtime: None,
        network: Some(network.to_owned()),
        // No pinhole: this matrix measures denial and allowance, and a proxy range would add a
        // second reason for a leg to differ from its control. The pinhole has its own coverage.
        proxy_port_range: None,
        // An explicit resolver, and deliberately not an empty list. Empty does not mean "no DNS":
        // `sandbox_dns::resolve` falls back to the HOST's resolv.conf and then to resolvectl, and
        // refuses a loopback address — which is exactly what a systemd host presents at
        // `127.0.0.53`. Left empty, every integrated leg here would depend on the DNS configuration
        // of whatever machine the matrix runs on, and would fail preparation on a perfectly healthy
        // one. So the gate names its own.
        //
        // TEST-NET-1 (RFC 5737), which is reserved for documentation and routed nowhere. It
        // exercises 995's real path — the resolver file is written and the port-53 exception is
        // rendered and read back — while opening reach to nothing that exists. The payloads here
        // dial numeric addresses and resolve nothing, so no leg depends on it answering.
        dns_servers: vec![GATE_DNS_RESOLVER.to_owned()],
        file_credentials: Vec::new(),
        codex_chatgpt: None,
        container_delivery: None,
        container_delivery_token: None,
        container_delivery_token_cap_secs: None,
    }
}

/// Production `prepare_launch` hardcodes [`DEFAULT_NETFILTER_IMAGE`] — it does NOT read
/// `MAXPLAYER_NETFILTER_IMAGE`, which only the hand-built fixtures above use. So an integrated run
/// measures whatever is tagged as that image on this host.
///
/// This asserts it exists, and says what to do about it, because the alternative is the failure this
/// whole change is about: a gate reporting containment from an image that has no `tc` in it.
fn require_default_netfilter_image() {
    let image = maxplayer_core::sandbox_netns::DEFAULT_NETFILTER_IMAGE;
    let (ok, _, err) = docker(&["image", "inspect", "--format", "{{.Id}}", image], None);
    assert!(
        ok,
        "the integrated gate goes through production `prepare_launch`, which hardcodes {image} and \
         ignores MAXPLAYER_NETFILTER_IMAGE. Tag the locally built sidecar as that image before \
         running this gate:\n  docker build -t {image} docker/maxplayer-netfilter\nThe published \
         v0.5.8 image does NOT contain tc, so an integrated run against it is expected to refuse \
         every launch rather than contain anything.\ndocker said: {err}"
    );
}

/// Make `ip` reachable on-link inside the holder's namespace, so a later refusal is the filters and
/// not the absence of a route. Run after preparation, which is the only moment the namespace exists
/// and the payload has not started.
fn route_on_link(holder: &str, ip: &str) {
    let (ok, _, err) = docker(
        &[
            "run",
            "--rm",
            "--network",
            &format!("container:{holder}"),
            "--cap-drop",
            "ALL",
            "--cap-add",
            "NET_ADMIN",
            "--entrypoint",
            "ip",
            &netfilter_image(),
            "route",
            "add",
            &format!("{ip}/32"),
            "dev",
            "eth0",
        ],
        None,
    );
    assert!(ok, "could not make {ip} routable inside {holder}: {err}");
}

/// Run one integrated leg: production preparation, production launch argv, attributable payload.
///
/// `before_payload` runs inside the prepared namespace after containment is installed and before the
/// payload starts — the window a route injection has to use.
fn integrated_leg(
    network: &str,
    ip: &str,
    port: &str,
    before_payload: impl FnOnce(&str),
) -> Result<PayloadOutcome, String> {
    integrated_leg_with(gate_config(network), ip, port, before_payload)
}

/// [`integrated_leg`], for a leg that needs a `[sandbox]` section other than the default one — a
/// configured pinhole, or a named runtime. The path through production is identical.
fn integrated_leg_with(
    config: maxplayer_core::home::SandboxConfig,
    ip: &str,
    port: &str,
    before_payload: impl FnOnce(&str),
) -> Result<PayloadOutcome, String> {
    let policy = maxplayer_core::seller_exec::SandboxPolicy::from_config(Some(&config))
        .expect("a docker policy");
    let workdir = std::env::temp_dir().join(owned_name("workdir"));
    std::fs::create_dir_all(&workdir).expect("a workdir");
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let outcome = runtime.block_on(maxplayer_core::seller_exec::with_prepared_launch(
        &payload_command(ip, port),
        &policy,
        &workdir,
        &gate_identity(),
        std::time::Duration::from_secs(120),
        |launch, holder| {
            let holder = holder.expect(
                "a docker policy with a configured network must establish containment — a `None` \
                 holder here means the job would run uncontained",
            );
            assert!(
                launch.args.iter().any(|arg| arg == &format!("container:{holder}")),
                "the production launch must join the holder's namespace: {:?}",
                launch.args
            );
            before_payload(holder);
            run_launch_attributably(launch)
        },
    ));
    let _ = std::fs::remove_dir_all(&workdir);
    outcome.map_err(|error| error.to_string())
}

/// **The integrated gate.** One preparation path, three ordered legs, each scored from the payload's
/// own markers.
///
/// The allowed leg is not a formality: a filter set that denies everything would pass the denied leg
/// and is not containment. The outside control is the discriminator for the denied leg — from
/// outside the namespace the policy does not apply, so a success there proves the listener was alive
/// and the refusal inside was the rules.
#[test]
#[ignore = "needs docker and the production-tagged netfilter image"]
fn a_job_prepared_and_launched_by_production_is_contained_on_its_veth() {
    require_default_netfilter_image();
    let net = RunscNet::new();

    // CONTROL, outside every namespace: both destinations answer.
    assert!(
        net.reachable_from_outside(RunscNet::DENIED_IP),
        "control: {} must answer from outside, or the denied leg below proves nothing",
        RunscNet::DENIED_IP
    );
    assert!(
        net.reachable_from_outside(&net.allowed_ip),
        "control: {} must answer from outside",
        net.allowed_ip
    );

    // LEG 1 — a destination the shipped policy denies, through the production launch path.
    let denied = integrated_leg(&net.network, RunscNet::DENIED_IP, Canary::PORT, |holder| {
        route_on_link(holder, RunscNet::DENIED_IP)
    })
    .expect("preparation must succeed");
    assert_eq!(
        denied,
        PayloadOutcome::Refused,
        "a job prepared and launched by production reached the denied {} \
         (NeverStarted here would mean the payload never ran, which is not containment either)",
        RunscNet::DENIED_IP
    );

    // LEG 2 — the allowed destination, same path, same image, same user.
    let allowed = integrated_leg(&net.network, &net.allowed_ip, Canary::PORT, |_| {})
        .expect("preparation must succeed");
    assert_eq!(
        allowed,
        PayloadOutcome::Connected,
        "positive control: the allowed {} must stay reachable through the production launch path — \
         a policy that denies everything is not containment",
        net.allowed_ip
    );

    // LEG 3 — a neighbouring port on the SAME allowed address. The policy's denials are not
    // port-scoped, so the allowed address stays allowed; what this leg rules out is a filter that
    // happened to match on one port number.
    let other_port = integrated_leg(&net.network, &net.allowed_ip, Canary::OTHER_PORT, |_| {})
        .expect("preparation must succeed");
    // Asserted as Connected, which is what this leg's own comment claims and what the saved record
    // states. `!= NeverStarted` passed on Refused too -- so a filter that DID silently become
    // port-scoped, the exact failure this leg exists to rule out, satisfied it. A leg that accepts
    // both answers to its own question is not a control.
    assert_eq!(
        other_port,
        PayloadOutcome::Connected,
        "the allowed {} must stay reachable on the neighbouring port {}: the policy's denials are \
         not port-scoped, and a leg that also accepted Refused would not have noticed if they were",
        net.allowed_ip,
        Canary::OTHER_PORT
    );
}

/// **The oracle's own red-prove.** A payload that cannot start must not be scored as a denial.
///
/// Without this the matrix above would pass with every leg broken: a bad image, a missing mount or a
/// runtime failure exits non-zero exactly like a refused connection, and round 1's oracle — docker's
/// status alone — could not tell them apart.
#[test]
#[ignore = "needs docker and the production-tagged netfilter image"]
fn a_payload_that_never_ran_is_not_scored_as_a_denial() {
    // The classifier first, on captured shapes, so the rule is stated independently of any daemon.
    assert_eq!(
        classify_payload("", "docker: Error response from daemon: no such image"),
        PayloadOutcome::NeverStarted,
        "a docker failure must never be read as containment"
    );
    assert_eq!(
        classify_payload(&format!("{STARTED_MARKER}\n{RESULT_MARKER}1\n"), ""),
        PayloadOutcome::Refused
    );
    assert_eq!(
        classify_payload(&format!("{STARTED_MARKER}\n{RESULT_MARKER}0\n"), ""),
        PayloadOutcome::Connected
    );
    assert_eq!(
        classify_payload(&format!("{STARTED_MARKER}\n"), ""),
        PayloadOutcome::NeverStarted,
        "started but killed before reporting is not a denial"
    );

    // Then live: a real production launch whose command does not exist. Containment is established
    // and correct; the payload still never runs, and the outcome must say so.
    require_default_netfilter_image();
    let net = RunscNet::new();
    let config = gate_config(&net.network);
    let policy = maxplayer_core::seller_exec::SandboxPolicy::from_config(Some(&config))
        .expect("a docker policy");
    let workdir = std::env::temp_dir().join(owned_name("workdir-nostart"));
    std::fs::create_dir_all(&workdir).expect("a workdir");
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let outcome = runtime
        .block_on(maxplayer_core::seller_exec::with_prepared_launch(
            &["mx-no-such-binary".to_owned()],
            &policy,
            &workdir,
            &gate_identity(),
            std::time::Duration::from_secs(60),
            |launch, _| run_launch_attributably(launch),
        ))
        .expect("preparation must succeed — it is the payload that cannot start");
    let _ = std::fs::remove_dir_all(&workdir);
    assert_eq!(
        outcome,
        PayloadOutcome::NeverStarted,
        "a payload that could not start was scored as something other than NeverStarted"
    );
}

/// **Fail-closed at preparation.** When containment cannot be established the launch is refused, no
/// payload is started, and nothing is left running.
///
/// The trigger is a configured network that does not exist, which is the shape of every preparation
/// failure that matters: the seat is configured for containment and the daemon cannot deliver it.
/// The alternative behaviour — running the job on whatever networking is available — is exactly what
/// "configured but not enforced" means, and it must not be representable.
#[test]
#[ignore = "needs docker"]
fn containment_that_cannot_be_established_refuses_the_launch_and_leaves_nothing_behind() {
    let missing = owned_name("net-that-does-not-exist");
    let config = gate_config(&missing);
    let policy = maxplayer_core::seller_exec::SandboxPolicy::from_config(Some(&config))
        .expect("a docker policy");
    let workdir = std::env::temp_dir().join(owned_name("workdir-failclosed"));
    std::fs::create_dir_all(&workdir).expect("a workdir");

    let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = std::sync::Arc::clone(&started);
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let outcome = runtime.block_on(maxplayer_core::seller_exec::with_prepared_launch(
        &payload_command("203.0.113.9", Canary::PORT),
        &policy,
        &workdir,
        &gate_identity(),
        std::time::Duration::from_secs(60),
        move |_, _| {
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
        },
    ));
    let _ = std::fs::remove_dir_all(&workdir);

    let error = outcome
        .err()
        .map(|error| error.to_string())
        .expect("a job whose containment cannot be established must not launch");
    assert!(
        error.contains("egress containment not established"),
        "the refusal must name what failed: {error}"
    );
    assert!(
        !started.load(std::sync::atomic::Ordering::SeqCst),
        "the payload closure ran despite containment failing — the job would have started uncontained"
    );
    // Nothing survives. The holder is named from the job id, which is derived from the workdir, so
    // this looks for any holder still carrying this run's workdir name.
    let (_, listed, _) = docker(
        &["ps", "--all", "--quiet", "--filter", "label=ai.maxplayer.netns-holder"],
        None,
    );
    for id in listed.lines().filter(|line| !line.trim().is_empty()) {
        let (_, name, _) = docker(&["inspect", "--format", "{{.Name}}", id], None);
        assert!(
            !name.contains("failclosed"),
            "a holder from the failed preparation is still running: {name}"
        );
    }
}

/// **Sibling isolation across cleanup.** One contained job's teardown must not disturb another's.
///
/// Two jobs are prepared through the production path on the same network. The first is torn down —
/// its holder removed, its namespace destroyed — while the second is still running, and the second
/// must still reach its allowed destination afterwards. This is the property a cleanup implemented
/// as "delete the tc filters" or "remove the containers matching our prefix" would break, and
/// neither failure is visible from a single-job test.
#[test]
#[ignore = "needs docker and the production-tagged netfilter image"]
fn one_jobs_cleanup_leaves_a_sibling_job_contained_and_running() {
    require_default_netfilter_image();
    let net = RunscNet::new();

    // The sibling: prepared, measured, and kept alive across the other job's whole lifetime. Its
    // holder name is captured so its survival can be asserted rather than assumed.
    let config = gate_config(&net.network);
    let policy = maxplayer_core::seller_exec::SandboxPolicy::from_config(Some(&config))
        .expect("a docker policy");
    let workdir = std::env::temp_dir().join(owned_name("workdir-sibling"));
    std::fs::create_dir_all(&workdir).expect("a workdir");
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");

    let survived = runtime.block_on(maxplayer_core::seller_exec::with_prepared_launch(
        &payload_command(&net.allowed_ip, Canary::PORT),
        &policy,
        &workdir,
        &gate_identity(),
        std::time::Duration::from_secs(180),
        |launch, holder| {
            let sibling_holder = holder.expect("containment").to_owned();
            // Before: the sibling reaches its allowed destination.
            assert_eq!(
                run_launch_attributably(launch),
                PayloadOutcome::Connected,
                "the sibling could not reach {} before the other job existed",
                net.allowed_ip
            );

            // A whole second job, prepared and torn down inside this window.
            //
            // On its own thread, because this closure is already being driven by the sibling's
            // runtime and `integrated_leg` builds one of its own: tokio refuses to start a runtime
            // from inside a runtime, and the first real run of this file panicked here. The second
            // job genuinely is a separate job, so giving it a separate thread is the shape the test
            // was describing all along — not a workaround for the assertion.
            let other = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        integrated_leg(&net.network, RunscNet::DENIED_IP, Canary::PORT, |h| {
                            route_on_link(h, RunscNet::DENIED_IP)
                        })
                    })
                    .join()
                    .expect("the second job's thread must not panic")
            })
            .expect("the second job must prepare");
            assert_eq!(other, PayloadOutcome::Refused, "the second job was not contained");

            // After the other job's guard dropped: the sibling's holder is still there…
            let (_, listed, _) = docker(
                &["ps", "--quiet", "--filter", &format!("name={sibling_holder}")],
                None,
            );
            assert!(
                !listed.is_empty(),
                "the sibling's holder {sibling_holder} was removed by another job's cleanup"
            );
            // …and it is still contained and still working.
            (
                {
                    // The sibling's first probe left a finished container holding this exact name.
                    free_job_name(launch);
                    run_launch_attributably(launch)
                },
                {
                    let denied_probe =
                        prepared_launch_for(&policy, &workdir, RunscNet::DENIED_IP, &sibling_holder);
                    free_job_name(&denied_probe);
                    run_launch_attributably(&denied_probe)
                },
            )
        },
    ));
    let _ = std::fs::remove_dir_all(&workdir);
    let (allowed_after, denied_after) = survived.expect("preparation must succeed");
    assert_eq!(
        allowed_after,
        PayloadOutcome::Connected,
        "the sibling lost its allowed destination after another job's cleanup"
    );
    assert_eq!(
        denied_after,
        PayloadOutcome::Refused,
        "the sibling lost its containment after another job's cleanup — its veth filters were \
         deleted by a teardown that was not scoped to the job that owned them"
    );
}

/// **The host's own egress is not collateral.** `host.unaffected.during-cleanup`.
///
/// This leg was REQUIRED by `sandbox_evidence::REQUIRED_CASES` and, until now, asserted by nothing.
/// The round-2 record scored it `connected` on the strength of the sibling test running nearby;
/// that test probes from inside containers, so it could not have observed the host's egress in
/// either direction. The row was a claim, not a measurement, and the review was right to say so.
///
/// What is measured here: the host reaches the canary directly, in its own namespace, at three
/// points — before a contained job exists, while one is prepared and running, and after its
/// teardown. A job whose containment mutated host-global state (an `OUTPUT` rule that was not
/// scoped to the veth, a teardown that flushed a shared chain) breaks one of the three.
///
/// The negative control is not decoration: a probe that returned `true` unconditionally would pass
/// all three legs. A port nobody listens on must come back unreachable, from the same function, on
/// the same address.
#[test]
#[ignore = "needs docker and the production-tagged netfilter image"]
fn the_hosts_own_egress_is_unaffected_before_during_and_after_a_jobs_cleanup() {
    require_default_netfilter_image();
    let net = RunscNet::new();

    // CONTROL: the instrument can say "no". 9997 is neither PORT nor OTHER_PORT, so nothing in the
    // canary is listening on it.
    assert!(
        !host_can_reach(&net.allowed_ip, "9997"),
        "the host probe reported a port nobody listens on as reachable — it cannot distinguish \
         anything, and the three legs below would pass without measuring"
    );

    // BEFORE: no contained job has existed on this network yet.
    assert!(
        wait_until(20, || host_can_reach(&net.allowed_ip, Canary::PORT)),
        "the host could not reach the canary at {}:{} before any job was prepared — the fixture, \
         not the containment, is what this leg would otherwise blame",
        net.allowed_ip,
        Canary::PORT
    );

    let config = gate_config(&net.network);
    let policy = maxplayer_core::seller_exec::SandboxPolicy::from_config(Some(&config))
        .expect("a docker policy");
    let workdir = std::env::temp_dir().join(owned_name("workdir-host"));
    std::fs::create_dir_all(&workdir).expect("a workdir");
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");

    // DURING: inside the window where the job is contained and its payload has run. The job itself
    // is asserted as contained, so this is not measuring an empty namespace.
    let during = runtime.block_on(maxplayer_core::seller_exec::with_prepared_launch(
        &payload_command(RunscNet::DENIED_IP, Canary::PORT),
        &policy,
        &workdir,
        &gate_identity(),
        std::time::Duration::from_secs(120),
        |launch, holder| {
            let holder = holder.expect("containment");
            route_on_link(holder, RunscNet::DENIED_IP);
            let contained = run_launch_attributably(launch);
            (contained, host_can_reach(&net.allowed_ip, Canary::PORT))
        },
    ));
    let _ = std::fs::remove_dir_all(&workdir);
    let (contained, host_during) = during.expect("preparation must succeed");

    assert_eq!(
        contained,
        PayloadOutcome::Refused,
        "the job was not contained, so the host leg beside it measured nothing about containment"
    );
    assert!(
        host_during,
        "the host lost its own egress to {}:{} while a job was contained — the job's rules are not \
         scoped to its veth",
        net.allowed_ip,
        Canary::PORT
    );

    // AFTER: the guard has dropped, so teardown has run.
    assert!(
        host_can_reach(&net.allowed_ip, Canary::PORT),
        "the host lost its own egress to {}:{} after a job's teardown — cleanup deleted something \
         it did not own",
        net.allowed_ip,
        Canary::PORT
    );
}

// ============================================================================================
// F2 — the legs round 1 named missing: IPv6, both registered runtimes, and the proxy pinhole.
//
// Authored under a hold that forbade running them; **first executed 2026-09-14** against a real
// daemon in the `gvisor-repro` VM, with both runtimes registered and the production tag resolved
// locally. The prediction written here at authoring time — that the first real run would need
// fixture-addressing adjustment — is what happened: the v6 leg's control read `docker run --detach`
// as readiness and probed a listener that had not yet reached `nc -l`. That is fixed by retrying
// the identical probe. The containment assertions themselves were not touched.
// ============================================================================================

/// The `[sandbox]` section of [`gate_config`] plus the pinhole an operator configures. Kept apart
/// from `gate_config` because the pinhole adds a second reason for a leg to differ from its
/// control, which is exactly what the base matrix is built to avoid.
fn gate_config_with_pinhole(network: &str, range: &str) -> maxplayer_core::home::SandboxConfig {
    maxplayer_core::home::SandboxConfig {
        proxy_port_range: Some(range.to_owned()),
        ..gate_config(network)
    }
}

/// [`gate_config`] pinned to one named container runtime.
fn gate_config_with_runtime(
    network: &str,
    runtime: &str,
) -> maxplayer_core::home::SandboxConfig {
    maxplayer_core::home::SandboxConfig {
        runtime: Some(runtime.to_owned()),
        ..gate_config(network)
    }
}

/// **The pinhole, as production installs it.**
///
/// Round 1's matrix configured no proxy range at all, so nothing measured the one rule whose job is
/// to let traffic *through* a denied range. The hand-built fixture earlier in this file covers the
/// pinhole's semantics; what was missing is that **production's own preparation path** puts it on
/// the veth the packets leave by, with the range the operator wrote and no other.
///
/// This leg reads the prepared namespace's own `tc` output back in the window between containment
/// and payload start, and checks the pinhole against the configured range rather than against
/// anything this file rendered. The payload leg that follows is the discriminator: a pinhole wide
/// enough to be useless would still satisfy a readback that only counted rules.
///
/// **Scope.** This establishes that the pinhole production installs is no wider than the
/// configuration -- including that the resolver exceptions go to the configured resolver and
/// nowhere else. It does NOT establish that resolution works through them; see
/// [`GATE_DNS_RESOLVER`] for where that is measured.
#[test]
#[ignore = "needs docker and the production-tagged netfilter image"]
fn the_pinhole_production_installs_is_the_one_the_policy_names() {
    require_default_netfilter_image();
    let net = RunscNet::new();
    const RANGE: &str = "49200-49299";
    const TC_RANGE: &str = "49200-49299";

    // Port paired with destination: a port-53 rule is only admissible if it goes to the resolver
    // the gate configured, and that is unanswerable from the port alone.
    let seen_range = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    let recorder = std::sync::Arc::clone(&seen_range);

    // The payload still goes to a denied destination: the pinhole must not become a hole.
    let denied = integrated_leg_with(
        gate_config_with_pinhole(&net.network, RANGE),
        RunscNet::DENIED_IP,
        Canary::PORT,
        move |holder| {
            route_on_link(holder, RunscNet::DENIED_IP);
            let dev = egress_dev(holder);
            let readback = iface_readback(holder, &dev);
            let filters = maxplayer_core::sandbox_iface::parse_filters(&readback)
                .expect("the prepared namespace's own tc output must parse");
            let ports: Vec<(String, String)> = filters
                .iter()
                .filter(|filter| filter.actions == vec!["pass".to_owned()])
                .filter_map(|filter| {
                    filter.key("dst_port").map(|port| {
                        (port.to_owned(), filter.key("dst_ip").unwrap_or("any").to_owned())
                    })
                })
                .collect();
            *recorder.lock().expect("the recorder") = ports;
        },
    )
    .expect("preparation must succeed");

    let ports = seen_range.lock().expect("the recorder").clone();
    assert!(
        ports.iter().any(|(port, _)| port == TC_RANGE),
        "production installed no pass rule for the configured proxy range {RANGE} — the pinhole the \
         operator wrote is not on the veth the packets leave by. Pass rules carried ports: {ports:?}"
    );
    // Every pass rule is one of exactly two things the configuration asked for: the proxy range, or
    // a resolver pinhole on port 53 to a resolver the gate NAMED. The second admits the DNS
    // exceptions without widening the check -- the destination is pinned to the configured address,
    // so a port-53 rule to anywhere else still fails here, and so does any other port.
    //
    // Both intents, in one assertion. "No pinhole wider than its configuration" is the whole point
    // of this leg, and a job that cannot resolve a name is useless: the resolver exception exists
    // and is bounded, rather than being either banned or waved through.
    let stray: Vec<&(String, String)> = ports
        .iter()
        .filter(|(port, dst)| {
            let proxy = port == TC_RANGE;
            let resolver = port == "53" && dst.starts_with(GATE_DNS_RESOLVER);
            !proxy && !resolver
        })
        .collect();
    assert!(
        stray.is_empty(),
        "production installed a pass rule the operator did not write: {stray:?} (all pass rules: \
         {ports:?}) — a pinhole wider than its configuration is a hole. Only the proxy range \
         {TC_RANGE} and port 53 to the configured resolver {GATE_DNS_RESOLVER} are configured here"
    );
    assert!(
        ports.iter().any(|(port, dst)| port == "53" && dst.starts_with(GATE_DNS_RESOLVER)),
        "production installed no resolver pinhole for the configured {GATE_DNS_RESOLVER} — a job \
         that cannot reach its own resolver resolves nothing. Pass rules carried: {ports:?}"
    );
    assert_eq!(
        denied,
        PayloadOutcome::Refused,
        "a configured pinhole must not make an unrelated denied destination reachable"
    );
}

/// **Both registered runtimes, one production path.**
///
/// Round 1 proved containment under whichever runtime docker happens to default to. gVisor is the
/// reason this work exists and `runsc` reimplements the network stack, so "contained under runc"
/// and "contained under runsc" are two claims, not one. Running the identical leg under each is the
/// only thing that tells them apart.
#[test]
#[ignore = "needs docker, the production-tagged image and a runsc runtime"]
fn both_registered_runtimes_are_contained_by_the_same_production_path() {
    require_default_netfilter_image();
    let net = RunscNet::new();
    assert!(
        net.reachable_from_outside(RunscNet::DENIED_IP),
        "control: {} must answer from outside, or every refusal below proves nothing",
        RunscNet::DENIED_IP
    );

    for runtime in ["runc".to_owned(), runsc_runtime()] {
        let denied = integrated_leg_with(
            gate_config_with_runtime(&net.network, &runtime),
            RunscNet::DENIED_IP,
            Canary::PORT,
            |holder| route_on_link(holder, RunscNet::DENIED_IP),
        )
        .unwrap_or_else(|error| panic!("preparation must succeed under {runtime}: {error}"));
        assert_eq!(
            denied,
            PayloadOutcome::Refused,
            "under runtime {runtime} a job prepared and launched by production reached the denied {}",
            RunscNet::DENIED_IP
        );

        let allowed = integrated_leg_with(
            gate_config_with_runtime(&net.network, &runtime),
            &net.allowed_ip,
            Canary::PORT,
            |_| {},
        )
        .unwrap_or_else(|error| panic!("preparation must succeed under {runtime}: {error}"));
        assert_eq!(
            allowed,
            PayloadOutcome::Connected,
            "positive control under runtime {runtime}: the allowed {} must stay reachable — a \
             runtime whose networking is broken denies everything and looks contained",
            net.allowed_ip
        );
    }
}

/// An IPv6-capable network with a listener answering on **both** an allowed and a denied v6 address.
///
/// The whole point of the v6 leg: the policy renders v6 drops, and until something dials a v6
/// address through the production path, an unfiltered second address family is the cheapest bypass
/// on the box — and the one a v4-only matrix cannot see.
struct V6Net {
    network: String,
    listener: String,
}

impl V6Net {
    /// Documentation prefix (RFC 3849). No policy rule denies it, so it is the allowed control.
    const SUBNET: &'static str = "2001:db8:ff::/64";
    const ALLOWED_IP: &'static str = "2001:db8:ff::2";
    /// Unique-local (RFC 4193), inside the `fc00::/7` this repo's policy drops.
    const DENIED_IP: &'static str = "fd00:dead:beef::2";

    fn new() -> Self {
        let network = owned_name("v6-net");
        let listener = owned_name("v6-listener");
        let (ok, _, err) = docker(
            &[
                "network",
                "create",
                "--ipv6",
                "--label",
                &owner_label(),
                "--subnet",
                Self::SUBNET,
                &network,
            ],
            None,
        );
        assert!(
            ok,
            "could not create the v6 network {network}: {err}\n\
             If this says IPv6 is not enabled, the daemon needs `\"ipv6\": true` — a daemon-level \
             change, which is a decision to name rather than to make from a test."
        );

        // One process, both addresses, exactly as the v4 fixture does it.
        let (ok, _, err) = docker(
            &[
                "run",
                "--detach",
                "--name",
                &listener,
                "--label",
                &owner_label(),
                "--network",
                &network,
                "--cap-add",
                "NET_ADMIN",
                "--entrypoint",
                "sh",
                &netfilter_image(),
                "-c",
                &format!(
                    "ip -6 addr add {}/128 dev eth0 && while :; do nc -l -p {} >/dev/null 2>&1; done",
                    Self::DENIED_IP,
                    Canary::PORT
                ),
            ],
            None,
        );
        assert!(ok, "could not start the v6 listener: {err}");
        let net = Self { network, listener };
        assert!(
            wait_until(20, || net.reachable_from_outside(Self::DENIED_IP)),
            "the v6 listener never answered on {} — see `RunscNet::await_listener`",
            Self::DENIED_IP
        );
        net
    }

    /// The listener's own link-local address, as the kernel assigned it.
    ///
    /// Discovered rather than constructed: it is derived from the interface's MAC, so computing it
    /// here would be a second implementation of SLAAC and would silently drift from the address
    /// the listener actually answers on.
    fn listener_link_local(&self) -> String {
        let (ok, out, err) = docker(
            &[
                "exec",
                &self.listener,
                "ip",
                "-6",
                "-oneline",
                "addr",
                "show",
                "dev",
                "eth0",
                "scope",
                "link",
            ],
            None,
        );
        assert!(ok, "could not read the v6 listener's link-local address: {err}");
        let address = out
            .split_whitespace()
            .skip_while(|token| *token != "inet6")
            .nth(1)
            .and_then(|cidr| cidr.split('/').next())
            .unwrap_or_default()
            .to_owned();
        assert!(
            address.starts_with("fe80:"),
            "expected a link-local address on the listener, read {address:?} from {out:?}"
        );
        address
    }

    /// [`Self::reachable_from_outside`], allowing the listener time to come up.
    ///
    /// `docker run --detach` returns when the container is *started*, not when the process inside
    /// it has added its second address and reached `nc -l`. The first real run of this file probed
    /// immediately and read that startup gap as a dead listener. This retries the identical probe,
    /// so a destination that never answers still fails — the wait buys the fixture time, it does
    /// not soften what the control proves.
    fn reachable_from_outside_within(&self, ip: &str, attempts: u32) -> bool {
        for attempt in 0..attempts {
            if self.reachable_from_outside(ip) {
                return true;
            }
            if attempt + 1 < attempts {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        false
    }

    /// From a container on the network but outside every contained namespace — the discriminator a
    /// refusal inside cannot supply for itself.
    fn reachable_from_outside(&self, ip: &str) -> bool {
        let (ok, _, _) = docker(
            &[
                "run",
                "--rm",
                "--network",
                &self.network,
                "--cap-add",
                "NET_ADMIN",
                "--entrypoint",
                "sh",
                &netfilter_image(),
                "-c",
                &format!("ip -6 route add {ip}/128 dev eth0; nc -w 2 {ip} {}", Canary::PORT),
            ],
            None,
        );
        ok
    }
}

impl Drop for V6Net {
    fn drop(&mut self) {
        remove_owned_container(&self.listener);
        remove_owned_network(&self.network);
    }
}

/// Make a v6 address reachable on-link inside the holder, so a refusal is the filters and not a
/// missing route.
fn route6_on_link(holder: &str, ip: &str) {
    let (ok, _, err) = docker(
        &[
            "run",
            "--rm",
            "--network",
            &format!("container:{holder}"),
            "--cap-drop",
            "ALL",
            "--cap-add",
            "NET_ADMIN",
            "--entrypoint",
            "ip",
            &netfilter_image(),
            "-6",
            "route",
            "add",
            &format!("{ip}/128"),
            "dev",
            "eth0",
        ],
        None,
    );
    assert!(ok, "could not make {ip} routable inside {holder}: {err}");
}

/// **The second address family, through the production path.**
///
/// Same three-leg shape as the v4 matrix, and for the same reasons: an outside control so a refusal
/// is not a dead listener, a denied leg, and an allowed leg so "denies everything" cannot pass as
/// containment.
#[test]
#[ignore = "needs docker, an IPv6-enabled daemon and the production image"]
fn the_denied_v6_prefix_is_denied_through_the_production_path() {
    require_default_netfilter_image();
    let net = V6Net::new();

    assert!(
        net.reachable_from_outside_within(V6Net::DENIED_IP, 10),
        "control: {} must answer from outside, or the denied leg below proves nothing",
        V6Net::DENIED_IP
    );

    let denied = integrated_leg(&net.network, V6Net::DENIED_IP, Canary::PORT, |holder| {
        route6_on_link(holder, V6Net::DENIED_IP)
    })
    .expect("preparation must succeed");
    assert_eq!(
        denied,
        PayloadOutcome::Refused,
        "a job prepared and launched by production reached the denied v6 {} — an unfiltered second \
         address family is the cheapest bypass there is",
        V6Net::DENIED_IP
    );

    // The positive control, and what makes the denial above mean something.
    //
    // On its first real run (2026-09-14) this control failed, and the cause was in the policy, not
    // in the test: the ALLOWED v6 address — in none of `DENIED_DESTINATIONS_V6` — was refused too.
    // IPv6 Neighbour Solicitation goes to a solicited-node MULTICAST address, so the `ff00::/8`
    // drop starved neighbour discovery and the namespace lost every v6 destination, denied and
    // allowed alike. A denial measured on a dead v6 stack proves nothing about `fc00::/7`.
    //
    // `sandbox_net` now permits exactly the two ND control messages — solicitation to
    // `ff02::1:ff00:0/104` and advertisement, both at hop limit 255 — and nothing else in
    // `ff00::/8`. This assertion is the live proof that the narrowing works: the allowed address
    // connects, which means ND resolved, which means the refusal above was the destination rule
    // doing its job. If it ever reads `Refused` again the two legs must be read together, because
    // a starved stack refuses both.
    let allowed = integrated_leg(&net.network, V6Net::ALLOWED_IP, Canary::PORT, |_| {})
        .expect("preparation must succeed");
    assert_eq!(
        allowed,
        PayloadOutcome::Connected,
        "the allowed v6 {} was refused by a job production contained — if neighbour discovery is \
         starved again then the denied leg above is denial by a dead stack and proves nothing",
        V6Net::ALLOWED_IP
    );
}

/// Permitting neighbour discovery must not have permitted link-local **traffic**.
///
/// This is the counter-control for the ND exception, and it is the leg that would catch the lazy
/// fix. Making `integrated.allowed.v6` pass by widening `fe80::/10` — or by accepting all ICMPv6,
/// or all multicast — would light up the positive control just as well, and this leg is what tells
/// the two apart: the listener answers on its own link-local address, an unfiltered joiner reaches
/// it, and a contained job must not.
///
/// `fe80::/10` is the range neighbour ADVERTISEMENT is sent into, so it is the one an over-broad ND
/// exception opens first.
#[test]
#[ignore = "needs docker, an IPv6-enabled daemon and the production image"]
fn permitting_neighbour_discovery_did_not_permit_link_local_traffic() {
    require_default_netfilter_image();
    let net = V6Net::new();
    let link_local = net.listener_link_local();
    // A link-local destination is only meaningful with the interface it is scoped to.
    let scoped = format!("{link_local}%eth0");

    assert!(
        net.reachable_from_outside_within(&scoped, 10),
        "control: the listener must answer on its own link-local {scoped} from an UNFILTERED \
         joiner, or a refusal below proves nothing about the filters"
    );

    let outcome = integrated_leg(&net.network, &scoped, Canary::PORT, |_| {})
        .expect("preparation must succeed");
    assert_eq!(
        outcome,
        PayloadOutcome::Refused,
        "a contained job reached the link-local {scoped} — the neighbour-discovery exception has \
         been widened past the two ICMPv6 control messages into ordinary link-local traffic"
    );
}

/// Build a launch into an EXISTING holder, for the sibling check's second probe. Goes through the
/// production argv builder, and names the namespace explicitly rather than preparing a new one.
fn prepared_launch_for(
    policy: &maxplayer_core::seller_exec::SandboxPolicy,
    workdir: &std::path::Path,
    ip: &str,
    holder: &str,
) -> maxplayer_core::seller_exec::AgentLaunch {
    policy
        .launch(
            &payload_command(ip, Canary::PORT),
            &maxplayer_core::seller_exec::JobLaunch {
                workdir,
                env: &[],
                uid: 0,
                gid: 0,
                netns: Some(holder),
                // The canary payload dials a numeric address and resolves nothing, so it is handed
                // no `/etc/resolv.conf` mount. Containment is what this launch measures.
                resolv_conf: None,
            },
        )
        .expect("the policy must build a launch")
}

// ---------------------------------------------------------------------------------------------
// The two legs the renewed round-1 verdict found missing: a proxy connection that actually
// SUCCEEDS, and a cancellation resolved by the real daemon rather than by a stand-in.
// ---------------------------------------------------------------------------------------------

/// A real TCP listener on the host, accepting until the test drops it.
///
/// A listener in the test process rather than a container, because the leg under test is precisely
/// container-to-HOST: a listener living on the docker network would prove the pinhole reaches
/// another container, which is not where the credential proxy runs.
struct HostListener {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HostListener {
    fn bind(port: u16) -> Self {
        let listener = std::net::TcpListener::bind(("0.0.0.0", port))
            .unwrap_or_else(|error| panic!("could not bind the host listener on {port}: {error}"));
        listener.set_nonblocking(true).expect("nonblocking");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
                }
            }
        });
        Self { stop, handle: Some(handle) }
    }
}

impl Drop for HostListener {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A contained job **actually reaches the credential proxy on the host**, through the pinhole
/// production installed, under the gVisor runtime.
///
/// Every earlier live proxy test measured a DENIAL. Denial is the cheap half: a policy that drops
/// everything passes all of them, and the job it contains cannot do its work. The half that was
/// missing — and that the renewed round-1 verdict called out — is that the one address the job is
/// supposed to reach is actually reachable. That is what this asserts first.
///
/// Both ports have a live listener on the host, and only one is inside `proxy_ports`. That is the
/// control built into the run: a refused connection here cannot be blamed on an absent listener,
/// and a permitted one cannot be blamed on a blanket allow, because the two legs differ only in
/// whether the pinhole names the port.
#[test]
#[ignore = "needs docker, gVisor and the netfilter image"]
fn a_contained_job_actually_connects_to_the_host_proxy_through_the_pinhole() {
    let runtime_name = runsc_runtime();
    let network = owned_name("net-proxy-reach");
    let network = network.as_str();
    let (ok, _, err) = docker(&["network", "create", "--label", &owner_label(), network], None);
    assert!(ok, "could not create the test network: {err}");

    // A multi-port range, which is the shape production ships and every other tc-path test uses.
    // The one-port shape is measured on its own by
    // [`a_single_port_pinhole_establishes_and_the_job_reaches_only_that_port`]: it used to render
    // `dst_port N-N` and be refused by the tc flower classifier outright. Keeping the two shapes in
    // separate gates means neither can cover for a regression in the other.
    let pinhole = PortRange::new(49220, 49229).expect("valid range");
    let allowed_port: u16 = 49221; // inside the pinhole
    let denied_port: u16 = 49401; // outside it
    let _allowed_listener = HostListener::bind(allowed_port);
    let _denied_listener = HostListener::bind(denied_port);

    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let outcome = rt.block_on(maxplayer_core::sandbox_netns::establish(
        network,
        &holder_image(),
        &netfilter_image(),
        "host.docker.internal",
        "live-proxy-reach",
        "3333333333333333333333333333333333333333333333333333333333333333",
        1000,
        1000,
        Some(pinhole),
        true,
        Vec::new(),
        2_000_000_000,
    ));

    let containment = match outcome {
        Ok(containment) => containment,
        Err(error) => {
            remove_owned_network(network);
            panic!("establish failed: {error}");
        }
    };
    let holder = containment.holder.name().to_owned();
    let proxy_host = containment.proxy_host.clone();
    let netns = format!("container:{holder}");

    let reached = connect_under(&runtime_name, &netns, &proxy_host, &allowed_port.to_string());
    let refused = connect_under(&runtime_name, &netns, &proxy_host, &denied_port.to_string());

    drop(containment);
    remove_owned_network(network);

    assert!(
        reached.connected(),
        "the contained job could NOT reach the proxy at {proxy_host}:{allowed_port}, the one \
         address the pinhole exists to permit — a job under this policy cannot do its work \
         ({reached:?})"
    );
    // `denied` refuses to read a tool failure as a denial: if the probe never ran, this panics
    // rather than crediting containment that was never exercised.
    assert!(
        refused.denied(&format!("{proxy_host}:{denied_port}")),
        "the contained job reached {proxy_host}:{denied_port}, which is OUTSIDE the pinhole: the \
         permit is not confined to the port the policy names"
    );
}

/// The SINGLE-PORT pinhole, end to end against a real kernel.
///
/// A one-port proxy range is supported configuration — `PortRange::new` accepts equal endpoints and
/// the parser documents a bare `"49200"` — and until this round it could not run at all. iptables
/// renders it `49221:49221`, the tc translation turned that into `dst_port 49221-49221`, and the
/// flower classifier refused it outright, so the filters never installed and establishment refused
/// the launch. Fail-closed, but a supported configuration that cannot start is still broken.
///
/// Rendering it as the bare port is what the offline gates assert. Only a real `tc` can say whether
/// that rendering is one it ACCEPTS, and that is what this measures — the half no amount of string
/// assertion can reach.
///
/// The control is the same as the range gate's, and tighter: both ports carry a live host listener,
/// and the denied one sits directly ABOVE the single permitted port, so a permit that quietly
/// widened by even one port fails here.
#[test]
#[ignore = "needs docker, gVisor and the netfilter image"]
fn a_single_port_pinhole_establishes_and_the_job_reaches_only_that_port() {
    let runtime_name = runsc_runtime();
    let network = owned_name("net-proxy-singleton");
    let network = network.as_str();
    let (ok, _, err) = docker(&["network", "create", "--label", &owner_label(), network], None);
    assert!(ok, "could not create the test network: {err}");

    // start == end: the exact shape that could not be installed before this round.
    let pinhole = PortRange::new(49221, 49221).expect("a single port is a valid range");
    let allowed_port: u16 = 49221;
    let denied_port: u16 = 49222;
    let _allowed_listener = HostListener::bind(allowed_port);
    let _denied_listener = HostListener::bind(denied_port);

    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let outcome = rt.block_on(maxplayer_core::sandbox_netns::establish(
        network,
        &holder_image(),
        &netfilter_image(),
        "host.docker.internal",
        "live-proxy-singleton",
        "3333333333333333333333333333333333333333333333333333333333333333",
        1000,
        1000,
        Some(pinhole),
        true,
        Vec::new(),
        2_000_000_000,
    ));

    let containment = match outcome {
        Ok(containment) => containment,
        Err(error) => {
            remove_owned_network(network);
            // Exactly the failure the defect produced: tc refuses the filter, so containment cannot
            // be established and the job never launches.
            panic!(
                "establish FAILED for a single-port pinhole: {error} — a supported one-port proxy \
                 range must install like any other"
            );
        }
    };
    let holder = containment.holder.name().to_owned();
    let proxy_host = containment.proxy_host.clone();
    let netns = format!("container:{holder}");

    let reached = connect_under(&runtime_name, &netns, &proxy_host, &allowed_port.to_string());
    let refused = connect_under(&runtime_name, &netns, &proxy_host, &denied_port.to_string());

    drop(containment);
    remove_owned_network(network);

    assert!(
        reached.connected(),
        "the contained job could NOT reach {proxy_host}:{allowed_port}, the single port its own \
         pinhole names ({reached:?}) — a one-port range must be installable, not merely accepted \
         by the type"
    );
    assert!(
        refused.denied(&format!("{proxy_host}:{denied_port}")),
        "the contained job reached {proxy_host}:{denied_port}, one port ABOVE its single-port \
         pinhole: collapsing an equal-endpoint range to a bare port must not widen what it permits"
    );
}

/// A cancelled `establish` leaves no holder behind **against the real docker daemon**.
///
/// The offline cancellation gate drives a stand-in client, which earns ordering credit and nothing
/// more: a stand-in cannot show what a real daemon does with a create that was still running when
/// its caller went away. Here the daemon is real, the create is real, and the question is answered
/// by asking docker what containers exist afterwards.
///
/// The cancellation walks across the create window rather than firing once, because the window is
/// short and a single fixed delay can miss it entirely — and a run that never cancelled mid-create
/// would pass no matter what cleanup did. A leak in ANY attempt fails the gate.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn a_cancelled_establish_leaves_no_holder_behind_against_the_real_daemon() {
    let network = owned_name("net-cancel-live");
    let network = network.as_str();
    let (ok, _, err) = docker(&["network", "create", "--label", &owner_label(), network], None);
    assert!(ok, "could not create the test network: {err}");

    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let mut leaked: Vec<String> = Vec::new();
    // How many attempts actually cancelled an establish that was still running. Nothing in the
    // previous version required this to be above zero, so a walk that never crossed the create
    // window would have reported a clean pass having cancelled nothing at all.
    let mut cancelled_in_flight = 0u32;
    // Bound outside the future: it is polled to cancellation below, so anything it borrows has to
    // outlive the statement that builds it.
    let holder_image = holder_image();
    let netfilter_image = netfilter_image();

    for attempt in 0..5u32 {
        let job = format!("live-cancel-{attempt}");
        let holder = format!("maxplayer-netns-{job}");
        let delay = std::time::Duration::from_millis(150 + u64::from(attempt) * 120);
        // Whether the cancellation landed on a still-running establish, rather than after one that
        // had already finished.
        let mut hit_creation = false;

        rt.block_on(async {
            let mut establishing = Box::pin(maxplayer_core::sandbox_netns::establish(
                network,
                &holder_image,
                &netfilter_image,
                "host.docker.internal",
                &job,
                "3333333333333333333333333333333333333333333333333333333333333333",
                1000,
                1000,
                None,
                true,
                Vec::new(),
                2_000_000_000,
            ));
            tokio::select! {
                // establish won the race: this attempt exercised cleanup after SUCCESS. That must
                // still not leak, but it says nothing about cancellation, so it is not counted as
                // one.
                _ = establishing.as_mut() => {}
                _ = tokio::time::sleep(delay) => hit_creation = true,
            }
            drop(establishing); // the cancellation under test
        });
        if hit_creation {
            cancelled_in_flight += 1;
        }

        // Absence, asked so that only a real absence can answer it. Three things together, because
        // any one of them alone is satisfiable by a run that measured nothing:
        //
        //  * a SUCCESSFUL daemon query. `docker ps` that fails prints nothing on stdout, and an
        //    empty stdout is exactly what a clean daemon prints too — success-shaped emptiness that
        //    reads identically whether the oracle worked or never ran at all.
        //  * the HOLDER AND ITS JOINERS. Sidecars are named `<holder>-<verb>-<pid>-<serial>`, so
        //    this substring filter covers them; a surviving sidecar pins the namespace the holder
        //    was torn down to release, and checking holder names alone would miss it entirely.
        //  * absence that is STABLE across consecutive answers rather than the first one seen. A
        //    create that outlived its cancellation can still land, so an early empty listing is a
        //    container that has not appeared YET, not one that never will.
        const STABLE_ANSWERS: u32 = 5;
        let mut consecutive_absent = 0u32;
        let mut answered = 0u32;
        let mut survivors: Vec<String> = Vec::new();
        let mut last_error = String::new();
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let (ok, listed, err) = docker(
                &["ps", "--all", "--format", "{{.Names}}", "--filter", &format!("name={holder}")],
                None,
            );
            if ok {
                answered += 1;
                let names: Vec<String> = listed
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect();
                if names.is_empty() {
                    consecutive_absent += 1;
                } else {
                    consecutive_absent = 0;
                    survivors = names;
                }
            } else {
                // A daemon that cannot answer is not a daemon reporting "clean".
                consecutive_absent = 0;
                last_error = err;
            }
            if consecutive_absent >= STABLE_ANSWERS || std::time::Instant::now() >= give_up {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }

        assert!(
            answered > 0,
            "the daemon never successfully answered what exists under {holder} (last error: \
             {last_error:?}); with no answer at all there is nothing to conclude, and concluding \
             \"absent\" from a failed query is the whole defect this gate exists to catch"
        );
        if consecutive_absent < STABLE_ANSWERS {
            leaked.push(if survivors.is_empty() { holder.clone() } else { survivors.join(", ") });
            for name in survivors.iter().chain(std::iter::once(&holder)) {
                remove_owned_container(name);
            }
        }
    }

    remove_owned_network(network);
    assert!(
        cancelled_in_flight > 0,
        "not one of the attempts cancelled an establish that was still running — every one of them \
         finished first, so this run measured cleanup after success and never exercised \
         cancellation at all. A green here would be a green for a property nothing tested."
    );
    assert!(
        leaked.is_empty(),
        "a cancelled establish left {leaked:?} running against the real daemon: the create landed \
         after cleanup had already given up on it, and nothing owns those namespaces now"
    );
}
