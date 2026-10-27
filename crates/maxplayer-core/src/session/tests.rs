//! Failure tests for the session invariants named in `docs/specs/session-jobs.md` §11. Each test
//! names the spec failure-test number it covers.
use super::*;
use crate::gateway::{self, ClaimPayment};

const OFFER: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const RESULT1: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const FOLLOWUP: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const BUYER: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const SELLER: &str = "5555555555555555555555555555555555555555555555555555555555555555";
const OTHER: &str = "6666666666666666666666666666666666666666666666666666666666666666";

fn turn2() -> FollowUp {
    FollowUp {
        offer_id: OFFER.into(),
        seller_pubkey: SELLER.into(),
        status: FollowUpStatus::Turn,
        prev_result_id: Some(RESULT1.into()),
        turn: Some(2),
        deadline_unix: Some(1_800_000_000),
        text: Some("now add tests".into()),
    }
}
fn on() -> SessionsConfig {
    SessionsConfig {
        enabled: true,
        ..SessionsConfig::default()
    }
}

#[test]
fn followup_round_trips_and_is_kind_3412() {
    let d = followup_draft(&turn2()).unwrap();
    assert_eq!(d.kind, 3412);
    assert_eq!(parse_followup(&d).unwrap(), turn2());
    let close = FollowUp {
        status: FollowUpStatus::Close,
        prev_result_id: None,
        turn: None,
        deadline_unix: None,
        text: None,
        ..turn2()
    };
    assert_eq!(
        parse_followup(&followup_draft(&close).unwrap()).unwrap(),
        close
    );
}

#[test]
fn followup_shape_refusals() {
    for bad in [
        FollowUp {
            turn: Some(1),
            ..turn2()
        },
        FollowUp {
            turn: None,
            ..turn2()
        },
        FollowUp {
            prev_result_id: None,
            ..turn2()
        },
        FollowUp {
            deadline_unix: None,
            ..turn2()
        },
        FollowUp {
            status: FollowUpStatus::Close,
            ..turn2()
        },
    ] {
        assert!(followup_draft(&bad).is_err(), "{bad:?}");
    }
    let mut d = followup_draft(&turn2()).unwrap();
    d.tags.push(TagSpec::new(["turn", "3"]));
    assert!(
        parse_followup(&d).is_err(),
        "duplicate turn refused, not resolved by position"
    );
    let mut d = followup_draft(&turn2()).unwrap();
    d.kind = 3410;
    assert_eq!(parse_followup(&d), Err(SessionError::WrongKind(3410)));
}

/// Failure test 1.
#[test]
fn followup_from_wrong_author_seat_or_turn_is_ignored() {
    let f = turn2();
    assert_eq!(
        admit_followup(&f, BUYER, BUYER, SELLER, OFFER, 1, true),
        Ok(())
    );
    assert_eq!(
        admit_followup(&f, OTHER, BUYER, SELLER, OFFER, 1, true),
        Err(Ignore::NotTheAwardBuyer)
    );
    assert_eq!(
        admit_followup(&f, BUYER, BUYER, OTHER, OFFER, 1, true),
        Err(Ignore::NotThisSeat)
    );
    assert_eq!(
        admit_followup(&f, BUYER, BUYER, SELLER, OTHER, 1, true),
        Err(Ignore::OtherJob)
    );
    // Duplicate (turn 2 again after turn 2 delivered) and skipped (turn 4 after 2).
    assert_eq!(
        admit_followup(&f, BUYER, BUYER, SELLER, OFFER, 2, true),
        Err(Ignore::TurnOutOfOrder {
            expected: 3,
            got: 2
        })
    );
    let skip = FollowUp {
        turn: Some(4),
        ..turn2()
    };
    assert_eq!(
        admit_followup(&skip, BUYER, BUYER, SELLER, OFFER, 2, true),
        Err(Ignore::TurnOutOfOrder {
            expected: 3,
            got: 4
        })
    );
    assert_eq!(
        admit_followup(&f, BUYER, BUYER, SELLER, OFFER, 1, false),
        Err(Ignore::SessionEnded)
    );
}

/// Failure test 13 (golden): with `[sessions] enabled = false` every claim/result/accept byte is
/// today's. `advertise_on_claim` is the only path a `session` tag enters a claim.
#[test]
fn disabled_sessions_leave_claim_bytes_identical() {
    let off = SessionsConfig::default();
    assert!(!off.enabled);
    for payment in [ClaimPayment::Sat("creqAtest"), ClaimPayment::None] {
        let before = gateway::claim_draft(
            OFFER,
            BUYER,
            SELLER,
            payment,
            &["claude".into()],
            &Default::default(),
        );
        let mut after = before.clone();
        advertise_on_claim(&mut after, &off, true);
        assert_eq!(
            serde_json::to_string(&before).unwrap(),
            serde_json::to_string(&after).unwrap()
        );
        assert!(!claim_supports_sessions(&after.tags));
        // Enabled but not a docker container-delivery seat: still identical.
        let mut host = before.clone();
        advertise_on_claim(&mut host, &on(), false);
        assert_eq!(before, host);
        // Enabled on a docker seat: exactly one tag added, nothing else moves.
        let mut adv = before.clone();
        advertise_on_claim(&mut adv, &on(), true);
        advertise_on_claim(&mut adv, &on(), true);
        assert!(claim_supports_sessions(&adv.tags));
        let mut stripped = adv.clone();
        stripped.tags.retain(|t| t.first() != Some(SESSION_TAG));
        assert_eq!(stripped, before);
    }
    // A first-turn `done` result without reset carries no session tags, and the default
    // `[sessions]` table is not written into a config.
    let result = gateway::inline_result_draft(OFFER, BUYER, "text/plain", 10, "hh", "ss", "a", &[]);
    let mut same = result.clone();
    append_result_tags(&mut same, None, TurnStatus::Done, false, None);
    assert_eq!(result, same);
    let rendered = toml::to_string(&crate::home::MaxplayerConfig::default()).unwrap();
    assert!(
        !rendered.contains("sessions"),
        "default [sessions] must not be serialized"
    );
    // Accept binding never applies to a non-accept draft.
    let mut claim = gateway::claim_draft(
        OFFER,
        BUYER,
        SELLER,
        ClaimPayment::None,
        &[],
        &Default::default(),
    );
    let before = claim.clone();
    bind_accept_to_result(&mut claim, RESULT1);
    assert_eq!(claim, before);
}

#[test]
fn claim_session_tag_is_fail_closed() {
    let tags = |rows: &[&[&str]]| -> Vec<TagSpec> {
        rows.iter()
            .map(|r| TagSpec(r.iter().map(|s| s.to_string()).collect()))
            .collect()
    };
    assert!(claim_supports_sessions(&tags(&[&["session", "1"]])));
    assert!(!claim_supports_sessions(&tags(&[])));
    assert!(!claim_supports_sessions(&tags(&[&["session", "2"]])));
    assert!(!claim_supports_sessions(&tags(&[&["session", "1", "x"]])));
    assert!(!claim_supports_sessions(&tags(&[
        &["session", "1"],
        &["session", "1"]
    ])));
}

#[test]
fn result_and_accept_tags() {
    let mut r = gateway::inline_result_draft(OFFER, BUYER, "text/plain", 10, "hh", "ss", "a", &[]);
    append_result_tags(
        &mut r,
        Some(FOLLOWUP),
        TurnStatus::NeedsInput,
        true,
        Some("creqAturn"),
    );
    assert!(r.tags.iter().any(|t| t.0 == ["e", FOLLOWUP, "", "reply"]));
    assert_eq!(result_turn_status(&r.tags), Ok(TurnStatus::NeedsInput));
    assert!(result_context_reset(&r.tags));
    assert!(r.tags.iter().any(|t| t.0 == ["creq", "creqAturn"]));
    assert_eq!(r.tags.last().unwrap().first(), Some("v"), "t/v stay last");
    let bad = vec![TagSpec::new(["turn_status", "partial"])];
    assert!(result_turn_status(&bad).is_err());
    let mut a = gateway::accept_draft(OFFER, "claim", BUYER, SELLER);
    bind_accept_to_result(&mut a, RESULT1);
    assert!(a.tags.iter().any(|t| t.0 == ["e", RESULT1, "", "reply"]));
    assert_eq!(
        gateway::parse_accept(&a).unwrap().claim_id,
        "claim",
        "reply e does not displace the claim e"
    );
}

/// Failure test 9 (pure half): wrap-up timing, out_of_time delivery, and the unchanged+empty refusal.
#[test]
fn wrap_up_and_out_of_time_delivery() {
    assert_eq!(wrap_up_at(10_000, 300), 9_700);
    assert_eq!(wrap_up_at(100, 300), 0);
    assert_eq!(
        TurnStatus::from_turn(true, "anything"),
        TurnStatus::OutOfTime
    );
    assert_eq!(
        TurnStatus::from_turn(false, &format!("{NEEDS_INPUT_MARKER}\nwhich db?")),
        TurnStatus::NeedsInput
    );
    assert_eq!(TurnStatus::from_turn(false, "done"), TurnStatus::Done);
    assert_eq!(
        session_delivery_content(true, TurnStatus::OutOfTime, ""),
        Some(SessionDelivery::Git)
    );
    assert_eq!(
        session_delivery_content(false, TurnStatus::OutOfTime, "partial notes"),
        Some(SessionDelivery::Inline("partial notes".into()))
    );
    assert_eq!(
        session_delivery_content(false, TurnStatus::OutOfTime, "  "),
        None
    );
    assert_eq!(
        session_delivery_content(false, TurnStatus::Done, "unmarked"),
        None
    );
    assert!(matches!(
        session_delivery_content(
            false,
            TurnStatus::NeedsInput,
            &format!("{NEEDS_INPUT_MARKER} q")
        ),
        Some(SessionDelivery::Inline(_))
    ));
}

#[test]
fn turn_start_needs_wrap_up_push_and_start_margin() {
    let need = 300 + PUSH_MARGIN_SECS + RESUME_START_MARGIN_SECS;
    assert!(turn_can_start(1_000, 1_000 + need, 300));
    assert!(!turn_can_start(1_000, 1_000 + need - 1, 300));
}

/// Failure tests 6 and 14.
#[test]
fn restarted_or_missing_holder_and_old_orchestrator_expire() {
    let at = "2026-10-21T10:00:00.123Z";
    let running = |s: &str| HolderObservation::Running {
        started_at: s.into(),
    };
    let up = HolderNetwork::Up;
    assert_eq!(
        resume_gate(Some(at), &running(at), SESSION_PROTOCOL, &up),
        ResumeGate::Start
    );
    assert_eq!(
        resume_gate(
            Some(at),
            &running("2026-10-21T11:00:00Z"),
            SESSION_PROTOCOL,
            &up
        ),
        ResumeGate::Expire("holder_restarted")
    );
    assert_eq!(
        resume_gate(
            Some(at),
            &HolderObservation::NotRunning,
            SESSION_PROTOCOL,
            &up
        ),
        ResumeGate::Expire("holder_not_running")
    );
    assert_eq!(
        resume_gate(Some(at), &HolderObservation::Missing, SESSION_PROTOCOL, &up),
        ResumeGate::Expire("holder_missing")
    );
    assert_eq!(
        resume_gate(None, &running(at), SESSION_PROTOCOL, &up),
        ResumeGate::Expire("holder_unrecorded")
    );
    assert_eq!(
        resume_gate(Some(at), &running(at), 0, &up),
        ResumeGate::Expire("orchestrator_too_old")
    );
}

/// rc2 (runsc sweep): a running, unrestarted holder whose namespace lost its address or default
/// route, or whose probe could not answer or never ran, expires with `holder_network_lost` and the
/// turn's container never starts.
#[test]
fn holder_without_network_expires_fail_closed() {
    let at = "2026-10-21T10:00:00.123Z";
    let running = HolderObservation::Running {
        started_at: at.into(),
    };
    for network in [
        HolderNetwork::NoAddress,
        HolderNetwork::NoDefaultRoute,
        HolderNetwork::ProbeFailed("exit 125: daemon".into()),
        HolderNetwork::RestoreFailed("re-verify: tc filters missing".into()),
        HolderNetwork::NotProbed,
    ] {
        assert_eq!(
            resume_gate(Some(at), &running, SESSION_PROTOCOL, &network),
            ResumeGate::Expire("holder_network_lost"),
            "{network:?}"
        );
    }
    // A holder-state inspect that never answered is a lost network, not a missing holder, whatever
    // the network says.
    for network in [HolderNetwork::NotProbed, HolderNetwork::Up] {
        assert_eq!(
            resume_gate(
                Some(at),
                &HolderObservation::Unanswered("timed out: `docker` did not finish".into()),
                SESSION_PROTOCOL,
                &network
            ),
            ResumeGate::Expire("holder_network_lost"),
            "{network:?}"
        );
    }
    // The holder's own failures still name themselves, whatever the network says.
    assert_eq!(
        resume_gate(
            Some(at),
            &HolderObservation::Missing,
            SESSION_PROTOCOL,
            &HolderNetwork::NotProbed
        ),
        ResumeGate::Expire("holder_missing")
    );
    assert_eq!(
        resume_gate(Some(at), &running, SESSION_PROTOCOL, &HolderNetwork::Up),
        ResumeGate::Start
    );
}

/// Failure test 12 and §5.5 lifetime.
#[test]
fn eviction_is_oldest_first_and_never_running() {
    let s = |id: &str, state, at, bytes| SessionSize {
        job_id: id.into(),
        state,
        last_turn_at: at,
        disk_bytes: bytes,
    };
    let sessions = vec![
        s("running-oldest", SessionState::Running, 1, 900),
        s("b", SessionState::Parked, 20, 400),
        s("a", SessionState::Parked, 10, 400),
        s("c", SessionState::Parked, 30, 400),
        s("closed", SessionState::Closed, 0, 999),
    ];
    assert_eq!(evict_over_budget(&sessions, 1_200), Vec::<String>::new());
    assert_eq!(evict_over_budget(&sessions, 800), vec!["a".to_owned()]);
    assert_eq!(evict_over_budget(&sessions, 0), vec!["a", "b", "c"]);
    // a: 10+15=25 <= 30 expired; b: 20+15=35 > 30 still parked.
    assert_eq!(expired_by_lifetime(&sessions, 30, 15), vec!["a".to_owned()]);
    assert_eq!(
        expired_by_lifetime(&sessions, 35, 15),
        vec!["b".to_owned(), "a".to_owned()]
    );
    assert!(!expired_by_lifetime(&sessions, 1_000_000, 1).contains(&"running-oldest".to_owned()));
}

/// §7.1 as amended: every turn binds the OFFER hash (the v2 validators require it); a turn without
/// its follow-up is refused. Turn distinctness is the parent check + per-turn creq (tests 3, 10).
#[test]
fn turn_hashes_are_the_offer_hash_on_every_turn() {
    for private in [false, true] {
        let t1 = turn_job_hash(private, OFFER, "task", 1, None, 10).unwrap();
        let t2 = turn_job_hash(private, OFFER, "task", 2, Some((FOLLOWUP, "more")), 10).unwrap();
        let t3 = turn_job_hash(private, OFFER, "task", 3, Some((RESULT1, "more")), 10).unwrap();
        assert_eq!(t1, t2);
        assert_eq!(t2, t3);
        assert!(turn_job_hash(private, OFFER, "task", 2, None, 10).is_err());
    }
    assert_eq!(
        turn_job_hash(true, OFFER, "task", 1, None, 10).unwrap(),
        maxplayer_private_protocol::job_hash(OFFER).unwrap()
    );
}

#[cfg(all(feature = "wallet", feature = "gateway"))]
#[test]
fn public_turn_hash_matches_the_offer_hash_algorithm() {
    assert_eq!(
        turn_job_hash(false, OFFER, "task", 1, None, 10).unwrap(),
        crate::job_lifecycle::job_hash_for_offer(OFFER, "task", 10)
    );
    assert_eq!(
        turn_job_hash(false, OFFER, "task", 2, Some((FOLLOWUP, "more")), 10).unwrap(),
        crate::job_lifecycle::job_hash_for_offer(OFFER, "task", 10)
    );
}

#[cfg(feature = "git-delivery")]
#[test]
fn push_margin_matches_the_orchestrator() {
    assert_eq!(
        PUSH_MARGIN_SECS,
        crate::delivery_orchestrator::PUSH_MARGIN_SECS
    );
}

/// Failure test 10 (parent check).
#[test]
fn turn_commit_must_descend_from_the_previous_turn() {
    let prev = "a".repeat(40);
    assert!(verify_turn_parent(std::slice::from_ref(&prev), &prev).is_ok());
    assert!(verify_turn_parent(&["b".repeat(40)], &prev).is_err());
    assert!(verify_turn_parent(&[prev.clone(), "b".repeat(40)], &prev).is_err());
    assert!(verify_turn_parent(&[], &prev).is_err());
}

#[test]
fn branches_are_new_refs_per_turn() {
    assert_eq!(
        delivery_branch(true, OFFER, 1, None),
        format!("refs/heads/delivery/{OFFER}")
    );
    assert_eq!(
        delivery_branch(true, OFFER, 2, Some(FOLLOWUP)),
        format!("refs/heads/delivery/{FOLLOWUP}")
    );
    assert_eq!(delivery_branch(false, OFFER, 1, None), "maxplayer/11111111");
    assert_eq!(
        delivery_branch(false, OFFER, 3, Some(FOLLOWUP)),
        "maxplayer/11111111-t3"
    );
    assert_eq!(collect_dir_name("j", 1), "j");
    assert_eq!(collect_dir_name("j", 2), "j-t2");
}

/// Failure test 3: turn 2's payment maps to its own receipt key, so the turn-1 receipt does not
/// swallow it; a replayed turn-2 wrap maps to the same key (deduped by `has_receipt`).
#[test]
fn turn_two_payment_has_its_own_receipt_key() {
    assert_eq!(payment_id("job", 1, None), "job");
    assert_eq!(payment_id("job", 2, Some(FOLLOWUP)), FOLLOWUP);
    let lookup = |id: &str| (id == FOLLOWUP).then(|| (OFFER.to_owned(), 2));
    let t1 = resolve_payment(OFFER, lookup);
    let t2 = resolve_payment(FOLLOWUP, lookup);
    assert_eq!((t1.turn, t1.receipt_key.as_str()), (1, OFFER));
    assert_eq!(
        (t2.job_id.as_str(), t2.turn, t2.receipt_key.as_str()),
        (OFFER, 2, FOLLOWUP)
    );
    assert_ne!(t1.receipt_key, t2.receipt_key);
    assert_eq!(
        resolve_payment(FOLLOWUP, lookup),
        t2,
        "replay resolves to the same key"
    );
}

/// Failure test 2.
#[test]
fn unpaid_previous_turn_holds_then_drops_without_charge() {
    assert_eq!(payment_gate(false, 100, 200), PaymentGate::Wait);
    assert_eq!(payment_gate(false, 200, 200), PaymentGate::DropExpired);
    assert_eq!(payment_gate(true, 100, 200), PaymentGate::Run);
    assert_eq!(payment_gate(true, 300, 200), PaymentGate::Run);
}

/// Failure test 4: every refusal is decided from local state alone, before signing.
#[test]
fn reply_preflight_refusals() {
    let ok = ReplyPreflight {
        claim_supports_sessions: true,
        session_live: true,
        uncollected_turn: None,
        turn_in_flight: false,
        price_sats: 100,
        per_job_cap_sats: 1_000,
        session_committed_sats: 800,
        session_cap_sats: 1_000,
    };
    assert_eq!(reply_preflight(&ok), Ok(()));
    let cases = [
        (
            ReplyPreflight {
                claim_supports_sessions: false,
                ..ok.clone()
            },
            ReplyRefusal::NoSessionSupport,
        ),
        (
            ReplyPreflight {
                session_live: false,
                ..ok.clone()
            },
            ReplyRefusal::SessionEnded,
        ),
        (
            ReplyPreflight {
                uncollected_turn: Some(2),
                ..ok.clone()
            },
            ReplyRefusal::CollectFirst { turn: 2 },
        ),
        (
            ReplyPreflight {
                turn_in_flight: true,
                ..ok.clone()
            },
            ReplyRefusal::TurnInFlight,
        ),
        (
            ReplyPreflight {
                per_job_cap_sats: 99,
                ..ok.clone()
            },
            ReplyRefusal::PerJobCap {
                price: 100,
                cap: 99,
            },
        ),
        (
            ReplyPreflight {
                session_committed_sats: 901,
                ..ok.clone()
            },
            ReplyRefusal::SessionCap {
                committed: 901,
                price: 100,
                cap: 1_000,
            },
        ),
    ];
    for (p, want) in cases {
        assert_eq!(reply_preflight(&p), Err(want));
    }
    assert!(
        ReplyRefusal::CollectFirst { turn: 2 }
            .to_string()
            .starts_with("collect first")
    );
    // Free sessions: price 0 never trips a cap.
    assert_eq!(
        reply_preflight(&ReplyPreflight {
            price_sats: 0,
            session_committed_sats: 0,
            session_cap_sats: 0,
            ..ok
        }),
        Ok(())
    );
}

#[test]
fn session_cap_default_is_ten_times_price() {
    let c = SessionsConfig::default();
    assert_eq!(c.cap_for(None, 50), 500);
    assert_eq!(c.cap_for(Some(70), 50), 70);
    assert_eq!(
        c.cap_for(None, u64::MAX),
        u64::MAX,
        "saturates, never wraps"
    );
    let c = SessionsConfig {
        session_cap_sats: Some(90),
        ..c
    };
    assert_eq!(c.cap_for(None, 50), 90);
    assert_eq!(c.cap_for(Some(70), 50), 70);
}

#[test]
fn review_context_keeps_current_followup_and_summarises_oldest() {
    let small = followup_review_context(
        "task",
        &["a".into(), "b".into()],
        &[DeliveryLine {
            turn: 1,
            status: TurnStatus::Done,
            commit: Some("c1".into()),
        }],
    );
    assert!(small.contains("turn 1: done, commit c1"));
    assert!(small.contains("FOLLOW-UP turn 2:\na") && small.contains("FOLLOW-UP turn 3:\nb"));
    let big = "x".repeat(100 * 1024);
    let current = "the current follow-up".to_owned();
    let ctx = followup_review_context("task", &[big.clone(), big, current.clone()], &[]);
    assert!(ctx.len() <= REVIEW_CONTEXT_MAX_BYTES);
    assert!(ctx.contains("FOLLOW-UP turn 2: [omitted"));
    assert!(ctx.ends_with(&format!("FOLLOW-UP turn 4:\n{current}\n")));
}

/// §9: the numbered renderer (private lane) labels follow-ups by their own turn, so a trimmed
/// oldest follow-up never shifts the labels; positional numbering is its turn-2-onward special case.
#[test]
fn numbered_review_context_keeps_each_followups_own_turn() {
    let lines = [DeliveryLine {
        turn: 3,
        status: TurnStatus::OutOfTime,
        commit: Some("c3".into()),
    }];
    let numbered = followup_review_context_numbered(
        "task",
        &[(3, "third".into()), (4, "fourth".into())],
        &lines,
    );
    assert_eq!(
        numbered,
        "ORIGINAL TASK:\ntask\nturn 3: out_of_time, commit c3\nFOLLOW-UP turn 3:\nthird\nFOLLOW-UP turn 4:\nfourth\n"
    );
    let positional = followup_review_context("task", &["a".into(), "b".into()], &lines);
    assert_eq!(
        positional,
        followup_review_context_numbered("task", &[(2, "a".into()), (3, "b".into())], &lines)
    );
}

#[test]
fn reset_history_cuts_oldest_and_keeps_newest() {
    let big = "y".repeat(40 * 1024);
    let h = reset_history("task", &[big.clone(), big, "newest".into()]);
    assert!(h.len() <= RESET_HISTORY_MAX_BYTES);
    assert!(h.contains("earlier follow-up(s) omitted") && h.ends_with("newest\n"));
    assert!(prompt_session_section().contains(NEEDS_INPUT_MARKER));
}

#[test]
fn state_vocabularies_round_trip() {
    for s in [
        SessionState::Running,
        SessionState::Parked,
        SessionState::Closed,
        SessionState::Expired,
    ] {
        assert_eq!(SessionState::parse(s.as_str()), Some(s));
    }
    for s in [
        TurnState::Received,
        TurnState::WaitingPayment,
        TurnState::Reviewing,
        TurnState::Running,
        TurnState::Delivered,
        TurnState::Paid,
        TurnState::Refused,
    ] {
        assert_eq!(TurnState::parse(s.as_str()), Some(s));
    }
    for s in [
        TurnStatus::Done,
        TurnStatus::NeedsInput,
        TurnStatus::OutOfTime,
    ] {
        assert_eq!(TurnStatus::parse(s.as_str()), Some(s));
    }
}
