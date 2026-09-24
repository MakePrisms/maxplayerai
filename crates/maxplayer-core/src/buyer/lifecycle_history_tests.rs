//! Local WebSocket model of the bundled relay's newest-first cap + exact EOSE.
//! Exercises the real reader and reservation reconciliation, without money movement.
use super::*;
use futures_util::{SinkExt, StreamExt};
use nostr_sdk::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CappedRelay {
    url: String,
    mode: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for CappedRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl CappedRelay {
    async fn start(mut variants: Vec<Vec<Event>>) -> Self {
        for events in &mut variants {
            events.sort_by_key(|e| (std::cmp::Reverse(e.created_at), e.id));
        }
        let variants = Arc::new(variants);
        let mode = Arc::new(AtomicUsize::new(0));
        let selected = mode.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let variants = variants.clone();
                let selected = selected.clone();
                connections.spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    use tokio_tungstenite::tungstenite::Message;
                    if ws
                        .send(Message::Text(
                            serde_json::json!(["AUTH", "history-fixture"])
                                .to_string()
                                .into(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    while let Some(Ok(message)) = ws.next().await {
                        let Ok(text) = message.to_text() else {
                            continue;
                        };
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                            continue;
                        };
                        if value[0] == "AUTH" || value[0] == "EVENT" {
                            let text =
                                serde_json::json!(["OK", value[1]["id"], true, ""]).to_string();
                            if ws.send(Message::Text(text.into())).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        if value[0] != "REQ" {
                            continue;
                        }
                        let id = &value[1];
                        let filter: Filter = serde_json::from_value(value[2].clone()).unwrap();
                        let limit = filter.limit.unwrap_or(2000).min(2000);
                        let active = selected.load(Ordering::SeqCst);
                        let is_result = filter.kinds.as_ref().is_some_and(|kinds| kinds.contains(&Kind::Custom(crate::kinds::JOB_RESULT_KIND)));
                        // Mode 6 fails the first result page; mode 7 fails a later
                        // page, after the initial full page has already succeeded.
                        if is_result && (active == 6 || (active == 7 && filter.until.is_some())) {
                            let text = serde_json::json!(["CLOSED", id, "error: historical query failed"]).to_string();
                            if ws.send(Message::Text(text.into())).await.is_err() { return; }
                            continue;
                        }
                        let events = &variants[match active { 6 => 3, 7 => 2, _ => active }];
                        // Buzz pushes e/kind/author/time into SQL, but applies t
                        // after SQL LIMIT. Model that distinction explicitly.
                        let mut sql_filter = filter.clone();
                        sql_filter
                            .generic_tags
                            .remove(&SingleLetterTag::lowercase(Alphabet::T));
                        for (n, event) in events
                            .iter()
                            .filter(|e| sql_filter.match_event(e, Default::default()))
                            .take(limit)
                            .filter(|e| filter.match_event(e, Default::default()))
                            .enumerate()
                        {
                            let text = serde_json::json!(["EVENT", id, event]).to_string();
                            if ws.send(Message::Text(text.into())).await.is_err() {
                                return;
                            }
                            // Avoid testing notification-channel overflow instead of pagination.
                            if n % 20 == 19 {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                            }
                        }
                        let text = serde_json::json!(["EOSE", id]).to_string();
                        if ws.send(Message::Text(text.into())).await.is_err() {
                            return;
                        }
                    }
                });
                while connections.try_join_next().is_some() {}
            }
        });
        Self { url, mode, task }
    }
}

#[tokio::test]
async fn private_content_capped_public_history_keeps_reservation_and_recovers() {
    exercise_capped_history(false).await;
}

#[tokio::test]
async fn private_content_capped_private_history_keeps_reservation_and_recovers() {
    exercise_capped_history(true).await;
}

async fn exercise_capped_history(private: bool) {
    let now = now_unix() as u64;
    let (e, buyer, policy) = if private {
        // Open-pool private trade exercises locally authenticated seller selection.
        let (e, _, buyer, policy) =
            crate::private_content::evidence::inline_fixture_with_deadline(false, true, now - 1);
        (e, buyer, Some(policy))
    } else {
        // Public targeted trade also exercises the offer-authenticated target.
        let (e, buyer) =
            crate::private_content::public_v2::tests::fixture_deadline(true, false, now - 1);
        crate::private_content::public_v2::validate_evidence(&e, &buyer.public_key().to_hex())
            .unwrap();
        (e, buyer, None)
    };
    // Signing the base chain can cross a wall-clock second under suite load.
    // Anchor newer filler rows to the finished result, not the pre-fixture clock.
    let now = now_unix().max(e.result.created_at.as_secs() as i64) as u64;
    let outsider = Keys::generate();
    let seller = Keys::parse(&format!("{:064x}", 2)).unwrap();
    let base = vec![
        e.offer.clone(),
        e.claim.clone(),
        e.award.clone(),
        e.result.clone(),
    ];
    let mut expired = base.clone();
    for i in 0..128 {
        let row = EventBuilder::new(Kind::Custom(crate::kinds::JOB_RESULT_KIND), format!("expired {i}"))
            .tags([Tag::event(e.offer.id), Tag::hashtag("maxplayer"), Tag::expiration(Timestamp::from(now + 2))])
            .custom_created_at(Timestamp::from(now + 1)).sign_with_keys(&seller).unwrap();
        assert!(row.created_at > e.result.created_at);
        row.verify().unwrap();
        expired.push(row);
    }
    let mut outsiders = base.clone();
    let mut saturated = base.clone();
    let mut paginated = base.clone();
    let mut foreign_namespace = base.clone();
    for i in 0..2000 {
        let draft = |namespace: &str| {
            EventBuilder::new(
                Kind::Custom(crate::kinds::JOB_RESULT_KIND),
                format!("invalid application result {i}"),
            )
            .tags([Tag::event(e.offer.id), Tag::hashtag(namespace)])
        };
        outsiders.push(
            draft("maxplayer")
                .custom_created_at(Timestamp::from(now + 1))
                .sign_with_keys(&outsider)
                .unwrap(),
        );
        saturated.push(
            draft("maxplayer")
                .custom_created_at(Timestamp::from(now + 1))
                .sign_with_keys(&seller)
                .unwrap(),
        );
        foreign_namespace.push(
            draft("foreign")
                .custom_created_at(Timestamp::from(now + 1))
                .sign_with_keys(&seller)
                .unwrap(),
        );
        paginated.push(
            draft(if i % 2 == 0 { "foreign" } else { "maxplayer" })
                .custom_created_at(Timestamp::from(now + 1 + i / 32))
                .sign_with_keys(&seller)
                .unwrap(),
        );
    }
    let relay = CappedRelay::start(vec![
        outsiders,
        saturated,
        paginated,
        base,
        foreign_namespace,
        expired,
    ])
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut home = crate::home::bootstrap(root.path()).unwrap();
    std::fs::write(&home.key_path, format!("{:064x}", 1)).unwrap();
    home.config.relay_url = relay.url.clone();
    home.config.buyer_reservation_floor.enabled = false;
    if let Some(policy) = policy {
        home.config.privacy.private_content_v2 = true;
        home.config.privacy.private_job_repos = true;
        home.config.privacy.private_jobs = true;
        home.config.privacy.service_pubkey = Some(policy.service.clone());
        home.config.privacy.git_base = Some(policy.host.git_prefix.clone());
        home.config.accepted_mints = policy.host.accepted_mints.clone();
        e.validate(&buyer.public_key().to_hex(), &policy).unwrap();
        let mut ctx = crate::private_content::channel::ContentContext::open(
            &home,
            &buyer.public_key().to_hex(),
        )
        .unwrap();
        for envelope in e.task_envelope.iter().chain(e.answer_envelope.iter()) {
            ctx.stage(
                &crate::private_content::PreparedContent::decode(envelope).unwrap(),
                now,
            )
            .unwrap();
        }
        ctx.select(&e.offer, &e.claim, &e.award).unwrap();
    } else {
        crate::private_content::public_v2::Context::open(&home)
            .unwrap()
            .select(&e.offer, &e.claim, &e.award)
            .unwrap();
    }
    let job = e.offer.id.to_hex();
    let (_lock, context, _socket) = bootstrap(home).await.unwrap();
    context.store.reserve(&job, 10, 1000, now_unix()).unwrap();
    // 2,000 outsider results no longer displace the selected seller's delivery.
    let view = job_lifecycle::fetch_job_view_async(
        &context.home,
        &buyer,
        &job,
        Duration::from_secs(20),
        now,
    )
    .await
    .unwrap();
    assert_eq!(view.results.len(), 1);
    assert!(view.live_claim_id.is_some());
    let report = reconcile_reservations(&context).await.unwrap();
    assert!(report.kept.contains(&job), "outsider page: {report:?}");
    // A selected author's saturated timestamp must be UNKNOWN, never confirmed empty.
    relay.mode.store(1, Ordering::SeqCst);
    assert!(matches!(
        job_lifecycle::fetch_job_view_async(
            &context.home,
            &buyer,
            &job,
            Duration::from_secs(20),
            now
        )
        .await,
        Err(job_lifecycle::JobLifecycleError::Relay(_))
    ));
    let report = reconcile_reservations(&context).await.unwrap();
    assert!(report.kept.contains(&job), "saturated page: {report:?}");
    assert!(!report.released.contains(&job));
    // Namespace post-filtering on the relay must not turn a SQL-saturated page
    // into a successful empty history. Count the requested superset first.
    relay.mode.store(4, Ordering::SeqCst);
    assert!(matches!(
        job_lifecycle::fetch_job_view_async(
            &context.home,
            &buyer,
            &job,
            Duration::from_secs(20),
            now
        )
        .await,
        Err(job_lifecycle::JobLifecycleError::Relay(_))
    ));
    let report = reconcile_reservations(&context).await.unwrap();
    assert!(
        report.kept.contains(&job),
        "foreign namespace page: {report:?}"
    );
    assert!(!report.released.contains(&job));
    // Retained rows can expire AFTER storage/query and BEFORE SDK delivery.
    // Wait for fixture expiry (real SDK uses wall clock); don't alter SDK validation.
    while Timestamp::now().as_secs() <= now + 2 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for mode in [5, 6, 7] {
        relay.mode.store(mode, Ordering::SeqCst);
        assert!(matches!(job_lifecycle::fetch_job_view_async(
            &context.home, &buyer, &job, Duration::from_secs(20), now).await,
            Err(job_lifecycle::JobLifecycleError::Relay(_))), "unsafe success in mode {mode}");
        let report = reconcile_reservations(&context).await.unwrap();
        assert!(report.kept.contains(&job), "mode {mode}: {report:?}");
        assert!(!report.released.contains(&job), "mode {mode}: {report:?}");
        relay.mode.store(3, Ordering::SeqCst);
        let report = reconcile_reservations(&context).await.unwrap();
        assert!(report.kept.contains(&job), "mode {mode} recovery: {report:?}");
    }
    // >2,000 results spread over seconds are actually paginated, not just rejected.
    relay.mode.store(2, Ordering::SeqCst);
    let view = job_lifecycle::fetch_job_view_async(
        &context.home,
        &buyer,
        &job,
        Duration::from_secs(30),
        now,
    )
    .await
    .unwrap();
    assert_eq!(view.results.len(), 1);
    assert!(view.live_claim_id.is_some());
    // Recovery after saturation clears: real reconciliation succeeds and keeps funds.
    relay.mode.store(3, Ordering::SeqCst);
    let report = reconcile_reservations(&context).await.unwrap();
    assert!(report.kept.contains(&job), "recovered: {report:?}");
    assert!(!report.released.contains(&job));
}
