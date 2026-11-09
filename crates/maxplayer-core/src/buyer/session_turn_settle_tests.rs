//! The delivery watcher settling session turns ≥ 2 (docs/specs/session-jobs.md §8.4).
//!
//! Two kinds of test. The end-to-end ones drive `settle_awarded`, the watcher's own pass, against a
//! real in-process relay and the sealed `collect_turn_async` (a free inline turn that settles, and a
//! priced turn whose forged payment request is refused before anything is spent). The others swap
//! the pay step for [`FakePay`] so the watcher's own guards (turn order, the money lock, the review
//! hold) can be driven without a mint; everything around the pay step is the production code.
use super::*;
use crate::home::bootstrap as bootstrap_home;
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::{Client, Keys};
use session_reply::{TurnPay, TurnPayFuture};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use store::BuyerTurn;

struct Daemon {
    context: Arc<BuyerContext>,
    net: Client,
    _lock: HomeLock,
    _root: tempfile::TempDir,
    _relay: LocalRelay,
}

impl Daemon {
    async fn start(buyer_review: bool) -> Self {
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        let root = tempfile::tempdir().expect("tempdir");
        let mut home = bootstrap_home(root.path()).expect("bootstrap home");
        home.config.relay_url = relay_url.clone();
        home.config.sessions.enabled = true;
        home.config.review.buyer_delivery = buyer_review;
        let (lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        // Counterparty events come from a client that is NOT the daemon's (one nostr-sdk client
        // does not observe what it published itself).
        let net = Client::new(Keys::generate());
        net.add_relay(&relay_url).await.expect("add relay");
        net.connect().await;
        net.wait_for_connection(Duration::from_secs(5)).await;
        Self {
            context,
            net,
            _lock: lock,
            _root: root,
            _relay: relay,
        }
    }

    fn buyer(&self) -> Keys {
        buyer_keys(&self.context.home).expect("buyer keys")
    }

    async fn publish(&self, keys: &Keys, draft: &crate::gateway::EventDraft) -> nostr_sdk::Event {
        let event = crate::gateway::nostr::event_builder(draft)
            .expect("event builder")
            .sign_with_keys(keys)
            .expect("sign");
        self.net.send_event(&event).await.expect("publish");
        event
    }
}

fn hex(n: u64) -> String {
    format!("{n:064x}")
}
// Keep the job id's distinguishing TAIL (test jobs differ only in their last digits).
fn followup_of(job: &str, turn: u32) -> String {
    format!("f{turn:03x}{}", &job[4..])
}
fn result_of(job: &str, turn: u32) -> String {
    format!("e{turn:03x}{}", &job[4..])
}

/// Turn 1's accept-bind, as `collect` leaves it: an inline delivery of `answer`.
fn write_turn_one_bind(
    home: &MaxplayerHome,
    job_id: &str,
    claim_id: &str,
    result_id: &str,
    seller: &str,
    payment_mode: crate::gateway::PaymentMode,
    amount_sats: u64,
    job_hash: &str,
) {
    let answer = "turn one answer";
    let bind = job_lifecycle::AcceptedBind {
        private_evidence: None,
        job_id: job_id.into(),
        claim_id: claim_id.into(),
        result_id: result_id.into(),
        seller_pubkey: seller.into(),
        commit_oid: crate::receipt::result_content_hash_hex(answer),
        repo: String::new(),
        branch: String::new(),
        delivery_kind: Some(crate::receipt::DeliveryKind::Inline.as_str().into()),
        inline_answer: Some(answer.into()),
        job_hash: job_hash.into(),
        amount_sats,
        accept_event_id: hex(0xacce),
        accepted_at: 1,
        seller_signature: "ab".repeat(32),
        creq_hash: None,
        accepted_mints: Vec::new(),
        funding_mint: None,
        delivery_mint: None,
        agent_used: None,
        model_used: None,
        contribution: None,
        payment_mode,
        followup_id: None,
    };
    let dir = home.root.join("jobs");
    std::fs::create_dir_all(&dir).expect("jobs dir");
    std::fs::write(
        dir.join(format!("{job_id}.json")),
        serde_json::to_string_pretty(&bind).expect("bind json"),
    )
    .expect("write bind");
}

/// A session job on the relay with turn 1 settled and turn 2 sent: offer, session-capable claim,
/// award, turn-1 result + bind, and the buyer's signed follow-up stored as `sent`.
struct Session {
    job_id: String,
    followup_id: String,
    seller: Keys,
    output: &'static str,
    job_hash: String,
    amount: u64,
}

async fn session_on_relay(daemon: &Daemon, amount: u64) -> Session {
    use crate::gateway::{self, ClaimPayment, OfferDraft, PaymentMode, TagSpec};
    let context = &daemon.context;
    let buyer = daemon.buyer();
    let buyer_hex = buyer.public_key().to_hex();
    let seller = Keys::generate();
    let seller_hex = seller.public_key().to_hex();
    let (task, output) = ("keep talking", "text/plain");
    let deadline = now_unix() as u64 + 3_600;
    let free = amount == 0;
    let mode = if free {
        PaymentMode::None
    } else {
        PaymentMode::Sat
    };

    let offer = OfferDraft::new(task, output, amount, deadline, &seller_hex)
        .with_payment_mode(mode)
        .accepting_delivery(["inline"])
        .to_event_draft();
    let job_id = daemon.publish(&buyer, &offer).await.id.to_hex();
    let creq;
    let payment = if free {
        ClaimPayment::None
    } else {
        creq = gateway::creq::build_seller_creq(
            &job_id,
            amount,
            "sat",
            &[crate::home::DEFAULT_MINT_URL.to_string()],
            &seller_hex,
        )
        .expect("creq");
        ClaimPayment::Sat(&creq)
    };
    let mut claim = gateway::claim_draft(
        &job_id,
        &buyer_hex,
        &seller_hex,
        payment,
        &[],
        &Default::default(),
    );
    claim.tags.push(TagSpec::new(["session", "1"]));
    let claim_id = daemon.publish(&seller, &claim).await.id.to_hex();
    let award = gateway::award_draft(&job_id, &claim_id, &buyer_hex, &seller_hex);
    let award_id = daemon.publish(&buyer, &award).await.id.to_hex();

    let job_hash = job_lifecycle::job_hash_for_offer(&job_id, task, amount);
    let turn_one = gateway::inline_result_draft(
        &job_id,
        &buyer_hex,
        output,
        amount,
        &job_hash,
        &"ab".repeat(32),
        "turn one answer",
        &[],
    );
    let turn_one_id = daemon.publish(&seller, &turn_one).await.id.to_hex();

    // Turn 1 is settled, exactly as the job pass leaves it: award row, spent reservation, bind.
    let now = now_unix();
    context
        .store
        .record_award(&job_id, &claim_id, &award_id, &seller_hex, amount, now)
        .expect("record award");
    context
        .store
        .convert_to_spent(&job_id, amount, now)
        .expect("turn 1 spent");
    write_turn_one_bind(
        &context.home,
        &job_id,
        &claim_id,
        &turn_one_id,
        &seller_hex,
        mode,
        amount,
        &job_hash,
    );
    context
        .store
        .open_session(&job_id, amount * 10, now)
        .expect("session");

    // Turn 2: the buyer's signed follow-up, reserved (priced) and sent.
    let followup = crate::session::followup_draft(&crate::session::FollowUp {
        offer_id: job_id.clone(),
        seller_pubkey: seller_hex.clone(),
        status: crate::session::FollowUpStatus::Turn,
        prev_result_id: Some(turn_one_id),
        turn: Some(2),
        deadline_unix: Some(deadline),
        text: Some("and another thing".into()),
    })
    .expect("follow-up draft");
    let followup = daemon.publish(&buyer, &followup).await;
    let followup_id = followup.id.to_hex();
    {
        use nostr_sdk::JsonUtil;
        context
            .store
            .put_turn(
                &followup_id,
                &job_id,
                2,
                &followup.as_json(),
                amount,
                deadline,
                now,
            )
            .expect("put turn");
    }
    if !free {
        context
            .store
            .reserve(&followup_id, amount, 1_000_000, now)
            .expect("reserve turn 2");
    }
    context
        .store
        .set_turn_state(&followup_id, "sent", None, now)
        .expect("sent");
    Session {
        job_id,
        followup_id,
        seller,
        output,
        job_hash,
        amount,
    }
}

/// The seller's turn-2 result: an inline answer replying to the follow-up, with `creq` if given.
async fn deliver_turn_two(
    daemon: &Daemon,
    session: &Session,
    answer: &str,
    creq: Option<&str>,
) -> nostr_sdk::Event {
    let buyer_hex = daemon.buyer().public_key().to_hex();
    let mut draft = crate::gateway::inline_result_draft(
        &session.job_id,
        &buyer_hex,
        session.output,
        session.amount,
        &session.job_hash,
        &"ab".repeat(32),
        answer,
        &[],
    );
    crate::session::append_result_tags(
        &mut draft,
        Some(&session.followup_id),
        crate::session::TurnStatus::Done,
        false,
        creq,
    );
    daemon.publish(&session.seller, &draft).await
}

async fn settlement_of(context: &BuyerContext, job_id: &str) -> Value {
    let read = get_job(context, json!(1), json!({ "job_id": job_id }))
        .await
        .result
        .expect("get_job returns a view");
    read["settlement"].clone()
}

// ★ Turn 2 of a FREE session settles with no collect call: the watcher's own pass, woken by the
// turn's result event, accepts it, verifies it, writes the free record and materializes
// `results/<job>-t2`, and get_job then reports `auto`/`settled` for turn 2. It never closes the
// session. Red-on-revert: drop the `settle_delivered_turns` call from `settle_awarded` and turn 2
// stays `delivered` with nothing materialized.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_free_turn_two_delivery_settles_with_no_collect_call() {
    let daemon = Daemon::start(false).await;
    let context = &daemon.context;
    let session = session_on_relay(&daemon, 0).await;
    assert_eq!(
        context.store.unsettled_turn_job_ids().expect("work set"),
        vec![session.job_id.clone()],
        "a sent free turn is in the watcher's turn work set"
    );
    assert!(
        context
            .store
            .awarded_unsettled_job_ids()
            .expect("awarded")
            .is_empty(),
        "turn 1 is settled, so the job pass has nothing: only the turn pass can settle turn 2"
    );

    let result = deliver_turn_two(&daemon, &session, "turn two answer", None).await;

    // A result event that names ANOTHER job wakes the watcher but must not touch this session.
    let other = daemon
        .publish(
            &session.seller,
            &crate::gateway::EventDraft::new(
                crate::kinds::JOB_RESULT_KIND,
                vec![crate::gateway::TagSpec::new(["e", &hex(0x0dd), "", "root"])],
                "",
            ),
        )
        .await;
    settle_awarded(context, Some(&other)).await;
    assert_eq!(
        context
            .store
            .turn(&session.followup_id)
            .unwrap()
            .unwrap()
            .state,
        "sent",
        "a wake narrowed to another job must not settle this one"
    );

    settle_awarded(context, Some(&result)).await;

    let turn = context.store.turn(&session.followup_id).unwrap().unwrap();
    assert_eq!(turn.state, "collected", "the watcher settled turn 2");
    assert_eq!(turn.result_id.as_deref(), Some(result.id.to_hex().as_str()));
    let dest = context
        .home
        .root
        .join("results")
        .join(crate::session::collect_dir_name(&session.job_id, 2));
    assert_eq!(
        std::fs::read_to_string(dest.join(collect::INLINE_ANSWER_FILENAME)).expect("answer"),
        "turn two answer"
    );
    assert!(
        collect::free_collect_record_path(&context.home, &session.followup_id).exists(),
        "a free turn leaves its free collect record, keyed by the follow-up"
    );
    assert!(
        job_lifecycle::load_accepted_bind(&context.home, &session.followup_id)
            .unwrap()
            .is_some(),
        "the turn was accepted through the sealed turn path"
    );
    assert!(
        !context.home.root.join("payment-journal").exists(),
        "a free turn enters no payment path"
    );
    assert!(
        matches!(
            context.store.reservation(&session.followup_id).unwrap(),
            Some((reservations::ReservationState::Spent, 0))
        ),
        "a free turn settles to a zero spent row"
    );
    assert!(context.store.unsettled_turn_job_ids().unwrap().is_empty());
    assert_eq!(
        context
            .store
            .session_row(&session.job_id)
            .unwrap()
            .map(|(_, s)| s),
        Some("open".to_owned()),
        "the watcher never closes a session"
    );

    let settlement = settlement_of(context, &session.job_id).await;
    assert_eq!(settlement["mode"], json!("auto"), "{settlement}");
    assert_eq!(settlement["state"], json!("settled"), "{settlement}");
    assert_eq!(settlement["turn"], json!(2), "{settlement}");
    assert_eq!(
        settlement["results_path"],
        json!(dest.display().to_string()),
        "{settlement}"
    );

    // A later periodic or boot sweep (same function, no wake) finds nothing left to do.
    settle_awarded(context, None).await;
    assert_eq!(
        context
            .store
            .turn(&session.followup_id)
            .unwrap()
            .unwrap()
            .state,
        "collected"
    );
}

// ★ A forged priced turn costs NOTHING. Turn 2's result replays turn 1's payment request
// (`payment_id` = the job id, not the follow-up id). The periodic sweep finds it through its relay
// read, the sealed turn accept refuses it before any bind, budget or wallet step, and get_job reports
// `failed` for turn 2 with the refusal. The reservation stays held; nothing is materialized.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_turn_delivery_spends_nothing_and_reads_failed() {
    let daemon = Daemon::start(false).await;
    let context = &daemon.context;
    let session = session_on_relay(&daemon, 2).await;
    let replayed = crate::gateway::creq::build_seller_creq(
        &session.job_id,
        session.amount,
        "sat",
        &[crate::home::DEFAULT_MINT_URL.to_string()],
        &session.seller.public_key().to_hex(),
    )
    .expect("creq");
    deliver_turn_two(&daemon, &session, "turn two answer", Some(&replayed)).await;

    settle_awarded(context, None).await;

    let turn = context.store.turn(&session.followup_id).unwrap().unwrap();
    assert_eq!(
        turn.state, "delivered",
        "the sweep read the delivery (non-vacuity) but must not mark it collected"
    );
    assert!(
        job_lifecycle::load_accepted_bind(&context.home, &session.followup_id)
            .unwrap()
            .is_none(),
        "a refused turn writes no bind"
    );
    assert_eq!(
        BudgetGate::from_home(&context.home).expect("gate").spent(),
        0
    );
    assert!(!context.home.root.join("payment-journal").exists());
    assert!(
        !context
            .home
            .root
            .join("results")
            .join(crate::session::collect_dir_name(&session.job_id, 2))
            .exists()
    );
    assert!(
        matches!(
            context.store.reservation(&session.followup_id).unwrap(),
            Some((reservations::ReservationState::Reserved, 2))
        ),
        "a refused settle keeps the turn's reservation held"
    );
    let recorded = context
        .settle_failures
        .lock()
        .unwrap()
        .get(&session.followup_id)
        .cloned()
        .expect("the refusal is recorded under the follow-up id");
    let settlement = settlement_of(context, &session.job_id).await;
    assert_eq!(settlement["mode"], json!("auto"), "{settlement}");
    assert_eq!(settlement["state"], json!("failed"), "{settlement}");
    assert_eq!(settlement["turn"], json!(2), "{settlement}");
    assert_eq!(settlement["error"], json!(recorded), "{settlement}");
}

/// The pay step, replaced. Records which turns it was asked to pay, refuses the turns in `refuse`,
/// and models the sealed path's pays-once property the way the payment journal gives it: the first
/// call for a follow-up "sends", a later call finds it already sent. The check and the record are
/// separated by `delay`, so two calls that are NOT serialized by the money lock both send.
#[derive(Default)]
struct FakePay {
    refuse: BTreeSet<u32>,
    delay: Duration,
    calls: std::sync::Mutex<Vec<u32>>,
    sent: std::sync::Mutex<BTreeSet<String>>,
    sends: AtomicUsize,
    in_flight: AtomicUsize,
    overlapped: AtomicBool,
}

impl TurnPay for FakePay {
    fn pay<'a>(
        &'a self,
        _home: &'a MaxplayerHome,
        _gate: Option<&'a mut BudgetGate>,
        row: &'a BuyerTurn,
        _earlier: &'a [String],
    ) -> TurnPayFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(row.turn);
            if self.in_flight.fetch_add(1, Ordering::SeqCst) > 0 {
                self.overlapped.store(true, Ordering::SeqCst);
            }
            let already = self.sent.lock().unwrap().contains(&row.followup_id);
            tokio::time::sleep(self.delay).await;
            let result = if self.refuse.contains(&row.turn) {
                Err(collect::CollectError::Integrity(
                    "refused by the test".into(),
                ))
            } else {
                if !already {
                    self.sent.lock().unwrap().insert(row.followup_id.clone());
                    self.sends.fetch_add(1, Ordering::SeqCst);
                }
                Ok(collect::CollectOutcome {
                    pay: collect::CollectPayment::Free,
                    commit_oid: "c".repeat(40),
                    path: String::new(),
                    files: Vec::new(),
                    agent_used: None,
                    model_used: None,
                })
            };
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        })
    }
}

impl FakePay {
    fn calls(&self) -> Vec<u32> {
        self.calls.lock().unwrap().clone()
    }
}

const SELLER: &str = "5e115e115e115e115e115e115e115e115e115e115e115e115e115e115e115e11";

/// An awarded priced session job in the store; turn 1 settled when `turn_one_settled`.
fn seed_job(context: &BuyerContext, job: &str, turn_one_settled: bool) {
    let now = now_unix();
    context
        .store
        .record_award(job, &hex(0xc1a1), &hex(0xa3a2), SELLER, 100, now)
        .expect("award");
    context
        .store
        .reserve(job, 100, 1_000_000, now)
        .expect("reserve turn 1");
    if turn_one_settled {
        context
            .store
            .convert_to_spent(job, 100, now)
            .expect("turn 1 spent");
    }
}

/// A priced turn in `state`, its reservation held.
fn seed_turn(context: &BuyerContext, job: &str, turn: u32, state: &str) -> String {
    let now = now_unix();
    let followup = followup_of(job, turn);
    context
        .store
        .put_turn(&followup, job, turn, "{}", 100, now as u64 + 3_600, now)
        .expect("turn");
    let result = (state == "delivered").then(|| result_of(job, turn));
    context
        .store
        .set_turn_state(&followup, state, result.as_deref(), now)
        .expect("state");
    context
        .store
        .reserve(&followup, 100, 1_000_000, now)
        .expect("reserve turn");
    followup
}

fn reservation_state(context: &BuyerContext, id: &str) -> Option<reservations::ReservationState> {
    context
        .store
        .reservation(id)
        .unwrap()
        .map(|(state, _)| state)
}

// ★ STRICT TURN ORDER. Two delivered turns (only reachable through a bug or a crash, which is
// exactly when the order guard matters): turn 3 is never paid while turn 2 is outstanding — not
// when turn 2's settle is refused, not while turn 2 is still in flight, and not while turn 1 is
// unsettled — and when both can settle, one pass settles 2 then 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turns_settle_strictly_in_order() {
    let daemon = Daemon::start(false).await;
    let context = &daemon.context;

    let job = hex(0x101);
    seed_job(context, &job, true);
    let two = seed_turn(context, &job, 2, "delivered");
    let three = seed_turn(context, &job, 3, "delivered");

    let refusing = FakePay {
        refuse: [2].into(),
        ..Default::default()
    };
    session_reply::settle_delivered_turns(context, None, &refusing).await;
    assert_eq!(
        refusing.calls(),
        vec![2],
        "turn 2 is tried first, and turn 3 never"
    );
    assert_eq!(
        reservation_state(context, &three),
        Some(reservations::ReservationState::Reserved)
    );
    assert!(context.settle_failures.lock().unwrap().contains_key(&two));
    // The settle body refuses out of order on its own too (the `collect turn=3` path):
    // turn 3 cannot be paid while turn 2 is outstanding, and the pay step is never entered.
    let row3 = context.store.turn(&three).unwrap().unwrap();
    let direct = FakePay::default();
    let refused = session_reply::settle_turn_with(context, &row3, &direct).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("collect turn 2 first")),
        "{refused:?}"
    );
    assert!(direct.calls().is_empty(), "{:?}", direct.calls());

    let paying = FakePay::default();
    session_reply::settle_delivered_turns(context, None, &paying).await;
    assert_eq!(paying.calls(), vec![2, 3], "one pass settles 2, then 3");
    for id in [&two, &three] {
        assert_eq!(context.store.turn(id).unwrap().unwrap().state, "collected");
        assert_eq!(
            reservation_state(context, id),
            Some(reservations::ReservationState::Spent)
        );
    }
    assert!(
        !context.settle_failures.lock().unwrap().contains_key(&two),
        "a successful settle clears the recorded refusal"
    );

    // Turn 2 still in flight (no result on the relay): turn 3's delivery waits.
    let job = hex(0x102);
    seed_job(context, &job, true);
    seed_turn(context, &job, 2, "sent");
    let three = seed_turn(context, &job, 3, "delivered");
    let waiting = FakePay::default();
    session_reply::settle_delivered_turns(context, None, &waiting).await;
    assert!(waiting.calls().is_empty(), "{:?}", waiting.calls());
    assert_eq!(
        reservation_state(context, &three),
        Some(reservations::ReservationState::Reserved)
    );

    // Turn 1 not settled yet: no later turn settles before it.
    let job = hex(0x103);
    seed_job(context, &job, false);
    let two = seed_turn(context, &job, 2, "delivered");
    let early = FakePay::default();
    session_reply::settle_delivered_turns(context, None, &early).await;
    assert!(early.calls().is_empty(), "{:?}", early.calls());
    assert_eq!(
        reservation_state(context, &two),
        Some(reservations::ReservationState::Reserved)
    );
}

// ★ WATCHER vs COLLECT RACE. A watcher pass and a manual `collect turn=2` (the RPC's turn path,
// `settle_turn`) start together for the same delivered turn, in both orders. The money lock
// serializes them, and the watcher re-reads the turn under the lock, so the turn is sent ONCE, the
// two pay steps never overlap, and the reservation flips once. When collect wins, the watcher does
// not even enter the pay step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watcher_racing_collect_pays_a_turn_exactly_once() {
    let daemon = Daemon::start(false).await;
    let context = &daemon.context;
    for (n, watcher_first) in [(0x201, true), (0x202, false)] {
        let job = hex(n);
        seed_job(context, &job, true);
        let two = seed_turn(context, &job, 2, "delivered");
        let row = context.store.turn(&two).unwrap().unwrap();
        let pay = FakePay {
            delay: Duration::from_millis(150),
            ..Default::default()
        };
        let watcher = session_reply::settle_delivered_turns(context, None, &pay);
        let collect = session_reply::settle_turn_with(context, &row, &pay);
        let collected = if watcher_first {
            let ((), collected) = tokio::join!(watcher, collect);
            collected
        } else {
            let (collected, ()) = tokio::join!(collect, watcher);
            collected
        };
        assert!(collected.is_ok(), "{collected:?}");
        assert_eq!(
            pay.sends.load(Ordering::SeqCst),
            1,
            "watcher_first={watcher_first}"
        );
        assert!(
            !pay.overlapped.load(Ordering::SeqCst),
            "two pay steps ran at once (watcher_first={watcher_first})"
        );
        if !watcher_first {
            assert_eq!(
                pay.calls(),
                vec![2],
                "collect settled first: the watcher must re-read under the lock and skip"
            );
        }
        assert_eq!(
            context.store.turn(&two).unwrap().unwrap().state,
            "collected"
        );
        assert_eq!(
            reservation_state(context, &two),
            Some(reservations::ReservationState::Spent)
        );
    }
}

/// A RESULT event from `author` replying to `followup` on `job`.
fn turn_result_event(author: &Keys, job: &str, followup: &str) -> nostr_sdk::Event {
    use crate::gateway::{EventDraft, TagSpec};
    crate::gateway::nostr::event_builder(&EventDraft::new(
        crate::kinds::JOB_RESULT_KIND,
        vec![
            TagSpec::new(["e", job, "", "root"]),
            TagSpec::new(["e", followup, "", "reply"]),
        ],
        "",
    ))
    .expect("builder")
    .sign_with_keys(author)
    .expect("sign")
}

// ★ REVIEW HOLD, exactly as turn 1. A turn whose delivery review failed or timed out is filed under
// the OFFER (`review-job-status/<job>.json`, subject.event = the turn's result id). The background
// watcher never retries it on its own (no unbounded paid reviewer calls), and get_job reports
// `failed` with the review's detail. A held status for an earlier, settled turn is history and
// holds nothing. A NEW result from the awarded seller answering this follow-up replaces the held
// one and is settled; the same from any other author is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_held_turn_is_not_retried_in_the_background() {
    let daemon = Daemon::start(true).await;
    let context = &daemon.context;
    let seller = Keys::generate();
    let seller_hex = seller.public_key().to_hex();
    let job = hex(0x301);
    let now = now_unix();
    context
        .store
        .record_award(&job, &hex(0xc1a1), &hex(0xa3a2), &seller_hex, 100, now)
        .expect("award");
    context
        .store
        .convert_to_spent(&job, 100, now)
        .expect("turn 1 spent");
    let two = seed_turn(context, &job, 2, "delivered");
    let held = |event: String, state: &str, detail: &str| {
        crate::review::state::write(
            &context.home.root,
            &crate::review::Subject {
                offer: job.clone(),
                event,
                kind: crate::kinds::JOB_RESULT_KIND,
                commit: None,
            },
            state,
            detail,
        )
        .expect("review status");
    };
    held(result_of(&job, 2), "error", "review timed out");

    let pay = FakePay::default();
    for _ in 0..3 {
        session_reply::settle_delivered_turns(context, None, &pay).await;
    }
    let stranger = Keys::generate();
    session_reply::settle_delivered_turns(
        context,
        Some(&turn_result_event(&stranger, &job, &two)),
        &pay,
    )
    .await;
    assert!(
        pay.calls().is_empty(),
        "a held turn is never retried by the watcher: {:?}",
        pay.calls()
    );
    assert_eq!(
        reservation_state(context, &two),
        Some(reservations::ReservationState::Reserved)
    );
    let hint = settlement::hint(&settlement::facts(context, &job, true, true).expect("facts"));
    assert_eq!(hint["mode"], json!("auto"), "{hint}");
    assert_eq!(hint["state"], json!("failed"), "{hint}");
    assert_eq!(hint["turn"], json!(2), "{hint}");
    assert_eq!(hint["error"], json!("review timed out"), "{hint}");

    // A replacement result from the awarded seller, answering this follow-up, is settled.
    session_reply::settle_delivered_turns(
        context,
        Some(&turn_result_event(&seller, &job, &two)),
        &pay,
    )
    .await;
    assert_eq!(pay.calls(), vec![2]);
    assert_eq!(
        context.store.turn(&two).unwrap().unwrap().state,
        "collected"
    );

    // Turn 3: the job's status file still names turn 2's (now settled) result: history, no hold.
    held(result_of(&job, 2), "error", "an old timeout");
    let three = seed_turn(context, &job, 3, "delivered");
    context
        .store
        .set_turn_state(&two, "collected", Some(&result_of(&job, 2)), now)
        .unwrap();
    session_reply::settle_delivered_turns(context, None, &pay).await;
    assert_eq!(
        pay.calls(),
        vec![2, 3],
        "an earlier turn's review status holds nothing"
    );
    assert_eq!(
        context.store.turn(&three).unwrap().unwrap().state,
        "collected"
    );
}
