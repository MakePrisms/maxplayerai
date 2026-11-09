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
        advertise_on_claim(&mut after, &off, true, &RunscOverlayGate::NotApplicable);
        assert_eq!(
            serde_json::to_string(&before).unwrap(),
            serde_json::to_string(&after).unwrap()
        );
        assert!(!claim_supports_sessions(&after.tags));
        // Enabled but not a docker container-delivery seat: still identical.
        let mut host = before.clone();
        advertise_on_claim(&mut host, &on(), false, &RunscOverlayGate::NotApplicable);
        assert_eq!(before, host);
        // Enabled on a docker seat: exactly one tag added, nothing else moves.
        let mut adv = before.clone();
        advertise_on_claim(&mut adv, &on(), true, &RunscOverlayGate::Open);
        advertise_on_claim(&mut adv, &on(), true, &RunscOverlayGate::Open);
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
                uncollected_turn: Some((2, UncollectedTurn::Settling)),
                ..ok.clone()
            },
            ReplyRefusal::TurnSettling { turn: 2 },
        ),
        (
            ReplyPreflight {
                uncollected_turn: Some((2, UncollectedTurn::CollectRequired("held".into()))),
                ..ok.clone()
            },
            ReplyRefusal::CollectFirst {
                turn: 2,
                reason: "held".into(),
            },
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
    // A turn the daemon is settling is never sent to `collect`; one it cannot settle is, with why.
    let settling = ReplyRefusal::TurnSettling { turn: 2 }.to_string();
    assert_eq!(
        settling,
        "turn 2 is still settling (the daemon settles it automatically); check get_job \
         settlement and retry reply shortly"
    );
    assert!(!settling.contains("collect"), "{settling}");
    assert_eq!(
        ReplyRefusal::CollectFirst {
            turn: 2,
            reason: "review timed out".into()
        }
        .to_string(),
        "turn 2 could not settle automatically (review timed out); run collect for turn 2, then \
         retry reply"
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

// ───────────────────────────── §5.6 runsc overlay gate ─────────────────────────────

fn runsc_json(args: &str) -> String {
    format!(
        r#"{{"io.containerd.runc.v2":{{"path":"runc"}},"runc":{{"path":"runc"}},"runsc":{{"path":"/usr/bin/runsc","runtimeArgs":{args}}}}}"#
    )
}

/// Bug 2: the gate opens ONLY for a registered `runsc` whose args leave `--overlay2=none`; every
/// other probe result closes it with a reason and the daemon.json fix.
#[test]
fn runsc_gate_opens_only_with_overlay2_none() {
    // Flag present, in every spelling runsc accepts, alone or beside other args.
    for args in [
        r#"["--overlay2=none"]"#,
        r#"["--overlay2","none"]"#,
        r#"["-overlay2=none"]"#,
        r#"["--platform=systrap","--overlay2=none"]"#,
        r#"["--overlay2=root:self","--overlay2=none"]"#,
    ] {
        assert_eq!(
            runsc_overlay_gate(true, Ok(&runsc_json(args))),
            RunscOverlayGate::Open,
            "{args}"
        );
    }
    // Flag absent, other args only, another overlay value, or overridden later: closed.
    for args in [
        "[]",
        r#"["--platform=systrap"]"#,
        r#"["--overlay2=root:memory"]"#,
        r#"["--overlay2=none","--overlay2=root:self"]"#,
    ] {
        let gate = runsc_overlay_gate(true, Ok(&runsc_json(args)));
        assert!(!gate.allows_sessions(), "{args}: {gate:?}");
    }
    // No runtimeArgs key at all (the default registration).
    let default = runsc_overlay_gate(true, Ok(r#"{"runsc":{"path":"/usr/bin/runsc"}}"#));
    let RunscOverlayGate::Closed { reason, fix, .. } = &default else {
        panic!("default runsc must close the gate: {default:?}");
    };
    assert!(reason.contains("--overlay2=none"), "{reason}");
    assert!(
        reason.contains("stopped and restarted"),
        "says why: {reason}"
    );
    assert!(
        fix.contains(r#""runsc": {"path": "/usr/bin/runsc", "runtimeArgs": ["--overlay2=none"]}"#),
        "the exact daemon.json member, runtime still named runsc: {fix}"
    );
    assert!(
        fix.contains("restart docker (this stops running containers)"),
        "{fix}"
    );
    // Other args are kept and the overlay setting is replaced, not duplicated.
    let RunscOverlayGate::Closed { fix, .. } = runsc_overlay_gate(
        true,
        Ok(&runsc_json(
            r#"["--platform=systrap","--overlay2","root:memory"]"#,
        )),
    ) else {
        panic!("closed");
    };
    assert!(
        fix.contains(r#""runtimeArgs": ["--platform=systrap", "--overlay2=none"]"#),
        "{fix}"
    );
}

/// The fix a human can act on without reading code: the one-liner with the seat's own binary path
/// and its other args, the heartbeat re-check (and the restart when the heartbeat is off), doctor,
/// and the troubleshooting guide (site + GitHub).
#[test]
fn runsc_fix_names_the_one_liner_the_recheck_and_the_guide() {
    let RunscOverlayGate::Closed { fix, .. } =
        runsc_overlay_gate(true, Ok(r#"{"runsc":{"path":"/usr/bin/runsc"}}"#))
    else {
        panic!("closed");
    };
    assert!(
        fix.starts_with("sudo -- /usr/bin/runsc install -- --overlay2=none # "),
        "the command comes first and the rest is a shell comment: {fix}"
    );
    assert!(
        !fix.contains("within 5 minutes"),
        "the re-check is the heartbeat, which can be off or slower: {fix}"
    );
    assert!(
        fix.contains("next heartbeat (every 5 minutes by default)"),
        "{fix}"
    );
    assert!(
        fix.contains("if the seller heartbeat is turned off, restart the seller"),
        "{fix}"
    );
    assert!(fix.contains("maxplayer doctor"), "{fix}");
    assert!(fix.contains(RUNSC_GUIDE_GITHUB_URL), "{fix}");
    assert!(fix.contains(RUNSC_GUIDE_URL), "{fix}");
    assert!(fix.contains(RUNSC_GUIDE_HEADING), "{fix}");
    assert!(
        !fix.contains('`'),
        "no backticks around a pasteable command: {fix}"
    );

    // `runsc install` rewrites the whole entry, so the other args ride along in the one-liner.
    let RunscOverlayGate::Closed { fix, .. } = runsc_overlay_gate(
        true,
        Ok(&runsc_json(
            r#"["--platform=systrap","--overlay2=root:self"]"#,
        )),
    ) else {
        panic!("closed");
    };
    assert!(
        fix.contains("install -- --platform=systrap --overlay2=none # "),
        "{fix}"
    );

    // Unknown path: the bare binary name, never a guessed path.
    let RunscOverlayGate::Closed { fix, .. } =
        runsc_overlay_gate(true, Err("docker daemon did not answer"))
    else {
        panic!("closed");
    };
    assert!(
        fix.starts_with("sudo -- runsc install -- --overlay2=none # "),
        "{fix}"
    );
}

/// The fix reads overlay args exactly as the gate does: a bare `overlay2` word (no dash) is some
/// other flag's value, not the flag, so neither it nor the arg after it is dropped.
#[test]
fn runsc_fix_strips_overlay_args_exactly_as_the_gate_reads_them() {
    let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    let bare = args(&["--debug-log", "overlay2", "--platform=systrap"]);
    assert!(
        !runtime_args_disable_overlay(&bare),
        "a bare word is not the flag"
    );
    let fix = runsc_daemon_json_fix(Some("/usr/bin/runsc"), &bare);
    assert!(
        fix.starts_with(
            "sudo -- /usr/bin/runsc install -- --debug-log overlay2 --platform=systrap --overlay2=none # "
        ),
        "{fix}"
    );
    assert!(
        fix.contains(
            r#""runtimeArgs": ["--debug-log", "overlay2", "--platform=systrap", "--overlay2=none"]"#
        ),
        "{fix}"
    );

    // Every dashed spelling is replaced, the separate-value form with its value.
    let dashed = args(&[
        "--overlay2",
        "root:self",
        "-overlay2=root:memory",
        "--platform=systrap",
    ]);
    let fix = runsc_daemon_json_fix(Some("/usr/bin/runsc"), &dashed);
    assert!(
        fix.starts_with("sudo -- /usr/bin/runsc install -- --platform=systrap --overlay2=none # "),
        "{fix}"
    );
}

/// The printed command, pasted into bash, hands `sudo` exactly the argv it shows: `--` before the
/// binary (a leading-dash path is not a sudo option), and spaces, quotes, `$(...)` and backticks in
/// the path or args stay literal. Run for real under bash with `sudo` replaced by a function that
/// prints its argv; a command substitution that ran would create the marker file.
#[test]
fn runsc_fix_command_pastes_into_bash_with_the_exact_argv() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("ran");
    let marker_s = marker.display().to_string();
    let cases: Vec<(String, Vec<String>)> = vec![
        ("--preserve-env".into(), vec![]),
        ("/opt/my runsc/runsc".into(), vec!["--flag=a b".into()]),
        (
            format!("/x/$(touch {marker_s})"),
            vec![format!("--a=`touch {marker_s}`")],
        ),
        ("/x/it's".into(), vec!["x';touch /dev/null;'".into()]),
        ("/usr/bin/runsc".into(), vec!["--platform=systrap".into()]),
    ];
    for (path, extra) in cases {
        let fix = runsc_daemon_json_fix(Some(&path), &extra);
        let script = format!("sudo() {{ printf '%s\\n' \"$@\"; }}\n{fix}\n");
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("bash runs");
        assert!(out.status.success(), "{fix}: {out:?}");
        let argv: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let mut want = vec!["--".to_owned(), path.clone(), "install".into(), "--".into()];
        want.extend(extra.iter().cloned());
        want.push(RUNSC_OVERLAY_NONE.to_owned());
        assert_eq!(argv, want, "{fix}");
        assert!(!marker.exists(), "a substitution in the fix ran: {fix}");
    }
}

/// The guide section the fix links to exists under the exact heading, and the GitHub anchor is that
/// heading's slug — so a renamed heading fails here instead of shipping a dead link in a binary.
#[test]
fn runsc_guide_heading_exists_and_matches_the_github_anchor() {
    let guide = include_str!("../../../../web/app/.well-known/skills/debug-selling/skill.md");
    let heading = format!("## {RUNSC_GUIDE_HEADING}");
    assert!(
        guide.lines().any(|line| line == heading),
        "debug-selling guide lacks `{heading}`"
    );
    let slug: String = RUNSC_GUIDE_HEADING
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_ascii_alphanumeric() || c == '-' => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(
        RUNSC_GUIDE_GITHUB_URL,
        format!(
            "https://github.com/MakePrisms/maxplayerai/blob/main/web/app/.well-known/skills/debug-selling/skill.md#{slug}"
        )
    );
    assert_eq!(
        RUNSC_GUIDE_URL,
        "https://www.maxplayer.ai/.well-known/skills/debug-selling/skill.md"
    );
}

/// Unreadable, unparseable and unregistered all fail closed — never open by default.
#[test]
fn runsc_gate_fails_closed_when_docker_cannot_confirm() {
    for (label, probe) in [
        ("unreadable", Err("docker daemon did not answer")),
        ("unparseable", Ok("not json")),
        ("empty", Ok("")),
        ("unregistered", Ok(r#"{"runc":{"path":"runc"}}"#)),
        ("not an object", Ok(r#"{"runsc":"x"}"#)),
    ] {
        let gate = runsc_overlay_gate(true, probe);
        let RunscOverlayGate::Closed { reason, fix, .. } = &gate else {
            panic!("{label} must close the gate: {gate:?}");
        };
        assert!(
            !reason.is_empty() && fix.contains("--overlay2=none"),
            "{label}: {gate:?}"
        );
    }
    let RunscOverlayGate::Closed { reason, fix, .. } =
        runsc_overlay_gate(true, Err("docker daemon did not answer"))
    else {
        panic!("closed");
    };
    assert!(
        reason.contains("docker daemon did not answer"),
        "carries the cause: {reason}"
    );
    assert!(
        fix.contains(r#""path": "<runsc path>""#),
        "unknown path is named, not guessed: {fix}"
    );
}

/// A closed gate says whether docker ANSWERED and showed the seat cannot keep a session
/// (`misconfigured`, doctor FAILs and the seller does not boot) or could not confirm either way
/// (doctor WARNs). A hiccup must never read as a misconfiguration, and a missing flag never as one.
#[test]
fn runsc_gate_tells_a_missing_flag_from_an_unconfirmed_one() {
    let misconfigured = |probe: Result<&str, &str>| match runsc_overlay_gate(true, probe) {
        RunscOverlayGate::Closed { misconfigured, .. } => Some(misconfigured),
        _ => None,
    };
    for (label, probe) in [
        (
            "default registration",
            Ok(r#"{"runsc":{"path":"/usr/bin/runsc"}}"#),
        ),
        (
            "other args only",
            Ok(r#"{"runsc":{"path":"/usr/bin/runsc","runtimeArgs":["--platform=systrap"]}}"#),
        ),
        (
            "another overlay value",
            Ok(r#"{"runsc":{"path":"/usr/bin/runsc","runtimeArgs":["--overlay2=root:memory"]}}"#),
        ),
        (
            "overridden later",
            Ok(
                r#"{"runsc":{"path":"/usr/bin/runsc","runtimeArgs":["--overlay2=none","--overlay2=root:self"]}}"#,
            ),
        ),
        ("no runsc registered", Ok(r#"{"runc":{"path":"runc"}}"#)),
    ] {
        assert_eq!(misconfigured(probe), Some(true), "{label}");
    }
    for (label, probe) in [
        ("unreadable", Err("docker daemon did not answer")),
        ("timed out", Err("docker info timed out after 10s")),
        ("unparseable", Ok("not json")),
        ("empty", Ok("")),
        ("not an object", Ok("[]")),
        ("runsc entry not an object", Ok(r#"{"runsc":"x"}"#)),
    ] {
        assert_eq!(misconfigured(probe), Some(false), "{label}");
    }
    assert_eq!(
        misconfigured(Ok(
            r#"{"runsc":{"path":"/usr/bin/runsc","runtimeArgs":["--overlay2=none"]}}"#
        )),
        None,
        "the flag opens the gate"
    );
}

/// The gate is scoped: a seat that is not docker + runsc + sessions is never gated, whatever docker
/// would say — including a probe that would close it.
#[test]
fn runsc_gate_leaves_other_seats_alone() {
    let runsc = SandboxConfig {
        mode: SandboxMode::Docker,
        runtime: Some("runsc".into()),
        ..Default::default()
    };
    assert!(runsc_overlay_gate_applies(Some(&runsc), &on()));
    let padded = SandboxConfig {
        runtime: Some(" runsc ".into()),
        ..runsc.clone()
    };
    assert!(
        runsc_overlay_gate_applies(Some(&padded), &on()),
        "trimmed like the seat resolves it"
    );
    let runc = SandboxConfig {
        runtime: None,
        ..runsc.clone()
    };
    let other = SandboxConfig {
        runtime: Some("sysbox-runc".into()),
        ..runsc.clone()
    };
    let launcher = SandboxConfig {
        mode: SandboxMode::Launcher,
        ..runsc.clone()
    };
    for (label, sandbox, sessions) in [
        ("default runtime", Some(&runc), on()),
        ("non-runsc runtime", Some(&other), on()),
        ("launcher mode", Some(&launcher), on()),
        ("no sandbox", None, on()),
        ("sessions off", Some(&runsc), SessionsConfig::default()),
    ] {
        let applies = runsc_overlay_gate_applies(sandbox, &sessions);
        assert!(!applies, "{label}");
        let gate = runsc_overlay_gate(applies, Err("would close"));
        assert_eq!(gate, RunscOverlayGate::NotApplicable, "{label}");
        assert!(
            gate.allows_sessions(),
            "{label}: an unaffected seat keeps advertising"
        );
    }
}

/// A closed gate keeps `["session","1"]` off the claim; an open or inapplicable one does not.
#[test]
fn closed_runsc_gate_keeps_the_session_tag_off_the_claim() {
    let before = gateway::claim_draft(
        OFFER,
        BUYER,
        SELLER,
        ClaimPayment::None,
        &["claude".into()],
        &Default::default(),
    );
    let closed = runsc_overlay_gate(true, Ok(r#"{"runsc":{"path":"/usr/bin/runsc"}}"#));
    let mut claim = before.clone();
    advertise_on_claim(&mut claim, &on(), true, &closed);
    assert_eq!(
        claim, before,
        "closed: the claim bytes are a single-turn claim's"
    );
    for open in [RunscOverlayGate::Open, RunscOverlayGate::NotApplicable] {
        let mut claim = before.clone();
        advertise_on_claim(&mut claim, &on(), true, &open);
        assert!(claim_supports_sessions(&claim.tags), "{open:?}");
    }
}

/// A gate that closed between claim and execution runs the job single-turn (never parks).
#[test]
fn closed_runsc_gate_runs_an_advertised_job_single_turn() {
    let closed = runsc_overlay_gate(true, Err("docker daemon did not answer"));
    assert!(
        !runs_as_session(&on(), &closed, true),
        "closed gate: no park"
    );
    assert!(runs_as_session(&on(), &RunscOverlayGate::Open, true));
    assert!(runs_as_session(
        &on(),
        &RunscOverlayGate::NotApplicable,
        true
    ));
    assert!(
        !runs_as_session(&on(), &RunscOverlayGate::Open, false),
        "not advertised"
    );
    assert!(!runs_as_session(
        &SessionsConfig::default(),
        &RunscOverlayGate::Open,
        true
    ));
}

// ───────────── Non-runsc seats are unchanged (Petar, 15:33): no probe, no log, main's claims ─────────────

/// Every seat whose `[sandbox] runtime` does not trim to exactly `runsc` — unset (runc), another
/// name, launcher mode, no sandbox — gets `NotApplicable` WITHOUT the docker probe being called
/// (the stub panics), logs nothing, advertises sessions exactly as on main, and runs advertised jobs
/// as sessions exactly as on main.
#[tokio::test]
async fn non_runsc_seats_never_probe_and_advertise_as_on_main() {
    let docker = SandboxConfig {
        mode: SandboxMode::Docker,
        ..Default::default()
    };
    let seats: Vec<(&str, Option<SandboxConfig>)> = vec![
        (
            "runtime unset (runc default)",
            Some(SandboxConfig {
                runtime: None,
                ..docker.clone()
            }),
        ),
        (
            "runtime runc",
            Some(SandboxConfig {
                runtime: Some("runc".into()),
                ..docker.clone()
            }),
        ),
        (
            "runtime sysbox-runc",
            Some(SandboxConfig {
                runtime: Some("sysbox-runc".into()),
                ..docker.clone()
            }),
        ),
        (
            "runtime kata",
            Some(SandboxConfig {
                runtime: Some("kata-runtime".into()),
                ..docker.clone()
            }),
        ),
        (
            "runtime RUNSC (case differs)",
            Some(SandboxConfig {
                runtime: Some("RUNSC".into()),
                ..docker.clone()
            }),
        ),
        (
            "runtime runsc-debug (other name)",
            Some(SandboxConfig {
                runtime: Some("runsc-debug".into()),
                ..docker.clone()
            }),
        ),
        (
            "runtime empty",
            Some(SandboxConfig {
                runtime: Some("  ".into()),
                ..docker.clone()
            }),
        ),
        (
            "launcher mode, runtime runsc",
            Some(SandboxConfig {
                mode: SandboxMode::Launcher,
                runtime: Some("runsc".into()),
                ..Default::default()
            }),
        ),
        ("no sandbox", None),
    ];
    let before = gateway::claim_draft(
        OFFER,
        BUYER,
        SELLER,
        ClaimPayment::None,
        &["claude".into()],
        &Default::default(),
    );
    // main's claim on an enabled docker container-delivery seat: exactly the session tag added.
    let mut on_main = before.clone();
    advertise_on_claim(&mut on_main, &on(), true, &RunscOverlayGate::NotApplicable);
    assert!(claim_supports_sessions(&on_main.tags));
    for (label, sandbox) in seats {
        assert!(!seat_runs_runsc(sandbox.as_ref()), "{label}");
        let gate = resolve_runsc_gate(sandbox.as_ref(), &on(), || async {
            panic!("{label}: a non-runsc seat must never call docker info")
        })
        .await;
        assert_eq!(gate, RunscOverlayGate::NotApplicable, "{label}");
        assert_eq!(runsc_gate_log_line(&gate), None, "{label}: no new log line");
        let mut claim = before.clone();
        advertise_on_claim(&mut claim, &on(), true, &gate);
        assert_eq!(claim, on_main, "{label}: claim bytes exactly as on main");
        assert!(
            runs_as_session(&on(), &gate, true),
            "{label}: still runs as a session"
        );
    }
}

/// Positive control for the probe stub: a runsc seat with sessions on DOES call it exactly once,
/// and logs one line; with sessions off it does not.
#[tokio::test]
async fn runsc_seat_probes_once_only_with_sessions_on() {
    let runsc = SandboxConfig {
        mode: SandboxMode::Docker,
        runtime: Some(" runsc ".into()),
        ..Default::default()
    };
    let calls = std::cell::Cell::new(0);
    let gate = resolve_runsc_gate(Some(&runsc), &on(), || async {
        calls.set(calls.get() + 1);
        Ok(r#"{"runsc":{"path":"/usr/bin/runsc","runtimeArgs":["--overlay2=none"]}}"#.to_owned())
    })
    .await;
    assert_eq!(calls.get(), 1);
    assert_eq!(gate, RunscOverlayGate::Open);
    assert!(runsc_gate_log_line(&gate).is_some());
    let off = resolve_runsc_gate(Some(&runsc), &SessionsConfig::default(), || async {
        panic!("sessions off: no probe")
    })
    .await;
    assert_eq!(off, RunscOverlayGate::NotApplicable);
}
