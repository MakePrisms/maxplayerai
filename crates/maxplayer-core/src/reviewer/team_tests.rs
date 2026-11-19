//! Worker-level Maxplayer Team review tests (docs/specs/maxplayer-team.md §8).
use super::*;
use crate::kinds::REVIEW_KIND;
use crate::review::private;
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

const CHANNEL: &str = "0b0e5f1c-9d4a-4c3e-8f21-6a7b8c9d0e1f";

async fn send(client: &Client, who: &Keys, reviewer: &Keys, request: &crate::review::team::Request) {
    let inner = crate::gateway::nostr::event_builder(
        &request.draft(&reviewer.public_key().to_hex()).unwrap(),
    )
    .unwrap()
    .sign_with_keys(who)
    .unwrap();
    let outer = private::wrap(who, reviewer.public_key(), &inner).await.unwrap();
    client.send_event(&outer).await.unwrap();
}

async fn verdicts(client: &Client, who: &Keys, reviewer: &Keys, subject: &Subject) -> Vec<crate::review::Decision> {
    let wraps = client
        .fetch_events(
            Filter::new().kind(Kind::GiftWrap).pubkey(who.public_key()).limit(200),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    let mut out = Vec::new();
    for event in wraps {
        if let Ok(reply) = private::unwrap(who, &event).await {
            if reply.kind == Kind::Custom(REVIEW_KIND) {
                let review = wire::verify(&reply, &reviewer.public_key(), subject).unwrap();
                out.push(review.decision(subject, 500_000).unwrap());
            }
        }
    }
    out
}

async fn scenario() {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.unwrap();
    let url = relay.url().await.to_string();
    // Exactly one provider response: a second classifier call would fail.
    let (provider, calls, http) =
        super::tests::http_provider(vec![(200, None, super::tests::response(0.01))]).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let reviewer = Keys::generate();
    let config = ServiceConfig {
        relay: url.clone(),
        signer_file: root.join("unused"),
        provider_key_file: root.join("unused"),
        database: root.join("reviews.db"),
        repositories: BTreeMap::new(),
        accepted_mints: review_mints(),
        private_git_base: Some("https://relay.example/git/".into()),
        model: "fixture".into(),
        window_seconds: 300,
    };
    let (alice, bob, author) = (Keys::generate(), Keys::generate(), Keys::generate());
    // Alice's job-review budget is spent; her team request must still be admitted.
    let store = Store::open(&config.database).unwrap();
    for _ in 0..10 {
        store.admit(&alice.public_key().to_hex(), Timestamp::now().as_secs()).unwrap();
    }
    drop(store);
    let worker = tokio::task::spawn_local(run_worker(config, reviewer.clone(), provider));
    let client = Client::new(alice.clone());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let message = EventBuilder::new(Kind::Custom(9), "team message text")
        .tag(Tag::parse(["h", CHANNEL]).unwrap())
        .sign_with_keys(&author)
        .unwrap();
    let request = crate::review::team::Request::message(message).unwrap();
    let subject = request.subject.clone();
    // Both members ask while the first request is still queued.
    send(&client, &alice, &reviewer, &request).await;
    send(&client, &bob, &reviewer, &request).await;
    let mut got = (vec![], vec![]);
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        got = (
            verdicts(&client, &alice, &reviewer, &subject).await,
            verdicts(&client, &bob, &reviewer, &subject).await,
        );
        if !got.0.is_empty() && !got.1.is_empty() {
            break;
        }
    }
    assert_eq!(got.0, vec![crate::review::Decision::Passed], "first requester");
    assert_eq!(got.1, vec![crate::review::Decision::Passed], "second requester, same subject");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "one classifier call");
    // More requests than the per-minute budget: every one is a cache hit, so none is
    // admitted (no rate_limited error) and none reaches the provider. Sent one at a time:
    // a requester's second request while its first is in flight is deduplicated by design.
    let mut all = vec![];
    for n in 2..=13 {
        send(&client, &alice, &reviewer, &request).await;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            all = verdicts(&client, &alice, &reviewer, &subject).await;
            if all.len() >= n {
                break;
            }
        }
        assert_eq!(all.len(), n);
    }
    assert!(all.iter().all(|d| *d == crate::review::Decision::Passed), "{all:?}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!worker.is_finished());
    worker.abort();
    let _ = worker.await;
    http.abort();
    client.disconnect().await;
    relay.shutdown();
}

#[tokio::test]
async fn two_members_share_one_classifier_call_and_cache_hits_skip_admission() {
    tokio::task::LocalSet::new().run_until(scenario()).await;
}

/// A job-shaped `offer` inside a team bundle is refused before any fetch.
#[test]
fn team_bundle_with_job_shaped_offer_is_refused() {
    let repo = format!("{}{}", "ab".repeat(16), "0".repeat(32));
    let mut request = crate::review::team::Request::commit(&repo, &"a".repeat(64), &"c".repeat(40)).unwrap();
    // A job id as `offer`, with `event` re-bound to it, so only the team-id check can refuse it.
    request.subject.offer = "e".repeat(64);
    request.subject.event = crate::review::team_commit_event(&request.subject.offer, &"c".repeat(40));
    let json = serde_json::to_string(&request).unwrap();
    assert!(request.validate(&Keys::generate().public_key()).is_err());
    // It is still parsed as a team bundle, never as a job bundle.
    assert!(matches!(parse_bundle(&json), Ok(Bundle::Team(_))));
}
