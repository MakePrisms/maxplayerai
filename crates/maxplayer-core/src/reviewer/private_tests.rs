use super::*;
use crate::kinds::REVIEW_KIND;
use crate::review::private::{self, Identity, Request};
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn key(n: u8) -> Keys {
    Keys::parse(&format!("{n:064x}")).unwrap()
}

async fn scenario(targeted: bool, delivery: bool, provider_failure: bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.unwrap();
    let url = relay.url().await.to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let http = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..at]);
                    let len: usize = header
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= at + 4 + len {
                        let body: Value =
                            serde_json::from_slice(&bytes[at + 4..at + 4 + len]).unwrap();
                        let state = body["state"].as_str().unwrap();
                        assert!(state.contains(if targeted {
                            "private task"
                        } else {
                            "deliberately public discovery"
                        }));
                        if delivery {
                            assert!(state.contains("answer"));
                        }
                        count.fetch_add(1, Ordering::SeqCst);
                        break;
                    }
                }
            }
            let (status, body): (&str, &[u8]) = if provider_failure {
                ("400 Bad Request", b"private provider debug canary")
            } else {
                ("200 OK",br#"{"model":"fixture","answers":{"safety":{"type":"choice","choice":"safe","probabilities":{"safe":0.99,"unsafe":0.01}}}}"#)
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(body).await.unwrap();
        }
    });
    let (evidence, _, _, policy) = crate::private_content::evidence::inline_fixture_for(targeted);
    let subject = Subject {
        offer: evidence.offer.id.to_hex(),
        event: if delivery {
            evidence.result.id.to_hex()
        } else {
            evidence.offer.id.to_hex()
        },
        kind: if delivery {
            JOB_RESULT_KIND
        } else {
            JOB_OFFER_KIND
        },
        commit: None,
    };
    let request = Request {
        subject: subject.clone(),
        offer: evidence.offer.clone(),
        task_envelope: evidence.task_envelope.clone(),
        delivery: delivery.then_some(evidence.clone()),
    };
    let dir = tempfile::tempdir().unwrap();
    let mut home = crate::home::bootstrap(dir.path()).unwrap();
    home.config.relay_url = url.clone();
    home.config.privacy.service_pubkey = Some(policy.service.clone());
    home.config.privacy.git_base = Some(policy.host.git_prefix.clone());
    home.config.accepted_mints = policy.host.accepted_mints.clone();
    home.config
        .review
        .reviewers
        .insert(url.clone(), policy.service.clone());
    home.config.review.timeout_seconds = 15;
    let config = ServiceConfig {
        relay: url.clone(),
        signer_file: dir.path().join("unused"),
        provider_key_file: dir.path().join("unused"),
        database: dir.path().join("reviews.db"),
        repositories: BTreeMap::new(),
        accepted_mints: policy.host.accepted_mints,
        private_git_base: Some(policy.host.git_prefix),
        model: "fixture".into(),
    };
    let provider = TypeSafe {
        client: reqwest::Client::new(),
        endpoint: format!("http://{addr}"),
        key: "fixture-not-secret".into(),
    };
    let worker = tokio::task::spawn_local(run_worker(config, key(3), provider));
    let requester = if delivery { key(1) } else { key(2) };
    let client = Client::new(requester.clone());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    // Let the asynchronous service install its intake subscription before publication.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let result = if delivery {
        crate::review::check_buyer(
            &home,
            &requester,
            &subject,
            &key(2).public_key().to_hex(),
            Some(&evidence),
        )
        .await
    } else {
        std::fs::write(&home.key_path, format!("{:064x}", 2)).unwrap();
        let actor = crate::seller_node::signer::spawn(&home).unwrap();
        let mut ctx = crate::private_content::channel::ContentContext::open(
            &home,
            &requester.public_key().to_hex(),
        )
        .unwrap();
        if let Some(task) = &request.task_envelope {
            ctx.stage(
                &crate::private_content::PreparedContent::decode(task).unwrap(),
                Timestamp::now().as_secs(),
            )
            .unwrap();
        }
        ctx.resolve_offer(&request.offer, Timestamp::now().as_secs())
            .unwrap();
        let prepared =
            private::offer_request(&home, &requester.public_key().to_hex(), &subject.offer)
                .unwrap()
                .unwrap();
        private::check(
            &home,
            &client,
            Identity::Seller(&actor),
            &prepared,
            &key(1).public_key().to_hex(),
        )
        .await
    };
    if provider_failure {
        assert!(result.unwrap_err().contains("provider_rejected"));
    } else {
        assert!(result.unwrap().is_some());
    }
    assert!(!worker.is_finished(), "review worker unexpectedly exited");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    if !provider_failure {
        assert!(
            private::check(
                &home,
                &client,
                Identity::Buyer(&requester),
                &request,
                &key(2).public_key().to_hex()
            )
            .await
            .unwrap()
            .is_some()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "retry must reuse immutable result"
        );
    }
    let public = client
        .fetch_events(
            Filter::new().kinds([Kind::Custom(REVIEW_KIND), Kind::Custom(REVIEW_REQUEST_KIND)]),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(
        public.is_empty(),
        "private request/result/error must never appear in public kinds"
    );
    let outer = client
        .fetch_events(
            Filter::new().kind(Kind::GiftWrap).limit(100),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(!outer.is_empty());
    let mut received = std::collections::BTreeSet::new();
    for event in outer {
        let wire = event.as_json();
        for canary in [
            &subject.offer,
            &subject.event,
            "private task",
            "provider_rejected",
            "private provider debug canary",
        ] {
            assert!(!wire.contains(canary));
        }
        assert!(private::unwrap(&key(4), &event).await.is_err());
        for who in [1, 2, 3] {
            if let Ok(inner) = private::unwrap(&key(who), &event).await {
                if inner.kind == Kind::Custom(REVIEW_KIND) {
                    wire::verify(&inner, &key(3).public_key(), &subject).unwrap();
                    received.insert(who);
                }
            }
        }
    }
    assert_eq!(received, std::collections::BTreeSet::from([1, 2, 3]));
    // Even a signed PUBLIC review request referencing this private offer must not
    // elicit a public success or error (nor another provider call).
    client.send_event(&evidence.offer).await.unwrap();
    client
        .send_event_builder(
            crate::gateway::nostr::event_builder(
                &request_draft(&subject, &key(3).public_key().to_hex()).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        client
            .fetch_events(
                Filter::new().kind(Kind::Custom(REVIEW_KIND)),
                Duration::from_secs(2)
            )
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    worker.abort();
    let _ = worker.await;
    http.abort();
    client.disconnect().await;
    relay.shutdown();
}

#[tokio::test]
async fn encrypted_targeted_offer_review_real_worker_and_local_provider() {
    tokio::task::LocalSet::new()
        .run_until(scenario(true, false, false))
        .await;
}
#[tokio::test]
async fn encrypted_open_pool_offer_review_real_worker_and_local_provider() {
    tokio::task::LocalSet::new()
        .run_until(scenario(false, false, false))
        .await;
}
#[tokio::test]
async fn encrypted_targeted_delivery_review_real_worker_and_local_provider() {
    tokio::task::LocalSet::new()
        .run_until(scenario(true, true, false))
        .await;
}
#[tokio::test]
async fn encrypted_open_pool_delivery_review_real_worker_and_local_provider() {
    tokio::task::LocalSet::new()
        .run_until(scenario(false, true, false))
        .await;
}
#[tokio::test]
async fn encrypted_review_provider_errors_never_fall_back_to_public() {
    tokio::task::LocalSet::new()
        .run_until(scenario(true, true, true))
        .await;
}

#[tokio::test]
async fn private_git_review_reads_exact_commit_and_refuses_changed_binding() {
    use crate::private_content as pc;
    let (mut evidence, _, _, policy) = pc::evidence::inline_fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare");
    let repo = git2::Repository::init_bare(&path).unwrap();
    let blob = repo.blob(b"private git file canary").unwrap();
    let mut tree = repo.treebuilder(None).unwrap();
    tree.insert("answer.txt", blob, 0o100644).unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let sig = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
    let commit = repo
        .commit(None, &sig, &sig, "opaque", &tree, &[])
        .unwrap()
        .to_string();
    let tags = pc::wire::validate_private(&evidence.offer, &policy.host).unwrap();
    let url = policy
        .host
        .job_repo(
            &evidence.offer.pubkey.to_hex(),
            tags.required("job").unwrap(),
        )
        .unwrap();
    let branch = format!("refs/heads/delivery/{}", evidence.award.id.to_hex());
    let mut preimage = evidence
        .validate(&key(1).public_key().to_hex(), &policy)
        .unwrap()
        .preimage;
    preimage.delivery_kind = "fork".into();
    preimage.delivery_integrity_hash = commit.clone();
    let receipt = key(2)
        .sign_schnorr(&nostr_sdk::secp256k1::Message::from_digest(
            preimage.digest_bytes(),
        ))
        .to_string();
    let mut draft = crate::job_lifecycle::event_to_draft(&evidence.result);
    draft.tags.retain(|t| {
        !matches!(
            t.first(),
            Some("content-id" | "content-commitment" | "delivery" | "sig")
        )
    });
    draft.tags.extend([
        TagSpec::new(["delivery", "git"]),
        TagSpec::new(["repo", &url]),
        TagSpec::new(["branch", &branch]),
        TagSpec::new(["commit", &commit]),
        TagSpec::new(["sig", "seller", &receipt]),
    ]);
    evidence.result = pc::builders::sign(&key(2), draft).unwrap();
    evidence.answer_envelope = None;
    let subject = Subject {
        offer: evidence.offer.id.to_hex(),
        event: evidence.result.id.to_hex(),
        kind: JOB_RESULT_KIND,
        commit: Some(commit),
    };
    let request = Request {
        subject,
        offer: evidence.offer.clone(),
        task_envelope: evidence.task_envelope.clone(),
        delivery: Some(evidence),
    };
    request.validate(&key(1).public_key(), &policy).unwrap();
    let config = ServiceConfig {
        relay: "wss://git.example".into(),
        signer_file: dir.path().join("unused"),
        provider_key_file: dir.path().join("unused"),
        database: dir.path().join("db"),
        repositories: BTreeMap::from([(url, path)]),
        accepted_mints: policy.host.accepted_mints.clone(),
        private_git_base: Some(policy.host.git_prefix.clone()),
        model: "fixture".into(),
    };
    let input = private_snapshot(
        &config,
        &request,
        &policy,
        &key(3),
        std::time::Instant::now() + WINDOW,
    )
    .await
    .unwrap();
    assert!(
        String::from_utf8(input)
            .unwrap()
            .contains("private git file canary")
    );
    let mut changed = request;
    changed.subject.commit = Some("ff".repeat(20));
    assert!(changed.validate(&key(1).public_key(), &policy).is_err());
}

#[tokio::test]
async fn reviewer_public_v2_snapshot_remains_public_and_reads_exact_inline_envelope() {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.unwrap();
    let url = relay.url().await.to_string();
    let (evidence, buyer) = crate::private_content::public_v2::tests::fixture(false, false);
    let client = Client::new(buyer.clone());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    client.send_event(&evidence.offer).await.unwrap();
    client.send_event(&evidence.result).await.unwrap();
    let subject = Subject {
        offer: evidence.offer.id.to_hex(),
        event: evidence.result.id.to_hex(),
        kind: JOB_RESULT_KIND,
        commit: None,
    };
    let request = crate::gateway::nostr::event_builder(
        &request_draft(&subject, &key(3).public_key().to_hex()).unwrap(),
    )
    .unwrap()
    .sign_with_keys(&buyer)
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = ServiceConfig {
        relay: url,
        signer_file: dir.path().join("unused"),
        provider_key_file: dir.path().join("unused"),
        database: dir.path().join("db"),
        repositories: BTreeMap::new(),
        accepted_mints: review_mints(),
        private_git_base: None,
        model: "fixture".into(),
    };
    let body = snapshot(
        &client,
        &config,
        &request,
        &subject,
        &key(3),
        std::time::Instant::now() + WINDOW,
    )
    .await
    .unwrap();
    let provider: Value = serde_json::from_slice(&body).unwrap();
    let input: Value = serde_json::from_str(provider["state"].as_str().unwrap()).unwrap();
    assert_eq!(input["result"]["content"], evidence.result.content);
    assert!(!private::is_private(&request));
    client.disconnect().await;
    relay.shutdown();
}

#[test]
fn private_delivery_refs_are_not_double_prefixed() {
    let reference = format!("refs/heads/delivery/{}", "ab".repeat(32));
    assert_eq!(review_source_ref(&reference).unwrap(), reference);
    assert_eq!(review_source_ref("main").unwrap(), "refs/heads/main");
    for bad in ["", "refs/heads/", "../secrets", "main:other"] {
        assert!(review_source_ref(bad).is_err());
    }
}
