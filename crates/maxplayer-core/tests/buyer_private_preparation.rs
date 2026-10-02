//! Production buyer posting and Git transport over verified TLS. Provisioning is
//! mocked here; the real relay ACL/award/publication gates have separate HTTP tests.
#![cfg(all(
    unix,
    feature = "wallet",
    feature = "gateway",
    feature = "git-delivery"
))]

use maxplayer_core::{
    contribution, gateway, git_transport, home, job_lifecycle, private_content as pc,
};
use nostr_sdk::Keys;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[path = "git_http_fixture/mod.rs"]
mod git_http_fixture;
use git_http_fixture::{FixtureOptions, GitHttpAuthServer};

fn commit(repo: &git2::Repository, parent: Option<git2::Oid>, text: &str) -> git2::Oid {
    let blob = repo.blob(text.as_bytes()).unwrap();
    let mut builder = repo.treebuilder(None).unwrap();
    builder.insert("base.txt", blob, 0o100644).unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
    repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        "base",
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

#[test]
fn buyer_prepares_both_discovery_modes_retries_and_refuses_incomplete_publication() {
    let root = tempfile::tempdir().unwrap();
    let source_repo = git2::Repository::init_bare(root.path().join("source")).unwrap();
    source_repo
        .config()
        .unwrap()
        .set_bool("uploadpack.allowReachableSHA1InWant", true)
        .unwrap();
    let parent = commit(&source_repo, None, "history");
    let base = commit(&source_repo, Some(parent), "pinned base");
    let source = GitHttpAuthServer::spawn_with(
        source_repo.path(),
        "/public/source",
        FixtureOptions {
            allow_anonymous: true,
            ..Default::default()
        },
    );
    let destination = git2::Repository::init_bare(root.path().join("destination")).unwrap();
    let relay = GitHttpAuthServer::spawn_with(
        destination.path(),
        "/git/buyer/job",
        FixtureOptions {
            private_job_host: true,
            // PUT provisioning, GET receive-pack advertisement, POST forwarded pack.
            reject_request: Some((3, "503 Service Unavailable")),
            lose_receive_pack_response: true,
            ..Default::default()
        },
    );
    let refused_repo = git2::Repository::init_bare(root.path().join("refused")).unwrap();
    let refused = GitHttpAuthServer::spawn_with(
        refused_repo.path(),
        "/git/buyer/job",
        FixtureOptions {
            private_job_host: true,
            reject_request: Some((3, "403 Forbidden")),
            ..Default::default()
        },
    );
    // Contribution + confidential inputs: the base must still be forwarded (#1096 review).
    let with_inputs_repo = git2::Repository::init_bare(root.path().join("with-inputs")).unwrap();
    let with_inputs = GitHttpAuthServer::spawn_with(
        with_inputs_repo.path(),
        "/git/buyer/job",
        FixtureOptions {
            private_job_host: true,
            ..Default::default()
        },
    );
    // A permanent relay policy refusal of the forwarded pack (request 3) is not retried.
    let policy_repo = git2::Repository::init_bare(root.path().join("policy")).unwrap();
    let policy = GitHttpAuthServer::spawn_with(
        policy_repo.path(),
        "/git/buyer/job",
        FixtureOptions {
            private_job_host: true,
            reject_request: Some((3, "400 Bad Request")),
            reject_body: Some("private job snapshots cannot contain symlinks or submodules"),
            ..Default::default()
        },
    );
    let brief = root.path().join("brief.md");
    std::fs::write(&brief, "confidential brief\n").unwrap();
    // Install all test trust anchors before either reqwest client is constructed.
    let mut pem = String::new();
    for server in [&source, &relay, &refused, &with_inputs, &policy] {
        pem.push_str(&std::fs::read_to_string(server.ca_file(root.path())).unwrap());
    }
    let ca = root.path().join("all-ca.pem");
    std::fs::write(&ca, pem).unwrap();
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &ca);
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
    }
    assert!(std::env::var_os("GIT_SSL_NO_VERIFY").is_none());
    // Public test key (scalar 128): its pubkey contains "403". A transient
    // HTTP 503 must not become an auth refusal just because the URL has those digits.
    let buyer = Keys::parse(&format!("{:064x}", 128)).unwrap();
    assert!(buyer.public_key().to_hex().contains("403"));
    let seller = Keys::generate();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let pin = contribution::ContributionOffer {
        target: contribution::TargetRepoPin::new(buyer.public_key().to_hex(), source.repo_url())
            .unwrap(),
        base: contribution::ContributionBase::new("main", base.to_string()).unwrap(),
        accepts: vec!["fork".into()],
    };
    for (label, target, server, succeeds) in [
        ("open", None, &relay, true),
        ("targeted", Some(seller.public_key().to_hex()), &relay, true),
        ("refused", None, &refused, false),
        ("inputs", Some(seller.public_key().to_hex()), &with_inputs, true),
        ("policy", None, &policy, false),
    ] {
        let mut home = home::bootstrap(root.path().join(label)).unwrap();
        home.config.relay_url = "ws://127.0.0.1:1".into();
        home.config.privacy.private_jobs = true;
        home.config.privacy.private_content_v2 = true;
        home.config.privacy.private_job_repos = true;
        home.config.privacy.service_pubkey = Some(Keys::generate().public_key().to_hex());
        home.config.privacy.git_base = Some(format!(
            "{}/git/",
            server.repo_url().split("/git/").next().unwrap()
        ));
        let request = job_lifecycle::PostJobRequest {
            visibility: Some(pc::wire::Visibility::Private),
            output_category: Some(pc::wire::Output::Code),
            inputs: if label == "inputs" {
                vec![pc::inputs::InputFile {
                    source: brief.clone(),
                    path: "brief.md".into(),
                }]
            } else {
                vec![]
            },
            task: "contribute".into(),
            output: "git".into(),
            amount_sats: 0,
            seller_pubkey: target.clone(),
            untargeted: target.is_none(),
            deadline_unix: Some(4_000_000_000),
            repo: None,
            branch: None,
            job: job_lifecycle::JobKind::FromScratch,
            requested_agent: None,
            requested_harness_family: None,
            requested_model: None,
            required_capabilities: vec![],
            accepts_delivery: vec![],
            payment_mode: gateway::PaymentMode::None,
        };
        let mut offer = gateway::OfferDraft::untargeted("contribute", "git", 0, 4_000_000_000)
            .with_payment_mode(gateway::PaymentMode::None);
        offer.seller_pubkey = target;
        let result = runtime.block_on(pc::posting::post(
            &home,
            &buyer,
            &request,
            &offer.to_event_draft(),
            Some(&pin),
        ));
        if label == "inputs" {
            let outcome = result.unwrap();
            assert!(with_inputs_repo.find_commit(base).is_ok());
            assert!(with_inputs_repo.find_commit(parent).is_ok());
            assert_eq!(with_inputs_repo.references().unwrap().count(), 2, "base ref + input ref");
            let pushes: Vec<_> = with_inputs
                .requests()
                .into_iter()
                .filter(|r| r.method == "POST" && r.target.ends_with("git-receive-pack"))
                .collect();
            assert_eq!(pushes.len(), 2);
            assert_eq!(
                pushes[0].push_capabilities.as_deref(),
                Some("report-status ofs-delta"),
                "the fetched base is forwarded first, into the empty repository"
            );
            assert_ne!(
                pushes[1].push_capabilities.as_deref(),
                Some("report-status ofs-delta"),
                "inputs use the ordinary upload path"
            );
            let store =
                pc::store::ContentStore::open(&home.root.join("private-content.sqlite")).unwrap();
            assert!(store.event(&outcome.job_id).unwrap().is_some());
        } else if label == "policy" {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("cannot contain symlinks or submodules"), "{error}");
            assert_eq!(
                server.requests().len(),
                3,
                "a permanent 400 is not retried: PUT, advertisement, one POST"
            );
        } else if succeeds {
            let outcome = result.unwrap();
            let store =
                pc::store::ContentStore::open(&home.root.join("private-content.sqlite")).unwrap();
            assert!(
                store.event(&outcome.job_id).unwrap().is_some(),
                "only a fully uploaded offer is queued"
            );
            assert!(destination.find_commit(base).is_ok());
            assert!(
                destination.find_commit(parent).is_ok(),
                "base history must be present too"
            );
        } else {
            assert!(result.is_err());
            let db = rusqlite::Connection::open(home.root.join("private-content.sqlite")).unwrap();
            let events: i64 = db
                .query_row("SELECT COUNT(*) FROM content_events", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                events, 0,
                "a failed input upload must not publish or enqueue the offer"
            );
            assert_eq!(
                server.requests().len(),
                3,
                "permission refusal is not retried"
            );
        }
    }
    assert_eq!(
        destination.references().unwrap().count(),
        2,
        "one immutable input ref per offer, not per retry"
    );
    let requests = relay.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "POST" && r.target.ends_with("git-receive-pack"))
            .count(),
        3
    );
    let signed: Vec<nostr_sdk::Event> = requests
        .iter()
        .map(|r| {
            use base64::Engine;
            use nostr_sdk::JsonUtil;
            let token = r
                .authorization
                .as_ref()
                .unwrap()
                .strip_prefix("Nostr ")
                .unwrap();
            nostr_sdk::Event::from_json(
                base64::engine::general_purpose::STANDARD
                    .decode(token)
                    .unwrap(),
            )
            .unwrap()
        })
        .collect();
    for event in &signed {
        event.verify().unwrap();
    }
    // Git auth timestamps may share a second. Each wire leg still invoked the
    // minter; exercise that contract separately with unmistakable test headers.
    let counter = Arc::new(AtomicUsize::new(0));
    let counted = counter.clone();
    let mint: git_transport::AuthMinter = Arc::new(move |_| {
        Ok(format!(
            "Nostr test-{}",
            counted.fetch_add(1, Ordering::SeqCst)
        ))
    });
    let input_ref = destination
        .references()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .name()
        .unwrap()
        .to_owned();
    let before = relay.requests().len();
    assert_eq!(
        git_transport::push_private_input(
            &destination,
            &relay.repo_url(),
            &input_ref,
            &base.to_string(),
            mint.clone()
        )
        .unwrap(),
        base.to_string()
    );
    assert!(
        git_transport::push_private_input(
            &destination,
            &relay.repo_url(),
            &input_ref,
            &parent.to_string(),
            mint
        )
        .is_err()
    );
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    assert!(
        relay.requests()[before..].iter().all(|r| r.method == "GET"),
        "no overwrite or duplicate pack"
    );
}
