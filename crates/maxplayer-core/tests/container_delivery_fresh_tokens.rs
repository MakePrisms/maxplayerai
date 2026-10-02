//! Production container entry -> gated commit -> fresh per-leg authorization -> verified TLS
//! smart-HTTP push -> remote ref. Agent/reap and host signer are local fixtures, not Docker.
#![cfg(all(unix, feature = "wallet"))]

use maxplayer_core::delivery_orchestrator as orch;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[path = "git_http_fixture/mod.rs"]
mod git_http_fixture;

#[test]
fn container_entry_refreshes_each_wire_leg_and_delivers_the_gated_commit() {
    let root = tempfile::tempdir().unwrap();
    let bare = root.path().join("relay.git");
    git2::Repository::init_bare(&bare).unwrap();
    let relay = git_http_fixture::GitHttpAuthServer::spawn(&bare, "/git/seller/job.git");
    let ca = relay.ca_file(root.path());
    // Single-test process; set trust before the production HTTP clients are constructed.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", ca);
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
    }
    assert!(std::env::var_os("GIT_SSL_NO_VERIFY").is_none());
    let out = root.path().join("exchange");
    std::fs::create_dir(&out).unwrap();
    let nonce = "0123456789abcdef".repeat(4);
    let branch = "maxplayer/abc12345";
    let inputs = orch::Phase1Inputs {
        job_hash: "a".repeat(64),
        seller_pubkey_hex: "e".repeat(64),
        base: None,
        delivery_branch: branch.into(),
        message: "delivery".into(),
        author_date_unix: 1_700_000_000,
        agent_argv: vec![],
        workdir: root.path().join("work"),
        out_dir: out.clone(),
        prompt: "write answer".into(),
        deadline_unix: 4_000_000_000,
        max_agent_attempts: 1,
        agent_env_names: vec![],
        mcp_servers: vec![],
        codex_session: None,
        relay_url: relay.repo_url(),
        push_token: orch::PushTokenSource::FreshAfterAgent { wait_secs: 5 },
        handoff_nonce: nonce.clone(),
    };
    let path = out.join(orch::PHASE1_INPUTS_FILE);
    orch::write_phase1_inputs(&path, &inputs).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let host_stopped = stopped.clone();
    let host_out = out.clone();
    let host = std::thread::spawn(move || {
        assert!(orch::wait_for_file(
            &host_out.join(orch::AGENT_DONE_MARKER),
            Duration::from_secs(10),
            Duration::from_millis(5)
        ));
        let marker = orch::read_agent_done_marker(&host_out, &nonce)
            .unwrap()
            .unwrap();
        orch::write_secret_file(
            &host_out.join(orch::PUSH_TOKEN_FILE),
            "Nostr readiness-not-for-wire",
        )
        .unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        let mut previous = 0;
        while !host_stopped.load(Ordering::SeqCst) && Instant::now() < until {
            if let Some(sequence) =
                orch::read_push_token_request(&host_out, &nonce, previous).unwrap()
            {
                std::fs::remove_file(host_out.join(orch::PUSH_TOKEN_REQUEST)).unwrap();
                orch::write_secret_file(
                    &host_out.join(orch::PUSH_TOKEN_FILE),
                    &format!("Nostr leg-{sequence}"),
                )
                .unwrap();
                previous = sequence;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        (marker, previous)
    });
    let result = orch::run_phase1_entry_with(
        &path,
        |key| {
            (key == orch::CONTAINER_DELIVERY_ENV).then(|| orch::CONTAINER_DELIVERY_ENV_VALUE.into())
        },
        |inputs, workdir| {
            assert!(!inputs.out_dir.join(orch::PUSH_TOKEN_FILE).exists());
            std::fs::write(workdir.join("answer.txt"), "the delivered answer\n").unwrap();
            Ok(orch::AgentOutcome::default())
        },
        || Ok(()),
    );
    stopped.store(true, Ordering::SeqCst);
    let (marker, minted) = host.join().unwrap();
    let result = result.expect("real container entry must deliver");
    let requests = relay.requests();
    assert!(requests.iter().any(|r| r.method == "POST"));
    assert!(requests.len() >= 2);
    assert_eq!(minted as usize, requests.len());
    for (i, request) in requests.iter().enumerate() {
        assert_eq!(
            request.authorization.as_deref(),
            Some(format!("Nostr leg-{}", i + 1).as_str())
        );
    }
    assert_eq!(marker.expected_oid, result.delivery_oid);
    let repo = git2::Repository::open_bare(bare).unwrap();
    assert_eq!(
        repo.refname_to_id(&format!("refs/heads/{branch}"))
            .unwrap()
            .to_string(),
        result.delivery_oid
    );
    assert!(!out.join(orch::PUSH_TOKEN_FILE).exists());
}
