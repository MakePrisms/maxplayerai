//! LOCAL-RELAY integration tests: drive [`run_forever_hooked`] end-to-end against an in-process
//! NIP-01 relay (`nostr-relay-builder`) and assert on RELAY TRAFFIC + TIMING — never on
//! stderr/scrollback. They prove the open-pool live-delivery behaviour:
//!  * **A** — the daemon reaches READY quickly (mpsc readiness hook, not a log scrape);
//!  * **B** — a running seller does NOT replay the relay's offer history (bounded startup ingest);
//!  * **C** — a fresh UNtargeted offer posted to a RUNNING seller is CLAIMED without a
//!    restart (relay receives the seller's claim-kind `status=processing` tagged `e=<offer id>`) —
//!    the "untargeted offer → claim without restart" proof that was previously never obtained;
//!  * **D** — if the relay CLOSES the broad open-pool subscription, the daemon still reaches READY
//!    and degrades to the TARGETED-only filter, still claiming a fresh targeted offer (the
//!    Loud-Closed fallback).
//!
//! NIP-42 deviation (noted): the in-process relay issues AUTH challenges only LAZILY (on the
//! first REQ/EVENT), whereas mobee-relay challenges on connect. So these tests run the relay
//! WITHOUT NIP-42 gating and rely on the daemon's non-fatal `NoChallenge` auth path (a
//! challenge-on-connect relay still authenticates in milliseconds — unchanged behaviour there).
//! Offer DELIVERY is the behaviour under test here; the p-gated kind-1059 receive (money path) is
//! exercised separately against a real relay.

    use super::*;
    use crate::gateway::{self, OfferDraft};
    use crate::home::{self, SellerConfig, DEFAULT_MINT_URL};
    use nostr_relay_builder::prelude::{
        Alphabet, BoxedFuture, LocalRelay, PolicyResult, QueryPolicy, RelayBuilder, SingleLetterTag,
    };
    use nostr_sdk::prelude::{
        Client, Filter, Keys, Kind, PublicKey, RelayPoolNotification, Timestamp,
    };
    use std::collections::HashSet;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    static IT_SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_root(label: &str) -> std::path::PathBuf {
        let n = IT_SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-it-{label}-{}-{n}", std::process::id()))
    }

    /// Bootstrap a seller home bound to `relay_url` (mint stays the fail-closed testnut default).
    /// `offer_backfill_secs` sets the offer-backfill window (0 = live-only, pre-backfill shape).
    fn seller_home(
        root: &std::path::Path,
        relay_url: &str,
        claim_open_pool: bool,
        offer_backfill_secs: u64,
    ) -> home::MobeeHome {
        let mut h = home::bootstrap(root).expect("bootstrap seller home");
        h.config.relay_url = relay_url.to_string();
        assert_eq!(
            h.config.default_mint(),
            DEFAULT_MINT_URL,
            "home mint must be the fail-closed testnut default"
        );
        h.config.seller = Some(SellerConfig {
            // Stub agent: the CLAIM (claim-kind processing) is published in `on_offer_event` BEFORE
            // execution, so no real ACP agent is needed — execution fails fast after the claim we
            // assert on, releasing single-flight.
            agent_command: vec!["true".into()],
            rate_sats: 1,
            git_remote: "https://example.invalid/mobee-it.git".into(),
            job_timeout_secs: Some(5),
            agent: None,
            claim_open_pool,
            offer_backfill_secs,
            contribution_enabled: true,
        });
        h
    }

    /// Start the in-process relay from `builder`. Keep the returned handle alive for the whole
    /// test — dropping every clone shuts the relay down.
    async fn start_relay(builder: RelayBuilder) -> (LocalRelay, String) {
        let relay = LocalRelay::new(builder);
        relay.run().await.expect("relay run");
        let url = relay.url().await.to_string();
        (relay, url)
    }

    async fn connect_client(relay_url: &str) -> Client {
        let client = Client::new(Keys::generate());
        client.add_relay(relay_url).await.expect("add relay");
        client.connect().await;
        client.wait_for_connection(Duration::from_secs(5)).await;
        client
    }

    /// Build an offer draft carrying the fail-closed testnut mint (so it is never skipped for the
    /// wrong reason) and a FUTURE deadline (~1h out) so the offer-freshness gate does not refuse
    /// it. `targeted_to = Some(hex)` ⇒ a `#p`-tagged (targeted) offer; `None` ⇒ open.
    fn offer_draft(targeted_to: Option<&str>) -> OfferDraft {
        offer_draft_with_deadline(targeted_to, now_unix() + 3_600)
    }

    /// Like [`offer_draft`] but with an explicit `deadline_unix` — used to build an already-lapsed
    /// offer (deadline in the past) for the deadline-expiry money-safety test.
    fn offer_draft_with_deadline(targeted_to: Option<&str>, deadline_unix: u64) -> OfferDraft {
        match targeted_to {
            Some(pk) => OfferDraft::new("do a task", "text", 10, deadline_unix, pk),
            None => OfferDraft::untargeted("do a task", "text", 10, deadline_unix),
        }
    }

    /// Sign an offer with `buyer` via the SAME event bridge the buyer CLI uses, then publish it.
    async fn publish_offer(
        client: &Client,
        buyer: &Keys,
        draft: &OfferDraft,
        created_at: Option<Timestamp>,
    ) -> nostr_sdk::EventId {
        let mut builder =
            gateway::nostr::event_builder(&draft.to_event_draft()).expect("offer event builder");
        if let Some(ts) = created_at {
            builder = builder.custom_created_at(ts);
        }
        let event = builder.sign_with_keys(buyer).expect("sign offer");
        let id = event.id;
        client.send_event(&event).await.expect("publish offer");
        id
    }

    fn tag_value(event: &nostr_sdk::Event, name: &str) -> Option<String> {
        event.tags.iter().find_map(|t| {
            let slice = t.as_slice();
            (slice.first().map(String::as_str) == Some(name))
                .then(|| slice.get(1).cloned())
                .flatten()
        })
    }

    /// Collect every feedback-kind the seller publishes as `(e-tag, status)`. The receiver is created
    /// before the daemon starts so no startup claim is missed.
    fn spawn_claim_collector(
        client: &Client,
        seller_pk: PublicKey,
    ) -> Arc<Mutex<Vec<(String, String)>>> {
        let claims: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = claims.clone();
        let mut notif = client.notifications();
        tokio::spawn(async move {
            while let Ok(n) = notif.recv().await {
                if let RelayPoolNotification::Event { event, .. } = n {
                    let kind = event.kind.as_u16();
                    if (kind == gateway::JOB_CLAIM_KIND || kind == gateway::JOB_FEEDBACK_KIND)
                        && event.pubkey == seller_pk
                    {
                        if let Some(e) = tag_value(&event, "e") {
                            let status = tag_value(&event, "status").unwrap_or_default();
                            sink.lock().unwrap_or_else(|e| e.into_inner()).push((e, status));
                        }
                    }
                }
            }
        });
        claims
    }

    fn claims_contain(claims: &Arc<Mutex<Vec<(String, String)>>>, offer_id: &str, status: &str) -> bool {
        claims
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(e, s)| e == offer_id && s == status)
    }

    /// Poll `cond` until true or `timeout` elapses.
    async fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut cond: F) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn hooks(ready: tokio::sync::mpsc::UnboundedSender<()>) -> RunHooks {
        RunHooks {
            ready: Some(ready),
            // Short auth-wait: the ungated in-process relay never challenges, so proceed fast.
            auth_wait: Some(Duration::from_millis(500)),
            wrap_backfill_done: None,
            awarded: None,
            released: None,
        }
    }

    /// Run the daemon on a dedicated OS thread with its own current-thread runtime — mirroring
    /// production `run_forever_blocking`. This sidesteps `tokio::spawn`'s `Send` bound: under the
    /// `acp` feature the daemon future is not `Send` (`AcpDriver` holds a non-`Sync` mpsc
    /// receiver). The daemon reaches the relay over localhost and signals READY over `ready`.
    fn spawn_daemon_thread(
        daemon: SellerDaemon,
        ready: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("daemon runtime");
            let _ = rt.block_on(run_forever_hooked(daemon, hooks(ready)));
        })
    }

    /// Like [`spawn_daemon_thread`] but also wires the PERIODIC wrap-backfill hook, so the
    /// periodic tests can observe each periodic run's stored-1059 count without scraping stderr.
    fn spawn_daemon_thread_with_backfill_hook(
        daemon: SellerDaemon,
        ready: tokio::sync::mpsc::UnboundedSender<()>,
        backfill_done: tokio::sync::mpsc::UnboundedSender<usize>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("daemon runtime");
            let _ = rt.block_on(run_forever_hooked(
                daemon,
                RunHooks {
                    ready: Some(ready),
                    auth_wait: Some(Duration::from_millis(500)),
                    wrap_backfill_done: Some(backfill_done),
                    awarded: None,
                    released: None,
                },
            ));
        })
    }

    // ── Assertions A + B + C on one running open-pool seller ────────────────────────────────
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_untargeted_delivery_to_running_open_pool_seller() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Clear any single-flight left by a prior test's (now relay-less) daemon thread.
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        // Seed the relay's offer HISTORY: M offers dated 2 min in the PAST (mix untargeted +
        // targeted-foreign), all with the fail-closed testnut mint. A BOUNDED running seller must
        // ingest ~none of these; an unbounded one would fetch them all and claim one.
        let buyer = Keys::generate();
        let foreign_seller = Keys::generate().public_key().to_hex();
        let seeder = connect_client(&relay_url).await;
        let past = Timestamp::from(Timestamp::now().as_secs().saturating_sub(120));
        const M: usize = 60;
        let mut historical_ids: HashSet<String> = HashSet::new();
        for i in 0..M {
            let draft = if i % 2 == 0 {
                offer_draft(None)
            } else {
                offer_draft(Some(&foreign_seller))
            };
            let id = publish_offer(&seeder, &buyer, &draft, Some(past)).await;
            historical_ids.insert(id.to_hex());
        }

        // Open the seller daemon (open-pool) and capture its pubkey.
        let home = seller_home(&unique_root("live"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        // Observer collects the seller's feedback-kind BEFORE the daemon starts.
        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        // Start the daemon (own thread + current-thread runtime) with the readiness hook.
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let _daemon = spawn_daemon_thread(daemon, ready_tx);

        // A. READY ≤ 10s (mpsc hook — not a stderr scrape).
        let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "A: daemon must reach READY within 10s (got {ready:?})"
        );

        // B. BOUNDED startup ingest: after a settle window a bounded seller has claimed ZERO
        // historical offers. On the unbounded/ungrouped filter it would fetch all M and claim ≥1.
        tokio::time::sleep(Duration::from_secs(3)).await;
        {
            let claimed = claims.lock().unwrap_or_else(|e| e.into_inner());
            let historical_hits: Vec<&(String, String)> = claimed
                .iter()
                .filter(|(e, _)| historical_ids.contains(e))
                .collect();
            assert!(
                historical_hits.is_empty(),
                "B: bounded seller must NOT claim historical offers; saw {} historical claim(s): {:?}",
                historical_hits.len(),
                historical_hits
            );
        }

        // C. LIVE untargeted delivery (THE goal proof): publish a FRESH untargeted offer to the
        // RUNNING seller and assert the relay receives its feedback-kind status=processing claim
        // (e=<offer id>) within 10s — an untargeted offer claimed WITHOUT a restart.
        let live_id = publish_offer(&seeder, &buyer, &offer_draft(None), None)
            .await
            .to_hex();
        let claimed_live = wait_until(Duration::from_secs(10), || {
            claims_contain(&claims, &live_id, "processing")
        })
        .await;
        assert!(
            claimed_live,
            "C: running open-pool seller must CLAIM a fresh untargeted offer (feedback-kind \
             processing e={live_id}) within 10s — the live untargeted-delivery proof"
        );

        // Quiesce: let execution finish so single-flight is released before the guard drops.
        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    // ── Boot backfill retrieves a stored kind-1059 without wedging the daemon ───────────
    // A stored gift-wrap p-tagged to the seller (the stranded-payment shape) is seeded BEFORE boot.
    // The reordered boot backfill (after online/readiness, auth-confirmed, hard-capped) must fetch
    // and ingest it, reach READY, and keep processing LIVE offers. Proves the recovery path runs
    // end-to-end over a real stored 1059 without hanging boot. (The auth-gate itself is exercised by
    // the live relay on deploy; here the local relay serves 1059 without NIP-42.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn boot_backfill_over_stored_wrap_stays_healthy() {
        use nostr_sdk::prelude::{EventBuilder, Tag};

        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        // Open the seller (to learn its pubkey), then seed a stored 1059 gift-wrap p-tagged to it.
        let home = seller_home(&unique_root("backfill-wrap"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        let seeder = connect_client(&relay_url).await;
        let past = Timestamp::from(Timestamp::now().as_secs().saturating_sub(120));
        let stored_wrap = EventBuilder::new(Kind::GiftWrap, "opaque-stored-wrap")
            .tag(Tag::public_key(seller_pk))
            .custom_created_at(past)
            .sign_with_keys(&Keys::generate())
            .expect("sign stored wrap");
        seeder
            .send_event(&stored_wrap)
            .await
            .expect("seed stored wrap");

        // Observer for the seller's claims (before boot, so none is missed).
        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let _daemon = spawn_daemon_thread(daemon, ready_tx);

        // The boot backfill retrieves + ingests the stored 1059 (opaque ⇒ not a decodable
        // own-payment wrap) WITHOUT wedging boot: readiness still fires.
        let ready = tokio::time::timeout(Duration::from_secs(12), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "daemon must reach READY despite a stored wrap in the boot backfill (got {ready:?})"
        );

        // And it still processes LIVE work after the backfill: a fresh targeted offer is claimed.
        let buyer = Keys::generate();
        let live_id = publish_offer(&seeder, &buyer, &offer_draft(Some(&seller_pk.to_hex())), None)
            .await
            .to_hex();
        let claimed = wait_until(Duration::from_secs(12), || {
            claims_contain(&claims, &live_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "seller must still CLAIM a live offer after boot-backfilling a stored wrap (e={live_id})"
        );

        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    // ── A wrap stored AFTER boot is picked up by a PERIODIC backfill run ────────
    // The field bug: a live-but-AGED relay subscription silently stops delivering kind-1059 wraps,
    // so a payment sent to a running daemon sits unredeemed until restart. The fix is a periodic
    // timer that re-runs the boot stored-wrap backfill. TESTABILITY (honest): the in-process relay
    // CANNOT reproduce an aged, silently-deaf live subscription — fresh in-process subs always
    // deliver. What is reproducible, and what actually fixes the bug, is the RECOVERY mechanism: a
    // periodic run independently RE-FETCHES stored wraps p-tagged to us (a fresh short-lived REQ)
    // and runs each through the SAME `ingest_gift_wrap` path. We seed a wrap AFTER boot and assert
    // a periodic run returns it (count >= 1) — proving the timer fires and the fetch+ingest runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn periodic_backfill_picks_up_wrap_stored_after_boot() {
        use nostr_sdk::prelude::{EventBuilder, Tag};

        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        // Fast periodic cadence via the test seam (held under the guard, so no concurrent daemon
        // test reads this env).
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "2");
        }

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;
        let home = seller_home(&unique_root("periodic-pickup"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let (backfill_tx, mut backfill_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
        let _daemon = spawn_daemon_thread_with_backfill_hook(daemon, ready_tx, backfill_tx);

        // Up and running; boot backfill already ran (no wrap stored yet).
        let ready = tokio::time::timeout(Duration::from_secs(12), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "daemon must reach READY (got {ready:?})"
        );

        // Seed a stored 1059 gift-wrap p-tagged to the seller AFTER boot (the stranded-payment
        // shape an aged live sub would miss). `last_receipt_ts == 0`, so the since-cursor covers it.
        let seeder = connect_client(&relay_url).await;
        let stored_wrap = EventBuilder::new(Kind::GiftWrap, "opaque-post-boot-wrap")
            .tag(Tag::public_key(seller_pk))
            .sign_with_keys(&Keys::generate())
            .expect("sign stored wrap");
        seeder
            .send_event(&stored_wrap)
            .await
            .expect("seed stored wrap");

        // A periodic run AFTER the wrap was stored must return >= 1 stored 1059 (fetched+ingested).
        let picked_up = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match backfill_rx.recv().await {
                    Some(count) if count >= 1 => break true,
                    Some(_) => continue,
                    None => break false,
                }
            }
        })
        .await;
        assert!(
            matches!(picked_up, Ok(true)),
            "a periodic backfill run must FETCH+INGEST the wrap stored after boot (got {picked_up:?})"
        );

        unsafe {
            std::env::remove_var(WRAP_BACKFILL_INTERVAL_ENV);
        }
        relay.shutdown();
    }

    // ── Repeated periodic runs stay idempotent and never wedge the loop ─────────
    // A stored wrap seeded BEFORE boot is re-seen by EVERY periodic run. Each run re-ingests it
    // through the SAME guarded, idempotent path (`ingest_gift_wrap` → `try_apply_or_buffer`:
    // journal pay-once / mint-refuse — money guards unchanged and covered by the money-path tests;
    // an opaque wrap decodes to "not ours" and makes no redeem attempt at all). We prove: (a) the
    // timer fires REPEATEDLY over the same wrap, and (b) the daemon still claims a fresh live offer
    // afterwards — i.e. the periodic runs never block the event loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn periodic_backfill_reruns_stay_idempotent_and_healthy() {
        use nostr_sdk::prelude::{EventBuilder, Tag};

        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "2");
        }

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;
        let home = seller_home(&unique_root("periodic-idem"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        // Seed the stored wrap BEFORE boot so every periodic run re-sees the SAME wrap.
        let seeder = connect_client(&relay_url).await;
        let stored_wrap = EventBuilder::new(Kind::GiftWrap, "opaque-repeat-wrap")
            .tag(Tag::public_key(seller_pk))
            .sign_with_keys(&Keys::generate())
            .expect("sign stored wrap");
        seeder
            .send_event(&stored_wrap)
            .await
            .expect("seed stored wrap");

        // Observer for the seller's claims (before boot, so none is missed).
        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let (backfill_tx, mut backfill_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
        let _daemon = spawn_daemon_thread_with_backfill_hook(daemon, ready_tx, backfill_tx);

        let ready = tokio::time::timeout(Duration::from_secs(12), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "daemon must reach READY (got {ready:?})"
        );

        // Wait for at least TWO periodic runs over the SAME stored wrap — the re-run path.
        let mut runs = 0usize;
        let two_runs = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match backfill_rx.recv().await {
                    Some(count) => {
                        assert!(count >= 1, "each periodic run must re-see the stored wrap");
                        runs += 1;
                        if runs >= 2 {
                            break true;
                        }
                    }
                    None => break false,
                }
            }
        })
        .await;
        assert!(
            matches!(two_runs, Ok(true)),
            "the periodic timer must fire repeatedly over the same wrap (got {two_runs:?}, runs={runs})"
        );

        // Still healthy after repeated backfills: a fresh targeted (live) offer is claimed without
        // a restart — the periodic runs never block the event loop.
        let buyer = Keys::generate();
        let live_id = publish_offer(&seeder, &buyer, &offer_draft(Some(&seller_pk.to_hex())), None)
            .await
            .to_hex();
        let claimed = wait_until(Duration::from_secs(12), || {
            claims_contain(&claims, &live_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "seller must still CLAIM a live offer after repeated periodic backfills (e={live_id})"
        );

        unsafe {
            std::env::remove_var(WRAP_BACKFILL_INTERVAL_ENV);
        }
        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    // ── D: relay restricts the broad open-pool filter → seller degrades to targeted ──────────

    /// A [`QueryPolicy`] that rejects a REQ carrying a broad offer filter (offer-kind, no authors,
    /// no `#p`) — i.e. the un-pinned open-pool filter — while allowing the targeted `#p==self`
    /// filter. Models a relay that refuses to serve the open-pool firehose.
    #[derive(Debug)]
    struct RejectBroadOfferQueries;

    impl QueryPolicy for RejectBroadOfferQueries {
        fn admit_query<'a>(
            &'a self,
            query: &'a Filter,
            _addr: &'a SocketAddr,
        ) -> BoxedFuture<'a, PolicyResult> {
            Box::pin(async move {
                let is_offer_kind = query
                    .kinds
                    .as_ref()
                    .is_some_and(|ks| ks.contains(&Kind::Custom(gateway::JOB_OFFER_KIND)));
                let has_authors = query.authors.as_ref().is_some_and(|a| !a.is_empty());
                let has_p_tag = query
                    .generic_tags
                    .contains_key(&SingleLetterTag::lowercase(Alphabet::P));
                if is_offer_kind && !has_authors && !has_p_tag {
                    PolicyResult::Reject("blocked: broad open-pool offer filter".to_string())
                } else {
                    PolicyResult::Accept
                }
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn relay_restricts_broad_filter_seller_degrades_to_targeted() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) =
            start_relay(RelayBuilder::default().query_policy(RejectBroadOfferQueries)).await;

        let home = seller_home(&unique_root("fallback"), &relay_url, true, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");
        let seller_hex = daemon.seller_pubkey().to_string();

        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let _daemon = spawn_daemon_thread(daemon, ready_tx);

        // D-1: the daemon still reaches READY even though the relay CLOSES the broad grouped offer
        // subscription (ready fires after subscribe; the CLOSED is handled in the loop after).
        let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "D-1: daemon must reach READY despite the broad-filter CLOSE (got {ready:?})"
        );

        // Give the CLOSED → targeted-only fallback subscribe time to land.
        tokio::time::sleep(Duration::from_secs(1)).await;

        // D-2: publish a FRESH TARGETED offer; assert it is CLAIMED via the targeted-only
        // fallback — proving the seller degraded to targeted-alive instead of going silently deaf.
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let targeted_id = publish_offer(&seeder, &buyer, &offer_draft(Some(&seller_hex)), None)
            .await
            .to_hex();
        let claimed = wait_until(Duration::from_secs(10), || {
            claims_contain(&claims, &targeted_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "D-2: after the broad-filter CLOSE, the seller must still CLAIM a fresh TARGETED offer \
             (claim-kind processing e={targeted_id}) within 10s via the targeted-only fallback"
        );

        // Quiesce: let execution finish so single-flight is released before the guard drops.
        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    // ── Backfill window: a daemon started AFTER an offer was posted ──────────────────────────

    /// Publish a claim-kind `status=processing` claim for `offer_id` signed by `seller` — models
    /// a DIFFERENT seller having already claimed the offer. Reuses the daemon's own claim draft.
    async fn publish_claim(
        client: &Client,
        seller: &Keys,
        offer_id: &str,
        buyer_pubkey: &str,
    ) -> nostr_sdk::EventId {
        let seller_hex = seller.public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            offer_id,
            1,
            "sat",
            &[crate::home::DEFAULT_MINT_URL.to_string()],
            &seller_hex,
        )
        .expect("build claim creq");
        let draft = gateway::claim_draft(offer_id, buyer_pubkey, &seller_hex, &creq);
        let event = gateway::nostr::event_builder(&draft)
            .expect("claim event builder")
            .sign_with_keys(seller)
            .expect("sign claim");
        let id = event.id;
        client.send_event(&event).await.expect("publish claim");
        id
    }

    /// Boot a seller daemon (own thread + readiness hook) with a feedback-kind claim collector.
    /// Returns the live observer client (KEEP it bound — dropping it ends the collector task),
    /// the collected-claims handle, and the daemon join handle. Asserts READY within 10s.
    async fn start_collected_seller(
        label: &str,
        relay_url: &str,
        claim_open_pool: bool,
        offer_backfill_secs: u64,
    ) -> (
        Client,
        Arc<Mutex<Vec<(String, String)>>>,
        std::thread::JoinHandle<()>,
    ) {
        let home = seller_home(&unique_root(label), relay_url, claim_open_pool, offer_backfill_secs);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");

        let observer = connect_client(relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = spawn_daemon_thread(daemon, ready_tx);
        let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "daemon must reach READY within 10s (got {ready:?})"
        );
        (observer, claims, handle)
    }

    /// THE acceptance fixture: post an OPEN-POOL offer, THEN start the daemon, and the daemon
    /// BACKFILLS + CLAIMS it (feedback-kind `processing` e=<offer id>) within the poll interval.
    ///
    /// RED-ON-REVERT (the since-window mechanic): restore `since(now)` / `limit(0)` in
    /// `offer_subscription_filters` (ignore `backfill_secs`) and this fixture goes RED — the
    /// pre-posted offer is never delivered, so never claimed (assert fails, rc=101).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn backfilled_in_window_offer_is_claimed_after_daemon_start() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        // Publish an OPEN-POOL offer BEFORE the daemon exists, dated 30s in the PAST so it is
        // unambiguously earlier than the daemon's subscribe time (deterministic: on the
        // reverted `since(now)` filter it is excluded; within the 20-min window it backfills).
        // Future deadline so it is not expiry-refused. Open-pool is the real field gap.
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let recent = Timestamp::from(Timestamp::now().as_secs().saturating_sub(30));
        let offer_id = publish_offer(&seeder, &buyer, &offer_draft(None), Some(recent))
            .await
            .to_hex();

        // Start the daemon AFTER the offer is on the relay, with a 20-min backfill window.
        let (_observer, claims, _daemon) =
            start_collected_seller("backfill-in", &relay_url, true, 1200).await;

        let claimed = wait_until(Duration::from_secs(10), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "a daemon started AFTER the offer was posted must BACKFILL + CLAIM it (feedback-kind \
             processing e={offer_id}) within 10s — the start-after-post delivery proof"
        );

        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    /// An offer OLDER than the backfill window is NOT delivered (the relay's `since(now-window)`
    /// bound excludes it) and therefore never claimed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn out_of_window_offer_is_not_claimed() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        // Offer dated an hour ago (future deadline, so ONLY the window — not expiry — excludes it).
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let past = Timestamp::from(Timestamp::now().as_secs().saturating_sub(3_600));
        let offer_id = publish_offer(&seeder, &buyer, &offer_draft(None), Some(past))
            .await
            .to_hex();

        // 60s window ≪ 1h age ⇒ out of window.
        let (_observer, claims, _daemon) =
            start_collected_seller("backfill-out", &relay_url, true, 60).await;

        let claimed = wait_until(Duration::from_secs(6), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            !claimed,
            "an offer older than the backfill window MUST NOT be delivered or claimed (e={offer_id})"
        );

        relay.shutdown();
    }

    /// Money-safety guard #a end-to-end: a backfilled offer WITHIN the window but PAST its own
    /// deadline is delivered, then REFUSED by the offer-freshness gate — never claimed, never
    /// resurrected with a fresh deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn backfilled_expired_offer_is_refused_never_claimed() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        let now = Timestamp::now().as_secs();
        // created_at 30s ago (well inside a 300s window) but deadline already lapsed 5s ago.
        let recent = Timestamp::from(now.saturating_sub(30));
        let draft = offer_draft_with_deadline(None, now.saturating_sub(5));
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let offer_id = publish_offer(&seeder, &buyer, &draft, Some(recent))
            .await
            .to_hex();

        let (_observer, claims, _daemon) =
            start_collected_seller("backfill-exp", &relay_url, true, 300).await;

        let claimed = wait_until(Duration::from_secs(6), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            !claimed,
            "a backfilled offer past its deadline MUST be refused (never claimed/resurrected) e={offer_id}"
        );

        relay.shutdown();
    }

    /// Money-safety guard #d end-to-end: a backfilled offer already LIVE-CLAIMED by ANOTHER
    /// seller is skipped — the daemon does not stomp an in-flight trade.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn backfilled_offer_live_claimed_by_another_seller_is_skipped() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;

        let now = Timestamp::now().as_secs();
        let recent = Timestamp::from(now.saturating_sub(30));
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        // Offer within the window, future deadline (so the foreign claim reads LIVE, not expired).
        let offer_id = publish_offer(&seeder, &buyer, &offer_draft(None), Some(recent))
            .await
            .to_hex();
        // A DIFFERENT seller has already claimed it (live claim-kind processing).
        let foreign_seller = Keys::generate();
        publish_claim(&seeder, &foreign_seller, &offer_id, &buyer.public_key().to_hex()).await;

        let (_observer, claims, _daemon) =
            start_collected_seller("backfill-claimed", &relay_url, true, 300).await;

        // OUR seller (the collector filters to our pubkey) must publish NO processing claim.
        let stomped = wait_until(Duration::from_secs(6), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            !stomped,
            "must NOT claim an offer already live-claimed by another seller (no stomping) e={offer_id}"
        );

        relay.shutdown();
    }

    /// Publish a TARGETED contribution offer (job-class + target-repo pin + base + accepts)
    /// signed by `buyer`, returning its event id.
    async fn publish_contribution_offer(
        client: &Client,
        buyer: &Keys,
        seller_hex: &str,
    ) -> nostr_sdk::EventId {
        let offer = OfferDraft::new(
            "improve the repo",
            "text/plain",
            10,
            now_unix() + 3_600,
            seller_hex,
        );
        let mut draft = offer.to_event_draft();
        let contribution = crate::contribution::ContributionOffer {
            target: crate::contribution::TargetRepoPin::new(
                "aa".repeat(32),
                "https://mobee-relay.orveth.dev/git/owner/repo.git",
            )
            .unwrap(),
            base: crate::contribution::ContributionBase::new("main", "77".repeat(20)).unwrap(),
            accepts: vec!["fork".into()],
        };
        draft
            .tags
            .extend(crate::contribution::contribution_offer_tags(&contribution));
        let event = gateway::nostr::event_builder(&draft)
            .expect("contribution offer builder")
            .sign_with_keys(buyer)
            .expect("sign contribution offer");
        let id = event.id;
        client.send_event(&event).await.expect("publish contribution offer");
        id
    }

    // ── A CONTRIBUTION offer round-trips over a real relay and the seller recognises it,
    //    claiming it (claim-kind processing) — the offer→claim leg of the ONE state machine live. ──
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn contribution_offer_round_trips_to_claim_over_local_relay() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;
        let home = seller_home(&unique_root("contrib"), &relay_url, false, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");
        let seller_hex = daemon.seller_pubkey().to_string();

        let observer = connect_client(&relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kinds([
                        Kind::Custom(gateway::JOB_CLAIM_KIND),
                        Kind::Custom(gateway::JOB_FEEDBACK_KIND),
                    ])
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claims = spawn_claim_collector(&observer, seller_pk);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let _daemon = spawn_daemon_thread(daemon, ready_tx);
        let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.recv()).await;
        assert!(matches!(ready, Ok(Some(()))), "daemon must reach READY (got {ready:?})");

        // Post a TARGETED contribution offer; the seller must recognise the class + pins and CLAIM
        // it (claim-kind processing) — proving the additive offer round-trips over a live relay.
        let buyer = Keys::generate();
        let seeder = connect_client(&relay_url).await;
        let offer_id = publish_contribution_offer(&seeder, &buyer, &seller_hex).await.to_hex();
        let claimed = wait_until(Duration::from_secs(10), || {
            claims_contain(&claims, &offer_id, "processing")
        })
        .await;
        assert!(
            claimed,
            "seller must CLAIM a fresh contribution offer (claim-kind processing e={offer_id}) within 10s"
        );

        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    // ── Award-before-work: the seller executes ONLY on the buyer's award ──────────────────────
    //
    // The claim is cheap (no compute); execution waits for a kind-award AWARD naming this claim.
    // These prove the three award outcomes end-to-end over the in-process relay via the RunHooks
    // seams (`awarded` / `released`), never stderr scraping:
    //  * (a) award for OUR claim → the seller executes;
    //  * (b) award for a DIFFERENT claim → the seller releases, never executing;
    //  * (c) no award → the offer deadline sweep releases, never executing.

    /// Collect the seller's published CLAIM ids keyed by the offer they e-tag: `(offer_id, claim_id)`.
    /// A claim's own event id IS the claim id the buyer awards.
    fn spawn_claim_id_collector(
        client: &Client,
        seller_pk: PublicKey,
    ) -> Arc<Mutex<Vec<(String, String)>>> {
        let ids: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = ids.clone();
        let mut notif = client.notifications();
        tokio::spawn(async move {
            while let Ok(n) = notif.recv().await {
                if let RelayPoolNotification::Event { event, .. } = n {
                    if event.kind.as_u16() == gateway::JOB_CLAIM_KIND && event.pubkey == seller_pk {
                        if let Some(offer) = tag_value(&event, "e") {
                            sink.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push((offer, event.id.to_hex()));
                        }
                    }
                }
            }
        });
        ids
    }

    /// Poll for the seller's claim id on `offer_id`.
    async fn wait_for_claim_id(
        ids: &Arc<Mutex<Vec<(String, String)>>>,
        offer_id: &str,
        timeout: Duration,
    ) -> Option<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some((_, claim_id)) = ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|(o, _)| o == offer_id)
            {
                return Some(claim_id.clone());
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Publish the buyer's kind-award AWARD selecting `claim_id` for `offer_id`.
    async fn publish_award(
        client: &Client,
        buyer: &Keys,
        offer_id: &str,
        claim_id: &str,
        seller_hex: &str,
    ) -> nostr_sdk::EventId {
        let draft =
            gateway::award_draft(offer_id, claim_id, &buyer.public_key().to_hex(), seller_hex);
        let event = gateway::nostr::event_builder(&draft)
            .expect("award event builder")
            .sign_with_keys(buyer)
            .expect("sign award");
        let id = event.id;
        client.send_event(&event).await.expect("publish award");
        id
    }

    /// Boot a daemon wired with the award-decision hooks (`awarded` / `released`) + readiness.
    fn spawn_daemon_thread_with_award_hooks(
        daemon: SellerDaemon,
        ready: tokio::sync::mpsc::UnboundedSender<()>,
        awarded: tokio::sync::mpsc::UnboundedSender<String>,
        released: tokio::sync::mpsc::UnboundedSender<(String, String)>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("daemon runtime");
            let _ = rt.block_on(run_forever_hooked(
                daemon,
                RunHooks {
                    ready: Some(ready),
                    auth_wait: Some(Duration::from_millis(500)),
                    wrap_backfill_done: None,
                    awarded: Some(awarded),
                    released: Some(released),
                },
            ));
        })
    }

    /// Boot a targeted-offer seller with award hooks; return `(seeder, seller_hex, claim_ids,
    /// awarded_rx, released_rx, handle)` after READY. The observer client feeds the claim-id
    /// collector; the seeder is kept for publishing the offer + award.
    async fn start_award_seller(
        label: &str,
        relay_url: &str,
    ) -> (
        Client,
        String,
        Arc<Mutex<Vec<(String, String)>>>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        tokio::sync::mpsc::UnboundedReceiver<(String, String)>,
        std::thread::JoinHandle<()>,
    ) {
        let home = seller_home(&unique_root(label), relay_url, false, 0);
        let daemon = SellerDaemon::open(home).expect("open seller daemon");
        let seller_pk = PublicKey::parse(daemon.seller_pubkey()).expect("seller pubkey");
        let seller_hex = daemon.seller_pubkey().to_string();

        let observer = connect_client(relay_url).await;
        observer
            .subscribe(
                Filter::new()
                    .kind(Kind::Custom(gateway::JOB_CLAIM_KIND))
                    .author(seller_pk),
                None,
            )
            .await
            .expect("observer subscribe");
        let claim_ids = spawn_claim_id_collector(&observer, seller_pk);
        // The collector task holds its own notification handle; keep the client alive for the test.
        std::mem::forget(observer);

        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let (awarded_tx, awarded_rx) = tokio::sync::mpsc::unbounded_channel();
        let (released_tx, released_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle =
            spawn_daemon_thread_with_award_hooks(daemon, ready_tx, awarded_tx, released_tx);
        let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.recv()).await;
        assert!(
            matches!(ready, Ok(Some(()))),
            "daemon must reach READY within 10s (got {ready:?})"
        );

        let seeder = connect_client(relay_url).await;
        (seeder, seller_hex, claim_ids, awarded_rx, released_rx, handle)
    }

    /// (a) A claim awarded to THIS seller executes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn awarded_claim_executes() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;
        let (seeder, seller_hex, claim_ids, mut awarded_rx, _released_rx, _daemon) =
            start_award_seller("award-a", &relay_url).await;

        let buyer = Keys::generate();
        let offer_id = publish_offer(&seeder, &buyer, &offer_draft(Some(&seller_hex)), None)
            .await
            .to_hex();
        let claim_id = wait_for_claim_id(&claim_ids, &offer_id, Duration::from_secs(10))
            .await
            .expect("a: seller must publish its claim within 10s");

        // The buyer awards THIS claim before any work — the seller must now execute it.
        publish_award(&seeder, &buyer, &offer_id, &claim_id, &seller_hex).await;
        let awarded = tokio::time::timeout(Duration::from_secs(10), awarded_rx.recv()).await;
        assert_eq!(
            awarded.ok().flatten().as_deref(),
            Some(offer_id.as_str()),
            "a: awarding THIS seller's claim must move the job into execution"
        );

        wait_until(Duration::from_secs(8), || !SellerDaemon::in_flight()).await;
        relay.shutdown();
    }

    /// (b) A claim awarded to a DIFFERENT seller is released, never executed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn losing_claim_is_released_never_executed() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;
        let (seeder, seller_hex, claim_ids, mut awarded_rx, mut released_rx, _daemon) =
            start_award_seller("award-b", &relay_url).await;

        let buyer = Keys::generate();
        let offer_id = publish_offer(&seeder, &buyer, &offer_draft(Some(&seller_hex)), None)
            .await
            .to_hex();
        wait_for_claim_id(&claim_ids, &offer_id, Duration::from_secs(10))
            .await
            .expect("b: seller must publish its claim within 10s");

        // The buyer awards a DIFFERENT claim (another seller) — ours must release, never execute.
        let other_claim = "00".repeat(32);
        publish_award(&seeder, &buyer, &offer_id, &other_claim, &seller_hex).await;
        let released = tokio::time::timeout(Duration::from_secs(10), released_rx.recv()).await;
        let released = released.ok().flatten().expect("b: our claim must be released");
        assert_eq!(released.0, offer_id, "b: released the awaited job");
        assert!(
            released.1.contains("different claim"),
            "b: release reason names the different-award cause: {}",
            released.1
        );
        assert!(
            !SellerDaemon::in_flight(),
            "b: a released claim must NOT take the single-flight slot (no execution)"
        );
        assert!(
            awarded_rx.try_recv().is_err(),
            "b: a losing seller must never be signalled to execute"
        );

        relay.shutdown();
    }

    /// (c) No award → the offer-deadline sweep releases the claim, never executing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unawarded_claim_releases_on_deadline_never_executes() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        SellerDaemon::end_flight();
        // Fast sweep via the test seam (held under the guard, so no concurrent daemon reads it).
        unsafe {
            std::env::set_var(AWARD_SWEEP_INTERVAL_ENV, "1");
        }

        let (relay, relay_url) = start_relay(RelayBuilder::default()).await;
        let (seeder, seller_hex, claim_ids, mut awarded_rx, mut released_rx, _daemon) =
            start_award_seller("award-c", &relay_url).await;

        // Short offer deadline: claimable now, but lapses in a few seconds with no award.
        let buyer = Keys::generate();
        let deadline = now_unix() + 4;
        let offer_id = publish_offer(
            &seeder,
            &buyer,
            &offer_draft_with_deadline(Some(&seller_hex), deadline),
            None,
        )
        .await
        .to_hex();
        wait_for_claim_id(&claim_ids, &offer_id, Duration::from_secs(10))
            .await
            .expect("c: seller must publish its claim within 10s");

        // No award is ever published — the deadline sweep must release the parked claim.
        let released = tokio::time::timeout(Duration::from_secs(15), released_rx.recv()).await;
        let released = released.ok().flatten().expect("c: claim must release on deadline");
        assert_eq!(released.0, offer_id, "c: released the unawarded job");
        assert!(
            released.1.contains("deadline"),
            "c: release reason names the deadline cause: {}",
            released.1
        );
        assert!(
            awarded_rx.try_recv().is_err(),
            "c: an unawarded claim must never be signalled to execute"
        );
        assert!(
            !SellerDaemon::in_flight(),
            "c: an unawarded claim must NOT take the single-flight slot"
        );

        unsafe {
            std::env::remove_var(AWARD_SWEEP_INTERVAL_ENV);
        }
        relay.shutdown();
    }
