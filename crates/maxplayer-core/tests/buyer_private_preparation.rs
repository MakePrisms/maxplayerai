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
            // PUT provisioning, GET reconciliation, GET advertisement, POST upload.
            reject_request: Some((4, "503 Service Unavailable")),
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
            reject_request: Some((4, "403 Forbidden")),
            ..Default::default()
        },
    );
    // Install all test trust anchors before either reqwest client is constructed.
    let mut pem = String::new();
    for server in [&source, &relay, &refused] {
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
    let buyer = Keys::generate();
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
            inputs: vec![],
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
        if succeeds {
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
                4,
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
