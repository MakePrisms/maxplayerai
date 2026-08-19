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

#![cfg(feature = "acp")]

use std::process::Command;

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
        1000,
        1000,
        Some(PortRange::new(49200, 49299).expect("valid range")),
        true,
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
