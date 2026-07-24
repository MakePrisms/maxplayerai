    use super::*;
    use crate::home::SellerConfig;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-daemon-{label}-{}-{id}",
            std::process::id()
        ))
    }

    /// Finding Y: the backfill `since` cursor decision. A journal READ ERROR (corrupt journal) must
    /// ABORT the cycle (propagate `Err`) rather than defaulting to since=0 and running a full-history
    /// backfill. A missing/empty journal (folded to `Ok(None)` by the journal reads) yields 0
    /// (legitimate first boot); the oldest-unsettled anchor clamps the cursor.
    #[test]
    fn backfill_since_aborts_on_journal_error() {
        // Corrupt last-receipt read ⇒ abort (Err), NOT since=0.
        let err = resolve_backfill_since(
            Err(SellerError::Journal("corrupt journal line".into())),
            Ok(None),
        )
        .expect_err("journal error must abort the backfill cycle");
        assert!(matches!(err, SellerError::Journal(_)));

        // Corrupt oldest-unsettled read ⇒ abort as well.
        assert!(resolve_backfill_since(
            Ok(Some(1_000)),
            Err(SellerError::Journal("corrupt journal line".into())),
        )
        .is_err());

        // Missing/empty journal (both Ok(None)) ⇒ since=0, the legitimate first-boot path.
        assert_eq!(resolve_backfill_since(Ok(None), Ok(None)).unwrap(), 0);

        // No unsettled delivery ⇒ cursor is the last-receipt ts.
        assert_eq!(
            resolve_backfill_since(Ok(Some(5_000)), Ok(None)).unwrap(),
            5_000
        );

        // Oldest unsettled delivery clamps the cursor to (oldest - margin) when that is earlier.
        assert_eq!(
            resolve_backfill_since(Ok(Some(50_000)), Ok(Some(10_000))).unwrap(),
            10_000u64.saturating_sub(WRAP_BACKFILL_MARGIN_SECS)
        );
    }

    /// Periodic backfill CADENCE decision function. The interval is a fixed
    /// constant in production (300s); the env seam (used only by tests) overrides it, and a
    /// `0`/unparseable value is ignored. This is the extracted `resolve_wrap_backfill_interval_secs`
    /// that lets the cadence be exercised without a live relay. Serialised against the daemon
    /// integration tests (which also read this env) via `FLIGHT_TEST_GUARD`.
    #[test]
    fn wrap_backfill_interval_env_overrides_default() {
        let _serial = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // No env seam ⇒ the fixed production constant (5 min).
        unsafe {
            std::env::remove_var(WRAP_BACKFILL_INTERVAL_ENV);
        }
        assert_eq!(WRAP_BACKFILL_INTERVAL_SECS, 300);
        assert_eq!(
            resolve_wrap_backfill_interval_secs(),
            WRAP_BACKFILL_INTERVAL_SECS
        );
        // A valid positive override wins (the fast-cadence test seam).
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "2");
        }
        assert_eq!(resolve_wrap_backfill_interval_secs(), 2);
        // `0` and unparseable values are ignored ⇒ fall back to the constant.
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "0");
        }
        assert_eq!(
            resolve_wrap_backfill_interval_secs(),
            WRAP_BACKFILL_INTERVAL_SECS
        );
        unsafe {
            std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, "nonsense");
        }
        assert_eq!(
            resolve_wrap_backfill_interval_secs(),
            WRAP_BACKFILL_INTERVAL_SECS
        );
        unsafe {
            std::env::remove_var(WRAP_BACKFILL_INTERVAL_ENV);
        }
    }

    // The seller-side receipt-preimage delivery discriminator is DERIVED from the typed
    // `GitDelivery` ("fork"), not a hardcoded label — buyer and seller agree by construction.
    #[test]
    fn seller_delivery_kind_derives_fork_from_typed_delivery() {
        let kind = seller_delivery_kind(
            "https://relay.example/git/job.git",
            "mobee/abcd1234",
            &"a".repeat(40),
        )
        .expect("commit delivery types");
        assert_eq!(kind, crate::receipt::DeliveryKind::Fork);
        assert_eq!(kind.as_str(), "fork");
    }

    #[test]
    fn open_refuses_non_testnut_mint() {
        let root = temp("mint");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home::save_config(&mut home, |config| {
            config.accepted_mints = vec!["https://real-mint.example".into()];
            config.seller = Some(SellerConfig {
                agent_command: vec!["echo".into()],
                rate_sats: 1,
                git_remote: "https://example.invalid/repo.git".into(),
                job_timeout_secs: None,
                agent: None,
                claim_open_pool: false,
                offer_backfill_secs: 0,
                contribution_enabled: true,
            });
        })
        .expect("save");
        let err = match SellerDaemon::open(home) {
            Ok(_) => panic!("non-testnut must fail-closed"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("fail-closed") || err.to_string().contains("testnut"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Real-mint switch: with `allow_real_mints=true` the boot fence admits a real
    // (non-testnut) accepted_mints entry that is refused by default.
    #[test]
    fn open_admits_real_mint_when_allow_real_mints_true() {
        let root = temp("mint-real");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home::save_config(&mut home, |config| {
            config.accepted_mints = vec!["https://minibits.example".into()];
            config.allow_real_mints = true;
            config.seller = Some(SellerConfig {
                agent_command: vec!["echo".into()],
                rate_sats: 1,
                git_remote: "https://example.invalid/repo.git".into(),
                job_timeout_secs: None,
                agent: None,
                claim_open_pool: false,
                offer_backfill_secs: 0,
                contribution_enabled: true,
            });
        })
        .expect("save");
        SellerDaemon::open(home).expect("real mint admitted with allow_real_mints=true");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_requires_seller_section() {
        let root = temp("noseller");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let err = match SellerDaemon::open(home) {
            Ok(_) => panic!("missing seller must refuse"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("seller") || err.to_string().contains("agent_command"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn boot_preflight_enabled_defaults_on_and_env_disables() {
        let mut config = crate::home::MobeeConfig::default();
        assert!(boot_preflight_enabled(&config, None), "default must be on");
        for off in ["0", "false", "no", "OFF", " false "] {
            assert!(
                !boot_preflight_enabled(&config, Some(off)),
                "env {off:?} must disable"
            );
        }
        assert!(
            boot_preflight_enabled(&config, Some("1")),
            "a non-disabling env value keeps it on"
        );
        config.seller_preflight.boot_push_preflight = false;
        assert!(
            !boot_preflight_enabled(&config, None),
            "config-off wins with no env"
        );
        assert!(
            !boot_preflight_enabled(&config, Some("1")),
            "env cannot force a config-off preflight on"
        );
    }

    #[test]
    fn boot_preflight_failure_logs_and_continues() {
        // Mock the probe seam: a FAILING probe must still yield a (Some) advisory line naming the
        // git-version cause + fix — never an error, never a panic. The daemon logs it and keeps
        // running (this fn returning Some, not the probe's Err, IS the logs-and-continues contract).
        let line = run_boot_push_preflight(true, || {
            Err(crate::seller_git::SellerGitError::AuthFailed("mock 401".into()))
        })
        .expect("enabled preflight must yield a log line");
        assert!(line.contains("FAILED"), "{line}");
        assert!(line.contains("2.54"), "{line}");
        assert!(line.to_lowercase().contains("continuing"), "{line}");
    }

    #[test]
    fn boot_preflight_success_reports_ok() {
        let line =
            run_boot_push_preflight(true, || Ok(())).expect("enabled preflight yields a line");
        assert!(line.contains("OK"), "{line}");
    }

    #[test]
    fn boot_preflight_disabled_skips_probe_seam() {
        let mut probe_ran = false;
        let out = run_boot_push_preflight(false, || {
            probe_ran = true;
            Ok(())
        });
        assert!(out.is_none(), "disabled preflight must produce no line");
        assert!(!probe_ran, "disabled preflight must not invoke the probe seam");
    }

    #[test]
    fn single_flight_mutex() {
        // Serialise against the daemon integration tests: both touch the global FLIGHT lock.
        let _guard = FLIGHT_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        assert!(SellerDaemon::try_begin_flight());
        assert!(!SellerDaemon::try_begin_flight());
        SellerDaemon::end_flight();
        assert!(SellerDaemon::try_begin_flight());
        SellerDaemon::end_flight();
    }

    // The ACP timeout is unified with `--job-timeout-secs` — one deadline.
    #[test]
    fn unified_job_timeout_is_the_remaining_deadline_not_a_hardcoded_constant() {
        // The effective timeout is strictly the remaining window to the job's deadline.
        assert_eq!(unified_job_timeout(1_000, 940), Duration::from_secs(60));
        assert_eq!(unified_job_timeout(1_000, 100), Duration::from_secs(900));
        // Two different deadlines ⇒ two different timeouts — proves it is DERIVED from the
        // deadline, not a fixed 300s that could override or conflict with `--job-timeout-secs`.
        assert_ne!(
            unified_job_timeout(1_000, 940),
            unified_job_timeout(1_000, 100)
        );
        assert_ne!(unified_job_timeout(1_000, 940), Duration::from_secs(300));
        // At/past the deadline ⇒ ZERO (fail cleanly at the deadline, never hang, never wrap).
        assert_eq!(unified_job_timeout(1_000, 1_000), Duration::ZERO);
        assert_eq!(unified_job_timeout(1_000, 5_000), Duration::ZERO);
    }

    // A transient agent error is retried WITHIN the deadline; feedback-kind is
    // published only after the attempt budget or the deadline is spent.
    #[tokio::test]
    async fn retry_recovers_from_a_transient_error_within_the_deadline() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // Deadline far away ⇒ never the limiter; a transient first error must be retried,
        // NOT burn the claim (publish feedback) while the deadline still has room.
        let out = run_agent_with_retry(u64::MAX, 3, || 0, |attempt| {
            attempts.set(attempt);
            async move {
                if attempt < 2 {
                    Err(DaemonError::Agent("transient".into()))
                } else {
                    Ok::<Option<UsageMetadata>, DaemonError>(None)
                }
            }
        })
        .await;
        assert!(out.is_ok(), "transient error retried within deadline, not fatal: {out:?}");
        assert_eq!(attempts.get(), 2, "retried once, then succeeded");
    }

    #[tokio::test]
    async fn retry_exhausts_bounded_attempts_then_surfaces_the_error() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // Deadline never the limiter (u64::MAX) — only the attempt budget stops the loop.
        let out = run_agent_with_retry(u64::MAX, 3, || 0, |attempt| {
            attempts.set(attempt);
            async move {
                Err::<Option<UsageMetadata>, DaemonError>(DaemonError::Agent("always".into()))
            }
        })
        .await;
        assert!(out.is_err(), "exhausted retries ⇒ error so caller publishes feedback-kind");
        assert_eq!(attempts.get(), 3, "bounded to the attempt budget");
    }

    #[tokio::test]
    async fn retry_past_deadline_makes_one_attempt_then_surfaces_the_error() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // `now` (5_000) is already past the deadline (1_000) ⇒ no retry budget at all: one
        // attempt, then the error surfaces so the caller publishes feedback-kind.
        let out = run_agent_with_retry(1_000, 3, || 5_000, |attempt| {
            attempts.set(attempt);
            async move {
                Err::<Option<UsageMetadata>, DaemonError>(DaemonError::Agent("late".into()))
            }
        })
        .await;
        assert!(out.is_err(), "past deadline ⇒ error (caller publishes feedback-kind)");
        assert_eq!(attempts.get(), 1, "no retry once the deadline has passed");
    }

    // The daemon appends explicit, secret-free delivery instructions.
    #[test]
    fn composed_prompt_carries_task_and_daemon_owned_delivery_instructions() {
        let remote = "https://relay.example/git/abc.git";
        let prompt = compose_agent_prompt("build a widget", remote, None);
        // The original task stays up front.
        assert!(prompt.starts_with("build a widget"), "task preserved: {prompt}");
        // Explicit, daemon-owned delivery instructions are appended.
        assert!(prompt.contains("DELIVERY"), "has a delivery section: {prompt}");
        assert!(
            prompt.contains("commit") || prompt.contains("Commit"),
            "tells the agent to commit: {prompt}"
        );
        assert!(prompt.contains("git"), "delivery is via git: {prompt}");
        assert!(
            prompt.contains(remote),
            "names the bound remote so delivery is not guessed: {prompt}"
        );
        // Public prompt text — never embeds a secret.
        let lower = prompt.to_lowercase();
        assert!(!prompt.contains("nsec"), "no nostr secret key");
        assert!(!lower.contains("private key"), "no private key");
        assert!(!lower.contains("secret"), "no secret material");
    }

    #[test]
    fn seller_exec_metadata_is_harness_generic_public_and_absent_stays_absent() {
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        // claude ⇒ side-channel; codex ⇒ acp-native; unknown ⇒ basename + side-channel.
        // `None` usage: the pre-capture block — token/model/cost stay absent.
        let claude = seller_exec_metadata(&["claude".into(), "--print".into()], None, 1234, None);
        assert_eq!(value(&claude, "harness").as_deref(), Some("claude-agent-acp"));
        assert_eq!(value(&claude, "usage_transport").as_deref(), Some("side-channel"));
        // Anchor rule: metadata_trust present whenever any field is present.
        assert_eq!(value(&claude, "metadata_trust").as_deref(), Some("seller-claimed"));
        assert_eq!(value(&claude, "wall_time").as_deref(), Some("1234"));
        // Absent-stays-absent: no zero-filled token/model/cost fields (not sourced this run).
        assert!(value(&claude, "tokens").is_none());
        assert!(value(&claude, "model").is_none());
        assert!(value(&claude, "cost").is_none());

        let codex = seller_exec_metadata(&["/nix/store/x/bin/codex-acp".into()], None, 5, None);
        assert_eq!(value(&codex, "harness").as_deref(), Some("codex-acp-ng"));
        assert_eq!(value(&codex, "usage_transport").as_deref(), Some("acp-native"));

        let unknown = seller_exec_metadata(&["/opt/tools/mytool".into()], None, 5, None);
        assert_eq!(value(&unknown, "harness").as_deref(), Some("mytool"));
        assert_eq!(value(&unknown, "usage_transport").as_deref(), Some("side-channel"));
    }

    #[test]
    fn claude_preset_resolves_harness_family_claude_despite_npx_argv0() {
        // Mirror the downstream harness-family classifier:
        // a family substring wins; present-but-unrecognized (e.g. "npx") → "other".
        fn harness_family(id: &str) -> &'static str {
            let s = id.to_ascii_lowercase();
            if s.contains("claude") {
                "claude"
            } else if s.contains("cursor") {
                "cursor"
            } else if s.contains("codex") {
                "codex"
            } else {
                "other"
            }
        }
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        // The `claude` preset launches the ACP adapter via `npx` (argv0 = "npx"). An argv0-naive
        // id emits "npx" → harness_family "other" (the dashboard bug). The preset
        // label must drive resolution to "claude-agent-acp" → family "claude".
        let npx_claude = vec![
            "/usr/bin/npx".to_string(),
            "-y".to_string(),
            "@agentclientprotocol/claude-agent-acp".to_string(),
        ];
        let tags = seller_exec_metadata(&npx_claude, Some("claude"), 100, None);
        let harness = value(&tags, "harness").expect("harness tag");
        assert_eq!(harness, "claude-agent-acp");
        assert_eq!(
            harness_family(&harness),
            "claude",
            "claude preset must map to harness_family 'claude', not 'other'"
        );

        // Preset label is authoritative even when the argv carries no family hint at all.
        let opaque = vec![
            "/usr/bin/npx".to_string(),
            "-y".to_string(),
            "@acp/opaque-adapter".to_string(),
        ];
        let opaque_tags = seller_exec_metadata(&opaque, Some("claude"), 100, None);
        assert_eq!(
            harness_family(&value(&opaque_tags, "harness").expect("harness")),
            "claude"
        );

        // Regression guard: bare argv0 = "npx" with NO preset label used to yield "other";
        // the full-argv fallback now recovers "claude" from the adapter package name.
        let hatch = seller_exec_metadata(&npx_claude, None, 100, None);
        assert_eq!(
            harness_family(&value(&hatch, "harness").expect("harness")),
            "claude"
        );
    }

    #[test]
    fn custom_preset_label_is_the_reported_harness_identity() {
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        // A config-defined `[agents]` preset (non-built-in label): the preset name IS the
        // harness id — never argv0, never a family guess from the launch command.
        let argv = vec!["/opt/adapters/grok-acp".to_string(), "stdio".to_string()];
        let tags = seller_exec_metadata(&argv, Some("grok"), 42, None);
        assert_eq!(value(&tags, "harness").as_deref(), Some("grok"));
        assert_eq!(value(&tags, "usage_transport").as_deref(), Some("side-channel"));

        // Built-in labels keep their adapter identities (custom seam must not regress them).
        let builtin = seller_exec_metadata(&argv, Some("codex"), 42, None);
        assert_eq!(value(&builtin, "harness").as_deref(), Some("codex-acp-ng"));
    }

    #[test]
    fn open_pool_filter_lands_fresh_untargeted_offer_only_when_enabled_and_bounds_history() {
        use nostr_sdk::prelude::{
            EventBuilder, Filter, Keys, Kind, MatchEventOptions, Tag, Timestamp,
        };

        let seller = Keys::generate();
        let buyer = Keys::generate();

        // A TARGETED offer carries a p-tag == seller; an UNtargeted (open-pool) offer has none.
        let targeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .tag(Tag::public_key(seller.public_key()))
            .sign_with_keys(&buyer)
            .expect("sign targeted offer");
        // A FRESH untargeted offer (dated after the filter's `since(now)`). Built in the future
        // so it deterministically clears the bound regardless of sub-second test timing.
        let fresh_untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .custom_created_at(Timestamp::now() + 60u64)
            .sign_with_keys(&buyer)
            .expect("sign fresh untargeted offer");
        // A STALE untargeted offer (an hour old) — the relay's offer history.
        let stale_untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .custom_created_at(Timestamp::from(Timestamp::now().as_secs().saturating_sub(3600)))
            .sign_with_keys(&buyer)
            .expect("sign stale untargeted offer");

        let matches_any = |filters: &[Filter], event: &nostr_sdk::Event| {
            filters
                .iter()
                .any(|filter| filter.match_event(event, MatchEventOptions::new()))
        };

        // A 300s backfill window: comfortably covers the `now`/`now+60` offers, excludes the
        // hour-old one. (`match_event` only consults `since`, not `limit`.)
        let window = 300u64;

        // Targeted-only (claim_open_pool = false): the targeted offer matches, the untargeted
        // (open-pool) offer does NOT — this is exactly why --claim-open-pool was DOA.
        let targeted_only =
            offer_subscription_filters(seller.public_key(), false, Timestamp::now(), window);
        assert!(
            matches_any(&targeted_only, &targeted),
            "targeted offer must match the pinned filter"
        );
        assert!(
            !matches_any(&targeted_only, &fresh_untargeted),
            "untargeted offer must NOT match without open-pool"
        );

        // Open-pool (claim_open_pool = true): the 2nd un-pinned filter lands the FRESH untargeted
        // offer. RED-ON-REVERT: drop the `filters.push(...)` in offer_subscription_filters and
        // this assert fails — the untargeted offer no longer matches, so no claim fires.
        let open_pool =
            offer_subscription_filters(seller.public_key(), true, Timestamp::now(), window);
        assert!(
            matches_any(&open_pool, &targeted),
            "targeted offer still matches under open-pool"
        );
        assert!(
            matches_any(&open_pool, &fresh_untargeted),
            "fresh untargeted offer MUST match under open-pool (the fix)"
        );
        // The un-pinned filter is BOUNDED (`since(now - window)`): an offer OLDER than the window
        // does NOT match, so a running seller never replays the relay's full offer history.
        // RED-ON-REVERT: drop the `.since(..)` bound and this assert fails (the flood returns).
        assert!(
            !matches_any(&open_pool, &stale_untargeted),
            "an untargeted offer older than the backfill window MUST NOT match the bounded filter"
        );
    }

    // Regression: the seller's kind-1059 payment filter must match a gift-wrap that carries NO
    // t=mobee tag. NIP-59 wraps are opaque and cannot carry a namespace tag; a hashtag-namespace
    // filter here would return zero wraps and silently strand real payments.
    #[test]
    fn wrap_filter_matches_untagged_gift_wrap() {
        use nostr_sdk::prelude::{EventBuilder, Keys, Kind, MatchEventOptions, Tag};

        let seller = Keys::generate();
        let sender = Keys::generate();
        let filter = wrap_subscription_filter(seller.public_key());

        // A 1059 wrap p-tagged to the seller with NO t=mobee tag (as real gift-wraps are).
        let wrap = EventBuilder::new(Kind::GiftWrap, "opaque")
            .tag(Tag::public_key(seller.public_key()))
            .sign_with_keys(&sender)
            .expect("sign wrap");
        assert!(
            filter.match_event(&wrap, MatchEventOptions::new()),
            "seller 1059 filter must match a p-tagged wrap that has no t=mobee tag"
        );

        // A wrap p-tagged to someone else must NOT match.
        let other = EventBuilder::new(Kind::GiftWrap, "opaque")
            .tag(Tag::public_key(Keys::generate().public_key()))
            .sign_with_keys(&sender)
            .expect("sign wrap");
        assert!(!filter.match_event(&other, MatchEventOptions::new()));
    }

    #[test]
    fn open_pool_offers_ride_one_grouped_subscription() {
        use nostr_sdk::prelude::{
            EventBuilder, Filter, Keys, Kind, MatchEventOptions, Tag, Timestamp,
        };

        let seller = Keys::generate();
        let buyer = Keys::generate();

        let targeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .tag(Tag::public_key(seller.public_key()))
            .sign_with_keys(&buyer)
            .expect("sign targeted offer");
        let fresh_untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .custom_created_at(Timestamp::now() + 60u64)
            .sign_with_keys(&buyer)
            .expect("sign fresh untargeted offer");

        let matches_any = |filters: &[Filter], event: &nostr_sdk::Event| {
            filters
                .iter()
                .any(|filter| filter.match_event(event, MatchEventOptions::new()))
        };

        // Open-pool: the offer filters are registered as EXACTLY ONE live subscription (a single
        // REQ) carrying BOTH the pinned and the (bounded) un-pinned filter, so the un-pinned
        // filter rides the same long-lived subscription and streams LIVE offers. Registering the
        // un-pinned filter as a SEPARATE second subscription (two subscriptions) leaves the relay
        // dropping it after backfill. RED-ON-REVERT: return one subscription per filter and this
        // `len() == 1` assertion fails.
        let subs = offer_subscriptions(seller.public_key(), true, Timestamp::now(), 300);
        assert_eq!(
            subs.len(),
            1,
            "open-pool offers must ride ONE live subscription, not a separate un-pinned one"
        );
        let live = &subs[0];
        assert!(
            matches_any(live, &targeted),
            "the single live subscription must match targeted offers"
        );
        assert!(
            matches_any(live, &fresh_untargeted),
            "the single live subscription must ALSO match fresh untargeted (open-pool) offers"
        );

        // Targeted-only: still one subscription; matches targeted, not untargeted.
        let subs = offer_subscriptions(seller.public_key(), false, Timestamp::now(), 300);
        assert_eq!(subs.len(), 1, "one offer subscription when not open-pool");
        assert!(matches_any(&subs[0], &targeted));
        assert!(
            !matches_any(&subs[0], &fresh_untargeted),
            "untargeted offers must not match without open-pool"
        );
    }

    #[test]
    fn untargeted_offer_routes_to_on_offer_event_and_dedups() {
        use nostr_sdk::prelude::{EventBuilder, Keys, Kind};

        let buyer = Keys::generate();
        // An UNtargeted (open-pool) offer carries no p-tag.
        let untargeted = EventBuilder::new(Kind::Custom(JOB_OFFER_KIND), "task")
            .tag(nostr_sdk::prelude::Tag::hashtag(gateway::MOBEE_TAG))
            .sign_with_keys(&buyer)
            .expect("sign untargeted offer");

        let mut seen = BoundedSeen::default();

        // A non-p-tagged offer MUST route to `on_offer_event` on the LIVE push — this rules
        // out the "notification path drops non-p-tagged offer" failure mode. RED-ON-REVERT: gate
        // routing on a p-tag and this first assertion fails for the untargeted offer.
        assert!(
            offer_event_should_process(untargeted.kind.as_u16(), untargeted.id, &mut seen),
            "an untargeted offer must route to on_offer_event"
        );
        // Dedup by event id: a re-delivered offer id is processed at most once.
        assert!(
            !offer_event_should_process(untargeted.kind.as_u16(), untargeted.id, &mut seen),
            "a re-delivered offer id must be deduped, not double-processed"
        );
        // A non-offer kind (e.g. gift-wrap 1059) does not route as an offer.
        assert!(
            !offer_event_should_process(Kind::GiftWrap.as_u16(), untargeted.id, &mut seen),
            "non-offer events must not route to on_offer_event"
        );
    }

    /// A distinct, deterministic `EventId` per index — a zero-padded 64-char hex (32 bytes).
    /// Avoids signing thousands of real events just to fill the bounded set in the cap test.
    fn eid(i: usize) -> nostr_sdk::EventId {
        nostr_sdk::EventId::from_hex(&format!("{i:064x}")).expect("valid 32-byte event-id hex")
    }

    #[test]
    fn bounded_seen_caps_and_forgets_oldest() {
        // (a) CAP + eviction. Insert CAP+K distinct ids; the set must retain at most CAP, and
        // the OLDEST K (the first inserted) must be FORGOTTEN — a forgotten id re-inserts as
        // NEW (true) again. RED-ON-REVERT: drop the cap/eviction (plain HashSet) and the set
        // grows to CAP+K (the `<= CAP` assert fails) AND forgotten ids read as already-seen
        // (the "re-inserts as NEW" assert flips to false).
        const K: usize = 5;
        let mut seen = BoundedSeen::default();
        for i in 0..(SEEN_OFFERS_CAP + K) {
            assert!(
                seen.insert(eid(i)),
                "each of the first CAP+K distinct ids is new on first sight: {i}"
            );
        }
        assert!(
            seen.len() <= SEEN_OFFERS_CAP,
            "bounded set must never exceed the cap (held {})",
            seen.len()
        );
        assert_eq!(
            seen.len(),
            SEEN_OFFERS_CAP,
            "after CAP+K inserts the set holds exactly CAP (oldest K evicted)"
        );
        // The oldest K ids (0..K) were evicted → each re-inserts as NEW.
        for i in 0..K {
            assert!(
                seen.insert(eid(i)),
                "a forgotten (oldest, evicted) id must re-insert as NEW: {i}"
            );
        }
    }

    #[test]
    fn bounded_seen_dedups_recent_id() {
        // (b) window dedup. A recently-inserted id is "already seen": first insert is NEW
        // (true), a second insert within the retained window returns false (skip) — the
        // semantic `offer_event_should_process` relies on for filter-overlap / re-delivery.
        let mut seen = BoundedSeen::default();
        let id = eid(42);
        assert!(seen.insert(id), "first sight of an id is NEW");
        assert!(!seen.insert(id), "a recently-seen id is already seen (deduped)");
        // Interleave other ids while staying well under the cap — the recent id is retained.
        for i in 100..200 {
            seen.insert(eid(i));
        }
        assert!(
            !seen.insert(id),
            "an id still within the retained window stays already-seen"
        );
    }

    #[test]
    fn already_redeemed_1059_classified_info_not_error() {
        // Journal pay-once guard = idempotent already-redeemed re-see → info, not error.
        let journal_dup = DaemonError::Seller(SellerError::Journal(
            "job abcd already receipted (pay-once)".into(),
        ));
        assert!(
            is_idempotent_already_redeemed(&journal_dup),
            "journal pay-once re-see is idempotent (logged info)"
        );

        // Mint says the proofs are already spent (cdk TokenAlreadySpent) → info, not error.
        let mint_spent =
            DaemonError::Wallet(PaymentWalletError::Wallet("Token Already Spent".into()));
        assert!(
            is_idempotent_already_redeemed(&mint_spent),
            "mint already-spent re-see is idempotent (logged info)"
        );

        // Genuine failures are NOT downgraded — they stay on the error channel.
        assert!(!is_idempotent_already_redeemed(&DaemonError::Relay(
            "connection refused".into()
        )));
        assert!(!is_idempotent_already_redeemed(&DaemonError::Policy(
            "payment bind refused: payload job/result mismatch".into()
        )));
    }

    // Finding S: a receive that reports the token "already spent" is discriminated ONLY by a COMPLETED
    // receipt for the job (positive proof of our own prior collection) — NEVER by the pending-receive
    // breadcrumb (appended before every swap, so it proves nothing). The old recovery inferred "our
    // swap landed" from the breadcrumb, so a replayed already-redeemed seller-locked token forged a
    // receipt for a new same-value job with zero new funds. `has_receipt` is read FAIL-CLOSED.
    #[test]
    fn receive_already_spent_discriminated_by_completed_receipt() {
        let already_spent =
            || DaemonError::Wallet(PaymentWalletError::Wallet("Token Already Spent".into()));

        // 1. already-spent + a COMPLETED receipt exists → idempotent no-op (legit backfill/restart
        //    re-see: already collected AND receipted, no double-anything).
        let decision = classify_redeem_outcome(Err(already_spent()), || Ok(true));
        assert!(
            matches!(decision, RedeemDecision::IdempotentNoOp),
            "already-spent + has_receipt=true must be an idempotent no-op"
        );

        // 2. already-spent + NO completed receipt → refuse (the replay/theft case, and a genuine
        //    interrupted redeem — indistinguishable, so fail closed; NO receipt forged).
        let decision = classify_redeem_outcome(Err(already_spent()), || Ok(false));
        assert!(
            matches!(decision, RedeemDecision::Refuse(DaemonError::Wallet(_))),
            "already-spent + has_receipt=false must refuse (no forged receipt)"
        );

        // 3. already-spent + has_receipt READ ERROR → refuse, FAIL CLOSED (an unreadable receipt
        //    journal must never be read as "no receipt ⇒ safe to proceed" — same class as item G).
        let decision = classify_redeem_outcome(Err(already_spent()), || {
            Err(SellerError::Journal("corrupt journal line".into()))
        });
        assert!(
            matches!(
                decision,
                RedeemDecision::Refuse(DaemonError::Seller(SellerError::Journal(_)))
            ),
            "has_receipt read error must fail closed (refuse), never proceed"
        );

        // A genuine successful receive finalizes exactly its amount — has_receipt is never consulted.
        let decision =
            classify_redeem_outcome(Ok(7), || panic!("has_receipt must not be read on success"));
        assert!(matches!(decision, RedeemDecision::Finalize(7)));

        // A non-already-spent receive error refuses without consulting has_receipt.
        let decision = classify_redeem_outcome(
            Err(DaemonError::Relay("connection refused".into())),
            || panic!("has_receipt must not be read for a non-already-spent error"),
        );
        assert!(matches!(decision, RedeemDecision::Refuse(DaemonError::Relay(_))));
    }

    // Finding T(1): the seller redeem path fails closed on a non-allowlisted REAL mint when
    // allow_real_mints=false, BEFORE any swap — the same real-mint fence send/melt enforce. Refuses
    // even though the mint is in the seller's configured accepted_mints (the redeem guard alone would
    // admit it); no receipt is journaled.
    #[tokio::test]
    async fn redeem_refuses_non_allowlisted_real_mint_when_disallowed() {
        let real_mint = "https://real-mint.example/";
        let (root, mut daemon) = test_daemon("redeem-real-mint-fence");
        home::save_config(&mut daemon.home, |config| {
            config.accepted_mints = vec![real_mint.into()];
            config.allow_real_mints = false;
        })
        .expect("save");
        let buyer = nostr_sdk::Keys::generate();
        let job_id = "job-real-mint";
        let mut job = active_job(job_id, daemon.seller_pubkey(), Some("r-real"), 0, &root);
        job.buyer_pubkey = buyer.public_key().to_hex();
        daemon.awaiting_payment.push(job);

        let received = recon_received(job_id, real_mint, &buyer);
        let err = daemon
            .try_apply_payment(received)
            .await
            .expect_err("real mint must refuse under allow_real_mints=false");
        match err {
            DaemonError::Policy(message) => assert!(
                message.contains("not allowed") && message.contains("allow_real_mints=false"),
                "unexpected refusal: {message}"
            ),
            other => panic!("expected Policy refusal, got {other:?}"),
        }
        assert!(
            !daemon.journal.has_receipt(job_id).expect("has_receipt"),
            "no receipt may be journaled for a fenced real mint"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_ok_log_line_carries_amount_and_no_key_material() {
        let line = collect_ok_log_line("job1", "res1", 5, 5, "https://testnut.example");
        assert!(
            line.contains("amount_received=5"),
            "collect log must surface the collected amount"
        );
        assert!(line.contains("job_id=job1") && line.contains("result_id=res1"));
        assert!(line.contains("mint=https://testnut.example"));
        // No key/token material ever in the collect log.
        let lower = line.to_ascii_lowercase();
        assert!(
            !lower.contains("token") && !lower.contains("secret") && !lower.contains("nsec"),
            "collect log must never carry token/key material"
        );
    }

    #[test]
    fn seller_exec_metadata_emits_captured_usage_into_result_tags() {
        use crate::driver::{UsageCost, UsageMetadata};

        // A tag qualified by cell index 1 (value) + cell 2 (qualifier), e.g. ["tokens","140","total"].
        let qualified = |tags: &[TagSpec], name: &str, qualifier: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name) && tag.0.get(2).map(String::as_str) == Some(qualifier))
                .and_then(|tag| tag.value().map(str::to_owned))
        };
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        let usage = UsageMetadata {
            model: Some("claude-opus-4-8".into()),
            input_tokens: Some(100),
            output_tokens: Some(40),
            reasoning_tokens: None,
            cache_read_tokens: Some(4096),
            cache_write_tokens: Some(512),
            cost: Some(UsageCost {
                amount: "0.0123".into(),
                basis: "harness-reported-usd".into(),
            }),
        };
        // usage_transport is the harness's declared axis: a claude command is side-channel.
        let tags = seller_exec_metadata(&["claude".into()], None, 4321, Some(&usage));

        assert_eq!(value(&tags, "usage_transport").as_deref(), Some("side-channel"));
        assert_eq!(value(&tags, "model").as_deref(), Some("claude-opus-4-8"));
        // total = input + output (reasoning absent = unknown, not zero); cache NOT folded in.
        assert_eq!(qualified(&tags, "tokens", "total").as_deref(), Some("140"));
        assert_eq!(qualified(&tags, "tokens", "input").as_deref(), Some("100"));
        assert_eq!(qualified(&tags, "tokens", "output").as_deref(), Some("40"));
        assert_eq!(qualified(&tags, "tokens", "reasoning"), None);
        assert_eq!(qualified(&tags, "tokens", "cache_read").as_deref(), Some("4096"));
        assert_eq!(qualified(&tags, "tokens", "cache_write").as_deref(), Some("512"));
        // cost tag: ["cost","<amount>","usd","<basis>"].
        let cost = tags
            .iter()
            .find(|t| t.first() == Some("cost"))
            .expect("cost tag");
        assert_eq!(cost.0, vec!["cost", "0.0123", "usd", "harness-reported-usd"]);

        // Partial capture (output only) → NO total tag (a partial never masquerades as complete).
        let partial = UsageMetadata {
            output_tokens: Some(40),
            ..UsageMetadata::default()
        };
        let partial_tags = seller_exec_metadata(&["claude".into()], None, 1, Some(&partial));
        assert_eq!(qualified(&partial_tags, "tokens", "total"), None);
        assert_eq!(qualified(&partial_tags, "tokens", "output").as_deref(), Some("40"));
    }

    fn sample_offer(amount: u64, seller: &str) -> ParsedOffer {
        ParsedOffer {
            task: "task".into(),
            output: "text/plain".into(),
            amount,
            unit: "sat".into(),
            deadline_unix: 2_000_000_000,
            seller_pubkey: Some(seller.to_owned()),
        }
    }

    fn active_job(
        job_id: &str,
        seller: &str,
        result_id: Option<&str>,
        deadline: u64,
        root: &Path,
    ) -> ActiveJob {
        ActiveJob {
            job_id: job_id.into(),
            buyer_pubkey: "bb".repeat(32),
            offer: sample_offer(5, seller),
            claim_id: format!("claim-{job_id}"),
            result_id: result_id.map(str::to_owned),
            deadline_unix: deadline,
            workdir: root.join(job_id),
            contribution: None,
            delivery: None,
            accepted_mints: Vec::new(),
        }
    }

    // ── Award match logic ────────────────────────────────────────────────────────────────
    // `active_job` parks a claim with buyer_pubkey="bb"*32 and claim_id="claim-<job_id>".

    /// The buyer's award naming our parked claim selects it for execution.
    #[test]
    fn award_for_our_parked_claim_executes() {
        let root = temp("award-exec");
        let jobs = vec![active_job("job-a", &"aa".repeat(32), None, 0, &root)];
        let award = ParsedAward {
            offer_id: "job-a".into(),
            claim_id: "claim-job-a".into(),
        };
        assert_eq!(
            match_award(&jobs, &award, &"bb".repeat(32)),
            AwardMatch::Execute(0)
        );
    }

    /// The buyer awarding a DIFFERENT claim for our offer releases ours (we lost).
    #[test]
    fn award_for_a_different_claim_releases_ours() {
        let root = temp("award-release");
        let jobs = vec![active_job("job-a", &"aa".repeat(32), None, 0, &root)];
        let award = ParsedAward {
            offer_id: "job-a".into(),
            claim_id: "claim-some-other-seller".into(),
        };
        assert_eq!(
            match_award(&jobs, &award, &"bb".repeat(32)),
            AwardMatch::Release(0)
        );
    }

    /// An award authored by anyone other than the offer buyer is ignored — only the buyer awards.
    #[test]
    fn award_from_a_non_buyer_is_ignored() {
        let root = temp("award-spoof");
        let jobs = vec![active_job("job-a", &"aa".repeat(32), None, 0, &root)];
        let award = ParsedAward {
            offer_id: "job-a".into(),
            claim_id: "claim-job-a".into(),
        };
        assert_eq!(
            match_award(&jobs, &award, &"cc".repeat(32)),
            AwardMatch::Ignored
        );
    }

    /// An award for an offer we never claimed is ignored.
    #[test]
    fn award_for_an_unclaimed_offer_is_ignored() {
        let root = temp("award-unknown");
        let jobs = vec![active_job("job-a", &"aa".repeat(32), None, 0, &root)];
        let award = ParsedAward {
            offer_id: "job-zzz".into(),
            claim_id: "claim-job-zzz".into(),
        };
        assert_eq!(
            match_award(&jobs, &award, &"bb".repeat(32)),
            AwardMatch::Ignored
        );
    }

    /// With several parked claims the award binds to the one rooted on its offer.
    #[test]
    fn award_selects_the_matching_offer_among_several() {
        let root = temp("award-many");
        let seller = "aa".repeat(32);
        let jobs = vec![
            active_job("job-a", &seller, None, 0, &root),
            active_job("job-b", &seller, None, 0, &root),
            active_job("job-c", &seller, None, 0, &root),
        ];
        let award = ParsedAward {
            offer_id: "job-b".into(),
            claim_id: "claim-job-b".into(),
        };
        assert_eq!(
            match_award(&jobs, &award, &"bb".repeat(32)),
            AwardMatch::Execute(1)
        );
    }

    fn test_daemon(label: &str) -> (PathBuf, SellerDaemon) {
        let root = temp(label);
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home::save_config(&mut home, |config| {
            config.accepted_mints = vec![DEFAULT_MINT_URL.into()];
            config.seller = Some(SellerConfig {
                agent_command: vec!["echo".into()],
                rate_sats: 1,
                git_remote: "https://example.invalid/repo.git".into(),
                job_timeout_secs: None,
                agent: None,
                claim_open_pool: false,
                offer_backfill_secs: 0,
                contribution_enabled: true,
            });
        })
        .expect("save");
        let keys = nostr_sdk::Keys::generate();
        let seller_pubkey = keys.public_key().to_hex();
        let journal = SellerJournal::open(&home).expect("journal");
        let daemon = SellerDaemon {
            home,
            keys,
            seller_pubkey,
            journal,
            pay_buffer: VecDeque::new(),
            awaiting_award: Vec::new(),
            active: None,
            awaiting_payment: Vec::new(),
        };
        (root, daemon)
    }

    // Z3 (fail-closed startup journal): with a clean journal the daemon may claim new work; with an
    // unreadable/corrupt journal it must NOT — the startup path reads this gate to skip the offer
    // subscription (receive-only degraded mode), so a daemon that lost its prior state never claims
    // fresh liability.
    #[test]
    fn unreadable_journal_disables_new_claims_at_startup() {
        let (root, daemon) = test_daemon("z3-journal-gate");
        // Clean journal (fresh bootstrap) ⇒ claiming allowed.
        assert!(
            daemon.claim_allowed_after_journal_check(),
            "clean journal must allow claiming"
        );
        // Corrupt the journal with a non-JSON line ⇒ entries() errors ⇒ claiming refused.
        let journal_path = daemon.journal.path().to_path_buf();
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&journal_path)
                .expect("open journal");
            f.write_all(b"{not valid json\n").expect("corrupt");
            f.sync_all().expect("sync");
        }
        assert!(
            !daemon.claim_allowed_after_journal_check(),
            "unreadable journal must DISABLE new claims (receive-only degraded mode)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn offer_event(
        buyer: &nostr_sdk::Keys,
        seller_pubkey: &str,
        amount: u64,
        deadline: u64,
    ) -> nostr_sdk::Event {
        let offer = crate::gateway::OfferDraft::new(
            "do a task",
            "text/plain",
            amount,
            deadline,
            seller_pubkey,
        );
        let draft = offer.to_event_draft();
        let builder = gateway::nostr::event_builder(&draft).expect("event builder");
        builder.sign_with_keys(buyer).expect("sign offer")
    }

    fn contribution_offer_event(
        buyer: &nostr_sdk::Keys,
        seller_pubkey: &str,
        deadline: u64,
        extra_tags: Vec<TagSpec>,
    ) -> nostr_sdk::Event {
        let offer =
            crate::gateway::OfferDraft::new("do a task", "text/plain", 5, deadline, seller_pubkey);
        let mut draft = offer.to_event_draft();
        draft.tags.extend(extra_tags);
        let builder = gateway::nostr::event_builder(&draft).expect("event builder");
        builder.sign_with_keys(buyer).expect("sign offer")
    }

    fn sample_contribution_tags() -> Vec<TagSpec> {
        let c = ContributionOffer {
            target: crate::contribution::TargetRepoPin::new(
                "aa".repeat(32),
                "https://mobee-relay.orveth.dev/git/owner/repo.git",
            )
            .unwrap(),
            base: crate::contribution::ContributionBase::new("main", "77".repeat(20)).unwrap(),
            accepts: vec!["fork".into()],
        };
        crate::contribution::contribution_offer_tags(&c)
    }

    // The daemon RECOGNISES a contribution offer, threads the pins into the claim intent.
    #[test]
    fn classify_admits_contribution_offer_and_threads_pins() {
        let (root, daemon) = test_daemon("contrib-admit");
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;
        let ev = contribution_offer_event(&buyer, &seller_pk, now + 3600, sample_contribution_tags());
        match daemon.classify_offer(&ev, now).expect("classify") {
            OfferDisposition::Claim(intent) => {
                let c = intent.contribution.expect("contribution threaded into claim intent");
                assert_eq!(c.target.owner_pubkey(), "aa".repeat(32));
                assert_eq!(c.base.oid(), "77".repeat(20));
                assert!(c.accepts_fork());
            }
            other => panic!("must admit a contribution offer, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // A seller with contribution support DISABLED refuses (interop feedback-kind skip).
    #[test]
    fn classify_refuses_contribution_when_disabled() {
        let (root, mut daemon) = test_daemon("contrib-disabled");
        if let Some(seller) = daemon.home.config.seller.as_mut() {
            seller.contribution_enabled = false;
        }
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;
        let ev = contribution_offer_event(&buyer, &seller_pk, now + 3600, sample_contribution_tags());
        assert!(matches!(
            daemon.classify_offer(&ev, now).expect("classify"),
            OfferDisposition::Skip(OfferSkip::ContributionUnsupported)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    // A malformed contribution offer is REFUSED (fail-closed) — never run from-scratch.
    #[test]
    fn classify_refuses_malformed_contribution_offer() {
        let (root, daemon) = test_daemon("contrib-malformed");
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;
        // job-class=contribution but a broken base oid.
        let bad = vec![
            TagSpec::new([crate::contribution::TAG_JOB_CLASS, crate::contribution::JOB_CLASS_CONTRIBUTION]),
            TagSpec::new([crate::contribution::TAG_TARGET_REPO, &"aa".repeat(32), "https://x/git/o/r.git"]),
            TagSpec::new([crate::contribution::TAG_BASE, "main", "not-an-oid"]),
            TagSpec::new([crate::contribution::TAG_ACCEPTS, "fork"]),
        ];
        let ev = contribution_offer_event(&buyer, &seller_pk, now + 3600, bad);
        assert!(matches!(
            daemon.classify_offer(&ev, now).expect("classify"),
            OfferDisposition::Skip(OfferSkip::ContributionMalformed { .. })
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Behavior 1: a delivered-but-unpaid job MUST NOT block claiming a new offer;
    // only a PROCESSING job holds single-flight, and any skip is a NAMED reason.
    #[test]
    fn delivered_unpaid_does_not_block_new_offer_but_processing_does() {
        let (root, mut daemon) = test_daemon("admit");
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;

        // Idle slot ⇒ offer is admitted (Claim).
        let ev1 = offer_event(&buyer, &seller_pk, 5, now + 3600);
        match daemon.classify_offer(&ev1, now).expect("classify idle") {
            OfferDisposition::Claim(intent) => assert_eq!(intent.job_id, ev1.id.to_hex()),
            other => panic!("idle daemon must admit an offer, got {other:?}"),
        }

        // A DELIVERED-but-unpaid job awaiting payment ⇒ STILL admits a new offer (the fix).
        daemon.awaiting_payment.push(active_job(
            "delivered-prev",
            &seller_pk,
            Some("result-prev"),
            now + 3600,
            &root,
        ));
        assert!(daemon.active.is_none(), "delivered job must not hold the slot");
        let ev2 = offer_event(&buyer, &seller_pk, 5, now + 3600);
        match daemon.classify_offer(&ev2, now).expect("classify while awaiting-pay") {
            OfferDisposition::Claim(_) => {}
            other => panic!("delivered-but-unpaid must NOT block a new claim, got {other:?}"),
        }

        // A PROCESSING job (holds the slot) ⇒ skip, but with an explicit, non-empty reason.
        daemon.active = Some(active_job("processing-now", &seller_pk, None, now + 3600, &root));
        let ev3 = offer_event(&buyer, &seller_pk, 5, now + 3600);
        match daemon.classify_offer(&ev3, now).expect("classify while processing") {
            OfferDisposition::Skip(skip) => {
                assert!(matches!(skip, OfferSkip::ProcessingBusy { .. }), "got {skip:?}");
                let reason = skip.reason();
                assert!(!reason.is_empty(), "skip reason must never be empty (never silent)");
                assert!(
                    reason.contains("processing"),
                    "reason must name the processing single-flight: {reason}"
                );
            }
            other => panic!("processing job must block with a reason, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // Behavior 2: restart-reconcile over a REAL orphaned-claim fixture (journaled in-flight
    // claim + past deadline). The orphan is released (durable, no relay) and never re-fired.
    #[test]
    fn reconcile_journal_releases_real_orphaned_claim_and_is_idempotent() {
        let (root, mut daemon) = test_daemon("reconcile");
        let buyer = "cc".repeat(32);

        // A real journaled in-flight claim with a PAST deadline, no receipt, no release —
        // exactly the orphaned live claim a crashed daemon leaves behind.
        daemon
            .journal
            .append_claim("orphan-job", "orphan-claim", &buyer, 1_000_000_000)
            .expect("journal orphaned claim");

        // Before reconcile, it reads as an in-flight orphan (would show "processing").
        let pre = daemon.reconcile_plan(2_000_000_000).expect("plan");
        assert_eq!(pre.len(), 1, "one orphaned claim in flight: {pre:?}");
        assert_eq!(pre[0].job_id, "orphan-job");
        assert_eq!(pre[0].buyer_pubkey, buyer);
        assert_eq!(pre[0].liveness, ClaimLiveness::Expired, "past deadline ⇒ EXPIRED");

        // Reconcile releases it durably (journal RELEASE) with no relay.
        let released = daemon.reconcile_journal(2_000_000_000).expect("reconcile");
        assert_eq!(released.len(), 1);
        assert!(
            daemon.journal.has_release("orphan-job").expect("has_release"),
            "orphan must be journaled as released — never left silently live"
        );

        // Idempotent: a second restart finds nothing to release.
        let again = daemon.reconcile_journal(2_000_000_000).expect("reconcile again");
        assert!(again.is_empty(), "released orphan is terminal: {again:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Money-safety guard #a (PURE): an offer whose OWN deadline already passed is REFUSED, so a
    // (backfilled) offer can never be resurrected with a fresh `now + timeout` deadline. Injected
    // `now` keeps it deterministic — no wall clock.
    #[test]
    fn classify_refuses_offer_past_its_own_deadline() {
        let (root, daemon) = test_daemon("expired");
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;

        // Deadline in the PAST ⇒ DeadlineExpired skip with a named, non-empty reason.
        let expired = offer_event(&buyer, &seller_pk, 5, now - 1);
        match daemon.classify_offer(&expired, now).expect("classify expired") {
            OfferDisposition::Skip(skip) => {
                assert!(matches!(skip, OfferSkip::DeadlineExpired { .. }), "got {skip:?}");
                let reason = skip.reason();
                assert!(
                    !reason.is_empty() && reason.contains("expired"),
                    "skip reason must name expiry (never silent): {reason}"
                );
            }
            other => panic!("an offer past its deadline must be REFUSED, got {other:?}"),
        }

        // Boundary: deadline == now is refused (`<= now`); a strictly-future deadline is admitted.
        assert!(
            matches!(
                daemon.classify_offer(&offer_event(&buyer, &seller_pk, 5, now), now).expect("at-now"),
                OfferDisposition::Skip(OfferSkip::DeadlineExpired { .. })
            ),
            "deadline == now must be refused"
        );
        assert!(
            matches!(
                daemon.classify_offer(&offer_event(&buyer, &seller_pk, 5, now + 1), now).expect("future"),
                OfferDisposition::Claim(_)
            ),
            "a strictly-future deadline must be admitted"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Targeted under-rate refusal: the feedback-kind `status=error` draft CONTENT must carry the
    // same machine-readable rate-gate reason the skip log already has (buyers distinguish
    // rate-refusal from a crash / empty-content failure).
    #[test]
    fn under_rate_error_draft_content_carries_rate_gate_reason() {
        let reason = "offer amount 3 sat below seller rate_sats 5";
        let draft = under_rate_error_draft("offer-id", "buyer-pk", "seller-pk", reason);
        assert_eq!(draft.kind, gateway::JOB_FEEDBACK_KIND);
        assert_eq!(
            draft.content, reason,
            "feedback-kind content must carry the rate-gate reason, not stay empty"
        );
        assert!(
            draft.tags.iter().any(|t| {
                t.0.first().map(String::as_str) == Some("status")
                    && t.0.get(1).map(String::as_str) == Some("error")
            }),
            "must be status=error: {:?}",
            draft.tags
        );
        assert!(
            draft.tags.iter().any(|t| {
                t.0.first().map(String::as_str) == Some("e")
                    && t.0.get(1).map(String::as_str) == Some("offer-id")
            }),
            "must e-tag the refused offer: {:?}",
            draft.tags
        );
    }

    // classify → RateGate reason is the clean policy string that under_rate_error_draft embeds.
    #[test]
    fn classify_under_rate_reason_is_plumbed_into_error_draft_content() {
        let (root, mut daemon) = test_daemon("under-rate-content");
        if let Some(seller) = daemon.home.config.seller.as_mut() {
            seller.rate_sats = 5;
        }
        let seller_pk = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let now = 1_000_000u64;
        let ev = offer_event(&buyer, &seller_pk, 3, now + 3600);
        let reason = match daemon.classify_offer(&ev, now).expect("classify") {
            OfferDisposition::Skip(OfferSkip::RateGate { reason }) => reason,
            other => panic!("targeted under-rate must RateGate-skip, got {other:?}"),
        };
        assert_eq!(
            reason, "offer amount 3 sat below seller rate_sats 5",
            "classify must surface the machine-readable rate-gate reason"
        );
        let draft = under_rate_error_draft(
            &ev.id.to_hex(),
            &ev.pubkey.to_hex(),
            &seller_pk,
            &reason,
        );
        assert_eq!(
            draft.content, reason,
            "emitted feedback-kind content must equal the rate-gate reason"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn assert_error_content_starts_with(code: &str, content: &str) {
        assert!(!content.is_empty(), "error content must not be empty");
        assert!(
            content.starts_with(&format!("{code}: ")),
            "error content must start with {code}:, got {content:?}"
        );
    }

    #[test]
    fn seller_error_content_is_machine_readable_truncated_and_path_scrubbed() {
        let long = format!(
            "failed in /Users/seller/private/job/worktree/file.rs while using https://relay.example/git/repo.git {}",
            "x".repeat(400)
        );
        let content = seller_error_content(ErrorReasonCode::AgentRunFailed, &long);
        assert_error_content_starts_with("agent_run_failed", &content);
        let human = content
            .strip_prefix("agent_run_failed: ")
            .expect("prefix checked");
        assert!(human.chars().count() <= 300, "human part is capped: {}", human.len());
        assert!(!human.contains("/Users/seller/private"), "absolute path leaked: {human}");
        assert!(human.contains("<path>"), "path redaction marker absent: {human}");
        assert!(
            human.contains("https://relay.example/git/repo.git"),
            "URLs are public locators, not filesystem paths: {human}"
        );
    }

    #[test]
    fn agent_error_reason_code_covers_spawn_timeout_and_run_failure() {
        assert_eq!(
            agent_error_reason_code(&DaemonError::Agent("failed to spawn ACP agent: no such file".into()))
                .as_str(),
            "agent_spawn_failed"
        );
        assert_eq!(
            agent_error_reason_code(&DaemonError::Agent(
                "ACP request 3 timed out waiting for response".into()
            ))
            .as_str(),
            "agent_timeout"
        );
        assert_eq!(
            agent_error_reason_code(&DaemonError::Agent("agent terminal Failed".into())).as_str(),
            "agent_run_failed"
        );
    }

    #[test]
    fn active_job_abort_error_drafts_carry_machine_readable_content() {
        // These are the active-job fail_active publishers. Live publish itself is intentionally
        // not exercised here: publish_draft hits the relay; the regression is the draft content
        // passed to that publisher.
        let cases = [
            (
                "agent_spawn_failed",
                seller_error_content(ErrorReasonCode::AgentSpawnFailed, "failed to spawn ACP agent"),
            ),
            (
                "agent_run_failed",
                seller_error_content(ErrorReasonCode::AgentRunFailed, "agent terminal Failed"),
            ),
            (
                "agent_timeout",
                seller_error_content(ErrorReasonCode::AgentTimeout, "job deadline exceeded"),
            ),
            (
                "git_fork_failed",
                seller_error_content(ErrorReasonCode::GitForkFailed, "seller git fetch failed"),
            ),
            (
                "git_push_failed",
                seller_error_content(ErrorReasonCode::GitPushFailed, "seller git push failed"),
            ),
            (
                "internal",
                seller_error_content(ErrorReasonCode::Internal, "result publish failed"),
            ),
        ];
        for (code, content) in cases {
            let draft = error_draft("offer-id", "buyer-pk", "seller-pk", content);
            assert_error_content_starts_with(code, &draft.content);
        }
    }

    #[test]
    fn contribution_refusal_error_drafts_carry_machine_readable_content() {
        let unsupported = contribution_refusal_error_content(&OfferSkip::ContributionUnsupported);
        assert_error_content_starts_with("contribution_unsupported", &unsupported);

        let malformed = contribution_refusal_error_content(&OfferSkip::ContributionMalformed {
            reason: "bad base oid".into(),
        });
        assert_error_content_starts_with("contribution_malformed", &malformed);
    }

    // A delivered-but-unpaid job in the journal is rebuilt into awaiting_payment on boot, so a
    // stored/buffered wrap can bind and redeem. Without this the wrap buffers forever.
    #[test]
    fn restore_delivered_unpaid_rebuilds_awaiting_payment() {
        let (root, mut daemon) = test_daemon("restore-unpaid");
        let buyer = "cc".repeat(32);
        daemon
            .journal
            .append_claim("del-job", "del-claim", &buyer, 1_000_000_000)
            .expect("claim");
        // Journal an ORIGINAL advertised-mint list that DIFFERS from the daemon's current config
        // (config = [DEFAULT_MINT_URL]) so the restore can be proven to carry the original terms,
        // not current config (Fix Q — terms-drift).
        let original_mint = "https://mint-original.testnut.example";
        daemon
            .journal
            .append_delivery(
                "del-job",
                "del-result",
                15,
                "sat",
                &buyer,
                &[original_mint.to_string()],
            )
            .expect("delivery");
        assert!(daemon.awaiting_payment.is_empty());

        let restored = daemon.restore_delivered_unpaid().expect("restore");
        assert_eq!(restored, 1);
        assert_eq!(daemon.awaiting_payment.len(), 1);
        let job = &daemon.awaiting_payment[0];
        assert_eq!(job.job_id, "del-job");
        assert_eq!(job.result_id.as_deref(), Some("del-result"));
        assert_eq!(job.offer.amount, 15);
        assert_eq!(job.offer.unit, "sat");
        // Fix Q: the restored job carries the ORIGINAL advertised mints (not current config), and
        // the redeem-mint selection therefore resolves to them.
        assert_eq!(job.accepted_mints, vec![original_mint.to_string()]);
        assert_eq!(
            redeem_accepted_mints(&job.accepted_mints, &daemon.home.config.accepted_mints),
            &[original_mint.to_string()],
            "restored job must redeem against its original mints, not current config"
        );
        // Rebuilt offer is pinned to THIS seller (not a None wildcard) — assert_seller_matches
        // binds to us exactly.
        assert_eq!(job.offer.seller_pubkey.as_deref(), Some(daemon.seller_pubkey()));

        // Idempotent: a delivered job already in awaiting_payment is not duplicated.
        let again = daemon.restore_delivered_unpaid().expect("restore again");
        assert_eq!(again, 0);
        assert_eq!(daemon.awaiting_payment.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    // Fix Q — redeem-mint selection: a restored job (non-empty original mints) settles against
    // THOSE mints, not current config; a live job (empty) settles against config.
    #[test]
    fn redeem_accepted_mints_prefers_original_terms_for_restored_job() {
        let original = vec!["https://mint-orig.testnut.example".to_string()];
        let config = vec!["https://mint-current.testnut.example".to_string()];
        // Restored job: original terms win over current config.
        assert_eq!(redeem_accepted_mints(&original, &config), original.as_slice());
        // Live job (no original terms recorded): falls back to current config.
        assert_eq!(redeem_accepted_mints(&[], &config), config.as_slice());
    }

    // ── #b: reconstruct a delivered-job bind from on-relay result+claim during wrap backfill ──
    // The recovery case `restore_delivered_unpaid` cannot cover: no local journal Delivery entry.

    const RECON_KEYSET_ID: &str = "009a1f293253e41e";

    /// A received NUT-18 payment for `job_id` paid at `mint`, sealed by `buyer`.
    fn recon_received(job_id: &str, mint: &str, buyer: &nostr_sdk::Keys) -> ReceivedPayment {
        use std::str::FromStr;

        use cashu::secret::Secret;
        use cashu::{Amount, CurrencyUnit, Id, MintUrl, Proof, SecretKey};

        let proof = Proof::new(
            Amount::from(5),
            Id::from_str(RECON_KEYSET_ID).expect("keyset id"),
            Secret::new("recon-test-secret-do-not-leak"),
            SecretKey::generate().public_key(),
        );
        ReceivedPayment {
            payload: crate::payment_send::PaymentPayload {
                seller_pubkey: String::new(),
                payload: cashu::nuts::nut18::PaymentRequestPayload {
                    id: Some(job_id.to_owned()),
                    memo: None,
                    mint: MintUrl::from_str(mint).expect("mint url"),
                    unit: CurrencyUnit::Sat,
                    proofs: vec![proof],
                },
            },
            buyer_pubkey: buyer.public_key(),
        }
    }

    /// A JobView carrying one result + one claim, each authored by the given seller, plus an offer.
    /// `claim_creq` is the seller-authored `creq` tag value (built with `build_seller_creq`).
    fn recon_job_view(
        job_id: &str,
        offer_amount: u64,
        buyer_pubkey: &str,
        result_seller: &str,
        claim_seller: &str,
        claim_creq: Option<String>,
    ) -> crate::job_lifecycle::JobView {
        use crate::job_lifecycle::{ClaimView, JobView, OfferView, ResultView};
        JobView {
            job_id: job_id.to_owned(),
            offer: Some(OfferView {
                event_id: job_id.to_owned(),
                created_at: 1_000,
                author_pubkey: buyer_pubkey.to_owned(),
                author_display_name: None,
                task: "task".into(),
                output: "text/plain".into(),
                amount_sats: offer_amount,
                deadline_unix: 2_000_000_000,
                seller_pubkey: Some(result_seller.to_owned()),
                seller_display_name: None,
                targeted: true,
                repo: None,
                branch: None,
                job_class: None,
                contribution: None,
            }),
            claims: vec![ClaimView {
                claim_id: format!("claim-{job_id}"),
                created_at: 1_100,
                seller_pubkey: claim_seller.to_owned(),
                display_name: None,
                status: "processing".into(),
                live: false,
                creq: claim_creq,
            }],
            results: vec![ResultView {
                result_id: format!("result-{job_id}"),
                created_at: 1_200,
                seller_pubkey: result_seller.to_owned(),
                display_name: None,
                job_hash: None,
                repo: None,
                branch: None,
                commit_oid: None,
                amount_sats: Some(offer_amount),
                seller_signature: None,
                contribution: None,
            }],
            live_claim_id: None,
            accepted: None,
            pending: false,
        }
    }

    // Happy path: no journal bind, but a self-authored result + claim (with a matching creq) on the
    // relay → the bind is reconstructed and lands in awaiting_payment so the normal redeem path's
    // job+result lookup succeeds (redeem fires through the SAME guards — no bypass).
    #[test]
    fn reconstruct_binds_from_relay_when_journal_lacks_delivery() {
        let (root, daemon) = test_daemon("recon-happy");
        let seller = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "job-recon-happy";
        let creq = gateway::creq::build_seller_creq(
            job_id,
            15,
            "sat",
            &[DEFAULT_MINT_URL.into()],
            &seller,
        )
        .expect("build creq");
        let view = recon_job_view(job_id, 15, &buyer_hex, &seller, &seller, Some(creq));
        let received = recon_received(job_id, DEFAULT_MINT_URL, &buyer);

        assert!(daemon.awaiting_payment.is_empty());
        let job = daemon
            .reconstruct_delivered_bind(&view, &received)
            .expect("reconstruct must bind a self-authored result+claim");
        assert_eq!(job.job_id, job_id);
        assert_eq!(job.result_id.as_deref(), Some(format!("result-{job_id}").as_str()));
        assert_eq!(job.claim_id, format!("claim-{job_id}"));
        assert_eq!(job.offer.amount, 15);
        assert_eq!(job.offer.unit, "sat");
        assert_eq!(job.buyer_pubkey, buyer_hex);
        // Rebuilt offer is pinned to THIS seller (not a None wildcard) — assert_seller_matches
        // binds to us exactly.
        assert_eq!(job.offer.seller_pubkey.as_deref(), Some(seller.as_str()));

        // The reconstructed job binds the payment (the redeem path's job+result lookup succeeds).
        let mut daemon = daemon;
        daemon.awaiting_payment.push(job);
        assert!(daemon
            .awaiting_payment
            .iter()
            .any(|j| j.job_id == received.payload.job_id()));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding J: the claim id is derivable from the SIGNED event BEFORE publish, so the CLAIMED
    // transition can be journaled before a live creq exists on the relay. Proves the write-before-
    // publish primitive: sign → (journal) → publish, with the journaled id == the deterministic
    // published id.
    #[test]
    fn claim_journaled_with_presign_id_before_publish() {
        let (root, daemon) = test_daemon("claim-presign");
        let seller = daemon.seller_pubkey().to_owned();
        let creq = gateway::creq::build_seller_creq(
            "job-presign",
            5,
            "sat",
            &[DEFAULT_MINT_URL.into()],
            &seller,
        )
        .expect("creq");
        let claim = claim_draft("job-presign", "buyer-x", &seller, &creq);
        let event = sign_draft(&daemon.keys, &claim).expect("sign");
        let claim_id = event.id.to_hex();
        assert_eq!(claim_id.len(), 64, "pre-publish claim id must be a full event id");

        // The journal records the pre-publish id — a durable claim exists with NO network publish.
        daemon
            .journal
            .append_claim("job-presign", &claim_id, "buyer-x", 2_000_000_000)
            .expect("journal claim");
        assert!(daemon.journal.has_claim("job-presign").expect("has_claim"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding G: a journal read error in the already-receipted guard must FAIL CLOSED — buffer the
    // wrap rather than treat the error as "no receipt" and fall through to redeem (which could
    // re-pay an already-receipted job).
    #[tokio::test]
    async fn has_receipt_read_error_fails_closed_and_buffers() {
        let (root, mut daemon) = test_daemon("has-receipt-failclosed");
        let buyer = nostr_sdk::Keys::generate();
        let job_id = "job-corrupt-journal";
        let received = recon_received(job_id, DEFAULT_MINT_URL, &buyer);

        // Corrupt the journal so has_receipt() returns Err (a corrupt-line Journal error).
        let journal_path = daemon.home.root.join("seller-journal.jsonl");
        std::fs::write(&journal_path, "this-is-not-json\n").expect("corrupt journal");
        assert!(
            daemon.journal.has_receipt(job_id).is_err(),
            "corrupt journal must make has_receipt error"
        );

        let outcome = try_apply_or_buffer(&mut daemon, "evt-corrupt".into(), received)
            .await
            .expect("fail-closed path returns Ok(Buffered), never an error");
        assert!(matches!(outcome, ApplyResult::Buffered));
        assert_eq!(
            daemon.pay_buffer.len(),
            1,
            "wrap must be buffered on journal read error, not redeemed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding M: a payment whose authenticated NIP-17 seal sender is NOT the bound offer buyer is
    // refused BEFORE any redeem (no third-party pay-once close of someone else's job).
    #[tokio::test]
    async fn payment_from_non_offer_buyer_is_refused_before_redeem() {
        let (root, mut daemon) = test_daemon("third-party-settle");
        let offer_buyer = nostr_sdk::Keys::generate();
        let attacker = nostr_sdk::Keys::generate();
        let job_id = "job-3p";

        let mut job = active_job(job_id, daemon.seller_pubkey(), Some("r-3p"), 0, &root);
        job.buyer_pubkey = offer_buyer.public_key().to_hex();
        daemon.awaiting_payment.push(job);

        // The wrap's seal sender is the ATTACKER, not the offer buyer.
        let received = recon_received(job_id, DEFAULT_MINT_URL, &attacker);
        let err = daemon
            .try_apply_payment(received)
            .await
            .expect_err("third-party settlement must refuse");
        match err {
            DaemonError::Policy(message) => assert!(
                message.contains("not the bound offer buyer"),
                "unexpected refusal: {message}"
            ),
            other => panic!("expected Policy refusal, got {other:?}"),
        }
        // No receipt journaled, job still awaiting payment.
        assert!(!daemon.journal.has_receipt(job_id).expect("has_receipt"));
        assert_eq!(daemon.awaiting_payment.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    // A result authored by ANOTHER seller for J → refuse (never bind money to a foreign delivery).
    #[test]
    fn reconstruct_refuses_foreign_authored_result() {
        let (root, daemon) = test_daemon("recon-foreign");
        let seller = daemon.seller_pubkey().to_owned();
        let foreign = nostr_sdk::Keys::generate().public_key().to_hex();
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "job-recon-foreign";
        // creq is well-formed and self-authored on the claim, but the RESULT is foreign.
        let creq = gateway::creq::build_seller_creq(
            job_id,
            15,
            "sat",
            &[DEFAULT_MINT_URL.into()],
            &seller,
        )
        .expect("build creq");
        let view = recon_job_view(job_id, 15, &buyer_hex, &foreign, &seller, Some(creq));
        let received = recon_received(job_id, DEFAULT_MINT_URL, &buyer);

        let err = daemon
            .reconstruct_delivered_bind(&view, &received)
            .expect_err("foreign result must refuse");
        assert!(err.contains("no self-authored result"), "reason: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The self claim's creq lists a DIFFERENT mint than the payload realized mint (creq mismatch vs
    // payload), while config accepts the payload mint → refuse at the creq-binding guard.
    #[test]
    fn reconstruct_refuses_creq_mismatch_vs_payload() {
        let (root, daemon) = test_daemon("recon-creq-mismatch");
        let seller = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "job-recon-creq";
        // creq authored over a mint the buyer did NOT pay at; payload pays at DEFAULT_MINT_URL
        // (which IS in accepted_mints), isolating the creq-vs-payload mismatch from the allow-list.
        let other_mint = "https://other-mint.example/";
        let creq =
            gateway::creq::build_seller_creq(job_id, 15, "sat", &[other_mint.into()], &seller)
                .expect("build creq");
        let view = recon_job_view(job_id, 15, &buyer_hex, &seller, &seller, Some(creq));
        let received = recon_received(job_id, DEFAULT_MINT_URL, &buyer);

        let err = daemon
            .reconstruct_delivered_bind(&view, &received)
            .expect_err("creq mismatch must refuse");
        assert!(err.contains("creq mismatch"), "reason: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The payload realized mint is NOT in the seller's accepted_mints (and the creq lists it, so
    // this isolates the allow-list guard) → refuse; the mint allow-list is never relaxed.
    #[test]
    fn reconstruct_refuses_mint_outside_accepted_mints() {
        let (root, daemon) = test_daemon("recon-mint-unlisted");
        let seller = daemon.seller_pubkey().to_owned();
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "job-recon-mint";
        let unlisted = "https://unlisted-mint.example/";
        // creq authors the unlisted mint (so it binds the payload), but config accepted_mints only
        // holds DEFAULT_MINT_URL → the allow-list guard refuses.
        let creq =
            gateway::creq::build_seller_creq(job_id, 15, "sat", &[unlisted.into()], &seller)
                .expect("build creq");
        let view = recon_job_view(job_id, 15, &buyer_hex, &seller, &seller, Some(creq));
        let received = recon_received(job_id, unlisted, &buyer);

        let err = daemon
            .reconstruct_delivered_bind(&view, &received)
            .expect_err("unlisted mint must refuse");
        assert!(
            err.contains("not in seller accepted_mints"),
            "reason: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_release_error_draft_carries_machine_readable_content() {
        let content = reconcile_error_content(ClaimLiveness::Expired);
        let draft = error_draft("orphan-job", "buyer-pk", "seller-pk", content);
        assert_error_content_starts_with("claim_released", &draft.content);
    }

    // window == 0 reproduces the pre-backfill subscription shape byte-identically PER FILTER:
    // targeted = ORIGINAL no-since/no-limit (full targeted-history backfill, always the
    // behavior); untargeted = `since(now) + limit(0)` (live-only, zero stored offers).
    #[test]
    fn window_zero_reproduces_live_only_filter_shape() {
        use nostr_sdk::prelude::{Keys, Timestamp};
        let seller = Keys::generate();
        let now = Timestamp::now();
        let filters = offer_subscription_filters(seller.public_key(), true, now, 0);
        assert_eq!(filters.len(), 2, "open-pool ⇒ targeted + untargeted filter");
        // Targeted: ORIGINAL shape — no time bound, no stored-offer cap.
        assert_eq!(filters[0].since, None, "window=0 targeted must carry NO since (original shape)");
        assert_eq!(filters[0].limit, None, "window=0 targeted must carry NO limit (original shape)");
        // Untargeted: since(now) + limit(0) — the byte-identical live-only shape.
        assert_eq!(filters[1].since, Some(now), "window=0 untargeted must be since(now)");
        assert_eq!(
            filters[1].limit,
            Some(0),
            "window=0 untargeted must keep limit(0) (request ZERO stored offers)"
        );
    }

    // window > 0 bounds + caps the UNTARGETED (open-pool) filter ONLY: `since(now - window)`
    // with the flood cap replacing limit(0). The TARGETED filter keeps its original
    // no-since/no-limit shape at ALL window values (the field gap was
    // open-pool; bounding the p-pinned filter would be a pure regression — the deadline-expiry
    // refusal in classify_offer is the staleness guard on both paths).
    #[test]
    fn positive_window_bounds_and_caps_untargeted_filter_only() {
        use nostr_sdk::prelude::{Keys, Timestamp};
        let seller = Keys::generate();
        let now = Timestamp::now();
        let window = 1200u64;
        let expected_since = Timestamp::from(now.as_secs() - window);
        let filters = offer_subscription_filters(seller.public_key(), true, now, window);
        assert_eq!(filters.len(), 2);
        // Targeted: shape UNCHANGED by the window knob.
        assert_eq!(filters[0].since, None, "targeted must carry NO since at any window value");
        assert_eq!(filters[0].limit, None, "targeted must carry NO limit at any window value");
        // Untargeted: since(now - window) + flood cap; limit(0) dropped.
        assert_eq!(filters[1].since, Some(expected_since), "untargeted since must be now-window");
        assert_eq!(
            filters[1].limit,
            Some(OFFER_BACKFILL_LIMIT),
            "untargeted must DROP limit(0) for the flood cap when window > 0"
        );
    }

    #[cfg(not(feature = "acp"))]
    #[tokio::test]
    async fn agent_run_fail_closed_without_acp_feature() {
        let identity = seller_git::DeliveryAgentIdentity::for_seller(&"aa".repeat(32));
        let err = run_agent_job(
            &["echo".into()],
            "task",
            Path::new("."),
            &identity,
            Duration::from_secs(1),
        )
        .await
        .expect_err("acp required");
        assert!(matches!(err, DaemonError::AcpRequired));
        assert!(err.to_string().contains("acp"));
    }

    // ── Layer-0 episode capture ────────────────────────────────────────────────────

    /// A delivered→paid job appends exactly ONE episode line with
    /// `outcome=delivered_paid`, populated `result_id`/`commit_oid`/`amount_received` and a
    /// `transcript_ref` pointing at an on-disk `seller-run.jsonl`. `record_paid_episode` is the
    /// exact writer `try_apply_payment` invokes after journaling the receipt — reverting its body
    /// drops the line (line-count 1→0), the red-on-revert for the paid writer.
    #[test]
    fn paid_episode_writer_appends_one_complete_delivered_paid_line() {
        let (root, daemon) = test_daemon("ep-paid");
        // A real on-disk transcript at the pointed path (run_agent_job writes this in production).
        let transcript_rel = "seller-jobs/jobpaid/seller-run.jsonl";
        let transcript_abs = daemon.home().root.join(transcript_rel);
        std::fs::create_dir_all(transcript_abs.parent().unwrap()).expect("mkdir jobdir");
        std::fs::write(&transcript_abs, b"{\"event\":\"stub\"}\n").expect("write transcript");

        let mut job = active_job(
            "jobpaid",
            daemon.seller_pubkey(),
            Some("res-xyz"),
            2_000_000_000,
            &root,
        );
        job.delivery = Some(DeliveryRecord {
            result_id: "res-xyz".into(),
            commit_oid: "c".repeat(40),
            git_remote: "https://example.invalid/repo.git".into(),
            branch: "mobee/jobpaid".into(),
            delivery_kind: "fork".into(),
            harness: "claude-agent-acp".into(),
            wall_time_ms: 4242,
            usage: None,
            transcript_ref: transcript_rel.into(),
            deliver_ts: 111,
        });

        daemon.record_paid_episode(&job, 21, 21);

        let log = crate::episode::EpisodeLog::open(&daemon.home().root);
        let entries = log.entries().expect("entries");
        assert_eq!(entries.len(), 1, "exactly one episode line for a paid job");
        let episode = &entries[0];
        assert_eq!(episode.outcome, EpisodeOutcome::DeliveredPaid);
        assert_eq!(episode.episode_kind, EpisodeKind::Claimed);
        assert_eq!(episode.result_id.as_deref(), Some("res-xyz"));
        assert_eq!(episode.commit_oid.as_deref(), Some("c".repeat(40).as_str()));
        assert_eq!(episode.amount_received, Some(21));
        assert_eq!(episode.expected_amount, Some(21));
        assert_eq!(episode.transcript_ref.as_deref(), Some(transcript_rel));
        // The pointer resolves to a real file under MOBEE_HOME (pointer, never a copy).
        assert!(
            daemon.home().root.join(episode.transcript_ref.as_ref().unwrap()).is_file(),
            "transcript_ref must point at an on-disk seller-run.jsonl"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A refused offer appends exactly one `episode_kind=refused` episode
    /// with a non-empty `refusal_reason_code` matching the `OfferSkip` variant, AND the money-path
    /// `seller-journal.jsonl` is byte-unchanged (episodes are a separate stream) and still parses
    /// green. Drives the real daemon writer via `on_offer_event` → reverting the
    /// `record_refused_episode` call drops the line (red-on-revert for the refused writer).
    #[tokio::test]
    async fn refused_offer_appends_one_refused_episode_and_leaves_journal_untouched() {
        let (root, mut daemon) = test_daemon("ep-refused");
        let journal_before = std::fs::read(daemon.journal.path()).unwrap_or_default();

        // Untargeted offer + claim_open_pool=false ⇒ RateGate refusal on a NON-self target, so
        // the skip path does no relay I/O (publish_under_rate_error_if_targeted early-returns).
        let buyer = nostr_sdk::Keys::generate();
        let draft = crate::gateway::OfferDraft::untargeted(
            "do a task",
            "text/plain",
            10,
            now_unix() + 3_600,
        )
        .to_event_draft();
        let event = gateway::nostr::event_builder(&draft)
            .expect("event builder")
            .sign_with_keys(&buyer)
            .expect("sign offer");

        let claimed = daemon.on_offer_event(&event).await.expect("skip is Ok");
        assert!(claimed.is_none(), "untargeted offer must be refused, not claimed");

        let log = crate::episode::EpisodeLog::open(&daemon.home().root);
        let entries = log.entries().expect("entries");
        assert_eq!(entries.len(), 1, "exactly one refused episode");
        assert_eq!(entries[0].episode_kind, EpisodeKind::Refused);
        assert_eq!(entries[0].outcome, EpisodeOutcome::Refused);
        assert_eq!(entries[0].refusal_reason_code.as_deref(), Some("RateGate"));
        assert!(
            !entries[0].refusal_reason.as_deref().unwrap_or("").is_empty(),
            "refusal_reason is never empty"
        );
        assert_eq!(entries[0].job_id, event.id.to_hex());

        // Money-safety: the journal is a SEPARATE, untouched stream.
        let journal_after = std::fs::read(daemon.journal.path()).unwrap_or_default();
        assert_eq!(
            journal_before, journal_after,
            "seller-journal.jsonl must be byte-unchanged by episode capture"
        );
        daemon.journal.entries().expect("journal still parses green");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Layer-1 read-on-start ──────────────────────────────────────────────────────

    /// With memory DISABLED the composed prompt is byte-identical to the memory-disabled
    /// golden. The expected string is a hardcoded literal (NOT recomputed from the function
    /// under test), so any drift in the disabled path fails this golden.
    #[test]
    fn composed_prompt_disabled_memory_is_byte_identical_golden() {
        let remote = "https://relay.example/git/abc.git";
        let expected = "build a widget\n\n\
---\n\
DELIVERY (required). Deliver your work by committing it with git in your current working directory:\n\
- Make one or more non-empty commits authored by you. Do not leave the deliverable uncommitted and do not only print it to the console.\n\
- You do NOT need to push and you are NOT handed any credentials: the daemon pushes your committed branch to the bound git remote (https://relay.example/git/abc.git) on your behalf.\n\
Anything not committed to git will not be delivered.";
        assert_eq!(
            compose_agent_prompt("build a widget", remote, None),
            expected,
            "disabled-memory prompt must be byte-identical to the golden"
        );
    }

    /// memory_enabled=false ⇒ the daemon produces NO read-on-start section
    /// and does not even create the memory dir.
    #[tokio::test]
    async fn disabled_memory_yields_no_section_and_no_dir() {
        let (root, mut daemon) = test_daemon("mem-disabled");
        daemon.home.config.seller_memory.memory_enabled = false;
        assert!(daemon.read_on_start_section().is_none(), "no section when disabled");
        assert!(
            !crate::seller_memory::memory_dir(&daemon.home().root).exists(),
            "disabled memory must not create the memory dir"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With memory enabled and a non-empty index, the composed prompt
    /// contains the index text and the absolute memory path; and first use seeds operator-notes.md
    /// stamped author: operator.
    #[tokio::test]
    async fn enabled_memory_inlines_index_and_seeds_operator_notes() {
        let (root, daemon) = test_daemon("mem-enabled");
        // default config ⇒ memory_enabled = true.
        let section = daemon.read_on_start_section().expect("section present");
        let dir = crate::seller_memory::memory_dir(&daemon.home().root);
        assert!(section.contains("Seller memory index"), "index text inlined: {section}");
        assert!(
            section.contains(&dir.display().to_string()),
            "absolute memory path named: {section}"
        );
        // The full composed prompt is a superset of the disabled output.
        let prompt = compose_agent_prompt("t", "https://r/git/x.git", Some(&section));
        assert!(prompt.contains("Seller memory index"));
        assert!(prompt.starts_with("t\n\n---\nDELIVERY"), "delivery block preserved");

        // On first creation the dir carries operator-notes.md stamped author: operator.
        let notes = dir.join(crate::seller_memory::OPERATOR_NOTES_FILE);
        assert!(notes.is_file(), "operator-notes.md seeded on first read-on-start");
        let author = crate::seller_memory::frontmatter_author(
            &std::fs::read_to_string(&notes).expect("read notes"),
        );
        assert_eq!(author.as_deref(), Some(crate::seller_memory::AUTHOR_OPERATOR));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An over-bound `MEMORY.md` is refused at the injection site: the daemon gets no
    /// read-on-start section (the job runs memoryless) instead of a bloated prompt. Goes red if
    /// the size check in `seller_memory::read_on_start_section` is removed.
    #[tokio::test]
    async fn oversized_index_is_refused_not_injected() {
        let (root, daemon) = test_daemon("mem-overbound");
        let dir = crate::seller_memory::memory_dir(&daemon.home().root);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join(crate::seller_memory::MEMORY_INDEX_FILE),
            "z".repeat(crate::seller_memory::MAX_MEMORY_INDEX_BYTES + 1),
        )
        .expect("write oversized index");
        assert!(
            daemon.read_on_start_section().is_none(),
            "over-bound index must be refused, never inlined into the prompt"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The read-on-start template seam overrides the in-repo default when
    /// `read_on_start_template_path` is set (daemon path).
    #[tokio::test]
    async fn read_on_start_template_seam_used_by_daemon() {
        let (root, mut daemon) = test_daemon("mem-seam");
        let template = daemon.home().root.join("my-read.tmpl");
        std::fs::write(&template, "CUSTOM-FRAME {memory_index}").expect("write template");
        daemon.home.config.seller_memory.read_on_start_template_path = Some(template);
        let section = daemon.read_on_start_section().expect("section");
        assert!(section.starts_with("CUSTOM-FRAME"), "operator template used: {section}");
        assert!(!section.contains("SELLER MEMORY"), "default framing not used");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A not-an-offer / dedup re-see does not produce an episode (only freshly-classified jobs do).
    #[test]
    fn refused_episode_skips_non_offer_and_already_processed() {
        let (root, daemon) = test_daemon("ep-refused-skip");
        let event = offer_event(&nostr_sdk::Keys::generate(), daemon.seller_pubkey(), 5, now_unix() + 3_600);
        daemon.record_refused_episode(&event, &OfferSkip::NotAnOffer { kind: 1 });
        daemon.record_refused_episode(&event, &OfferSkip::AlreadyProcessed);
        let log = crate::episode::EpisodeLog::open(&daemon.home().root);
        assert!(log.entries().expect("entries").is_empty(), "no episode for non-job skips");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Retro write-back ───────────────────────────────────────────────────────────

    /// A test Driver that simulates a misbehaving retro agent: on the prompt turn it CLOBBERS a
    /// target (operator) file, then completes. Proves merge-not-clobber is enforced at runtime
    /// (not by prompt prose): `run_retro_turn` must byte-revert the file afterward.
    struct ClobberingDriver {
        target: PathBuf,
        clobber_with: Vec<u8>,
    }

    impl crate::driver::Driver for ClobberingDriver {
        fn id(&self) -> crate::event::RuntimeId {
            crate::event::RuntimeId("clobber".into())
        }
        async fn ready(&mut self) -> Result<crate::driver::Readiness, crate::driver::DriverError> {
            Ok(crate::driver::Readiness {
                runtime_id: self.id(),
                protocol_version: crate::driver::acp::PROTOCOL_VERSION,
            })
        }
        async fn start_session(
            &mut self,
            _cfg: crate::driver::SessionConfig,
        ) -> Result<crate::driver::SessionId, crate::driver::DriverError> {
            Ok("clobber-session".into())
        }
        async fn prompt(
            &mut self,
            _session_id: &crate::driver::SessionId,
            _turn: crate::driver::PromptTurn,
        ) -> Result<crate::driver::UpdateStream, crate::driver::DriverError> {
            std::fs::write(&self.target, &self.clobber_with).expect("clobber write");
            Ok(crate::driver::UpdateStream::new(
                vec![crate::driver::SessionUpdate::TurnEnded(
                    crate::driver::StopReason::Completed,
                )],
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ))
        }
        async fn on_permission(
            &mut self,
            _req: crate::driver::PermissionRequest,
        ) -> crate::driver::PermissionOutcome {
            crate::driver::PermissionOutcome::Allow
        }
        async fn artifacts(
            &self,
            _session_id: &crate::driver::SessionId,
        ) -> Result<Vec<crate::driver::Artifact>, crate::driver::DriverError> {
            Ok(Vec::new())
        }
        async fn cancel(
            &mut self,
            _session_id: &crate::driver::SessionId,
        ) -> Result<(), crate::driver::DriverError> {
            Ok(())
        }
        async fn shutdown(&mut self) -> Result<(), crate::driver::DriverError> {
            Ok(())
        }
    }

    fn write_paid_episode(daemon: &SellerDaemon, job_id: &str) {
        let mut episode = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredPaid,
            7,
            daemon.seller_pubkey(),
            job_id,
        );
        episode.result_id = Some(format!("res-{job_id}"));
        episode.transcript_ref = Some(format!("seller-jobs/{job_id}/seller-run.jsonl"));
        crate::episode::EpisodeLog::open(&daemon.home().root)
            .append(&episode)
            .expect("append paid episode");
    }

    /// A completed job triggers exactly ONE extra agent turn (carrying the
    /// retro prompt) and `MEMORY.md` exists afterward. The turn's session cwd is the memory dir
    /// (set by `run_retro_turn`; the merge-not-clobber test below depends on that cwd's writes).
    #[tokio::test]
    async fn retro_turn_issues_exactly_one_turn_and_memory_index_exists() {
        use crate::driver::{MockDriver, ScriptedSession, SessionUpdate, StopReason};
        use crate::event::RuntimeId;
        let root = temp("retro-one");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        let dir = crate::seller_memory::memory_dir(&root);
        crate::seller_memory::ensure_memory_dir(&dir).expect("ensure");

        let script = ScriptedSession {
            session_id: "retro-1".into(),
            updates: vec![SessionUpdate::TurnEnded(StopReason::Completed)],
            artifacts: Vec::new(),
        };
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script]);
        let log_path = root.join("seller-retro.jsonl");
        run_retro_turn(&mut driver, &dir, "distill this job", &log_path)
            .await
            .expect("retro turn ok");

        assert_eq!(driver.prompt_history().len(), 1, "exactly one extra agent turn");
        let (_sid, turn) = &driver.prompt_history()[0];
        assert!(
            matches!(&turn.input[0], crate::driver::ContentBlock::Text { text } if text == "distill this job"),
            "the extra turn carried the retro prompt"
        );
        assert!(dir.join("MEMORY.md").is_file(), "MEMORY.md exists after retro");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With retro_enabled=false no retro is planned (no extra turn issued).
    #[test]
    fn retro_disabled_plans_nothing() {
        let (root, mut daemon) = test_daemon("retro-off");
        write_paid_episode(&daemon, "jd");
        daemon.home.config.seller_memory.retro_enabled = false;
        assert!(
            daemon.retro_context("jd").is_none(),
            "retro_enabled=false ⇒ no plan ⇒ no extra turn"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The retro plan is seeded with the episode (JSON) + the ABSOLUTE
    /// transcript path, uses the default framing (which instructs `author: agent` + never touch
    /// operator files), and honors the retro_prompt_path seam. No paid episode ⇒ no plan.
    #[test]
    fn retro_context_seeds_episode_and_honors_prompt_seam() {
        let (root, mut daemon) = test_daemon("retro-ctx");
        assert!(daemon.retro_context("absent").is_none(), "no paid episode ⇒ no plan");

        write_paid_episode(&daemon, "jp");
        let plan = daemon.retro_context("jp").expect("plan for paid job");
        assert!(plan.prompt.contains("\"job_id\": \"jp\""), "episode json seeded: {}", plan.prompt);
        assert!(plan.prompt.contains("DURABLE MEMORY"), "default retro framing");
        assert!(
            plan.prompt.contains("author: agent"),
            "prompt instructs the agent to stamp author: agent"
        );
        let transcript_abs = daemon
            .home()
            .root
            .join("seller-jobs/jp/seller-run.jsonl")
            .display()
            .to_string();
        assert!(plan.prompt.contains(&transcript_abs), "absolute transcript path seeded");

        // Seam override wins.
        let template = daemon.home().root.join("retro.tmpl");
        std::fs::write(&template, "SEAM DISTILLER for {episode_json}").expect("write template");
        daemon.home.config.seller_memory.retro_prompt_path = Some(template);
        let plan2 = daemon.retro_context("jp").expect("plan2");
        assert!(plan2.prompt.starts_with("SEAM DISTILLER for"), "uses operator retro template");
        assert!(!plan2.prompt.contains("DURABLE MEMORY"), "default framing not used");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A retro forced to fail surfaces an error the caller swallows, and the
    /// MONEY path is green — the journal (a real claim+receipt) is byte-unchanged and still parses.
    #[tokio::test]
    async fn retro_failure_leaves_money_path_green() {
        use crate::driver::MockDriver;
        use crate::event::RuntimeId;
        let (root, daemon) = test_daemon("retro-fail");
        // Money state: a real journaled claim + receipt.
        daemon.journal.append_claim("jf", "cf", "bf", 2_000_000_000).expect("claim");
        daemon
            .journal
            .append_receipt("jf", "rf", "jf", "rf", 7, 7, DEFAULT_MINT_URL, "bf", true)
            .expect("receipt");
        let journal_before = std::fs::read(daemon.journal.path()).expect("read journal");

        let dir = crate::seller_memory::memory_dir(&daemon.home().root);
        crate::seller_memory::ensure_memory_dir(&dir).expect("ensure");
        // Empty scripts ⇒ start_session ScriptExhausted ⇒ the retro turn FAILS.
        let mut driver = MockDriver::new(RuntimeId("mock".into()), Vec::new());
        let log_path = root.join("seller-retro.jsonl");
        let result = run_retro_turn(&mut driver, &dir, "distill", &log_path).await;
        assert!(result.is_err(), "forced retro failure surfaces Err (the caller swallows it)");

        assert_eq!(
            std::fs::read(daemon.journal.path()).expect("read"),
            journal_before,
            "money path GREEN: journal byte-unchanged across a failed retro"
        );
        daemon.journal.entries().expect("journal still parses green");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Merge-not-clobber across a retro RUN. An agent that clobbers an
    /// `author: operator` file mid-turn is byte-reverted by `run_retro_turn`. Reverting the
    /// restore leaves the file HIJACKED — the red-on-revert target.
    #[tokio::test]
    async fn retro_run_reverts_operator_clobber() {
        let root = temp("retro-merge");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        let dir = crate::seller_memory::memory_dir(&root);
        crate::seller_memory::ensure_memory_dir(&dir).expect("ensure");
        let notes = dir.join(crate::seller_memory::OPERATOR_NOTES_FILE);
        let notes_before = std::fs::read(&notes).expect("read notes");
        // A pre-existing operator topic file too.
        let house = dir.join("house-rules.md");
        std::fs::write(&house, "---\nauthor: operator\n---\nORIGINAL").expect("write house");

        let mut driver = ClobberingDriver {
            target: notes.clone(),
            clobber_with: b"HIJACKED BY AGENT".to_vec(),
        };
        let log_path = root.join("seller-retro.jsonl");
        run_retro_turn(&mut driver, &dir, "distill", &log_path)
            .await
            .expect("retro ok");

        assert_eq!(
            std::fs::read(&notes).expect("read"),
            notes_before,
            "operator-notes.md byte-unchanged across the retro (merge-not-clobber)"
        );
        assert_eq!(
            std::fs::read_to_string(&house).expect("read"),
            "---\nauthor: operator\n---\nORIGINAL",
            "operator topic file byte-unchanged across the retro"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── GATE-RED regression: the daemon must not go deaf to kind-1059 payments ────────────────

    /// Deaf-daemon guard: a broadcast LAG must NOT terminate the event loop.
    /// Before the fix, `while let Ok(..) = recv().await` ended the loop on `Lagged`, so a seller
    /// that fell behind during a long agent turn went silently deaf to ALL further offers and
    /// kind-1059 payments (wraps parked, never collected) until restart. Reverting the fix
    /// (Lagged ⇒ Stop) fails this test.
    #[test]
    fn lagged_recv_keeps_daemon_alive_only_closed_stops() {
        use tokio::sync::broadcast::error::RecvError;
        assert_eq!(
            classify_recv_error(&RecvError::Lagged(42)),
            RecvControl::Continue,
            "a broadcast lag must keep the daemon alive (never go deaf to payments)"
        );
        assert_eq!(
            classify_recv_error(&RecvError::Closed),
            RecvControl::Stop,
            "only a closed channel ends the loop"
        );
    }

    /// A Driver whose prompt turn BLOCKS (simulating a slow/hanging retro agent), then completes.
    struct SlowDriver {
        block: std::time::Duration,
    }

    impl crate::driver::Driver for SlowDriver {
        fn id(&self) -> crate::event::RuntimeId {
            crate::event::RuntimeId("slow".into())
        }
        async fn ready(&mut self) -> Result<crate::driver::Readiness, crate::driver::DriverError> {
            Ok(crate::driver::Readiness {
                runtime_id: self.id(),
                protocol_version: crate::driver::acp::PROTOCOL_VERSION,
            })
        }
        async fn start_session(
            &mut self,
            _cfg: crate::driver::SessionConfig,
        ) -> Result<crate::driver::SessionId, crate::driver::DriverError> {
            Ok("slow-session".into())
        }
        async fn prompt(
            &mut self,
            _session_id: &crate::driver::SessionId,
            _turn: crate::driver::PromptTurn,
        ) -> Result<crate::driver::UpdateStream, crate::driver::DriverError> {
            std::thread::sleep(self.block); // a retro agent turn that takes real time
            Ok(crate::driver::UpdateStream::new(
                vec![crate::driver::SessionUpdate::TurnEnded(
                    crate::driver::StopReason::Completed,
                )],
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ))
        }
        async fn on_permission(
            &mut self,
            _req: crate::driver::PermissionRequest,
        ) -> crate::driver::PermissionOutcome {
            crate::driver::PermissionOutcome::Allow
        }
        async fn artifacts(
            &self,
            _session_id: &crate::driver::SessionId,
        ) -> Result<Vec<crate::driver::Artifact>, crate::driver::DriverError> {
            Ok(Vec::new())
        }
        async fn cancel(
            &mut self,
            _session_id: &crate::driver::SessionId,
        ) -> Result<(), crate::driver::DriverError> {
            Ok(())
        }
        async fn shutdown(&mut self) -> Result<(), crate::driver::DriverError> {
            Ok(())
        }
    }

    /// Retro-must-not-block-the-loop guard: a retro turn runs to completion on a
    /// DETACHED thread, so the caller (the event loop) is not blocked for the retro's duration.
    /// This is exactly the pattern `maybe_run_retro` now uses (own thread + own runtime). Before
    /// the fix the retro ran inline via `.await`, blocking wrap collection for the whole turn.
    #[test]
    fn retro_turn_runs_detached_without_blocking_caller() {
        use std::time::{Duration, Instant};
        let root = temp("retro-detach");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        let dir = crate::seller_memory::memory_dir(&root);
        crate::seller_memory::ensure_memory_dir(&dir).expect("ensure");
        let log_path = root.join("seller-retro.jsonl");

        let block = Duration::from_millis(800);
        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let mut driver = SlowDriver { block };
            runtime.block_on(run_retro_turn(&mut driver, &dir, "distill", &log_path))
        });
        // The caller (loop) returns immediately — NOT blocked for the retro's ~800ms turn.
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "spawning the retro must not block the caller (elapsed {:?})",
            started.elapsed()
        );
        let result = handle.join().expect("retro thread");
        assert!(result.is_ok(), "detached retro completes: {result:?}");
        assert!(
            started.elapsed() >= block,
            "the retro really did run in the background (took its full turn)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Completion-check seam ──────────────────────────────────────────────────────────────────
    #[test]
    fn completion_check_none_is_a_pass() {
        let dir = temp("check-none");
        std::fs::create_dir_all(&dir).expect("mkdir");
        run_completion_check(&dir, None).expect("no check ⇒ pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completion_check_passing_command_delivers() {
        let dir = temp("check-pass");
        std::fs::create_dir_all(&dir).expect("mkdir");
        run_completion_check(&dir, Some("true")).expect("exit 0 ⇒ pass");
        // Blank/whitespace command is treated as no check.
        run_completion_check(&dir, Some("   ")).expect("blank ⇒ pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completion_check_failing_command_refuses_with_reason() {
        let dir = temp("check-fail");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let err = run_completion_check(&dir, Some("exit 3")).expect_err("non-zero ⇒ refuse");
        let msg = err.to_string();
        assert!(msg.contains("completion check") && msg.contains('3'), "precise reason: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completion_check_runs_in_the_job_workdir() {
        let dir = temp("check-cwd");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("marker"), "x").expect("write marker");
        // The check sees the workdir as its cwd.
        run_completion_check(&dir, Some("test -f marker")).expect("cwd is the job workdir");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delivery_message_summarizes_the_task() {
        assert_eq!(
            delivery_message("Fix the parser\n\nmore detail"),
            "mobee delivery: Fix the parser"
        );
        // Leading blank lines skipped; whitespace collapsed.
        assert_eq!(
            delivery_message("\n\n   add   retry   logic  "),
            "mobee delivery: add retry logic"
        );
        // Empty task falls back to a fixed label.
        assert_eq!(delivery_message("   \n  "), "mobee delivery");
        // Long first line is capped.
        let long = "x".repeat(200);
        let msg = delivery_message(&long);
        assert!(msg.starts_with("mobee delivery: "));
        assert_eq!(msg.len(), "mobee delivery: ".len() + 72);
    }
