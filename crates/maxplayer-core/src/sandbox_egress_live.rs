//! Live attribution gate: does a contained gVisor payload reach denied destinations, and did that
//! start with the DNS patch or predate it?
//!
//! This module exists to answer exactly one question and then stop: **PREEXISTING or REGRESSION**,
//! for base `a0e7c3f` against DNS production `6f5a7e7`. It is deliberately not a DNS test. Every
//! probe is a **numeric IP**, so a baseline that cannot resolve at all still reports its true
//! direct connectivity — a DNS failure must never be allowed to masquerade as containment.
//!
//! **The path under test is the product's own**: `SandboxPolicy::from_config` →
//! `seller_exec::prepare_launch` (which establishes containment through the real netfilter sidecar
//! and reads the rules back out of the kernel) → `SandboxPolicy::launch`, the same argv builder the
//! daemon uses. Nothing here hand-renders a rule or hand-rolls a `docker run`.
//!
//! **Identity is proven, never assumed.** A previous gate in this repo silently measured `runc`
//! while reporting gVisor, because it checked the argv it asked for instead of the container that
//! ran. So before any probe result is believed, this module asserts three independent facts about
//! the live payload: `docker inspect` reports `runsc`, its `NetworkMode` is the holder's container
//! **id**, and its own kernel says gVisor from the inside.
//!
//! **Controls.** Denial and unroutability are indistinguishable from inside, and so are a dropped
//! packet and a listener that died. Every contained probe is therefore paired, in the same run and
//! against the same fixtures, with a `runc` containment run and an **unfiltered** run. The
//! unfiltered run is what proves the listeners were alive.
//!
//! This file is byte-identical in the baseline worktree except for one line, recorded in the
//! comparison notes: baseline's `JobLaunch` has no `resolv_conf` field, because the DNS patch added
//! it. No DNS server is configured and no resolver file is injected in either tree, so both
//! policies are compared with their resolver behaviour untouched and neither target sits behind an
//! authorised exception.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::home::{SandboxConfig, SandboxMode};
use crate::sandbox_net::Family;
use crate::sandbox_netns::readback_argv;
use crate::seller_exec::{JobLaunch, SandboxPolicy};
use crate::seller_git::DeliveryAgentIdentity;

/// The runtime under attribution. The whole question is about the gVisor seat, so a result measured
/// under the daemon default would be an answer to a different question.
const RUNTIME: &str = "runsc";
const PROBE_IMAGE: &str = "mxdns-probe:local";

const SVC_NET: &str = "mxegr-svc";
const SVC_V4: &str = "172.29.7.0/24";
const PUB_NET: &str = "mxegr-pub";
const PUB_V4: &str = "203.0.113.0/24";

/// A private address inside `172.16.0.0/12`, which both policies deny wholesale. It is an ordinary
/// listener, not a resolver, so no port-53 exception can carry a packet to it under either tree.
const PRIVATE_TARGET: &str = "172.29.7.20";
const PRIVATE_PORT: &str = "9999";
/// A **non-53** service on the address the DNS patch would treat as a resolver. Under the patched
/// tree with no resolver configured it has no exception at all; even with one it would cover port 53
/// only. Either way this port must be denied under both policies.
const NON53_TARGET: &str = "172.29.7.53";
const NON53_PORT: &str = "8080";
/// The allowed control: a public address no deny range covers. If this is unreachable the fixture
/// is broken and no denial below means anything.
const PUBLIC_TARGET: &str = "203.0.113.10";
const PUBLIC_PORT: &str = "9999";

fn docker(args: &[&str]) -> (bool, String, String) {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("docker must be runnable");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    )
}

/// Bring up both fixture networks and the three listeners, from a clean slate every run so no
/// result can be inherited from a previous one.
fn fixtures_up() {
    fixtures_down();
    let (ok, _, err) = docker(&["network", "create", "--subnet", SVC_V4, SVC_NET]);
    assert!(ok, "could not create {SVC_NET}: {err}");
    let (ok, _, err) = docker(&["network", "create", "--subnet", PUB_V4, PUB_NET]);
    assert!(ok, "could not create {PUB_NET}: {err}");

    listener("mxegr-priv", SVC_NET, PRIVATE_TARGET, PRIVATE_PORT);
    listener("mxegr-svc53", SVC_NET, NON53_TARGET, NON53_PORT);
    listener("mxegr-pub", PUB_NET, PUBLIC_TARGET, PUBLIC_PORT);
    // The listeners are shells in a loop; give them a moment to bind before anything probes them.
    std::thread::sleep(Duration::from_secs(2));
}

fn listener(name: &str, network: &str, ip: &str, port: &str) {
    let script = format!("while true; do nc -l -p {port} </dev/null >/dev/null 2>&1 || sleep 0.2; done");
    let (ok, _, err) = docker(&[
        "run", "-d", "--name", name, "--network", network, "--ip", ip, PROBE_IMAGE, "sh", "-c",
        &script,
    ]);
    assert!(ok, "could not start listener {name}: {err}");
}

fn fixtures_down() {
    for name in ["mxegr-priv", "mxegr-svc53", "mxegr-pub"] {
        let _ = docker(&["rm", "-f", name]);
    }
    // ⛔ The product deliberately does NOT pass `--rm` to a job container, so a completed payload
    // SURVIVES its run by design — that is how the capture path still has something to read. This
    // gate reuses one job id per runtime so the two trees are compared under identical names, which
    // means the previous tree's payload is still holding that name when the next tree runs. Clearing
    // them here is what makes the comparison repeatable; without it the second tree dies on a name
    // conflict that looks nothing like a containment result.
    for tag in ["runc", "runsc"] {
        let _ = docker(&["rm", "-f", &format!("maxplayer-job-egress-{tag}")]);
        let _ = docker(&["rm", "-f", &format!("maxplayer-netns-egress-{tag}")]);
    }
    for net in [SVC_NET, PUB_NET] {
        let _ = docker(&["network", "rm", net]);
    }
}

/// The seat config under test. Named fields are exactly those both trees share; everything else
/// comes from `Default`, which is what keeps this constructor identical across the comparison.
/// In particular no DNS server is named, so the patched tree installs no resolver exception.
fn config(runtime: Option<&str>) -> SandboxConfig {
    SandboxConfig {
        mode: SandboxMode::Docker,
        launcher: Vec::new(),
        image: Some(PROBE_IMAGE.to_owned()),
        forward_env: Vec::new(),
        runtime: runtime.map(str::to_owned),
        network: Some(SVC_NET.to_owned()),
        ..Default::default()
    }
}

fn identity() -> DeliveryAgentIdentity {
    DeliveryAgentIdentity::for_seller(&"ab".repeat(32))
}

/// A workdir whose last component is the job id the product derives container names from.
struct Workdir(std::path::PathBuf);

impl Workdir {
    fn new(job: &str) -> Self {
        let path = std::env::temp_dir().join("mxegr-gate").join(format!("egress-{job}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a workdir");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn job(&self) -> String {
        self.0.file_name().expect("a job id").to_string_lossy().into_owned()
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Attach the public fixture to a holder so one namespace routes to both networks. Without this the
/// public control is merely unroutable, which is indistinguishable from denied.
fn attach_public(holder: &str) {
    let (ok, _, err) = docker(&["network", "connect", PUB_NET, holder]);
    assert!(ok, "could not attach {PUB_NET} to {holder}: {err}");
}

fn readback(holder: &str, family: Family) -> String {
    let argv = readback_argv(holder, crate::sandbox_netns::DEFAULT_NETFILTER_IMAGE, family);
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let (ok, stdout, err) = docker(&args);
    assert!(ok, "{} readback failed: {err}", family.binary());
    stdout
}

/// What the daemon actually did with the payload: its runtime, and the namespace it really joined.
/// `NetworkMode` reports a container **id**, so callers resolve the holder's name before comparing.
fn payload_facts(job: &str) -> (String, String) {
    let name = format!("maxplayer-job-{job}");
    let (ok, out, err) = docker(&[
        "inspect",
        "--format",
        "{{.HostConfig.Runtime}} {{.HostConfig.NetworkMode}}",
        &name,
    ]);
    assert!(ok, "could not inspect the payload container {name}: {err}");
    let (runtime, network) = out.split_once(' ').unwrap_or((out.as_str(), ""));
    (runtime.to_owned(), network.to_owned())
}

/// Three numeric-IP TCP probes and the payload's own kernel banner. `nc -z` with a short timeout
/// distinguishes nothing by itself; the controls do that. Exit status per probe is printed so a
/// refusal and a timeout are not conflated in the record.
fn probe_script() -> String {
    format!(
        "set -u; \
         echo KERN=$(dmesg 2>/dev/null | head -1 | tr -d '\\n'); \
         nc -z -w3 {PRIVATE_TARGET} {PRIVATE_PORT} >/dev/null 2>&1; echo PRIV_RC=$?; \
         nc -z -w3 {NON53_TARGET} {NON53_PORT} >/dev/null 2>&1; echo NON53_RC=$?; \
         nc -z -w3 {PUBLIC_TARGET} {PUBLIC_PORT} >/dev/null 2>&1; echo PUB_RC=$?"
    )
}

fn run_payload(
    policy: &SandboxPolicy,
    uid: u32,
    gid: u32,
    workdir: &Path,
    netns: Option<&str>,
    script: &str,
) -> (bool, String, String) {
    let command: Vec<String> = ["sh", "-c", script].into_iter().map(String::from).collect();
    let launch = policy
        .launch(
            &command,
            &JobLaunch {
                workdir,
                env: &[],
                uid,
                gid,
                netns,
                // ⛔ BASELINE ADAPTATION: this field does not exist at a0e7c3f and is removed there.
                // It is `None` here so the patched tree injects no resolver file, keeping the two
                // policies comparable.
                resolv_conf: None,
            },
        )
        .expect("the policy must build a launch");
    let out = Command::new(&launch.program)
        .args(&launch.args)
        .stdin(Stdio::null())
        .output()
        .expect("the launch must spawn");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    )
}

fn field<'a>(stdout: &'a str, key: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("payload printed no {key}= line:\n{stdout}"))
}

/// `nc -z` exit 0 means the TCP handshake completed.
fn verdict(rc: &str) -> &'static str {
    if rc == "0" { "REACH" } else { "DENY" }
}

/// One contained run under a named runtime, reporting the three probe verdicts. Prints the rule
/// readback and the proven payload identity so the record stands on its own.
async fn contained_run(tag: &str, runtime: &str) -> (String, String, String) {
    let workdir = Workdir::new(tag);
    let config = config(Some(runtime));
    let policy = SandboxPolicy::from_config(Some(&config)).expect("a docker policy");
    let prepared = crate::seller_exec::prepare_launch(
        &["sh".to_owned()],
        &policy,
        workdir.path(),
        &identity(),
        Duration::from_secs(300),
    )
    .await
    .expect("containment must establish");

    let holder = prepared.holder_name.clone().expect("a holder");
    attach_public(&holder);
    println!("[{tag}] --- iptables readback ---\n{}", readback(&holder, Family::V4));
    println!("[{tag}] --- ip6tables readback ---\n{}", readback(&holder, Family::V6));

    let (_, out, err) = run_payload(
        &policy,
        prepared.uid,
        prepared.gid,
        workdir.path(),
        Some(&holder),
        &probe_script(),
    );
    assert!(!out.is_empty(), "[{tag}] payload printed nothing; stderr: {err}");

    let (got_runtime, network) = payload_facts(&workdir.job());
    let (ok, holder_id, ierr) = docker(&["inspect", "--format", "{{.Id}}", &holder]);
    assert!(ok, "could not resolve the holder id: {ierr}");
    assert_eq!(got_runtime, runtime, "[{tag}] payload ran under {got_runtime}, not {runtime}");
    assert_eq!(
        network,
        format!("container:{holder_id}"),
        "[{tag}] payload did not join the holder namespace"
    );
    if runtime == RUNTIME {
        let kern = field(&out, "KERN");
        assert!(kern.contains("gVisor"), "[{tag}] payload kernel is not gVisor: {kern}");
    }
    println!("[{tag}] runtime={got_runtime} netmode={network} kern={}", field(&out, "KERN"));

    let (priv_v, non53_v, pub_v) = (
        verdict(field(&out, "PRIV_RC")),
        verdict(field(&out, "NON53_RC")),
        verdict(field(&out, "PUB_RC")),
    );
    println!("[{tag}] PRIVATE={priv_v} NON53={non53_v} PUBLIC={pub_v}");
    (priv_v.to_owned(), non53_v.to_owned(), pub_v.to_owned())
}

/// The unfiltered control: the same image, the same networks, no policy in force. This is the only
/// thing that proves the listeners were alive, so it runs in the same test as the denials it backs.
fn unfiltered_control() -> (String, String, String) {
    let (ok, out, err) = docker(&[
        "run", "--rm", "--network", SVC_NET, PROBE_IMAGE, "sh", "-c",
        &format!(
            "nc -z -w3 {PRIVATE_TARGET} {PRIVATE_PORT} >/dev/null 2>&1; echo PRIV_RC=$?; \
             nc -z -w3 {NON53_TARGET} {NON53_PORT} >/dev/null 2>&1; echo NON53_RC=$?"
        ),
    ]);
    assert!(ok, "the unfiltered control must run: {err}");
    let pub_probe = docker(&[
        "run", "--rm", "--network", PUB_NET, PROBE_IMAGE, "sh", "-c",
        &format!("nc -z -w3 {PUBLIC_TARGET} {PUBLIC_PORT} >/dev/null 2>&1; echo PUB_RC=$?"),
    ]);
    assert!(pub_probe.0, "the unfiltered public control must run: {}", pub_probe.2);
    println!("[control] unfiltered: {out} {}", pub_probe.1);
    (
        verdict(field(&out, "PRIV_RC")).to_owned(),
        verdict(field(&out, "NON53_RC")).to_owned(),
        verdict(field(&pub_probe.1, "PUB_RC")).to_owned(),
    )
}

/// The attribution gate. It asserts only what must hold for the comparison to mean anything — live
/// listeners, a proven runtime, a real namespace join — and PRINTS the containment verdicts rather
/// than asserting them, because the interesting outcome is the one that differs between trees and a
/// panic would truncate the record.
#[tokio::test]
#[ignore = "live: needs docker, runsc and the mxdns-probe fixture image"]
async fn numeric_egress_under_runsc_compared_with_runc_and_unfiltered() {
    fixtures_up();

    // Controls first: if the listeners are not alive, nothing below is evidence.
    let (c_priv, c_non53, c_pub) = unfiltered_control();
    assert_eq!(c_priv, "REACH", "the private listener must be alive for a denial to mean anything");
    assert_eq!(c_non53, "REACH", "the non-53 listener must be alive");
    assert_eq!(c_pub, "REACH", "the public listener must be alive");

    let runc = contained_run("runc", "runc").await;
    let runsc = contained_run("runsc", RUNTIME).await;

    println!("=== ATTRIBUTION SUMMARY (this tree) ===");
    println!("unfiltered control : PRIVATE={c_priv} NON53={c_non53} PUBLIC={c_pub}");
    println!("contained runc     : PRIVATE={} NON53={} PUBLIC={}", runc.0, runc.1, runc.2);
    println!("contained runsc    : PRIVATE={} NON53={} PUBLIC={}", runsc.0, runsc.1, runsc.2);

    fixtures_down();
}
