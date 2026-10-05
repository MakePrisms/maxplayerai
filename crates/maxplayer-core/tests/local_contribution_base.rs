//! Full private posting with a local base, HTTPS fallback and receive-pack.
#![cfg(all(unix, feature = "wallet"))]
#[path = "git_http_fixture/mod.rs"]
mod git_http_fixture;
use git_http_fixture::{FixtureOptions, GitHttpAuthServer};
use maxplayer_core::{contribution, gateway, home, job_lifecycle as jl, private_content as pc};
use nostr_sdk::Keys;
use std::collections::BTreeSet;

fn commit(repo: &git2::Repository, parent: Option<git2::Oid>, text: &str, mode: i32) -> git2::Oid {
    let object = if mode == 0o160000 {
        parent.unwrap()
    } else {
        repo.blob(text.as_bytes()).unwrap()
    };
    let mut builder = repo.treebuilder(None).unwrap();
    builder.insert("file", object, mode).unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
    repo.commit(
        None,
        &sig,
        &sig,
        text,
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}
fn objects(repo: &git2::Repository) -> BTreeSet<git2::Oid> {
    let mut ids = BTreeSet::new();
    repo.odb()
        .unwrap()
        .foreach(|id| {
            ids.insert(*id);
            true
        })
        .unwrap();
    ids
}

#[test]
fn local_base_post_uploads_only_pin_and_falls_back_without_leaking_or_bypassing_policy() {
    let root = tempfile::tempdir().unwrap();
    let source = git2::Repository::init(root.path().join("checkout")).unwrap();
    let parent = commit(&source, None, "ancestor", 0o100644);
    let base = commit(&source, Some(parent), "pinned", 0o100644);
    source
        .reference("refs/heads/main", base, true, "fixture")
        .unwrap();
    let expected = objects(&source);
    let secret = commit(&source, Some(parent), "PRIVATE OTHER BRANCH", 0o100644);
    source
        .reference("refs/heads/secret", secret, true, "fixture")
        .unwrap();
    let unreachable = commit(&source, None, "UNREACHABLE PRIVATE", 0o100644);
    std::fs::write(
        source.workdir().unwrap().join("uncommitted-secret"),
        "DO NOT UPLOAD",
    )
    .unwrap();
    source.set_head("refs/heads/main").unwrap();
    let linked = root.path().join("linked");
    source.worktree("linked", &linked, None).unwrap();
    // Put both private branches and unreachable objects into a shared source
    // pack. Copying that pack wholesale must fail the exact-set assertion below.
    {
        use std::io::Write;
        let mut pack = source.packbuilder().unwrap();
        for oid in objects(&source) {
            pack.insert_object(oid, None).unwrap();
        }
        let odb = source.odb().unwrap();
        let mut writer = odb.packwriter().unwrap();
        pack.foreach(|chunk| writer.write_all(chunk).is_ok())
            .unwrap();
        writer.commit().unwrap();
    }
    // The source's packing config must not control or be rewritten by import.
    source
        .config()
        .unwrap()
        .set_str("pack.windowMemory", "invalid-on-purpose")
        .unwrap();
    let empty = git2::Repository::init_bare(root.path().join("empty")).unwrap();
    let nonrepo = root.path().join("nonrepo");
    std::fs::create_dir(&nonrepo).unwrap();
    // Remote source contains only the pin, not the secret branch.
    let remote_repo = git2::Repository::init_bare(root.path().join("remote")).unwrap();
    for id in &expected {
        let odb = source.odb().unwrap();
        let obj = odb.read(*id).unwrap();
        remote_repo
            .odb()
            .unwrap()
            .write(obj.kind(), obj.data())
            .unwrap();
    }
    remote_repo
        .reference("refs/heads/main", base, true, "fixture")
        .unwrap();
    remote_repo
        .config()
        .unwrap()
        .set_bool("uploadpack.allowReachableSHA1InWant", true)
        .unwrap();
    let remote = GitHttpAuthServer::spawn_with(
        remote_repo.path(),
        "/public/source",
        FixtureOptions {
            allow_anonymous: true,
            ..Default::default()
        },
    );
    let incomplete = git2::Repository::init_bare(root.path().join("incomplete")).unwrap();
    for id in &expected {
        if *id == parent {
            continue;
        }
        let odb = source.odb().unwrap();
        let object = odb.read(*id).unwrap();
        incomplete
            .odb()
            .unwrap()
            .write(object.kind(), object.data())
            .unwrap();
    }
    std::fs::write(incomplete.path().join("shallow"), format!("{base}\n")).unwrap();
    let modes = [
        (
            "checkout",
            Some(source.workdir().unwrap().to_owned()),
            base,
            false,
            true,
        ),
        ("linked", Some(linked), base, false, true),
        (
            "bare",
            Some(remote_repo.path().to_owned()),
            base,
            false,
            true,
        ),
        ("absent", None, base, true, true),
        (
            "missing",
            Some(root.path().join("missing")),
            base,
            true,
            true,
        ),
        ("nonrepo", Some(nonrepo), base, true, true),
        (
            "missing-commit",
            Some(empty.path().to_owned()),
            base,
            true,
            true,
        ),
        (
            "incomplete-history",
            Some(incomplete.path().to_owned()),
            base,
            true,
            true,
        ),
        (
            "symlink",
            Some(source.workdir().unwrap().to_owned()),
            commit(&source, Some(base), "target", 0o120000),
            false,
            false,
        ),
        (
            "submodule",
            Some(source.workdir().unwrap().to_owned()),
            commit(&source, Some(base), "submodule", 0o160000),
            false,
            false,
        ),
    ];
    let servers: Vec<_> = modes
        .iter()
        .map(|(name, ..)| {
            let repo = git2::Repository::init_bare(root.path().join(format!("destination-{name}")))
                .unwrap();
            let server = GitHttpAuthServer::spawn_with(
                repo.path(),
                "/git/buyer/job",
                FixtureOptions {
                    private_job_host: true,
                    ..Default::default()
                },
            );
            (repo, server)
        })
        .collect();
    let ca = root.path().join("ca.pem");
    let mut certs = std::fs::read_to_string(remote.ca_file(root.path())).unwrap();
    for (_, server) in &servers {
        certs.push_str(&std::fs::read_to_string(server.ca_file(root.path())).unwrap());
    }
    std::fs::write(&ca, certs).unwrap();
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &ca);
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
    }
    let buyer = Keys::generate();
    let seller = Keys::generate();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for ((name, local, oid, fallback, succeeds), (destination, server)) in
        modes.into_iter().zip(&servers)
    {
        let mut home = home::bootstrap(root.path().join(format!("home-{name}"))).unwrap();
        // Delivery/publication over Nostr is outside this Git fixture. Queue durably
        // and fail connection setup immediately rather than waiting for NIP-42.
        home.config.relay_url = "not a relay URL".into();
        home.config.privacy.private_jobs = true;
        home.config.privacy.private_content_v2 = true;
        home.config.privacy.private_job_repos = true;
        home.config.privacy.service_pubkey = Some(Keys::generate().public_key().to_hex());
        home.config.privacy.git_base = Some(format!(
            "{}/git/",
            server.repo_url().split("/git/").next().unwrap()
        ));
        let url = if fallback {
            remote.repo_url()
        } else {
            "https://127.0.0.1:1/not-fetchable".into()
        };
        let pin = contribution::ContributionOffer {
            target: contribution::TargetRepoPin::new(buyer.public_key().to_hex(), url.clone())
                .unwrap(),
            base: contribution::ContributionBase::new("main", oid.to_string()).unwrap(),
            accepts: vec!["fork".into()],
        };
        let request = jl::PostJobRequest {
            visibility: Some(pc::wire::Visibility::Private),
            output_category: Some(pc::wire::Output::Code),
            inputs: vec![],
            task: "contribute".into(),
            output: "git".into(),
            amount_sats: 0,
            seller_pubkey: Some(seller.public_key().to_hex()),
            untargeted: false,
            deadline_unix: Some(4_000_000_000),
            repo: None,
            branch: None,
            job: jl::JobKind::Contribution(jl::ContributionSpec {
                target_repo_owner: buyer.public_key().to_hex(),
                target_repo_url: url,
                base_local_path: local.clone(),
                base_branch: "main".into(),
                base_oid: oid.to_string(),
                accepts: None,
            }),
            requested_agent: None,
            requested_harness_family: None,
            requested_model: None,
            required_capabilities: vec![],
            accepts_delivery: vec![],
            payment_mode: gateway::PaymentMode::None,
        };
        let mut offer = gateway::OfferDraft::untargeted("contribute", "git", 0, 4_000_000_000)
            .with_payment_mode(gateway::PaymentMode::None);
        offer.seller_pubkey = request.seller_pubkey.clone();
        let before = remote.requests().len();
        let result = runtime.block_on(pc::posting::post(
            &home,
            &buyer,
            &request,
            &offer.to_event_draft(),
            Some(&pin),
        ));
        if !succeeds {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("symlinks, submodules"), "{name}: {error}");
            assert!(
                server.requests().is_empty(),
                "{name}: policy must run before upload"
            );
            continue;
        }
        let outcome = result.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            remote.requests().len() > before,
            fallback,
            "{name}: source selection"
        );
        assert_eq!(
            objects(destination),
            expected,
            "{name}: exact reachable set including history"
        );
        assert!(!destination.odb().unwrap().exists(secret));
        assert!(!destination.odb().unwrap().exists(unreachable));
        assert_eq!(
            destination
                .references()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .target(),
            Some(base)
        );
        let pushes: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.target.ends_with("git-receive-pack"))
            .collect();
        assert_eq!(pushes.len(), 1);
        assert_eq!(
            pushes[0].push_capabilities.as_deref(),
            Some("report-status ofs-delta"),
            "{name}: single pack must forward"
        );
        let store =
            pc::store::ContentStore::open(&home.root.join("private-content.sqlite")).unwrap();
        let event = store.event(&outcome.job_id).unwrap().unwrap();
        let mut ctx =
            pc::channel::ContentContext::open(&home, &buyer.public_key().to_hex()).unwrap();
        let task = ctx
            .accept_content(
                &event,
                &event,
                None,
                None,
                None,
                nostr_sdk::Timestamp::now().as_secs(),
            )
            .unwrap();
        let json = format!(
            "{}{}",
            serde_json::to_string(&event).unwrap(),
            serde_json::to_string(task.body()).unwrap()
        );
        assert!(!json.contains("base_local_path"));
        if let Some(local) = local {
            assert!(!json.contains(local.to_str().unwrap()));
        }
        assert!(
            home.root
                .join("store-seeds")
                .join(base.to_string())
                .is_dir()
        );
    }
    assert_eq!(
        source
            .config()
            .unwrap()
            .get_string("pack.windowMemory")
            .unwrap(),
        "invalid-on-purpose"
    );
}
