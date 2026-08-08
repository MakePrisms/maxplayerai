//! A5 — the production git legs run entirely in-process via libgit2 and NEVER shell out to a
//! system `git` (issue #55).
//!
//! `seller_git`, `delivery_git`, and `git_transport` document this property in prose ("no system
//! `git` on any product path") and the whole-repo grep confirms every `Command::new("git")` site
//! lives under a `#[cfg(test)]` module. A comment is not a guarantee, so this test proves the
//! property at RUNTIME: it makes system `git` unreachable and then drives the real production
//! entry points, which can only succeed if they use libgit2 rather than a `git` subprocess.
//!
//! ## Mechanism (two phases, one dedicated process)
//! 1. **Positive control.** With a normal PATH `Command::new("git")` spawns; after pointing PATH at
//!    an EMPTY dir it fails to spawn with `NotFound`. This proves the scrub is real — a silent
//!    scrub failure would leave `git` reachable and fail this assertion rather than pass vacuously.
//! 2. **Detection.** PATH is then pointed at a dir holding a stand-in `git` that RECORDS any
//!    invocation to a marker file and exits non-zero (a tripwire). Every in-process production leg
//!    is driven under it. libgit2 never consults PATH, so a leg that truly uses libgit2 succeeds
//!    and never touches the tripwire; a leg that shells out trips the marker (caught even if the
//!    caller ignores the exit status — the case a bare empty-PATH check would miss) and/or fails.
//!    At the end the marker MUST NOT exist.
//!
//! ## Legs exercised (all local, no network — avoids the debug-only #152 HTTP-transport lifecycle bug)
//! - A: `seller_git` from-scratch delivery — `init_empty_delivery_workdir` + `snapshot_delivery`.
//! - B: `seller_git` contribution delivery — a base then a child snapshot parented on it.
//! - C: `git_transport::fetch_refspecs` fetch-at-base from a LOCAL repo (libgit2's built-in local
//!      transport) — also exercises the smart-HTTP transport REGISTRATION (`ensure_registered`).
//! - D: `delivery_git::PayPathDeliveryVerifier::merge_retained_commit` — the buyer's local
//!      store→working-clone fetch + fast-forward merge of a retained contribution.
//! The allowlisted HTTPS push/verify legs are https-only (the transport allowlist refuses `file`),
//! so they are covered by the loopback-TLS integration tests (`relay_git_http_auth`,
//! `collect_integrity`, `git_config_isolation`); A5 adds the PATH-scrubbed no-system-git proof for
//! the in-process legs those cannot reach without a `git`-built fixture.
//!
//! RED-ON-REVERT: inject any `Command::new("git")` into one exercised production fn (e.g.
//! `snapshot_delivery_at`) and this goes red — the tripwire marker appears even for an
//! ignored-error shell-out.
//!
//! CI: gated on `git-delivery` (`required-features` in Cargo.toml) — the only maxplayer-core test
//! job that enables it is the money-path suite
//! (`cargo test -p maxplayer-core --release --no-default-features --features gateway,git-delivery,wallet`),
//! so that is where A5 compiles and runs; a bare `cargo test` compiles it out.

#![cfg(all(unix, feature = "git-delivery"))]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use maxplayer_core::delivery::CommitOid;
use maxplayer_core::delivery_git::PayPathDeliveryVerifier;
use maxplayer_core::git_transport;
use maxplayer_core::seller_git::{self, DeliveryAgentIdentity};

/// Fixed authored-at seconds so contribution snapshots reproduce byte-identically across repos
/// (`snapshot_delivery_at`'s determinism invariant), which Leg D relies on to build two repos that
/// agree on the base commit oid.
const DATE_BASE: i64 = 1_700_000_000;
const DATE_CHILD: i64 = 1_700_000_100;

fn unique_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "maxplayer-a5-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn assert_full_oid(oid: &str, leg: &str) {
    assert_eq!(oid.len(), 40, "{leg}: expected a full 40-hex sha1 oid, got {oid:?}");
    assert!(
        oid.bytes().all(|b| b.is_ascii_hexdigit()),
        "{leg}: oid is not hex: {oid:?}"
    );
}

/// Install a stand-in `git` in a fresh dir. Any invocation appends its argv to `marker` and exits
/// non-zero. Returns `(bin_dir, marker_path)`; while `bin_dir` is the only entry on PATH, a
/// production shell-out to `git` cannot hide — the marker is a positive detector, not the absence
/// of one.
fn install_tripwire_git() -> (PathBuf, PathBuf) {
    let bin = unique_dir("tripwire-bin");
    let marker = unique_dir("tripwire-marker").join("fired");
    let git = bin.join("git");
    // `/bin/sh` is an absolute path, so the tripwire runs even with PATH scrubbed. The marker path
    // is baked in absolute (our temp path has no shell metacharacters), so detection does not
    // depend on the child inheriting any env.
    let script = format!(
        "#!/bin/sh\nprintf 'system git invoked with argv: %s\\n' \"$*\" >> \"{}\"\nexit 3\n",
        marker.display()
    );
    fs::write(&git, script).expect("write tripwire git");
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).expect("chmod tripwire git");
    (bin, marker)
}

#[test]
fn production_git_legs_never_shell_out_to_system_git() {
    // ---------- Phase 1: positive control ----------
    // git is reachable on the ambient PATH now (CI installs it; required so the scrub below is
    // meaningful rather than a no-op over an already-empty environment).
    assert!(
        Command::new("git").arg("--version").output().is_ok(),
        "A5 needs system git installed on the ambient PATH to prove the negative — it was not found"
    );

    let empty_path = unique_dir("emptypath");
    // SAFETY (edition-2024 set_var): this is a dedicated single-`#[test]` process with no other
    // thread reading the environment; every PATH mutation happens between synchronous calls.
    unsafe {
        std::env::set_var("PATH", &empty_path);
    }
    match Command::new("git").arg("--version").spawn() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        other => panic!(
            "scrub ineffective: expected NotFound spawning `git` with an empty PATH, got {other:?}"
        ),
    }

    // ---------- Phase 2: arm the tripwire, drive every in-process production leg under it ----------
    let (tripwire_bin, marker) = install_tripwire_git();
    // SAFETY: see above.
    unsafe {
        std::env::set_var("PATH", &tripwire_bin);
    }

    let identity = DeliveryAgentIdentity::for_seller(&"ab".repeat(32));

    // Leg A — seller from-scratch delivery: git2 init + git2 commit-authoring.
    let ra = unique_dir("legA-fromscratch");
    seller_git::init_empty_delivery_workdir(&ra, &identity).expect("A: init workdir");
    fs::write(ra.join("hello.txt"), b"scratch delivery\n").expect("A: write file");
    let oid_a = seller_git::snapshot_delivery(&ra, &identity, None, "main", "from-scratch", "jobA")
        .expect("A: snapshot from-scratch delivery");
    assert_full_oid(&oid_a, "A");

    // Leg B — seller contribution delivery: a base snapshot, then a child parented on it.
    let rb = unique_dir("legB-contribution");
    seller_git::init_empty_delivery_workdir(&rb, &identity).expect("B: init workdir");
    fs::write(rb.join("readme.txt"), b"base\n").expect("B: write base file");
    let base_b = seller_git::snapshot_delivery_at(&rb, &identity, None, "main", "base", DATE_BASE, "jobB0")
        .expect("B: base snapshot");
    assert_full_oid(&base_b, "B-base");
    fs::write(rb.join("feature.txt"), b"contribution work\n").expect("B: write feature file");
    let child_b = seller_git::snapshot_delivery_at(
        &rb,
        &identity,
        Some(&base_b),
        "main",
        "contribution",
        DATE_CHILD,
        "jobB1",
    )
    .expect("B: contribution snapshot");
    assert_full_oid(&child_b, "B-child");
    assert_ne!(base_b, child_b, "B: contribution commit must differ from its base");
    {
        // Correctness: the delivered contribution really descends from the pinned base.
        let repo = git2::Repository::open(&rb).expect("B: reopen workdir");
        let child = repo
            .find_commit(git2::Oid::from_str(&child_b).unwrap())
            .expect("B: find contribution commit");
        assert_eq!(child.parent_count(), 1, "B: contribution has exactly one parent");
        assert_eq!(
            child.parent_id(0).unwrap().to_string(),
            base_b,
            "B: contribution parent must be the base"
        );
    }

    // Leg C — git_transport fetch-at-base over a LOCAL repo (libgit2 local transport) plus the
    // smart-HTTP transport registration. Reuse Leg A's repo (tip on refs/heads/main) as the remote.
    let rc = unique_dir("legC-consumer");
    seller_git::init_empty_delivery_workdir(&rc, &identity).expect("C: init consumer");
    let consumer = git2::Repository::open(&rc).expect("C: open consumer");
    git_transport::fetch_refspecs(
        &consumer,
        ra.to_str().expect("C: remote path utf8"),
        &["+refs/heads/main:refs/maxplayer/base"],
        None,
        false,
    )
    .expect("C: local fetch-at-base");
    assert_eq!(
        consumer
            .refname_to_id("refs/maxplayer/base")
            .expect("C: fetched ref present")
            .to_string(),
        oid_a,
        "C: fetched tip must equal Leg A's delivered oid"
    );

    // Leg D — delivery_git buyer merge of a retained contribution: local store→target fetch + FF.
    let store = unique_dir("legD-store");
    seller_git::init_empty_delivery_workdir(&store, &identity).expect("D: init store");
    fs::write(store.join("readme.txt"), b"base\n").expect("D: store base file");
    let c0_store = seller_git::snapshot_delivery_at(&store, &identity, None, "main", "base", DATE_BASE, "jobD0")
        .expect("D: store base snapshot");
    fs::write(store.join("feature.txt"), b"retained work\n").expect("D: store feature file");
    let c1_store = seller_git::snapshot_delivery_at(
        &store,
        &identity,
        Some(&c0_store),
        "main",
        "retained contribution",
        DATE_CHILD,
        "jobD1",
    )
    .expect("D: store contribution snapshot");
    {
        // Retention: the buyer store advertises the fork tip under its deliveries ref.
        let repo = git2::Repository::open(&store).expect("D: open store");
        repo.reference(
            &PayPathDeliveryVerifier::store_ref_for(&c1_store),
            git2::Oid::from_str(&c1_store).unwrap(),
            true,
            "A5 retained delivery",
        )
        .expect("D: create retention ref");
    }
    // An independent target working clone whose HEAD is the base commit (an ancestor of the
    // retained tip). Determinism makes the base oid identical to the store's.
    let target = unique_dir("legD-target");
    seller_git::init_empty_delivery_workdir(&target, &identity).expect("D: init target");
    fs::write(target.join("readme.txt"), b"base\n").expect("D: target base file");
    let c0_target = seller_git::snapshot_delivery_at(&target, &identity, None, "main", "base", DATE_BASE, "jobD0")
        .expect("D: target base snapshot");
    assert_eq!(c0_target, c0_store, "D: deterministic base oid across store and target");

    let verifier = PayPathDeliveryVerifier::new(&store, None);
    let retained = CommitOid::parse(c1_store.clone()).expect("D: parse retained oid");
    verifier
        .merge_retained_commit(&target, &retained)
        .expect("D: fast-forward merge of retained commit");
    {
        let repo = git2::Repository::open(&target).expect("D: reopen target");
        let head = repo.head().unwrap().peel_to_commit().unwrap().id().to_string();
        assert_eq!(head, c1_store, "D: target must fast-forward to the retained contribution");
    }

    // ---------- The tripwire must never have fired ----------
    assert!(
        !marker.exists(),
        "A5: a production git leg shelled out to system `git` — tripwire fired:\n{}",
        fs::read_to_string(&marker).unwrap_or_default()
    );
}
