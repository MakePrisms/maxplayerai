//! Live issuer-sidecar test: ONE seat, ONE `cdk-mintd`, no counterparty.
//!
//! `#[ignore]`d because it needs a real `cdk-mintd 0.17.2` on this box, so it is not part of an
//! ordinary `cargo test` run. Run it with:
//!
//! ```text
//! CDK_MINTD_BIN=/path/to/cdk-mintd \
//!   cargo test -p maxplayer-core --features wallet --test issuer_sidecar_live -- --ignored
//! ```
//!
//! **Why it exists at all.** Every other test of this surface asserts what is *rendered* or what a
//! hand-built sqlite says. Two of the facts stage 3a rests on cannot be reached that way:
//!
//! 1. **A melt at this mint BURNS, and signs nothing new.** `retired` is a protocol-required counter
//!    and nothing in the tree could burn a proof before this. The mechanism — a NUT-05 melt of a
//!    bolt11 the mint never issued, that nothing pays — is a property of `cdk-mintd`'s fake-wallet
//!    backend, not of our code, and only a live mint can be asked whether it holds. A fixture that
//!    asserted it would be asserting our belief about somebody else's binary.
//! 2. **A dead sidecar states nothing.** Its sqlite survives the process and reads perfectly, so the
//!    only way to see the difference between "up" and "down" is to kill one.
//!
//! ⛔ NO SECOND SEAT and NO SECOND MINT. The two-seat loop is stage 3b, deferred by maxie's ruling:
//! at this base only the `Own` marker admits (`mint_class.rs:79-81`), so a counterparty reading this
//! seat's `issuer_mint` tag records it `Declared` and `home::mint_allowed` refuses a loopback
//! `http://` URL outright. Nothing here may pretend otherwise.

// `wallet` carries `crate::issuer` (the whole surface under test), the cdk wallet the seat mints and
// burns with, and rusqlite for the counter read. Gating on it is what makes this file compile at all;
// a compiled-out test and a passing test produce the same green, so verify membership with
// `cargo test … -- --list`, never by a green tick.
#![cfg(feature = "wallet")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use maxplayer_core::gateway::TagSpec;
use maxplayer_core::heartbeat::{
    self, parse_heartbeat, IssuerMintAd, SeatCapability, ISSUER_MINT_TAG,
};
use maxplayer_core::home::{self, AdmissionPolicy, MaxplayerHome, TargetedAdmission};
use maxplayer_core::issuer::{self, InitOptions};

/// The `cdk-mintd` to exercise. Deliberately REQUIRED rather than defaulted: a default would let
/// this test silently measure whatever happened to be on PATH, and its whole purpose is to measure
/// the binary the operator names.
///
/// It is NOT `MAXPLAYER_*`-prefixed, and must not be: the whole `MAXPLAYER_` namespace is reserved
/// for config (`home.rs:1816` lists the operational seams), and an unrecognised one is refused
/// fail-closed by `home::bootstrap` — which this very test calls. Measured: the refusal names the
/// variable and aborts before the sidecar starts.
fn cdk_mintd() -> PathBuf {
    let raw = std::env::var("CDK_MINTD_BIN").expect(
        "set CDK_MINTD_BIN to a cdk-mintd 0.17.2 binary (see this file's module docs)",
    );
    let path = PathBuf::from(raw);
    assert!(path.is_file(), "CDK_MINTD_BIN is not a file: {}", path.display());
    path
}

/// A port nothing is listening on: bound, its number taken, then released. Two live runs in
/// parallel would otherwise collide on the wizard's default 3338.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

/// The sidecar, owned so it is killed even when an assertion unwinds — a leaked `cdk-mintd` would
/// hold its port and poison the next run.
struct Sidecar {
    child: Child,
    work_dir: PathBuf,
}

impl Sidecar {
    /// Start the mint from EXACTLY the files `issuer init` wrote — the config it rendered and the
    /// seed it generated, passed by `--seed-file`. That is the point: if the wizard writes a config
    /// that does not parse (0.17.2's own `example.config.toml` does not), this fails here.
    async fn start(binary: &Path, report: &issuer::InitReport, url: &str) -> Self {
        let child = Command::new(binary)
            .arg("--work-dir")
            .arg(&report.work_dir)
            .arg("--config")
            .arg(&report.mintd_config)
            .arg("--seed-file")
            .arg(&report.seed_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cdk-mintd");
        let sidecar = Self {
            child,
            work_dir: report.work_dir.clone(),
        };
        sidecar.await_serving(url).await;
        sidecar
    }

    /// Async on purpose: `reqwest::blocking` builds its OWN runtime, and dropping one inside a
    /// `#[tokio::test]` panics ("Cannot drop a runtime in a context where blocking is not allowed").
    /// Measured here before this was written this way.
    async fn await_serving(&self, url: &str) {
        let info = format!("{}/v1/info", url.trim_end_matches('/'));
        let deadline = Instant::now() + Duration::from_secs(20);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("http client");
        while Instant::now() < deadline {
            if let Ok(response) = client.get(&info).send().await {
                if response.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "cdk-mintd did not serve {info} within 20s; its log is under {}/logs",
            self.work_dir.display()
        );
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.kill();
    }
}

fn temp_root(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "maxplayer-issuer-live-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

/// One beat, built through the ONE production mapping from seat state to a beat, and read back off
/// the EVENT rather than off the draft. `heartbeat_for_state` takes the advertisement as a REQUIRED
/// parameter, so this call site cannot accidentally omit it — which is exactly why it is required.
async fn beat(home: &MaxplayerHome) -> maxplayer_core::gateway::EventDraft {
    heartbeat::heartbeat_for_state(
        0,
        true,
        5,
        home.config.accepted_mints.clone(),
        vec!["claude".to_owned()],
        SeatCapability::default(),
        AdmissionPolicy {
            pool: true,
            targeted: TargetedAdmission::Open,
        },
        issuer::advertisement(home).await,
    )
    .to_event_draft()
}

/// The crate's own `first_tag` is private, and widening it for a test would be a product change
/// made for a test's convenience. This is the same lookup, spelled here.
fn first_tag<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a TagSpec> {
    tags.iter().find(|tag| tag.first() == Some(name))
}

fn counters(home: &MaxplayerHome) -> issuer::MintCounters {
    issuer::read_counters(&issuer::mint_db_path(home)).expect("counters read")
}

/// The whole of §7, in order, against a real mint: issue, read, advertise, retire, advertise, die.
#[tokio::test]
#[ignore = "needs a local cdk-mintd 0.17.2"]
async fn one_seat_issues_advertises_retires_and_falls_silent_when_its_sidecar_dies() {
    const ISSUE_SATS: u64 = 100;
    const RETIRE_SATS: u64 = 40;

    let binary = cdk_mintd();
    let root = temp_root("full");
    let mut home = home::bootstrap(&root).expect("bootstrap");

    // ── 1. `issuer init` → sidecar up on 127.0.0.1 from the written config and --seed-file ──────
    let port = free_port();
    let report = issuer::init(
        &mut home,
        &InitOptions {
            listen_host: issuer::DEFAULT_LISTEN_HOST.to_owned(),
            listen_port: port,
        },
    )
    .expect("issuer init");
    let url = home
        .config
        .issuer_mint()
        .expect("init set issuer_mint")
        .to_owned();
    assert!(report.seed_created, "a fresh home gets a fresh seed");
    assert!(
        home.config.accepted_mints.contains(&url),
        "a URL outside accepted_mints reads as UNSTATED to every reader"
    );
    assert!(
        !issuer::mint_db_path(&home).exists(),
        "init writes files but does not create the mint's database"
    );
    let mut sidecar = Sidecar::start(&binary, &report, &url).await;

    let start = counters(&home);
    assert_eq!(start.issued_sats, 0);
    assert_eq!(start.redeemed_sats, 0);
    assert_eq!(start.outstanding_sats, 0);

    // ── 2. the seat mints N sat at its OWN mint; counters read outstanding == N ─────────────────
    let issued = issuer::issue(&home, ISSUE_SATS).await.expect("issue");
    assert_eq!(issued.issued_sats, ISSUE_SATS);
    assert_eq!(issued.balance_sats, ISSUE_SATS);
    let after_issue = counters(&home);
    println!(
        "after issue: issued={} redeemed={} outstanding={}",
        after_issue.issued_sats, after_issue.redeemed_sats, after_issue.outstanding_sats
    );
    assert_eq!(after_issue.issued_sats, ISSUE_SATS);
    assert_eq!(after_issue.redeemed_sats, 0);
    assert_eq!(after_issue.outstanding_sats, ISSUE_SATS);

    // ── 3. the beat carries ["issuer_mint", url, N, 0, ts] — read off the EVENT ─────────────────
    let event = beat(&home).await;
    let tag = first_tag(&event.tags, ISSUER_MINT_TAG).expect("issuer_mint tag on a live beat");
    assert_eq!(tag.0[0], ISSUER_MINT_TAG);
    assert_eq!(tag.0[1], url);
    assert_eq!(tag.0[2], ISSUE_SATS.to_string(), "outstanding on the wire");
    assert_eq!(tag.0[3], "0", "nothing retired yet");
    let read_back = IssuerMintAd::from_tags(&event.tags, &home.config.accepted_mints)
        .expect("a reader parses it back");
    assert_eq!(read_back.mint_url, url);
    assert_eq!(read_back.outstanding_sats, ISSUE_SATS);
    assert_eq!(read_back.retired_sats, 0);
    assert!(read_back.last_seen > 0);

    // ── 4. the seat retires M; counters fall; the beat carries retired == M ─────────────────────
    //
    // The mechanism is the one deliverable 0 proved: a melt of a bolt11 the mint never issued, that
    // nothing pays. What makes it a BURN rather than a re-issue is that no new blind signature is
    // signed — so `issued` must not move.
    let record = issuer::retire(&home, RETIRE_SATS).await.expect("retire");
    assert_eq!(record.sats, RETIRE_SATS);
    assert_eq!(record.mint_url, url);
    let after_retire = counters(&home);
    println!(
        "after retire: issued={} redeemed={} outstanding={} retired={}",
        after_retire.issued_sats,
        after_retire.redeemed_sats,
        after_retire.outstanding_sats,
        RETIRE_SATS
    );
    assert!(
        after_retire.redeemed_sats >= RETIRE_SATS,
        "the mint burned at least what the seat asked to retire: redeemed={} retired={RETIRE_SATS}",
        after_retire.redeemed_sats
    );
    assert!(
        after_retire.outstanding_sats < ISSUE_SATS,
        "outstanding must FALL: before={ISSUE_SATS} after={}",
        after_retire.outstanding_sats
    );
    assert_eq!(
        after_retire.outstanding_sats,
        after_retire.issued_sats - after_retire.redeemed_sats,
        "outstanding ({}) != issued ({}) - redeemed ({})",
        after_retire.outstanding_sats,
        after_retire.issued_sats,
        after_retire.redeemed_sats
    );
    // §5's gate, against a live mint: the seat cannot have retired more than the mint ever burned.
    let retired_total =
        issuer::retired_total(&issuer::retired_ledger_path(&home)).expect("ledger total");
    assert_eq!(retired_total, RETIRE_SATS);
    assert!(
        retired_total <= after_retire.redeemed_sats,
        "retired ({retired_total}) > redeemed ({})",
        after_retire.redeemed_sats
    );

    let event = beat(&home).await;
    let read_back = IssuerMintAd::from_tags(&event.tags, &home.config.accepted_mints)
        .expect("still advertising");
    assert_eq!(read_back.retired_sats, RETIRE_SATS);
    assert_eq!(read_back.outstanding_sats, after_retire.outstanding_sats);

    // ── 5. sidecar killed ⇒ next beat carries NO issuer_mint tag, and the seat still advertises ──
    sidecar.kill();
    // The database is STILL THERE and still readable — that is the trap this step exists to catch.
    assert!(issuer::mint_db_path(&home).exists());
    assert!(
        issuer::read_counters(&issuer::mint_db_path(&home)).is_ok(),
        "a dead mint's sqlite still reads; the tag must be absent anyway"
    );

    let event = beat(&home).await;
    assert!(
        first_tag(&event.tags, ISSUER_MINT_TAG).is_none(),
        "a dead sidecar must publish no issuer_mint tag"
    );
    let parsed = parse_heartbeat(&event).expect("the beat still parses");
    assert!(
        parsed.accepting,
        "an optional tag must never take a working seat off the market"
    );
    assert_eq!(parsed.issuer_mint, None);
    assert!(!parsed.accepted_mints.is_empty(), "the seat is still payable");

    let _ = std::fs::remove_dir_all(&root);
}
