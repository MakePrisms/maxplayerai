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

/// A namespace holder plus the network it sits on, torn down on drop however the test exits.
struct Fixture {
    network: String,
    holder: String,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let network = format!("mx-live-net-{tag}");
        let holder = format!("mx-live-holder-{tag}");
        let (ok, _, err) = docker(&["network", "create", &network], None);
        assert!(ok, "could not create the test network: {err}");
        let (ok, _, err) = docker(
            &[
                "run",
                "--detach",
                "--name",
                &holder,
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
        docker(&["rm", "--force", "--volumes", &self.holder], None);
        docker(&["network", "rm", &self.network], None);
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

    let short: String = plan
        .lines()
        .take(expected - 4)
        .map(|line| format!("{line}\n"))
        .collect();
    let (ok, applied, err) = fixture.apply(&short);
    assert!(ok, "a short plan still applies cleanly, which is the point: {err}");
    assert_eq!(applied.parse::<usize>().expect("a count"), expected - 4);

    let readback = fixture.readback(Family::V4);
    let refusal = policy
        .verify_readback(Family::V4, &readback)
        .expect_err("a partially contained namespace must not verify");
    assert!(
        refusal.contains("rules in OUTPUT"),
        "the refusal must name the count it measured: {refusal}"
    );
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
    let idle = "mx-reap-idle";
    let busy = "mx-reap-busy";
    let job = "mx-reap-job";
    // Deliberately unattached, like a holder in its pre-attach window: the co-tenant case that the
    // host-wide reaper destroyed and attachment state cannot distinguish.
    let foreign = "mx-reap-cotenant";
    for name in [idle, busy, job, foreign] {
        docker(&["rm", "--force", "--volumes", name], None);
    }

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

    // Clean up before asserting, so a failure does not leak containers.
    for name in [idle, busy, job, foreign] {
        docker(&["rm", "--force", "--volumes", name], None);
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
    use std::path::Path;

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

    // Control first, while the namespace has no rules: an UNCONTAINED job reaches the address. This also
    // proves the argv itself works — image, mount, user and all — so a later failure is attributable to
    // containment rather than to a malformed launch.
    let uncontained = policy
        .launch(
            &agent_command,
            &JobLaunch {
                workdir: Path::new("/tmp"),
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
                workdir: Path::new("/tmp"),
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
        let allowed_net = "mx-canary-allowed".to_owned();
        let denied_net = "mx-canary-denied".to_owned();
        // Setup can panic before the guard exists, which leaks whatever was created first. Clearing
        // our own names up front makes a rerun idempotent instead of failing on the previous run's
        // debris. Nothing here can touch a name we did not create.
        for name in ["mx-canary-listener-allowed", "mx-canary-listener-denied", "mx-live-holder-canary"] {
            docker(&["rm", "--force", "--volumes", name], None);
        }
        for net in [&allowed_net, &denied_net, &"mx-live-net-canary".to_owned()] {
            docker(&["network", "rm", net], None);
        }

        let fixture = Fixture::new("canary");
        for (net, subnet) in [(&allowed_net, allowed_subnet), (&denied_net, denied_subnet)] {
            let (ok, _, err) = docker(&["network", "create", "--subnet", subnet, net], None);
            assert!(
                ok,
                "could not create {net} on {subnet}: {err}\n\
                 If this says the pool overlaps, another network on this host already holds that \
                 subnet — pick a free one inside the same policy range rather than widening the test."
            );
        }

        let allowed_listener = "mx-canary-listener-allowed".to_owned();
        let denied_listener = "mx-canary-listener-denied".to_owned();
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

        Self {
            fixture,
            allowed_net,
            denied_net,
            allowed_listener,
            denied_listener,
            allowed_ip: ips[0].clone(),
            denied_ip: ips[1].clone(),
        }
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
        docker(&["rm", "--force", "--volumes", &self.fixture.holder], None);
        for name in [&self.allowed_listener, &self.denied_listener] {
            docker(&["rm", "--force", "--volumes", name], None);
        }
        for net in [&self.allowed_net, &self.denied_net] {
            docker(&["network", "rm", net], None);
        }
    }
}

/// `establish` end to end: it creates the holder, installs the policy, verifies it, and the holder it
/// hands back really is contained. Then dropping the containment removes the holder.
#[test]
#[ignore = "needs docker and the netfilter image"]
fn establish_contains_a_namespace_and_tears_it_down_on_drop() {
    let network = "mx-live-net-establish";
    let (ok, _, err) = docker(&["network", "create", network], None);
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
            docker(&["network", "rm", network], None);
            panic!("establish failed: {error}");
        }
    };

    // The guard's Drop is synchronous, so by here the holder must be gone.
    let (_, listed, _) = docker(&["ps", "--all", "--quiet", "--filter", &format!("name={holder_name}")], None);
    docker(&["network", "rm", network], None);
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

/// Connect from a container started under an explicit `--runtime`.
fn connect_under(runtime: &str, network: &str, ip: &str, port: &str) -> bool {
    let (ok, _, _) = docker(
        &[
            "run",
            "--rm",
            "--runtime",
            runtime,
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
    select_egress_link(&parse_links(&stdout))
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
    let network = "mx-live-net-iface";
    docker(&["network", "rm", network], None);
    let (ok, _, err) = docker(&["network", "create", network], None);
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
    ));

    let containment = match outcome {
        Ok(containment) => containment,
        Err(error) => {
            docker(&["network", "rm", network], None);
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
    docker(&["network", "rm", network], None);
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
        let network = "mx-runsc-net".to_owned();
        let listener = "mx-runsc-listener".to_owned();
        docker(&["rm", "--force", "--volumes", &listener], None);
        docker(&["network", "rm", &network], None);
        let (ok, _, err) =
            docker(&["network", "create", "--subnet", "203.0.113.0/24", &network], None);
        assert!(ok, "could not create {network}: {err}");

        // One process, both addresses: `nc -l` binds every local address, so a refusal can never be
        // "that one was not listening".
        let (ok, _, err) = docker(
            &[
                "run",
                "--detach",
                "--name",
                &listener,
                "--network",
                &network,
                "--cap-add",
                "NET_ADMIN",
                "--entrypoint",
                "sh",
                &netfilter_image(),
                "-c",
                &format!(
                    "ip addr add {}/32 dev eth0 && while :; do nc -l -p {} >/dev/null 2>&1; done",
                    Self::DENIED_IP,
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
        Self { network, listener, allowed_ip }
    }

    /// The same destination, from a container on the network but **outside** every contained
    /// namespace. A success proves the listener is alive, which is the one thing a refusal inside
    /// cannot distinguish itself from.
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
                &format!("ip route add {ip}/32 dev eth0 && nc -w 2 {ip} {}", Canary::PORT),
            ],
            None,
        );
        ok
    }
}

impl Drop for RunscNet {
    fn drop(&mut self) {
        docker(&["rm", "--force", "--volumes", &self.listener], None);
        docker(&["network", "rm", &self.network], None);
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
        let holder = format!("mx-live-payload-{tag}");
        docker(&["rm", "--force", "--volumes", &holder], None);
        let (ok, _, err) = docker(
            &[
                "run",
                "--detach",
                "--name",
                &holder,
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
    fn reach(&self, runtime: &str, ip: &str) -> bool {
        connect_under(runtime, &format!("container:{}", self.holder), ip, Canary::PORT)
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        docker(&["rm", "--force", "--volumes", &self.holder], None);
    }
}
