//! Live DNS runtime gate: the verdict's "Minimal future Linux gate", executed against a real kernel.
//!
//! Everything else that tests [`crate::sandbox_dns`] asserts what is *rendered*. That is exactly the
//! gap the focused verdict left open: F1/F2/F3 are closed for the source contract, and nothing in the
//! repository has ever asked a running kernel whether a contained job can actually resolve a name
//! through the resolver file the launch wrote for it. A rendered `--dport 53 -j ACCEPT` and an
//! enforced one produce the same green.
//!
//! These tests are `#[ignore]`d: they need a docker daemon, the first-party netfilter image, and two
//! throwaway fixture images. They are HARNESS ONLY — they add no product behaviour and change none.
//! Run them inside an owned, isolated VM:
//!
//! ```text
//! docker build -t mxdns-resolver:local  <dnsmasq image>
//! docker build -t mxdns-probe:local     <bind-tools/git/nc image>
//! cargo test -p maxplayer-core --features acp,wallet --lib sandbox_dns_live -- --ignored --test-threads=1
//! ```
//!
//! **What each gate is for.** The product path under test is the real one, not a re-implementation:
//! [`crate::seller_exec::prepare_launch`] resolves the resolver set through
//! [`crate::sandbox_dns::resolve`], writes the job's `/etc/resolv.conf`, hands the SAME addresses to
//! [`crate::sandbox_netns::establish`], which installs the plan through the real sidecar and reads
//! both families back out of the kernel; the payload is then launched through
//! [`crate::seller_exec::SandboxPolicy::launch`], the same argv builder the daemon uses.
//!
//! **Controls.** Every denial is paired with a contemporaneous unfiltered probe of the same address
//! from outside the namespace. A destination that is merely unroutable, or a listener that died,
//! fails exactly like a dropped packet; only the control separates them.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::home::{SandboxConfig, SandboxMode};
use crate::sandbox_net::Family;
use crate::sandbox_netns::readback_argv;
use crate::seller_exec::{JobLaunch, SandboxPolicy};
use crate::seller_git::DeliveryAgentIdentity;

/// The private v4 network the resolver sits on. Inside `172.16.0.0/12`, which the policy denies
/// wholesale — so reaching the resolver here proves the per-resolver port-53 exception is what
/// carries the packet, not a hole in the deny.
const SVC_V4: &str = "172.29.7.0/24";
const RESOLVER_V4: &str = "172.29.7.53";
/// A second private address on the same subnet, running the same listener as the "public" one. It is
/// not a resolver, so nothing excepts it: this is the deny control.
const OTHER_PRIVATE_V4: &str = "172.29.7.20";
/// The private v6 network. Inside `fc00::/7`, denied for the same reason as its v4 twin.
const SVC_V6: &str = "fd00:d57::/64";
const RESOLVER_V6: &str = "fd00:d57::53";
/// TEST-NET-3 and the v6 documentation range: outside every denied CIDR, so the policy treats them
/// as public. Nothing here leaves the VM.
///
/// The "public" network is deliberately v4-ONLY. Attaching a second **v6** network to a holder that
/// is already contained fails inside docker itself — it advertises the new address to `ff02::1`, and
/// `ff00::/8` is one of the ranges the policy drops. That is the policy working, not a defect, but it
/// means this fixture cannot carry v6 on the network it attaches late. The v6 legs of this gate ride
/// the holder's own network, which is attached before containment exists.
const PUB_V4: &str = "203.0.113.0/24";
const PUB_HOST_V4: &str = "203.0.113.10";
/// Answered by the fixture resolver as data, to prove an AAAA lookup completes over v6 transport. Not
/// a destination this gate connects to.
const PUB_HOST_V6: &str = "2001:db8:aa::10";

/// The name the fixture resolver answers for, and the port every fixture listener listens on.
const DNS_NAME: &str = "git.gate.test";
/// A name whose TXT answer is deliberately larger than a 512-byte DNS message, so a non-EDNS query
/// comes back truncated and the resolver library must retry over TCP/53.
const BIG_NAME: &str = "big.gate.test";
const LISTEN_PORT: &str = "9999";
/// A second port on the resolver's own address. Port 53 is excepted; this one must not be.
const NON_DNS_PORT: &str = "8080";

const RESOLVER_IMAGE: &str = "mxdns-resolver:local";
const PROBE_IMAGE: &str = "mxdns-probe:local";
/// A disposable smart-HTTP Git endpoint, served over TLS because the product's own transport
/// allowlist refuses `http://`, `file://` and bare paths (`delivery_transport.rs:83-88`). Reached
/// ONLY by the DNS name below, so the delivery legs resolve through the resolver file the launch
/// wrote.
///
/// TLS verification stays ON everywhere in this module. The endpoint is issued a certificate by a
/// throwaway CA minted per run ([`fixture_tls`]), and every client is given that CA through its own
/// supported trust input. `GIT_SSL_NO_VERIFY` and `http.sslVerify=false` are NOT used: an endpoint
/// nobody authenticates is not the acceptance this gate exists to produce.
const GIT_IMAGE: &str = "mxgit-https:local";
const GIT_NAME: &str = "gitsrv.gate.test";
const GIT_HOST_V4: &str = "203.0.113.20";
const GIT_CT: &str = "mxdns-gate-git";
/// The pinned product sandbox image with a binary built from THIS source laid over it. The stock
/// image ships 0.5.8 from a different commit, which would answer a question about 0.5.8 rather than
/// about the source under test.
const DELIVERY_IMAGE: &str = "mxsandbox-6f5a7e7:local";
/// Where the delivery image keeps its trust store (Debian layout, read back out of the image).
const SYSTEM_CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
/// Branch names the delivery gate uses on the fixture remote.
const GIT_BASE_BRANCH: &str = "main";
/// The container runtime the payload must actually run under. The whole DNS problem this module
/// exists for — docker's embedded resolver at `127.0.0.11` never answering — is a gVisor property,
/// so a result measured under `runc` is not a result about it.
///
/// The netns holder and the netfilter sidecar are deliberately NOT bound to it: `holder_argv` and
/// `sidecar_argv` take no runtime, so on a gVisor seat the sandboxed job joins a `runc` holder's
/// namespace. That asymmetry is the product's own, and it is the configuration under test.
const RUNTIME: &str = "runsc";

const SVC_NET: &str = "mxdns-gate-svc";
const PUB_NET: &str = "mxdns-gate-pub";
const RESOLVER_CT: &str = "mxdns-gate-resolver";
const OTHER_CT: &str = "mxdns-gate-other";
const PUB_CT: &str = "mxdns-gate-pub-host";

fn docker(args: &[&str]) -> (bool, String, String) {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("docker must be on PATH for this gate");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    )
}

/// Two networks, a real caching resolver with fixed v4 and v6 addresses, and two plain TCP listeners
/// — one at a private address that nothing excepts, one at a public one.
///
/// Torn down on drop, by exact name. Nothing here removes an object it did not create.
struct Infra;

impl Infra {
    fn up() -> Self {
        // A previous run that panicked before its guard existed leaves debris under OUR names only.
        // Clearing them makes a rerun idempotent; it cannot touch anything else on the host.
        Self::down();

        let (ok, _, err) = docker(&[
            "network", "create", "--ipv6", "--subnet", SVC_V4, "--subnet", SVC_V6, SVC_NET,
        ]);
        assert!(ok, "could not create {SVC_NET} on {SVC_V4}/{SVC_V6}: {err}");
        let (ok, _, err) = docker(&["network", "create", "--subnet", PUB_V4, PUB_NET]);
        assert!(ok, "could not create {PUB_NET} on {PUB_V4}: {err}");

        // dnsmasq, answering A and AAAA for one name and an over-long TXT for another, bound to both
        // families at fixed addresses. `--no-resolv --no-hosts` so it forwards nothing: every answer
        // this gate sees came from this container, not from the VM's uplink.
        let txt = format!("{},{},{},{}", BIG_NAME, "x".repeat(250), "y".repeat(250), "z".repeat(250));
        let cmd = format!(
            "dnsmasq -k -p 53 --no-resolv --no-hosts --bind-interfaces \
             --listen-address={RESOLVER_V4} --listen-address={RESOLVER_V6} \
             --address=/{DNS_NAME}/{PUB_HOST_V4} --address=/{DNS_NAME}/{PUB_HOST_V6} \
             --address=/{GIT_NAME}/{GIT_HOST_V4} \
             --txt-record={txt} & \
             while :; do nc -l -p {NON_DNS_PORT} >/dev/null 2>&1; done"
        );
        let (ok, _, err) = docker(&[
            "run", "--detach", "--name", RESOLVER_CT, "--network", SVC_NET, "--ip", RESOLVER_V4,
            "--ip6", RESOLVER_V6, "--cap-add", "NET_BIND_SERVICE", "--entrypoint", "sh",
            RESOLVER_IMAGE, "-c", &cmd,
        ]);
        assert!(ok, "could not start the fixture resolver: {err}");

        for (name, net, ip) in
            [(OTHER_CT, SVC_NET, OTHER_PRIVATE_V4), (PUB_CT, PUB_NET, PUB_HOST_V4)]
        {
            let listen = format!("while :; do nc -l -p {LISTEN_PORT} >/dev/null 2>&1; done");
            let (ok, _, err) = docker(&[
                "run", "--detach", "--name", name, "--network", net, "--ip", ip, "--entrypoint",
                "sh", PROBE_IMAGE, "-c", &listen,
            ]);
            assert!(ok, "could not start listener {name}: {err}");
        }
        // The Git endpoint lives on the public net, at an address only the fixture resolver knows,
        // and serves the per-run fixture certificate. This is a FRESH container of this gate's own
        // (`GIT_CT`); no pre-existing container in the VM is reused, reconfigured or touched.
        let tls = fixture_tls();
        let (ok, _, err) = docker(&[
            "run", "--detach", "--name", GIT_CT, "--network", PUB_NET, "--ip", GIT_HOST_V4,
            "--volume", &format!("{}:/srv/tls:ro", tls.display()),
            GIT_IMAGE,
        ]);
        assert!(ok, "could not start the git endpoint: {err}");

        // The resolver has to be answering before any gate queries it; dnsmasq binds in well under a
        // second, but "well under" is not "before".
        for attempt in 0..30 {
            let (ok, out, _) = docker(&[
                "run", "--rm", "--network", SVC_NET, "--entrypoint", "dig", PROBE_IMAGE, "+short",
                "+time=1", "+tries=1", "A", DNS_NAME, &format!("@{RESOLVER_V4}"),
            ]);
            if ok && out.contains(PUB_HOST_V4) {
                return Self;
            }
            assert!(attempt < 29, "the fixture resolver never answered: {out}");
            std::thread::sleep(Duration::from_millis(300));
        }
        Self
    }

    fn down() {
        for name in [RESOLVER_CT, OTHER_CT, PUB_CT, GIT_CT] {
            docker(&["rm", "--force", "--volumes", name]);
        }
        // Holders and job containers are named by the product, from the job id. Only ids this gate
        // creates (`dnsgate-*`) are matched, so nothing else on the host is touched.
        let (_, listing, _) = docker(&["ps", "--all", "--format", "{{.Names}}"]);
        for name in listing.lines().filter(|line| line.contains("dnsgate-")) {
            docker(&["rm", "--force", "--volumes", name]);
        }
        for net in [SVC_NET, PUB_NET] {
            docker(&["network", "rm", net]);
        }
    }
}

impl Drop for Infra {
    fn drop(&mut self) {
        Self::down();
    }
}

/// The seat config every gate launches through, so each one exercises `SandboxPolicy::from_config`
/// rather than a hand-assembled policy.
fn config(dns_servers: Vec<String>) -> SandboxConfig {
    config_on(SVC_NET, dns_servers)
}

fn config_on(network: &str, dns_servers: Vec<String>) -> SandboxConfig {
    SandboxConfig {
        mode: SandboxMode::Docker,
        launcher: Vec::new(),
        image: Some(PROBE_IMAGE.to_owned()),
        forward_env: Vec::new(),
        // ⛔ NOT `None`. An unset runtime emits no `--runtime` flag at all, so the payload lands on
        // whatever the daemon defaults to — `runc` on this host. Every claim this module makes is
        // about the gVisor seat, and a gate that silently measured `runc` would be answering a
        // question nobody asked. The binding is asserted against the running container in
        // `payload_runtime`, not trusted because it is written here.
        runtime: Some(RUNTIME.to_owned()),
        network: Some(network.to_owned()),
        proxy_port_range: None,
        file_credentials: Vec::new(),
        dns_servers,
        codex_chatgpt: None,
        container_delivery: None,
        container_delivery_token: None,
        container_delivery_token_cap_secs: None,
    }
}

/// The same seat config, but launching the product sandbox image instead of the probe image.
fn config_with_image(image: &str, dns_servers: Vec<String>) -> SandboxConfig {
    let mut cfg = config(dns_servers);
    cfg.image = Some(image.to_owned());
    cfg
}

/// How many HTTP requests the endpoint has SERVED so far, from its own log. A TLS handshake the
/// endpoint aborts never reaches the request handler, so this counter not moving across a run is
/// the endpoint's own statement that the client never got past verification. (Python's
/// `socketserver` drops a failed `wrap_socket` as an ordinary `OSError` without logging it, so the
/// absence of a request line is the signal available here, not an error line.)
fn endpoint_requests_served() -> usize {
    let (_, out, err) = docker(&["logs", GIT_CT]);
    format!("{out}\n{err}")
        .lines()
        .filter(|line| line.contains("HTTP/1.1"))
        .count()
}

/// Ask the endpoint for the seeded base from OUTSIDE containment, over a connection whose
/// certificate is verified against the fixture CA. Used as a liveness check either side of the
/// negative control, so "refused" can be told apart from "dead".
fn endpoint_answers_trusted_client(base_oid: &str) -> bool {
    const LIVENESS_CT: &str = "mxdns-gate-git-live";
    docker(&["rm", "--force", "--volumes", LIVENESS_CT]);
    let (ok, _, err) = docker(&[
        "run", "--detach", "--name", LIVENESS_CT, "--network", SVC_NET, "--dns", RESOLVER_V4,
        "--volume", &format!("{}:/srv/tls:ro", tls_dir().display()),
        "--entrypoint", "sh", PROBE_IMAGE, "-c", "sleep 120",
    ]);
    assert!(ok, "could not start the liveness container: {err}");
    let (ok, _, err) = docker(&["network", "connect", PUB_NET, LIVENESS_CT]);
    assert!(ok, "could not attach {PUB_NET} to the liveness container: {err}");
    let (ok, out, _) = docker(&[
        "exec", LIVENESS_CT, "sh", "-c",
        &format!(
            "git -c http.sslCAInfo=/srv/tls/ca.pem ls-remote https://{GIT_NAME}/repo.git \
             refs/heads/{GIT_BASE_BRANCH}"
        ),
    ]);
    docker(&["rm", "--force", "--volumes", LIVENESS_CT]);
    ok && out.contains(base_oid)
}

/// Run a git query against the fixture remote, from the SERVER side.
fn git_server(args: &str) -> (bool, String, String) {
    docker(&["exec", GIT_CT, "sh", "-c", &format!("git --git-dir=/srv/git/repo.git {args}")])
}

/// Where this run's throwaway PKI lives on the host running the gate.
fn tls_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("mxdns-gate").join("tls")
}

/// Mint a throwaway CA and a certificate for [`GIT_NAME`], valid for one day, and build the trust
/// input each client needs. Returns the directory holding `cert.pem`, `key.pem`, `ca.pem` and
/// `bundle.pem`.
///
/// Why a CA at all: the order requires TLS verification to REMAIN ENABLED, so the fixture endpoint
/// has to be authenticatable rather than merely encrypted. `bundle.pem` is the delivery image's own
/// `ca-certificates.crt` with this CA APPENDED — the public roots are not replaced or removed, one
/// throwaway issuer is added, and it exists only inside this VM for the life of the run.
///
/// The SANs cover the DNS name the contained job uses, the endpoint's address, and loopback (the
/// server's own readiness probe), so no client anywhere in this module needs its verification
/// relaxed to talk to it.
fn fixture_tls() -> std::path::PathBuf {
    let dir = tls_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a tls directory");
    let path = dir.display().to_string();

    let script = format!(
        "set -e; cd {path}; \
         printf 'subjectAltName=DNS:{GIT_NAME},IP:{GIT_HOST_V4},IP:127.0.0.1\\n\
         basicConstraints=CA:FALSE\\nextendedKeyUsage=serverAuth\\n' > ext.cnf; \
         openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 1 \
           -subj '/CN=mxdns gate throwaway CA' >/dev/null 2>&1; \
         openssl req -newkey rsa:2048 -nodes -keyout key.pem -out srv.csr \
           -subj '/CN={GIT_NAME}' >/dev/null 2>&1; \
         openssl x509 -req -in srv.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out cert.pem \
           -days 1 -extfile ext.cnf >/dev/null 2>&1; \
         chmod 0644 cert.pem ca.pem; chmod 0600 key.pem ca.key"
    );
    let out = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .output()
        .expect("openssl must run");
    assert!(
        out.status.success(),
        "could not mint the fixture PKI: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The delivery image's own trust store, with this run's CA appended. Read out of the image
    // rather than assumed, so the bundle the container verifies against is the one it shipped.
    let (ok, roots, err) = docker(&[
        "run", "--rm", "--entrypoint", "cat", DELIVERY_IMAGE, SYSTEM_CA_BUNDLE,
    ]);
    assert!(ok, "could not read {SYSTEM_CA_BUNDLE} out of {DELIVERY_IMAGE}: {err}");
    let fixture_ca = std::fs::read_to_string(dir.join("ca.pem")).expect("the fixture CA");
    std::fs::write(dir.join("bundle.pem"), format!("{roots}\n{fixture_ca}"))
        .expect("the combined bundle");
    // The image's roots WITHOUT this run's CA — the negative control's trust store. A gate that
    // only ever runs with the right CA cannot tell verification from indifference.
    std::fs::write(dir.join("roots.pem"), &roots).expect("the roots-only bundle");
    dir
}

/// Where the fixture PKI is mounted inside the delivery container.
const CONTAINER_TLS_DIR: &str = "/maxplayer-tls";

/// What one contained delivery produced, kept together so a caller can run the SAME path twice and
/// compare. `_workdir` is held because dropping it removes the job directory the assertions read.
struct DeliveryRun {
    ok: bool,
    out: String,
    err: String,
    outcome: Option<crate::delivery_orchestrator::Phase1Outcome>,
    io_dir: std::path::PathBuf,
    job: String,
    branch: String,
    holder: String,
    _workdir: Workdir,
    /// Held because [`PreparedLaunch`](crate::seller_exec::PreparedLaunch) OWNS the containment:
    /// dropping it removes the netns holder synchronously (`sandbox_netns.rs:103`), and the
    /// assertions below read that holder back out of docker.
    _prepared: crate::seller_exec::PreparedLaunch,
}

/// Drive one `maxplayer __deliver phase1` inside containment, through the product's own launch
/// builder — holder namespace, written resolver file, seller uid/gid, host-owned exchange mount.
///
/// `ca_file` names the trust store the container is handed through `SSL_CERT_FILE`, and it is the
/// ONLY thing the caller varies between the accepted run and the negative control: `bundle.pem`
/// (image roots + this run's CA) must deliver, `roots.pem` (image roots alone) must not. TLS
/// verification is never disabled in either run.
async fn contained_delivery(
    job_name: &str,
    remote: &str,
    base_oid: &str,
    ca_file: &str,
) -> DeliveryRun {
    use crate::delivery_orchestrator as orch;

    let workdir = Workdir::new(job_name);
    let job = workdir.job();
    let config = config_with_image(
        DELIVERY_IMAGE,
        vec![RESOLVER_V4.to_owned(), RESOLVER_V6.to_owned()],
    );
    let policy = SandboxPolicy::from_config(Some(&config)).expect("a docker policy");

    // The SAME preparation the delivery path performs: containment, holder, resolver file, uid/gid
    // and the agent argv the orchestrator will drive inside the container.
    let agent_argv = delivery_agent_argv();
    let prepared = crate::seller_exec::prepare_launch(
        &agent_argv,
        &policy,
        workdir.path(),
        &identity(),
        Duration::from_secs(900),
    )
    .await
    .expect("containment must be established");
    let holder = prepared.holder_name.clone().expect("a holder");
    let resolv = prepared.resolv_conf.clone().expect("a resolver file");
    // The endpoint lives on the public net; without a route "unreachable" and "denied" are the same
    // failure, and neither would be a statement about DNS.
    attach_public(&holder);

    // The host-owned exchange directory, outside the workdir, exactly as the daemon places it.
    let io_dir = std::env::temp_dir().join("mxdns-gate").join(format!("io-{job}"));
    orch::create_exchange_dir(&io_dir).expect("an exchange directory");

    let branch = format!("maxplayer/{job}");
    let inputs = orch::Phase1Inputs {
        job_hash: "cd".repeat(32),
        seller_pubkey_hex: identity().seller_pubkey_hex().to_owned(),
        base: Some(orch::Phase1BaseOwned {
            clone_url: remote.to_owned(),
            branch: GIT_BASE_BRANCH.to_owned(),
            oid: base_oid.to_owned(),
        }),
        delivery_branch: branch.clone(),
        message: "delivery: live DNS/Git gate".to_owned(),
        author_date_unix: 1_760_000_000,
        agent_argv: prepared.effective_command.clone(),
        workdir: std::path::PathBuf::from(crate::seller_exec::CONTAINER_WORKDIR),
        out_dir: std::path::PathBuf::from(orch::CONTAINER_EXCHANGE_DIR),
        prompt: "write the deliverable".to_owned(),
        deadline_unix: (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after the epoch")
            .as_secs())
            + 600,
        max_agent_attempts: 1,
        agent_env_names: prepared.env.iter().map(|(key, _)| key.clone()).collect(),
        relay_url: remote.to_owned(),
        // A non-relay https remote takes no header — the same arm the host selects.
        push_token: orch::PushTokenSource::None,
        handoff_nonce: "ef".repeat(16),
    };
    orch::write_phase1_inputs(&io_dir.join(orch::PHASE1_INPUTS_FILE), &inputs)
        .expect("the inputs file must be written");

    let mut launch_env = prepared.env.clone();
    launch_env.push((
        orch::CONTAINER_DELIVERY_ENV.to_owned(),
        orch::CONTAINER_DELIVERY_ENV_VALUE.to_owned(),
    ));
    // Trust, not bypass: a CA file rides in on the standard `SSL_CERT_FILE` input, from a read-only
    // mount. Certificate verification stays ENABLED, so the endpoint the contained job pushes to
    // must actually authenticate as `gitsrv.gate.test`.
    launch_env.push((
        "SSL_CERT_FILE".to_owned(),
        format!("{CONTAINER_TLS_DIR}/{ca_file}"),
    ));

    let orchestrator: Vec<String> = vec![
        orch::CONTAINER_ORCHESTRATOR_BIN.to_owned(),
        "__deliver".to_owned(),
        "phase1".to_owned(),
        format!("{}/{}", orch::CONTAINER_EXCHANGE_DIR, orch::PHASE1_INPUTS_FILE),
    ];
    let (ok, out, err) = run_delivery(
        &policy,
        prepared.uid,
        prepared.gid,
        workdir.path(),
        Some(&holder),
        Some(resolv.as_path()),
        &orchestrator,
        &launch_env,
        &[
            (io_dir.clone(), orch::CONTAINER_EXCHANGE_DIR.to_owned()),
            // `:ro` rides on the destination because the mount list is rendered straight into
            // `-v host:dest` (`seller_exec.rs:899-901`); the job must not be able to edit the
            // trust store it is verified against.
            (tls_dir(), format!("{CONTAINER_TLS_DIR}:ro")),
        ],
    );
    // The outcome file is written on EVERY exit, so it carries the reason when the run failed.
    let outcome = orch::read_outcome(&io_dir).ok().flatten();
    DeliveryRun {
        ok,
        out,
        err,
        outcome,
        io_dir,
        job,
        branch,
        holder,
        _workdir: workdir,
        _prepared: prepared,
    }
}

/// Seed the fixture remote with exactly one base commit, and do not return until the endpoint
/// actually serves smart-HTTP over TLS. Returns the base oid.
///
/// System `git` here is FIXTURE construction, on the server side of the wire. The client side --
/// the thing under test -- is the product's own in-process libgit2, running inside the contained
/// job container.
fn seed_git_fixture() -> String {
    let script = format!(
        "set -e; \
         git config --global user.email fixture@gate.test; \
         git config --global user.name fixture; \
         rm -rf /srv/git/repo.git /tmp/seed; \
         git init --bare -q /srv/git/repo.git; \
         git --git-dir=/srv/git/repo.git symbolic-ref HEAD refs/heads/{GIT_BASE_BRANCH}; \
         git init -q /tmp/seed; cd /tmp/seed; \
         echo base > BASE.txt; git add -A; git commit -q -m base; \
         git branch -M {GIT_BASE_BRANCH}; \
         git push -q /srv/git/repo.git {GIT_BASE_BRANCH}; \
         git rev-parse HEAD"
    );
    let (ok, out, err) = docker(&["exec", GIT_CT, "sh", "-c", &script]);
    assert!(ok, "could not seed the git fixture: {out}\n{err}");
    let oid = out.lines().last().unwrap_or_default().trim().to_owned();
    assert_eq!(oid.len(), 40, "the seed did not yield a commit oid: {out}\n{err}");

    // Serving over TLS is a separate readiness fact from the repository existing. The probe
    // VERIFIES the certificate against the fixture CA — a readiness check that accepted any
    // certificate would also accept a broken one.
    let probe = format!(
        "git -c http.sslCAInfo=/srv/tls/ca.pem ls-remote https://127.0.0.1/repo.git \
         refs/heads/{GIT_BASE_BRANCH}"
    );
    for attempt in 0..40 {
        let (ok, out, _) = docker(&["exec", GIT_CT, "sh", "-c", &probe]);
        if ok && out.contains(&oid) {
            return oid;
        }
        assert!(attempt < 39, "the git endpoint never served {oid} over TLS: {out}");
        std::thread::sleep(Duration::from_millis(250));
    }
    oid
}

/// Launch a real argv (not a shell probe) in the contained namespace, with environment and the
/// exchange-directory mount, through the product's own launch builder -- the same
/// `launch_with_mounts` entry the seller's delivery path uses.
fn run_delivery(
    policy: &SandboxPolicy,
    prepared_uid: u32,
    prepared_gid: u32,
    workdir: &Path,
    netns: Option<&str>,
    resolv: Option<&Path>,
    command: &[String],
    env: &[(String, String)],
    mounts: &[(std::path::PathBuf, String)],
) -> (bool, String, String) {
    let launch = policy
        .launch_with_mounts(
            command,
            &JobLaunch {
                workdir,
                env,
                uid: prepared_uid,
                gid: prepared_gid,
                netns,
                resolv_conf: resolv,
            },
            mounts,
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

fn identity() -> DeliveryAgentIdentity {
    DeliveryAgentIdentity::for_seller(&"ab".repeat(32))
}

/// A workdir whose last component is the job id the product derives holder names from.
struct Workdir(std::path::PathBuf);

impl Workdir {
    fn new(job: &str) -> Self {
        let path = std::env::temp_dir().join("mxdns-gate").join(format!("dnsgate-{job}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a workdir");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    /// The job id the product derives the holder and payload container names from.
    fn job(&self) -> String {
        self.0.file_name().expect("a job id").to_string_lossy().into_owned()
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Attach the "public" network to a holder, so one namespace has a route to both fixtures. Without
/// it the public listener is unroutable, and unroutable fails exactly like denied.
fn attach_public(holder: &str) {
    let (ok, _, err) = docker(&["network", "connect", PUB_NET, holder]);
    assert!(ok, "could not attach {PUB_NET} to {holder}: {err}");
}

/// Read one family's rules back out of a live namespace through the product's own readback argv.
fn readback(holder: &str, family: Family) -> String {
    let argv = readback_argv(holder, crate::sandbox_netns::DEFAULT_NETFILTER_IMAGE, family);
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let (ok, stdout, err) = docker(&args);
    assert!(ok, "{} readback failed: {err}", family.binary());
    stdout
}

/// Run one shell payload in a namespace through the real argv builder, returning its stdout.
fn run_payload(policy: &SandboxPolicy, prepared_uid: u32, prepared_gid: u32, workdir: &Path,
    netns: Option<&str>, resolv: Option<&Path>, script: &str) -> (bool, String, String) {
    let command: Vec<String> =
        ["sh", "-c", script].into_iter().map(String::from).collect();
    let launch = policy
        .launch(
            &command,
            &JobLaunch {
                workdir,
                env: &[],
                uid: prepared_uid,
                gid: prepared_gid,
                netns,
                resolv_conf: resolv,
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

/// The payload every containment gate runs: it reports what its own resolver file is, whether that
/// file is writable, what it can resolve over each family's transport, and which of four TCP
/// destinations it can reach.
/// The runtime the payload container ACTUALLY ran under, read back out of docker after the fact.
///
/// The launch argv is what we asked for; this is what the daemon did. They are not the same claim,
/// and one gate in this module already shipped a wrong headline because only the first was checked.
fn payload_runtime(job: &str) -> (String, String) {
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

/// Prove, against the RUNNING container, that the payload really executed under `runsc` inside the
/// holder's namespace. Three independent facts, because any one alone is forgeable by a
/// misconfiguration:
///
/// 1. docker's own view of the job's runtime,
/// 2. the payload's own kernel banner (`runsc` reports a gVisor kernel; `runc` reports the host's),
/// 3. the job's `NetworkMode` resolving to the holder container — without which a "reachable"
///    result would just mean the job never joined the namespace the rules are in.
///
/// Every gate that launches a payload calls this. A gate that does not is a runc-era row and does
/// not count as a `runsc` result.
fn prove_runsc(job: &str, holder: &str, out: &str) {
    let (runtime, network) = payload_runtime(job);
    assert_eq!(
        runtime, RUNTIME,
        "the payload ran under {runtime}, not {RUNTIME} — these are not gVisor results"
    );
    let kern = field(out, "KERN");
    assert!(
        kern.contains("gVisor"),
        "the payload's own kernel does not identify as gVisor ({kern}) — docker reported \
         runtime={runtime}, and the two must agree before any result here is a gVisor result"
    );
    // `NetworkMode` reports the RESOLVED container id, not the name the argv asked for, so the
    // holder name is resolved to its id before comparing. Comparing against the name fails even
    // when the join is correct.
    let (ok, holder_id, err) = docker(&["inspect", "--format", "{{.Id}}", holder]);
    assert!(ok, "could not resolve the holder's id: {err}");
    assert_eq!(
        network,
        format!("container:{holder_id}"),
        "the payload did not join the holder's namespace (holder {holder} = {holder_id})"
    );
    println!("payload runtime={runtime} network={network}");
    println!("payload kernel banner={kern}");
}

fn probe_script() -> String {
    format!(
        "set -u; \
         echo KERN=$(dmesg 2>/dev/null | head -1 | tr -d '\\n'); \
         echo SHA=$(sha256sum /etc/resolv.conf | cut -d' ' -f1); \
         if echo tampered >> /etc/resolv.conf 2>/dev/null; then echo RO=no; else echo RO=yes; fi; \
         echo DEFAULT=$(dig +short +time=2 +tries=1 A {DNS_NAME} | head -1); \
         echo V4T=$(dig +short +time=2 +tries=1 A {DNS_NAME} @{RESOLVER_V4} | head -1); \
         echo V6T=$(dig +short +time=2 +tries=1 AAAA {DNS_NAME} @{RESOLVER_V6} | head -1); \
         nc -w 2 {PUB_HOST_V4} {LISTEN_PORT} </dev/null >/dev/null 2>&1 && echo PUB=reach || echo PUB=deny; \
         nc -w 2 {OTHER_PRIVATE_V4} {LISTEN_PORT} </dev/null >/dev/null 2>&1 && echo PRIV=reach || echo PRIV=deny; \
         nc -w 2 {RESOLVER_V4} {NON_DNS_PORT} </dev/null >/dev/null 2>&1 && echo NON53=reach || echo NON53=deny"
    )
}

/// What one full gate-A launch observed. Produced once by [`gate_a_scenario`] and then asserted on
/// by two SEPARATE tests, because the two questions have different standing:
///
/// - **Functional DNS** (resolver bytes, read-only mount, both-family resolution) — the thing this
///   lane is authorized to finish and certify.
/// - **Network protection** (other-private and non-53 denial) — a PREEXISTING defect, established
///   at base `a0e7c3f` as well as at `6f5a7e7`, tracked as its own OPEN item and NOT this lane's to
///   fix, chase, or convert into a pass.
///
/// They are split into two tests rather than one so that membership is explicit in the runner
/// output: the functional gates can complete and be reported green WITHOUT the deny assertions
/// being deleted, weakened, or silently absorbed into a passing test. The deny assertions still
/// exist, still run, and still fail under `runsc`.
struct GateA {
    out: String,
    err: String,
    host_sha: String,
    v4_rules: String,
    v6_rules: String,
}

/// One full product launch: `prepare_launch` → `sandbox_dns::resolve` → the resolver file →
/// `establish` → the sidecar → both-family readback → `SandboxPolicy::launch`, with the runtime
/// identity of the resulting payload proven before a single probe result is believed.
async fn gate_a_scenario(tag: &str) -> GateA {
    let workdir = Workdir::new(tag);

    // The unfiltered control FIRST, contemporaneous with the gate and on the same fixtures: every
    // destination below is reachable when nothing is enforcing. A later denial is then attributable
    // to the rules rather than to a dead listener or an unroutable address.
    let (_, control, control_err) = {
        let script = probe_script();
        let (ok, out, err) = docker(&[
            "run", "--rm", "--network", SVC_NET, "--entrypoint", "sh", PROBE_IMAGE, "-c", &script,
        ]);
        assert!(ok || !out.is_empty(), "the control payload produced nothing: {err}");
        (ok, out, err)
    };
    // The control container is on SVC_NET only, so the public listener is unroutable there; it is
    // reached in the contained case through the attached PUB_NET. What the control has to show is the
    // private pair, which is exactly what the policy denies.
    assert_eq!(field(&control, "PRIV"), "reach", "control: {control}\n{control_err}");
    assert_eq!(field(&control, "NON53"), "reach", "control: {control}\n{control_err}");
    assert_eq!(field(&control, "V4T"), PUB_HOST_V4, "control: {control}");
    assert_eq!(field(&control, "V6T"), PUB_HOST_V6, "control: {control}");

    let config = config(vec![RESOLVER_V4.to_owned(), RESOLVER_V6.to_owned()]);
    let policy = SandboxPolicy::from_config(Some(&config)).expect("a docker policy");
    let prepared = crate::seller_exec::prepare_launch(
        &["true".to_owned()],
        &policy,
        workdir.path(),
        &identity(),
        Duration::from_secs(300),
    )
    .await
    .expect("containment must be established");

    let holder = prepared.holder_name.clone().expect("a holder");
    let resolv = prepared.resolv_conf.clone().expect("a resolver file");
    let body = std::fs::read(&resolv).expect("the resolver file must exist on the host");
    let host_sha = sha256_hex(&body);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains(&format!("nameserver {RESOLVER_V4}")), "{text}");
    assert!(text.contains(&format!("nameserver {RESOLVER_V6}")), "{text}");

    attach_public(&holder);
    // Read the kernel back AFTER a second interface joined the namespace. Both families, and both
    // resolvers, asked of the namespace rather than of the policy struct: an exception that renders
    // and is not in force is precisely the failure the source-only verdict could not rule out.
    let v4_rules = readback(&holder, Family::V4);
    assert!(v4_rules.contains(RESOLVER_V4), "no v4 resolver exception in the kernel:\n{v4_rules}");
    let v6_rules = readback(&holder, Family::V6);
    assert!(v6_rules.contains(RESOLVER_V6), "no v6 resolver exception in the kernel:\n{v6_rules}");

    // Named `payload_err`, not `err`: the holder-id lookup below also binds an `err`, and a plain
    // `err` here would be shadowed by it — handing every functional failure message the stderr of
    // `docker inspect` instead of the stderr of the payload that actually failed.
    let (_, out, payload_err) = run_payload(
        &policy,
        prepared.uid,
        prepared.gid,
        workdir.path(),
        Some(&holder),
        Some(resolv.as_path()),
        &probe_script(),
    );
    // The runtime binding, asserted against the RUNNING container before anything else is read
    // from it. Every assertion made by either caller is a claim about a gVisor job, and none of
    // them means that unless this holds.
    prove_runsc(&workdir.job(), &holder, &out);
    println!("--- iptables readback of the namespace the payload is in ---\n{v4_rules}");

    GateA { out, err: payload_err, host_sha, v4_rules, v6_rules }
}

/// **Gate A — FUNCTIONAL membership.** A seat with configured private resolvers launches a contained
/// job that resolves a name over both families, from inside the namespace, through the exact file
/// the launch wrote, mounted read-only.
///
/// This is the DNS functional contract and nothing else. It deliberately makes NO claim about
/// whether the namespace denies anything; see
/// [`known_open_preexisting_defect_other_private_and_non53_must_be_denied`] for that, which is a
/// separate OPEN item on its own track.
#[tokio::test]
#[ignore = "needs docker, the netfilter image and the two fixture images"]
async fn functional_a_contained_job_resolves_over_private_v4_and_v6_through_the_written_file() {
    let _infra = Infra::up();
    let GateA { out, err, host_sha, .. } = gate_a_scenario("gatea-fn").await;

    assert_eq!(field(&out, "SHA"), host_sha, "the job's resolver file is not the one written: {out}\n{err}");
    assert_eq!(field(&out, "RO"), "yes", "the resolver file must be mounted read-only: {out}");
    assert_eq!(field(&out, "DEFAULT"), PUB_HOST_V4, "the written file must resolve: {out}\n{err}");
    assert_eq!(field(&out, "V4T"), PUB_HOST_V4, "private v4 DNS must answer: {out}\n{err}");
    assert_eq!(field(&out, "V6T"), PUB_HOST_V6, "private v6 DNS must answer: {out}\n{err}");
    assert_eq!(field(&out, "PUB"), "reach", "a public destination must stay allowed: {out}");
}

/// **Gate A — KNOWN-OPEN SECURITY membership. This test is EXPECTED TO FAIL under `runsc`.**
///
/// The contained namespace must deny other private destinations and non-53 ports on the resolver.
/// Under `runc` it does (measured). Under `runsc` it does not: both probes reach, while the kernel
/// readback shows the DROP rules installed. That failure is PREEXISTING — reproduced identically at
/// base `a0e7c3f`, which predates all DNS work — so it is a runtime/enforcement defect, not a
/// regression from `6f5a7e7`.
///
/// It is recorded here, still asserting, on purpose. The order that authorized finishing the
/// functional gates also forbids converting this to a pass or hiding it by removing assertions, and
/// forbids this lane from repairing or attributing it. So it stays red and stays cited.
#[tokio::test]
#[ignore = "KNOWN OPEN: fails under runsc (preexisting enforcement defect, tracked separately)"]
async fn known_open_preexisting_defect_other_private_and_non53_must_be_denied() {
    let _infra = Infra::up();
    let GateA { out, v4_rules, v6_rules, .. } = gate_a_scenario("gatea-sec").await;

    // Printed so a failure carries its own disproof of "the rules were never installed".
    println!("--- v4 rules in force ---\n{v4_rules}\n--- v6 rules in force ---\n{v6_rules}");
    assert_eq!(field(&out, "PRIV"), "deny", "another private address must be denied: {out}");
    assert_eq!(field(&out, "NON53"), "deny", "a non-53 port on the resolver must be denied: {out}");
}

/// **Diagnosis.** Not a gate: it prints what the kernel holds and what the namespace can do over v6,
/// so a v6 DNS failure can be attributed rather than guessed at. Kept because the attribution it
/// produces is the whole difference between "v6 resolvers do not work" and a named cause.
#[tokio::test]
#[ignore = "diagnostic, not a gate"]
async fn diagnose_v6_resolver_reachability_inside_containment() {
    let _infra = Infra::up();
    let workdir = Workdir::new("gatef");
    let config = config(vec![RESOLVER_V4.to_owned(), RESOLVER_V6.to_owned()]);
    let policy = SandboxPolicy::from_config(Some(&config)).expect("a docker policy");
    let prepared = crate::seller_exec::prepare_launch(
        &["true".to_owned()],
        &policy,
        workdir.path(),
        &identity(),
        Duration::from_secs(300),
    )
    .await
    .expect("containment must be established");
    let holder = prepared.holder_name.clone().expect("a holder");
    let resolv = prepared.resolv_conf.clone().expect("a resolver file");

    println!("--- ip6tables readback ---\n{}", readback(&holder, Family::V6));
    // One container, three attempts, printing the neighbour cache around each. If the first query
    // fails and a later one succeeds once the cache holds a link-layer address, the cause is
    // neighbour discovery being dropped, not the port-53 exception being absent.
    let script = format!(
        "echo N0=$(ip -6 neigh show | tr '\\n' '|'); \
         echo Q1=$(dig +time=2 +tries=1 +short AAAA {DNS_NAME} @{RESOLVER_V6} 2>&1 | tr '\\n' ' '); \
         echo N1=$(ip -6 neigh show | tr '\\n' '|'); \
         sleep 3; \
         echo Q2=$(dig +time=3 +tries=1 +short AAAA {DNS_NAME} @{RESOLVER_V6} 2>&1 | tr '\\n' ' '); \
         echo N2=$(ip -6 neigh show | tr '\\n' '|'); \
         echo P6=$(ping6 -c 1 -W 2 {RESOLVER_V6} >/dev/null 2>&1 && echo reach || echo deny); \
         echo Q3=$(dig +time=3 +tries=1 +short AAAA {DNS_NAME} @{RESOLVER_V6} 2>&1 | tr '\\n' ' '); \
         echo Q4=$(dig +time=3 +tries=1 +short A {DNS_NAME} @{RESOLVER_V4} 2>&1 | tr '\\n' ' ')"
    );
    let (_, out, err) = run_payload(
        &policy, prepared.uid, prepared.gid, workdir.path(), Some(&holder),
        Some(resolv.as_path()), &script,
    );
    println!("--- v6 attribution ---\n{out}\n[stderr] {err}");
}

/// **Attribution, part two.** The same launch with a resolver in the v6 documentation range —
/// `2001:db8::/32`, which the policy denies nowhere — on its own network.
///
/// This is the discriminator. If v6 DNS works here and not at a ULA address, the cause is the
/// `fc00::/7` DROP catching neighbour discovery to the resolver itself. If it fails here too, the
/// cause is the `ff00::/8` DROP catching the multicast solicitation, and NO v6 resolver on a link
/// that needs discovery can work under this policy.
#[tokio::test]
#[ignore = "diagnostic, not a gate"]
async fn diagnose_v6_resolver_outside_the_denied_ranges() {
    const NET: &str = "mxdns-gate-doc";
    const RES: &str = "mxdns-gate-doc-resolver";
    const ADDR: &str = "2001:db8:bb::53";
    docker(&["rm", "--force", "--volumes", RES]);
    docker(&["network", "rm", NET]);
    let (ok, _, err) = docker(&[
        "network", "create", "--ipv6", "--subnet", "192.0.2.0/24", "--subnet",
        "2001:db8:bb::/64", NET,
    ]);
    assert!(ok, "could not create {NET}: {err}");
    let cmd = format!(
        "dnsmasq -k -p 53 --no-resolv --no-hosts --bind-interfaces --listen-address={ADDR} \
         --address=/{DNS_NAME}/{PUB_HOST_V6}"
    );
    let (ok, _, err) = docker(&[
        "run", "--detach", "--name", RES, "--network", NET, "--ip6", ADDR, "--entrypoint", "sh",
        RESOLVER_IMAGE, "-c", &cmd,
    ]);
    assert!(ok, "could not start the documentation-range resolver: {err}");
    std::thread::sleep(Duration::from_secs(2));

    let workdir = Workdir::new("gateg");
    let config = config_on(NET, vec![ADDR.to_owned()]);
    let policy = SandboxPolicy::from_config(Some(&config)).expect("a docker policy");
    let prepared = crate::seller_exec::prepare_launch(
        &["true".to_owned()],
        &policy,
        workdir.path(),
        &identity(),
        Duration::from_secs(300),
    )
    .await
    .expect("containment must be established");
    let holder = prepared.holder_name.clone().expect("a holder");
    let resolv = prepared.resolv_conf.clone().expect("a resolver file");
    println!("--- ip6tables readback ---\n{}", readback(&holder, Family::V6));

    let script = format!(
        "echo UNC=$(dig +time=3 +tries=1 +short AAAA {DNS_NAME} @{ADDR} 2>&1 | tr '\\n' ' '); \
         echo NEIGH=$(ip -6 neigh show | tr '\\n' '|')"
    );
    let (_, out, err) = run_payload(
        &policy, prepared.uid, prepared.gid, workdir.path(), Some(&holder),
        Some(resolv.as_path()), &script,
    );
    println!("--- contained, resolver outside every denied range ---\n{out}\n[stderr] {err}");

    // The contemporaneous control on the same fixture, with no policy in force.
    let (_, control, _) = docker(&[
        "run", "--rm", "--network", NET, "--entrypoint", "dig", PROBE_IMAGE, "+short", "+time=3",
        "+tries=1", "AAAA", DNS_NAME, &format!("@{ADDR}"),
    ]);
    println!("--- uncontained control ---\n{control}");

    let (_, listing, _) = docker(&["ps", "--all", "--format", "{{.Names}}"]);
    for name in listing.lines().filter(|line| line.contains("dnsgate-gateg")) {
        docker(&["rm", "--force", "--volumes", name]);
    }
    docker(&["rm", "--force", "--volumes", RES]);
    docker(&["network", "rm", NET]);
}

/// **Gate B.** A UDP answer too large for a 512-byte message comes back truncated, and the retry over
/// TCP/53 — a different rule in the plan — succeeds from inside the same namespace.
#[tokio::test]
#[ignore = "needs docker, the netfilter image and the two fixture images"]
async fn a_truncated_udp_answer_falls_back_to_tcp_53_inside_containment() {
    let _infra = Infra::up();
    let workdir = Workdir::new("gateb");
    let config = config(vec![RESOLVER_V4.to_owned(), RESOLVER_V6.to_owned()]);
    let policy = SandboxPolicy::from_config(Some(&config)).expect("a docker policy");
    let prepared = crate::seller_exec::prepare_launch(
        &["true".to_owned()],
        &policy,
        workdir.path(),
        &identity(),
        Duration::from_secs(300),
    )
    .await
    .expect("containment must be established");
    let holder = prepared.holder_name.clone().expect("a holder");
    let resolv = prepared.resolv_conf.clone().expect("a resolver file");

    // `+noedns` caps the UDP buffer at 512 bytes, so a ~750-byte TXT answer sets TC and dig retries
    // over TCP by itself. `+ignore` on the second query proves the UDP answer really was truncated
    // rather than the whole thing having fitted.
    // The KERN banner is emitted here too so this gate proves its own runtime rather than
    // inheriting gate A's claim.
    let script = format!(
        "echo KERN=$(dmesg 2>/dev/null | head -1 | tr -d '\\n'); \
         V4=$(dig +noedns +time=2 +tries=1 TXT {BIG_NAME} @{RESOLVER_V4} 2>&1); \
         case \"$V4\" in *'retrying in TCP'*) echo TC4=yes;; *) echo TC4=no;; esac; \
         echo \"$V4\" | grep -c '\"' | sed 's/^/ANS4=/'; \
         T4=$(dig +noedns +tcp +time=2 +tries=1 +short TXT {BIG_NAME} @{RESOLVER_V4} 2>&1); \
         echo \"$T4\" | grep -c xxx | sed 's/^/TCP4=/'"
    );
    let (_, out, err) = run_payload(
        &policy,
        prepared.uid,
        prepared.gid,
        workdir.path(),
        Some(&holder),
        Some(resolv.as_path()),
        &script,
    );
    prove_runsc(&workdir.job(), &holder, &out);
    assert_eq!(field(&out, "TC4"), "yes", "the v4 UDP answer was not truncated: {out}\n{err}");
    assert_ne!(field(&out, "ANS4"), "0", "the v4 TCP retry returned nothing: {out}\n{err}");
    assert_ne!(field(&out, "TCP4"), "0", "an explicit TCP/53 query was refused: {out}\n{err}");
}

/// **Gate C.** With nothing configured, the launch discovers the host's own resolvers. On a
/// systemd-resolved host `/etc/resolv.conf` names only the `127.0.0.53` stub — an address no sandbox
/// can reach — and the canonical upstream must come from `resolvectl` instead.
///
/// This is F2's runtime face: the file a job is handed must name what a launch would *install*, never
/// the stub the host reads.
#[tokio::test]
#[ignore = "needs docker, the netfilter image and the two fixture images"]
async fn host_stub_discovery_hands_the_job_a_canonical_upstream_not_the_stub() {
    let _infra = Infra::up();
    let workdir = Workdir::new("gatec");
    let config = config(Vec::new());
    let policy = SandboxPolicy::from_config(Some(&config)).expect("a docker policy");
    assert!(
        policy.dns_servers().is_empty(),
        "this gate measures discovery; a configured list would short-circuit it"
    );
    let prepared = crate::seller_exec::prepare_launch(
        &["true".to_owned()],
        &policy,
        workdir.path(),
        &identity(),
        Duration::from_secs(300),
    )
    .await
    .expect("containment must be established from discovered resolvers");
    let resolv = prepared.resolv_conf.clone().expect("a resolver file");
    let text = std::fs::read_to_string(&resolv).expect("the resolver file");
    assert!(
        !text.contains("127.0.0."),
        "a loopback stub reached the job's resolver file:\n{text}"
    );
    assert!(
        text.lines().any(|line| line.starts_with("nameserver ")),
        "discovery produced no nameserver line:\n{text}"
    );
    // The same addresses must be the ones the kernel was told about, or the job is pointed at a
    // resolver its own firewall drops. Asked of the namespace, not of the policy struct.
    let holder = prepared.holder_name.clone().expect("a holder");
    let v4_rules = readback(&holder, Family::V4);
    let v6_rules = readback(&holder, Family::V6);
    for line in text.lines().filter_map(|line| line.strip_prefix("nameserver ")) {
        let rules = if line.contains(':') { &v6_rules } else { &v4_rules };
        assert!(
            rules.contains(line),
            "the kernel has no exception for the resolver the job was handed ({line}):\n{rules}"
        );
    }
}

/// **Gate D.** A seat whose only named resolver is unusable is refused BEFORE anything exists: no
/// holder container, no resolver file, no payload.
#[tokio::test]
#[ignore = "needs docker"]
async fn no_usable_resolver_refuses_before_a_holder_or_a_payload_exists() {
    let workdir = Workdir::new("gated");
    let config = config(vec!["127.0.0.53".to_owned()]);
    let policy = SandboxPolicy::from_config(Some(&config));
    // The refusal may land at config time or at launch time; both are "before anything exists", and
    // this gate accepts either — what it does not accept is a launch.
    let error = match policy {
        Err(error) => error.to_string(),
        Ok(policy) => crate::seller_exec::prepare_launch(
            &["true".to_owned()],
            &policy,
            workdir.path(),
            &identity(),
            Duration::from_secs(300),
        )
        .await
        .err()
        .expect("a loopback resolver must refuse the launch")
        .to_string(),
    };
    assert!(
        error.contains("resolver") || error.contains("loopback"),
        "the refusal must name the resolver: {error}"
    );
    let (_, listing, _) = docker(&["ps", "--all", "--format", "{{.Names}}"]);
    let holders: Vec<&str> =
        listing.lines().filter(|line| line.contains("dnsgate-gated")).collect();
    assert!(holders.is_empty(), "a refused launch left a container behind: {holders:?}");
    let stray: Vec<_> = std::fs::read_dir(workdir.path().parent().expect("a parent"))
        .expect("the workdir parent")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("resolv-dnsgate-gated"))
        .collect();
    assert!(stray.is_empty(), "a refused launch wrote a resolver file: {stray:?}");
}

/// **Gate E.** An installer that cannot apply the plan must leave no namespace behind — and so no
/// payload can be launched into one. The failure is injected by handing `establish` a sidecar image
/// that is not the applier, which is exactly the shape of "the sidecar ran and did not install".
#[tokio::test]
#[ignore = "needs docker and the two fixture images"]
async fn a_failed_installer_destroys_the_holder_and_leaves_nothing_to_launch_into() {
    let _infra = Infra::up();
    let job = "dnsgate-gatee";
    let result = crate::sandbox_netns::establish(
        SVC_NET,
        PROBE_IMAGE,
        // Not the netfilter image: no applier, so the plan is never installed and the count the
        // caller cross-checks never arrives.
        PROBE_IMAGE,
        crate::credential_proxy::PROXY_HOST_ALIAS,
        job,
        &"ab".repeat(32),
        1000,
        1000,
        None,
        true,
        vec![RESOLVER_V4.to_owned()],
    )
    .await;
    let error = result.err().expect("an installer that cannot apply must fail the launch");
    assert!(
        error.contains("containment") || error.contains("sidecar") || error.contains("rules"),
        "the failure must name the containment step: {error}"
    );
    let name = crate::sandbox_netns::holder_name(job);
    let (_, listing, _) = docker(&["ps", "--all", "--format", "{{.Names}}"]);
    assert!(
        !listing.lines().any(|line| line == name),
        "the holder {name} survived a failed installation:\n{listing}"
    );
}

/// The agent argv the delivery gate drives. `fake-acp-agent` is the fixture that satisfies the
/// product's ACP handshake and writes one deliverable; the wrapper in front of it records the
/// kernel the DELIVERY container itself is running on, INTO the tree that is about to be committed
/// and pushed.
///
/// That is deliberate. `docker inspect` reports what the daemon was asked for; this records what the
/// process doing the git work actually ran under, and the fixture remote then hands it back from the
/// other side of the wire. A `runc` delivery cannot produce this file.
fn delivery_agent_argv() -> Vec<String> {
    [
        "sh",
        "-c",
        "{ dmesg 2>/dev/null | head -1; cat /proc/version 2>/dev/null; } > /work/KERNEL.txt; \
         exec /usr/local/bin/fake-acp-agent",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// **Gate G — FUNCTIONAL container Git delivery.** A contained job FETCHES a pinned base over
/// `https` from a DNS-named disposable endpoint and PUSHES the exact gated commit back to it,
/// through the product's own container-delivery consumer — `maxplayer __deliver phase1`, launched
/// with the same `launch_with_mounts` argv builder, holder namespace and resolver file the seller
/// daemon uses (`seller_node::run::deliver_via_container`). No host git runs at any point in the
/// delivery: the host writes the inputs file, reads the oid file back, and asks the REMOTE what it
/// holds.
///
/// What each assertion is for:
/// - the endpoint is reached by NAME, so both git legs resolved through the resolver file the launch
///   wrote — the DNS contract and the delivery contract proven on one wire;
/// - the pushed commit's PARENT is the seeded base oid, which no local init could produce: the
///   clone/fetch leg really ran against the remote;
/// - the ref the remote advertises is the exact oid the orchestrator gated, read back from the
///   server side, not from the job's own repository;
/// - `KERNEL.txt` inside the pushed tree carries the delivery container's own gVisor banner.
///
/// TLS VERIFICATION IS ON for every leg. The endpoint presents a certificate issued by this run's
/// throwaway CA ([`fixture_tls`]) for the name the job dials, and the delivery container is handed
/// that CA through `SSL_CERT_FILE` — the standard trust input the product's own env allowlist
/// already carries (`delivery_orchestrator.rs:1208-1212`) and which its rustls client honours
/// through native-root loading. `GIT_SSL_NO_VERIFY` is NOT set and `http.sslVerify` is NOT
/// disabled anywhere in this module: a push accepted by an unauthenticated endpoint would not be
/// evidence of secure delivery, which is the only thing this gate exists to produce.
///
/// FIXTURE ACCOMMODATIONS, named so they are not mistaken for product behaviour: the issuer is a
/// per-run CA added ALONGSIDE the image's public roots (never replacing them) and discarded with
/// the run; and the remote is anonymous `https`, so the push token source is
/// [`PushTokenSource::None`], the same arm the host takes for a non-relay remote. No credential, no
/// external repository and no relay is involved.
#[tokio::test]
#[ignore = "needs docker, the netfilter image, the git fixture image and the pinned sandbox image"]
async fn functional_g_contained_job_fetches_and_pushes_an_exact_commit_over_dns() {
    use crate::delivery_orchestrator as orch;

    let _infra = Infra::up();
    let base_oid = seed_git_fixture();
    let remote = format!("https://{GIT_NAME}/repo.git");

    // Contemporaneous control, from OUTSIDE containment and on the same fixtures: the endpoint
    // answers by name and advertises the base. A later delivery failure is then attributable to the
    // contained path rather than to a dead fixture or an unresolvable name.
    //
    // It is built the way the contained holder is built — started on the resolver's network, then
    // attached to the endpoint's — because a container that can see only one of the two proves
    // nothing about a path that needs both.
    const CONTROL_CT: &str = "mxdns-gate-git-control";
    docker(&["rm", "--force", "--volumes", CONTROL_CT]);
    let (ok, _, err) = docker(&[
        "run", "--detach", "--name", CONTROL_CT, "--network", SVC_NET, "--dns", RESOLVER_V4,
        "--volume", &format!("{}:/srv/tls:ro", tls_dir().display()),
        "--entrypoint", "sh", PROBE_IMAGE, "-c", "sleep 600",
    ]);
    assert!(ok, "could not start the control container: {err}");
    let (ok, _, err) = docker(&["network", "connect", PUB_NET, CONTROL_CT]);
    assert!(ok, "could not attach {PUB_NET} to the control container: {err}");
    let (ok, control, control_err) = docker(&[
        "exec", CONTROL_CT, "sh", "-c",
        // The control verifies the certificate too, against the same fixture CA: a control that
        // accepted any certificate could not tell a live endpoint from a broken one.
        &format!(
            "git -c http.sslCAInfo=/srv/tls/ca.pem ls-remote {remote} refs/heads/{GIT_BASE_BRANCH}"
        ),
    ]);
    docker(&["rm", "--force", "--volumes", CONTROL_CT]);
    assert!(ok, "the unfiltered control could not reach the endpoint: {control}\n{control_err}");
    assert!(
        control.contains(&base_oid),
        "the control did not see the seeded base {base_oid}: {control}"
    );

    // The accepted run: the container trusts the image's public roots PLUS this run's CA, and
    // nothing anywhere relaxes verification.
    let run = contained_delivery("gitgate", &remote, &base_oid, "bundle.pem").await;
    let (out, err, outcome) = (run.out.clone(), run.err.clone(), &run.outcome);
    let (job, branch, holder, io_dir) =
        (run.job.clone(), run.branch.clone(), run.holder.clone(), run.io_dir.clone());
    assert!(run.ok, "the container delivery failed: {out}\n{err}\noutcome={outcome:?}");

    // The runtime identity of the container that did the git work, from docker's own view. The
    // delivery container is not removed on exit, so this is read from the container that ran.
    let (runtime, network) = payload_runtime(&job);
    assert_eq!(
        runtime, RUNTIME,
        "the delivery ran under {runtime}, not {RUNTIME} — this is not a gVisor result"
    );
    let (ok, holder_id, inspect_err) = docker(&["inspect", "--format", "{{.Id}}", &holder]);
    assert!(ok, "could not resolve the holder's id: {inspect_err}");
    assert_eq!(
        network,
        format!("container:{holder_id}"),
        "the delivery container did not join the holder's namespace"
    );

    // The host's own read of the result: the oid file on the exchange mount, never a git command.
    let delivered = orch::read_delivery_oid(&io_dir).expect("the delivery oid must be readable");
    assert_eq!(
        outcome.as_ref().map(|o| o.status),
        Some(orch::Phase1Status::Delivered),
        "the orchestrator did not report a delivery: {outcome:?}"
    );
    assert_eq!(
        out.lines().last().unwrap_or_default().trim(),
        delivered,
        "the orchestrator's stdout oid and its oid file disagree: {out}"
    );

    // INDEPENDENT verification, on the far side of the wire: what the remote actually holds.
    let (ok, advertised, err) = git_server(&format!("rev-parse refs/heads/{branch}"));
    assert!(ok, "the remote has no {branch}: {advertised}\n{err}");
    assert_eq!(
        advertised.trim(),
        delivered,
        "the remote's ref is not the gated commit: {advertised}"
    );
    let (ok, parent, err) = git_server(&format!("rev-parse {delivered}^"));
    assert!(ok, "the pushed commit has no parent: {parent}\n{err}");
    assert_eq!(
        parent.trim(),
        base_oid,
        "the pushed commit is not parented on the fetched base — the clone leg did not run"
    );
    let (ok, seeded, err) = git_server(&format!("show {delivered}:BASE.txt"));
    assert!(ok, "the pushed tree is missing the fetched base content: {seeded}\n{err}");
    assert_eq!(seeded.trim(), "base", "the fetched base content did not survive: {seeded}");
    let (ok, kernel, err) = git_server(&format!("show {delivered}:KERNEL.txt"));
    assert!(ok, "the pushed tree carries no kernel record: {kernel}\n{err}");
    assert!(
        kernel.contains("gVisor"),
        "the delivery container's own kernel is not gVisor ({kernel}) — docker reported \
         runtime={runtime}, and the two must agree before this is a gVisor result"
    );

    println!("delivery runtime={runtime} network={network}");
    println!("delivery kernel record={}", kernel.replace('\n', " | "));
    println!("remote {branch} = {delivered}, parent {parent} = seeded base");

    // NEGATIVE CONTROL — the same path, the same fixtures, ONE difference: the container trusts the
    // image's public roots alone, without this run's CA. If certificate verification were disabled
    // anywhere on the delivery path, this run would deliver exactly like the one above, and the
    // result above would say nothing about secure delivery. It must fail, and the remote must be
    // untouched.
    let served_before = endpoint_requests_served();
    let untrusted = contained_delivery("gitgate-untrusted", &remote, &base_oid, "roots.pem").await;
    assert!(
        !untrusted.ok,
        "a certificate signed by an UNTRUSTED issuer was accepted — TLS verification is not in \
         force on this path: {}\n{}\noutcome={:?}",
        untrusted.out, untrusted.err, untrusted.outcome
    );
    let (moved, _, _) = git_server(&format!("rev-parse --verify refs/heads/{}", untrusted.branch));
    assert!(
        !moved,
        "the untrusted run still moved {} on the remote",
        untrusted.branch
    );
    assert!(
        orch::read_delivery_oid(&untrusted.io_dir).is_err(),
        "the untrusted run reported a delivery oid"
    );
    // It must fail for the RIGHT reason. "Something went wrong" would also be produced by a dead
    // fixture or an unresolvable name, neither of which is a statement about verification, so the
    // cause is pinned from three sides rather than from the client's message text (the transport
    // renders `error sending request for url (...)` without the rustls cause behind it):
    //
    // 1. the failure is on the fetch leg against THIS endpoint, by name — so the name resolved and
    //    the job got far enough to dial it;
    // 2. the ENDPOINT's own log carries the aborted handshake for that attempt;
    // 3. the endpoint is still serving to a client that trusts the fixture CA, AFTER the refusal —
    //    so the refusal was not a dead fixture.
    let why = untrusted
        .outcome
        .as_ref()
        .map(|outcome| outcome.detail.clone())
        .unwrap_or_default()
        + &untrusted.err;
    assert!(
        why.contains(GIT_NAME),
        "the untrusted run did not fail against {GIT_NAME}: {why}"
    );
    let served_after = endpoint_requests_served();
    assert_eq!(
        served_after, served_before,
        "the endpoint SERVED the untrusted job a request — the connection got past the handshake, \
         so the failure was not certificate verification"
    );
    assert!(
        endpoint_answers_trusted_client(&base_oid),
        "the endpoint stopped serving, so the refusal above is not attributable to the trust store"
    );
    println!(
        "negative control (roots without the fixture CA): refused, status={:?}",
        untrusted.outcome.as_ref().map(|outcome| outcome.status)
    );
    println!(
        "negative control: endpoint served {served_before} requests before and {served_after} after; \
         still live to a client trusting the fixture CA"
    );

    let _ = std::fs::remove_dir_all(&io_dir);
    let _ = std::fs::remove_dir_all(&untrusted.io_dir);
}
