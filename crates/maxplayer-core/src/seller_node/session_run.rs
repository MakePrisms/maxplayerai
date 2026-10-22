//! Session jobs on the seller run loop (docs/specs/session-jobs.md §5–§8): follow-up ingest and
//! admission, the resumed turn (slot wait, holder check, fresh credential preparation,
//! `docker start -a -i`, the follow-up review overlapped with the start, then `prompt.json` or
//! `abort`), per-turn results, parking, close and seller-restart recovery.
//!
//! Inert while `[sessions] enabled = false`: no follow-up is subscribed to (the award filter adds
//! kind 3412 only when enabled), admitted, recovered or run, and turn 1 is never parked because
//! no claim carries `["session","1"]`.
use super::*;
use crate::delivery_orchestrator as orch;
use crate::seller_exec::{DockerCli as _, RealDockerCli};
use crate::seller_node::session_gc;
use crate::seller_node::store::{Offer, SessionRecord, SessionTurnRecord};
use crate::session::{self, FollowUpStatus, SessionState, TurnState, TurnStatus};
use crate::{opline, opline_verbose};

const SESSION_EXPIRED_FEEDBACK: &str =
    "the seller's parked session for this job has expired; post a new job to continue";
const AT_CAPACITY_TURN_FEEDBACK: &str = "seller is at capacity and could not start this turn before its deadline; nothing was charged \
     and the session stays parked — send the turn again";
const REVIEW_REFUSED_FEEDBACK: &str =
    "the seller's review refused this follow-up; nothing was run or charged";
/// Bound on private follow-ups waiting for their encrypted message.
const PENDING_FOLLOWUPS_CAP: usize = 256;
/// Poll interval while a follow-up waits for the previous turn's payment (§8.3).
const PAYMENT_WAIT_POLL: Duration = Duration::from_secs(5);

/// Per-runner session bookkeeping. The store is the durable half; this is only what must not be
/// repeated within one process (a second task for the same follow-up).
#[derive(Default)]
pub(super) struct SessionRuntime {
    in_flight: Mutex<std::collections::HashSet<String>>,
    /// Private follow-ups whose encrypted message has not arrived yet; re-driven on the tick.
    pending: Mutex<HashMap<String, nostr_sdk::Event>>,
}

/// The host's verdict on a follow-up (§6.1 step 3).
pub(super) enum Release {
    Prompt(orch::SessionPrompt),
    Abort(String),
}

/// Hands the review verdict to the container exactly once: `prompt.json` after a passed review,
/// `abort` otherwise. Until the verdict arrives nothing is written, so the buyer's text cannot
/// reach the container before its review passed (failure test 5).
pub(super) struct ReleaseGate {
    rx: Option<tokio::sync::oneshot::Receiver<Release>>,
    pub(super) verdict: Option<Release>,
}

impl ReleaseGate {
    pub(super) fn new(rx: tokio::sync::oneshot::Receiver<Release>) -> Self {
        Self {
            rx: Some(rx),
            verdict: None,
        }
    }

    pub(super) async fn poll(&mut self, io_dir: &std::path::Path, nonce: &str, job_id: &str) {
        let Some(rx) = self.rx.as_mut() else {
            return;
        };
        let verdict = match rx.try_recv() {
            Ok(verdict) => verdict,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
            Err(_) => Release::Abort("the review task ended without a verdict".into()),
        };
        self.rx = None;
        let verdict = match verdict {
            Release::Prompt(prompt) => match orch::write_session_prompt(io_dir, &prompt) {
                Ok(()) => Release::Prompt(prompt),
                Err(error) => {
                    let _ = orch::write_session_abort(io_dir, nonce);
                    Release::Abort(format!("prompt hand-off failed ({error})"))
                }
            },
            Release::Abort(reason) => {
                if let Err(error) = orch::write_session_abort(io_dir, nonce) {
                    opline!("seller node session job_id={job_id}: abort hand-off failed ({error})");
                }
                Release::Abort(reason)
            }
        };
        match &verdict {
            Release::Prompt(_) => {
                opline!("seller node session job_id={job_id}: follow-up released after review")
            }
            Release::Abort(reason) => {
                opline!("seller node session job_id={job_id}: follow-up withheld ({reason})")
            }
        }
        self.verdict = Some(verdict);
    }
}

/// Feedback reason and text for a failed turn. Never fails the JOB row: turn 1 stays delivered.
fn turn_failure(failure: ContainerDeliveryFailure) -> (ReasonCode, String) {
    use ContainerDeliveryFailure as F;
    match failure {
        F::NoSentinel { detail, .. } => (ReasonCode::NoSentinel, detail),
        F::Delivery(detail) => (ReasonCode::DeliveryFailed, detail),
        F::Setup(detail) | F::Snapshot(detail) | F::Timeout(detail) => {
            (ReasonCode::ExecutionFailed, detail)
        }
        F::Agent(error) => (ReasonCode::ExecutionFailed, error.to_string()),
    }
}

fn feedback_text(code: ReasonCode) -> &'static str {
    match code {
        ReasonCode::NoSentinel => NO_SENTINEL_FEEDBACK,
        ReasonCode::DeliveryFailed => DELIVERY_FAILURE_FEEDBACK,
        ReasonCode::AtCapacity => AT_CAPACITY_TURN_FEEDBACK,
        ReasonCode::SessionExpired => SESSION_EXPIRED_FEEDBACK,
        _ => EXEC_FAILURE_FEEDBACK,
    }
}

fn setup<E: std::fmt::Display>(error: E) -> (ReasonCode, String) {
    (ReasonCode::ExecutionFailed, error.to_string())
}

fn now_secs() -> u64 {
    now_unix().max(0) as u64
}

impl SellerNodeRunner {
    fn sessions_enabled(&self) -> bool {
        self.node.home().config.sessions.enabled
    }

    /// A kind-3412 FOLLOW-UP (§2.1, §6.1 step 1): parse, admit, and either close the session or
    /// record the turn and start its task. Anything not admitted is ignored: nothing starts,
    /// nothing is published (failure test 1).
    pub(super) async fn on_followup(self: &Arc<Self>, event: &nostr_sdk::Event) {
        if !self.sessions_enabled() {
            return;
        }
        let fid = event.id.to_hex();
        let draft = event_to_draft(event);
        let followup = match session::parse_followup(&draft) {
            Ok(followup) => followup,
            Err(error) => {
                opline!("seller node follow-up ignore id={fid}: {error}");
                return;
            }
        };
        let job_id = followup.offer_id.clone();
        let store = self.node.store();
        match store.claim_advertised_session(&job_id) {
            Ok(true) => {}
            Ok(false) => {
                opline_verbose!(
                    "seller node follow-up ignore id={fid}: job {job_id} is not a session job of this seat"
                );
                return;
            }
            Err(error) => {
                opline!("seller node follow-up ignore id={fid}: claim read failed ({error})");
                return;
            }
        }
        let Ok(Some(award_buyer)) = store.job_award_buyer(&job_id) else {
            opline!("seller node follow-up ignore id={fid}: job {job_id} has no award of ours");
            return;
        };
        let record = match store.session(&job_id) {
            Ok(Some(record)) => record,
            // No record: turn 1 did not park (old orchestrator, inline turn on a non-session
            // path) or GC already removed it. Answer as an ended session.
            Ok(None) => SessionRecord {
                job_id: job_id.clone(),
                buyer_pubkey: award_buyer.clone(),
                container: String::new(),
                holder: None,
                holder_started_at: None,
                image_id: String::new(),
                orchestrator_version: String::new(),
                acp_session_id: None,
                harness: String::new(),
                model: None,
                turn: 1,
                last_turn_at: 0,
                disk_bytes: 0,
                state: SessionState::Expired,
                proxy_host: None,
                base_oid: None,
            },
            Err(error) => {
                opline!("seller node follow-up ignore id={fid}: session read failed ({error})");
                return;
            }
        };
        let author = event.pubkey.to_hex();
        if let Err(why) = session::admit_followup(
            &followup,
            &author,
            &award_buyer,
            &self.seller_pubkey.to_hex(),
            &job_id,
            record.turn,
            record.state.is_live(),
        ) {
            opline!("seller node follow-up ignore id={fid} job_id={job_id}: {why:?}");
            // An ended session answers its buyer's turn once (the refused row dedups re-sends).
            if why == session::Ignore::SessionEnded
                && followup.status == FollowUpStatus::Turn
                && let (Some(turn), Some(deadline)) = (followup.turn, followup.deadline_unix)
                && matches!(
                    store.record_turn(
                        &fid,
                        &job_id,
                        turn,
                        deadline,
                        TurnState::Refused,
                        now_secs()
                    ),
                    Ok(true)
                )
            {
                self.turn_feedback(&job_id, &award_buyer, &fid, ReasonCode::SessionExpired)
                    .await;
            }
            return;
        }
        if followup.status == FollowUpStatus::Close {
            self.close_session(record).await;
            return;
        }
        let text = if crate::seller_node::privacy::is_private(&draft) {
            match self.resolve_private_followup(event, &job_id) {
                Ok(text) => Some(text),
                Err(error) => {
                    opline!("seller node follow-up pending id={fid}: {error}");
                    let mut pending = self.session_rt.pending.lock().expect("session state");
                    if pending.len() < PENDING_FOLLOWUPS_CAP {
                        pending.insert(fid, event.clone());
                    }
                    return;
                }
            }
        } else {
            followup.text.clone()
        };
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            opline!("seller node follow-up ignore id={fid}: no message");
            return;
        };
        let (turn, deadline) = (
            followup.turn.unwrap_or_default(),
            followup.deadline_unix.unwrap_or_default(),
        );
        if let Err(error) = store.record_turn(
            &fid,
            &job_id,
            turn,
            deadline,
            TurnState::Received,
            now_secs(),
        ) {
            opline!("seller node follow-up ignore id={fid}: turn write failed ({error})");
            return;
        }
        // Another follow-up already holds this turn number: this one never runs.
        if !matches!(store.turn(&fid), Ok(Some(_))) {
            opline!(
                "seller node follow-up ignore id={fid}: turn {turn} of {job_id} is already taken"
            );
            return;
        }
        if let Err(error) = store.set_turn_text(&fid, &text) {
            opline!("seller node follow-up id={fid}: message write failed ({error})");
            return;
        }
        opline!("seller node follow-up admitted id={fid} job_id={job_id} turn={turn}");
        self.spawn_turn(fid);
    }

    /// The encrypted message of a private follow-up, authenticated against the job's signed
    /// offer/claim/award chain.
    fn resolve_private_followup(
        &self,
        event: &nostr_sdk::Event,
        job_id: &str,
    ) -> crate::private_content::Result<String> {
        use crate::private_content as pc;
        let (mut ctx, offer) = crate::seller_node::privacy::known(
            self.node.home(),
            &self.seller_pubkey.to_hex(),
            job_id,
        )?
        .ok_or(pc::Error("private offer context unavailable"))?;
        let (claim, award) = ctx
            .store
            .selection(job_id)?
            .ok_or(pc::Error("private selection missing"))?;
        let content =
            ctx.accept_content(event, &offer, Some(&claim), Some(&award), None, now_secs())?;
        if content.body().kind != pc::ContentType::FollowUp {
            return Err(pc::Error("follow-up carries another content type"));
        }
        Ok(content.body().text.clone())
    }

    /// Re-drive private follow-ups whose message was not available yet (drain tick).
    pub(super) async fn redrive_followups(self: &Arc<Self>) {
        if !self.sessions_enabled() {
            return;
        }
        let pending: Vec<_> = self
            .session_rt
            .pending
            .lock()
            .expect("session state")
            .drain()
            .map(|(_, event)| event)
            .collect();
        for event in pending {
            self.on_followup(&event).await;
        }
    }

    /// A free job's turn is settled by the buyer's ACCEPT of its result (§8.3).
    pub(super) fn on_turn_accept(&self, event: &nostr_sdk::Event, job_id: &str) {
        if !self.sessions_enabled() {
            return;
        }
        let Some(result_id) = event.tags.iter().find_map(|tag| {
            let fields = tag.as_slice();
            (fields.first().map(String::as_str) == Some("e")
                && fields.get(3).map(String::as_str) == Some("reply"))
            .then(|| fields.get(1).cloned())
            .flatten()
        }) else {
            return;
        };
        let store = self.node.store();
        let Ok(Some(turn)) = store.turn_by_result(&result_id) else {
            return;
        };
        let free =
            matches!(store.offer_row(job_id), Ok(Some(offer)) if offer.payment_mode.is_free());
        if turn.job_id == job_id && free && turn.state == TurnState::Delivered {
            match store.set_turn_state(&turn.followup_id, TurnState::Paid, None, None, now_secs()) {
                Ok(()) => opline!(
                    "seller node session job_id={job_id}: turn {} accepted (free)",
                    turn.turn
                ),
                Err(error) => {
                    opline!("seller node session job_id={job_id}: accept write failed ({error})")
                }
            }
        }
    }

    fn spawn_turn(self: &Arc<Self>, fid: String) {
        if !self
            .session_rt
            .in_flight
            .lock()
            .expect("session state")
            .insert(fid.clone())
        {
            return;
        }
        let runner = Arc::clone(self);
        tokio::task::spawn_local(async move {
            runner.run_turn(&fid).await;
            runner
                .session_rt
                .in_flight
                .lock()
                .expect("session state")
                .remove(&fid);
        });
    }

    /// `close` from the buyer (§5.5): end now, or at the next park when a turn is running.
    async fn close_session(&self, record: SessionRecord) {
        let job_id = record.job_id.clone();
        if record.state == SessionState::Running {
            match self.node.store().request_session_close(&job_id) {
                Ok(()) => opline!(
                    "seller node session job_id={job_id}: close requested; ends when the running turn parks"
                ),
                Err(error) => {
                    opline!("seller node session job_id={job_id}: close write failed ({error})")
                }
            }
            return;
        }
        self.end_session_now(record, SessionState::Closed).await;
    }

    async fn end_session_now(&self, record: SessionRecord, state: SessionState) {
        let store = self.node.store().clone();
        let paths = session_gc::SessionPaths::for_job(&self.node.home().root, &record.job_id);
        let job_id = record.job_id.clone();
        let ended = tokio::task::spawn_blocking(move || {
            session_gc::end_session(
                &mut RealDockerCli,
                &store,
                &record,
                &paths,
                state,
                now_secs(),
            )
        })
        .await;
        match ended {
            Ok(Ok(errors)) if errors.is_empty() => {
                opline!(
                    "seller node session job_id={job_id}: ended ({})",
                    state.as_str()
                )
            }
            Ok(Ok(errors)) => opline!(
                "seller node session job_id={job_id}: ended ({}) with removal errors: {}",
                state.as_str(),
                errors.join("; ")
            ),
            Ok(Err(error)) => opline!("seller node session job_id={job_id}: end failed ({error})"),
            Err(error) => opline!("seller node session job_id={job_id}: end task failed ({error})"),
        }
    }

    async fn turn_feedback(&self, job_id: &str, buyer: &str, fid: &str, code: ReasonCode) {
        // The follow-up id makes each turn's encrypted feedback a distinct content intent.
        let reason = format!("{} (follow-up {fid})", feedback_text(code));
        let status = (code == ReasonCode::SessionExpired).then_some("refusal");
        self.publish_buyer_feedback_tagged(
            job_id,
            buyer,
            code,
            &reason,
            None,
            status,
            vec![gateway::TagSpec::new(["e", fid, "", "reply"])],
        )
        .await;
    }

    /// One follow-up end to end: §8.3 payment gate, slot, §4 start margin, §5.5 holder gate, the
    /// resumed container, then park. A turn that does not deliver is `refused` and unpaid; the
    /// session stays parked and the buyer may send that turn again.
    async fn run_turn(&self, fid: &str) {
        let store = self.node.store();
        let (turn, offer) = loop {
            let turn = match store.turn(fid) {
                Ok(Some(turn)) => turn,
                _ => return,
            };
            if !matches!(
                turn.state,
                TurnState::Received
                    | TurnState::WaitingPayment
                    | TurnState::Reviewing
                    | TurnState::Running
            ) {
                return;
            }
            let Ok(Some(offer)) = store.offer_row(&turn.job_id) else {
                opline!("seller node session follow-up {fid}: offer facts missing");
                return;
            };
            let settled = store
                .turn_settled(&turn.job_id, turn.turn - 1, offer.payment_mode.is_free())
                .unwrap_or(false);
            match session::payment_gate(settled, now_secs(), turn.deadline_unix) {
                session::PaymentGate::Run => break (turn, offer),
                session::PaymentGate::Wait => {
                    if turn.state != TurnState::WaitingPayment {
                        let _ = store.set_turn_state(
                            fid,
                            TurnState::WaitingPayment,
                            None,
                            None,
                            now_secs(),
                        );
                        opline!(
                            "seller node session job_id={}: turn {} waits for turn {} to be paid",
                            turn.job_id,
                            turn.turn,
                            turn.turn - 1
                        );
                    }
                    tokio::time::sleep(PAYMENT_WAIT_POLL).await;
                }
                session::PaymentGate::DropExpired => {
                    let _ = store.set_turn_state(fid, TurnState::Refused, None, None, now_secs());
                    opline!(
                        "seller node session job_id={}: turn {} dropped uncharged (previous turn unpaid past the deadline)",
                        turn.job_id,
                        turn.turn
                    );
                    return;
                }
            }
        };
        let job_id = turn.job_id.clone();
        let buyer = offer.buyer_pubkey.clone();
        // §6.2: a follow-up WAITS for a slot; new offers only try.
        let _slot = self.slots.acquire_unreserved().await;
        let wrap_up = self.node.home().config.sessions.wrap_up_secs;
        if !session::turn_can_start(now_secs(), turn.deadline_unix, wrap_up) {
            let _ = store.set_turn_state(fid, TurnState::Refused, None, None, now_secs());
            self.turn_feedback(&job_id, &buyer, fid, ReasonCode::AtCapacity)
                .await;
            return;
        }
        let record = match store.session(&job_id) {
            Ok(Some(record)) if record.state == SessionState::Parked => record,
            _ => {
                let _ = store.set_turn_state(fid, TurnState::Refused, None, None, now_secs());
                self.turn_feedback(&job_id, &buyer, fid, ReasonCode::SessionExpired)
                    .await;
                return;
            }
        };
        // §5.5 holder gate: the container is never started without the firewall it was created
        // into, nor with an orchestrator older than the inputs protocol (failure tests 6, 14).
        let protocol = record.orchestrator_version.parse::<u32>().unwrap_or(0);
        let gate = match record.holder.clone() {
            Some(holder) => {
                let observed = tokio::task::spawn_blocking(move || {
                    session_gc::observe_holder(&mut RealDockerCli, &holder)
                })
                .await
                .unwrap_or(session::HolderObservation::Missing);
                session::resume_gate(record.holder_started_at.as_deref(), &observed, protocol)
            }
            // An uncontained docker seat has no holder to verify.
            None if protocol >= session::SESSION_PROTOCOL => session::ResumeGate::Start,
            None => session::ResumeGate::Expire("orchestrator_too_old"),
        };
        if let session::ResumeGate::Expire(why) = gate {
            opline!("seller node session job_id={job_id}: expired at resume ({why})");
            self.end_session_now(record, SessionState::Expired).await;
            let _ = store.set_turn_state(fid, TurnState::Refused, None, None, now_secs());
            self.turn_feedback(&job_id, &buyer, fid, ReasonCode::SessionExpired)
                .await;
            return;
        }
        let _ = store.set_session_state(&job_id, SessionState::Running, now_secs());
        let _ = store.set_turn_state(fid, TurnState::Reviewing, None, None, now_secs());
        let outcome = self.resume_turn(&turn, &offer, &record).await;
        let _ = store.set_session_state(&job_id, SessionState::Parked, now_secs());
        match outcome {
            Ok(()) => {
                if let Some(gc) = self.session_gc() {
                    let (job, container) = (job_id.clone(), record.container.clone());
                    let _ = tokio::task::spawn_blocking(move || {
                        gc.measure_and_record(&mut RealDockerCli, &job, &container)
                    })
                    .await;
                }
            }
            Err((code, detail)) => {
                opline!(
                    "seller node session job_id={job_id}: turn {} not delivered ({}: {detail})",
                    turn.turn,
                    code.as_str()
                );
                let _ = store.set_turn_state(fid, TurnState::Refused, None, None, now_secs());
                self.turn_feedback(&job_id, &buyer, fid, code).await;
            }
        }
        if store.session_close_requested(&job_id).unwrap_or(false)
            && let Ok(Some(record)) = store.session(&job_id)
        {
            self.end_session_now(record, SessionState::Closed).await;
        }
    }

    /// The resumed container run and the turn's RESULT (§6.1 steps 2–4, §7, §8.2).
    async fn resume_turn(
        &self,
        turn: &SessionTurnRecord,
        offer: &Offer,
        record: &SessionRecord,
    ) -> Result<(), (ReasonCode, String)> {
        let (job_id, fid, n, deadline) = (
            turn.job_id.as_str(),
            turn.followup_id.as_str(),
            turn.turn,
            turn.deadline_unix,
        );
        let home = self.node.home();
        let store = self.node.store();
        let seller_pubkey = self.seller_pubkey.to_hex();
        let mut seller = home
            .config
            .seller
            .clone()
            .ok_or_else(|| setup("no [seller] config"))?;
        let private_execution =
            crate::seller_node::privacy::execution(home, &seller_pubkey, job_id).map_err(setup)?;
        if let Some(private) = &private_execution {
            seller.git_remote = private.remote.clone();
        }
        let private = private_execution.is_some();
        let contribution = private_execution
            .as_ref()
            .map(|p| p.contribution)
            .unwrap_or(matches!(store.contribution_pin(job_id), Ok(Some(_))));
        // Inline turns follow today's offer rule (see the spec note on §7.2).
        let inline_allowed = !contribution
            && offer
                .accepts_delivery
                .iter()
                .any(|mode| mode == gateway::DELIVERY_MODE_INLINE);
        let texts = store.turn_texts(job_id, n).map_err(setup)?;
        let text = texts
            .last()
            .cloned()
            .ok_or_else(|| setup("follow-up message missing"))?;
        let sandbox = SandboxPolicy::from_config(home.config.sandbox.as_ref()).map_err(setup)?;
        let delivery_policy = sandbox
            .container_delivery()
            .ok_or_else(|| setup("container delivery is not enabled for this seat"))?;
        let (agent_command, agent_label, harness) = {
            let selected = self
                .agents
                .dispatch(offer.requested_agent.as_deref())
                .ok_or((
                    ReasonCode::CapabilityMissing,
                    "requested agent unavailable".to_owned(),
                ))?;
            (
                selected.agent.argv.clone(),
                selected.agent.name.clone(),
                selected.index,
            )
        };
        let identity = DeliveryAgentIdentity::for_seller(&seller_pubkey);
        let workdir = job_workdir(home, job_id);
        let branch = session::delivery_branch(private, job_id, n, Some(fid));
        let push_ref = crate::git_transport::delivery_ref(&branch);
        // Every turn binds the OFFER's job hash (spec §7.1 as amended): the v2 result validators
        // require `job-hash == job_hash(offer)`, and the receipt preimage already names the offer.
        // Turns stay distinct through the per-turn creq hash and the delivered commit/answer digest.
        let sentinel_hash = match private_execution.as_ref() {
            Some(p) => p.job_hash.clone(),
            None => crate::private_content::public_v2::execution_hash(
                home,
                job_id,
                &offer.task,
                offer.amount_sats,
            )
            .map_err(setup)?,
        };
        let io_dir = container_exchange_dir(&home.root, job_id);
        orch::create_exchange_dir(&io_dir).map_err(setup)?;
        let tool_endpoints = self.attach_held_tools(job_id).await.map_err(setup)?;
        let attachments = crate::seller_exec::JobAttachments::merge(
            tool_endpoints
                .iter()
                .map(crate::held_tool::JobToolEndpoint::attachments),
        );
        let lifetime =
            unified_job_timeout(deadline, now_secs()) + Duration::from_secs(orch::PUSH_MARGIN_SECS);
        // §6.4: a fresh credential proxy and new placeholders; the holder is the session's.
        let prepared = crate::seller_exec::prepare_resume_launch(
            &agent_command,
            &sandbox,
            &identity,
            lifetime,
            record.proxy_host.as_deref(),
        )
        .await
        .map_err(setup)?;
        let push_token = if !crate::delivery_transport::is_relay_git_locator(&seller.git_remote) {
            orch::PushTokenSource::None
        } else {
            match delivery_policy.token {
                crate::home::ContainerDeliveryToken::FreshAfterAgent => {
                    orch::PushTokenSource::FreshAfterAgent {
                        wait_secs: orch::FRESH_TOKEN_WAIT_SECS,
                    }
                }
                crate::home::ContainerDeliveryToken::LongLived => {
                    let expiry = orch::long_lived_expiration(
                        deadline,
                        now_secs(),
                        delivery_policy.token_cap_secs,
                    )
                    .map_err(|error| (ReasonCode::DeliveryFailed, error.to_string()))?;
                    let header = self
                        .mint_push_header(&seller.git_remote, &push_ref, Some(expiry))
                        .await
                        .map_err(turn_failure)?;
                    orch::PushTokenSource::LongLived { header }
                }
            }
        };
        let fresh_mode = matches!(push_token, orch::PushTokenSource::FreshAfterAgent { .. });
        let nonce = random_nonce_hex().map_err(setup)?;
        let mut mcp_servers = prepared.mcp_servers.clone();
        mcp_servers.extend(attachments.mcp_servers.iter().cloned());
        let inputs = orch::Phase1Inputs {
            job_hash: sentinel_hash,
            seller_pubkey_hex: seller_pubkey.clone(),
            base: None,
            delivery_branch: branch.clone(),
            message: delivery_message(&text),
            author_date_unix: now_unix(),
            agent_argv: prepared.effective_command.clone(),
            codex_session: prepared.codex_session.clone(),
            workdir: std::path::PathBuf::from(CONTAINER_WORKDIR),
            out_dir: std::path::PathBuf::from(orch::CONTAINER_EXCHANGE_DIR),
            // Withheld: the prompt arrives through `prompt.json` after the review (§6.1).
            prompt: String::new(),
            deadline_unix: deadline,
            max_agent_attempts: 1,
            agent_env_names: prepared.env.iter().map(|(key, _)| key.clone()).collect(),
            mcp_servers,
            relay_url: seller.git_remote.clone(),
            push_token,
            handoff_nonce: nonce.clone(),
            session: Some(orch::SessionInputs {
                session_protocol: session::SESSION_PROTOCOL,
                turn: n,
                resume_acp_session_id: record.acp_session_id.clone(),
                base_oid: record.base_oid.clone(),
                wrap_up_unix: session::wrap_up_at(deadline, home.config.sessions.wrap_up_secs),
                prompt_from_exchange: true,
                agent_env_overrides: prepared.turn_env(),
            }),
        };
        orch::write_phase1_inputs(&io_dir.join(orch::PHASE1_INPUTS_FILE), &inputs)
            .map_err(setup)?;

        // §6.1 step 2: the review runs while the container starts and loads the conversation.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let section = session::prompt_session_section();
        let prompt = orch::SessionPrompt {
            nonce: nonce.clone(),
            prompt: format!("{text}\n\n{section}"),
            reset_prompt: format!("{}\n{section}", session::reset_history(&offer.task, &texts)),
        };
        {
            let review = home.config.review.clone();
            let (client, relay) = (self.client.clone(), self.relay_url.clone());
            let subject = crate::review::Subject {
                offer: job_id.to_owned(),
                event: fid.to_owned(),
                kind: crate::kinds::SESSION_FOLLOWUP_KIND,
                commit: None,
            };
            let buyer = offer.buyer_pubkey.clone();
            tokio::task::spawn_local(async move {
                let verdict = match review.enabled(subject.kind, &buyer) {
                    Ok(false) => Ok(None),
                    // The encrypted review lane has no follow-up subject yet: fail closed rather
                    // than send a private message over the public lane (spec §9 note).
                    Ok(true) if private => Err(
                        "private follow-up review is not available yet; the follow-up is withheld"
                            .into(),
                    ),
                    Ok(true) => {
                        let transport = crate::review::wire::RelayTransport {
                            client: &client,
                            relay: &relay,
                        };
                        crate::review::wire::check(
                            &transport, &review, &relay, &subject, &buyer, false,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                let _ = tx.send(match verdict {
                    Ok(_) => Release::Prompt(prompt),
                    Err(reason) => Release::Abort(reason),
                });
            });
        }
        let mut gate = ReleaseGate::new(rx);
        let _ = store.set_turn_state(fid, TurnState::Running, None, None, now_secs());
        let argv = crate::seller_exec::docker_start_argv(&record.container);
        let started = Instant::now();
        let mut child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|error| setup(format!("docker start: {error}")))?;
        opline!(
            "seller node session job_id={job_id}: turn {n} resumed container={}",
            record.container
        );
        let (exit, mut marker) = self
            .supervise_container(
                job_id,
                &mut child,
                &io_dir,
                &nonce,
                fresh_mode,
                &seller.git_remote,
                &push_ref,
                deadline,
                started,
                Some(&mut gate),
            )
            .await;
        if exit.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            let name = record.container.clone();
            let _ = tokio::task::spawn_blocking(move || {
                RealDockerCli.run(&session_gc::stop_argv(&name))
            })
            .await;
        }
        if marker.is_none()
            && let Ok(Some(seen)) = orch::read_agent_done_marker(&io_dir, &nonce)
        {
            marker = Some(seen);
        }
        let turn_session = orch::read_outcome(&io_dir)
            .ok()
            .flatten()
            .filter(|outcome| outcome.handoff_nonce.as_deref() == Some(nonce.as_str()))
            .and_then(|outcome| outcome.agent)
            .and_then(|agent| agent.session);
        let result = match exit {
            Ok(status) => self.classify_container_outcome(
                job_id,
                &io_dir,
                status,
                marker.as_ref(),
                &branch,
                started,
                inline_allowed,
                &nonce,
            ),
            Err(failure) => Err(failure),
        };
        // Evidence, then park again: the container stays for the next turn (§5.2).
        cleanup_job_container(
            JobContainer::adopt(record.container.clone()),
            &workdir,
            workdir.join(seller_git::SELLER_RUN_LOG),
            prepared.forwarded_secrets.clone(),
            CleanupPolicy::CaptureThenPark,
        )
        .await;
        for endpoint in tool_endpoints {
            if let Err(error) = endpoint.detach().await {
                opline!("seller node session job_id={job_id}: {error}");
            }
        }
        let _ = std::fs::remove_dir_all(&io_dir);
        // The proxy stops here: this turn's placeholders are revoked (§6.4).
        drop(prepared);

        if let Some(Release::Abort(reason)) = &gate.verdict {
            return Err((
                ReasonCode::ExecutionFailed,
                format!("{REVIEW_REFUSED_FEEDBACK}: {reason}"),
            ));
        }
        let status = turn_session
            .as_ref()
            .and_then(|s| TurnStatus::parse(&s.turn_status))
            .unwrap_or(TurnStatus::Done);
        let context_reset = turn_session.as_ref().is_some_and(|s| s.context_reset);
        let acp_session_id = turn_session.as_ref().and_then(|s| s.acp_session_id.clone());
        let delivery = match result {
            Ok(delivered) => TurnDelivery::Git(delivered),
            Err(ContainerDeliveryFailure::NoSentinel {
                last_agent_message: Some(message),
                usage,
                wall_time_ms,
                outcome_authentic: true,
                message_truncated: false,
                from_scratch: true,
                ..
            }) if inline_allowed
                && session::session_delivery_content(false, status, &message).is_some() =>
            {
                let answer = crate::engine::marked_inline_answer(&message)
                    .map(str::to_owned)
                    .unwrap_or(message);
                TurnDelivery::Inline {
                    answer,
                    usage,
                    wall_time_ms,
                }
            }
            Err(failure) => {
                if let ContainerDeliveryFailure::Agent(error) = &failure {
                    self.drop_harness(harness, harness_fault_for(error));
                }
                return Err(turn_failure(failure));
            }
        };
        self.publish_turn_result(TurnResult {
            job_id,
            fid,
            turn: n,
            offer,
            git_remote: &seller.git_remote,
            branch: &branch,
            agent_command: &agent_command,
            agent_label: agent_label.as_deref(),
            status,
            context_reset,
            delivery,
        })
        .await?;
        if let Err(error) =
            store.advance_session(job_id, n, None, acp_session_id.as_deref(), now_secs())
        {
            opline!("seller node session job_id={job_id}: session advance failed ({error})");
        }
        Ok(())
    }

    /// Co-sign, project and enqueue turn n's RESULT (§2.3, §7, §8.2): the per-turn tags, and on a
    /// priced job a fresh `creq` whose payment id is the follow-up id.
    async fn publish_turn_result(&self, r: TurnResult<'_>) -> Result<(), (ReasonCode, String)> {
        let home = self.node.home();
        let store = self.node.store();
        let seller_pubkey = self.seller_pubkey.to_hex();
        let offer = r.offer;
        let delivery_failed = |detail: String| (ReasonCode::DeliveryFailed, detail);
        let turn_creq = if offer.payment_mode.is_free() {
            None
        } else {
            Some(
                gateway::creq::build_seller_creq(
                    r.fid,
                    offer.amount_sats,
                    &offer.unit,
                    &home.config.accepted_mints,
                    &seller_pubkey,
                )
                .map_err(|error| delivery_failed(error.to_string()))?,
            )
        };
        let (integrity, kind, answer, commit, usage, wall_time_ms, message) = match &r.delivery {
            TurnDelivery::Git(delivered) => {
                let kind = seller_delivery_kind(r.git_remote, r.branch, &delivered.commit)
                    .map_err(|error| delivery_failed(error.to_string()))?;
                (
                    delivered.commit.clone(),
                    kind.as_str().to_owned(),
                    None,
                    Some(delivered.commit.clone()),
                    delivered.usage.clone(),
                    delivered.wall_time_ms,
                    delivered.last_agent_message.clone(),
                )
            }
            TurnDelivery::Inline {
                answer,
                usage,
                wall_time_ms,
            } => {
                if answer.len() > INLINE_ANSWER_MAX_BYTES {
                    return Err(delivery_failed("inline answer over the size limit".into()));
                }
                (
                    crate::receipt::result_content_hash_hex(answer),
                    crate::receipt::DeliveryKind::Inline.as_str().to_owned(),
                    Some(answer.clone()),
                    None,
                    usage.clone(),
                    *wall_time_ms,
                    None,
                )
            }
        };
        let mut preimage = delivery_receipt_preimage(
            r.job_id,
            &offer.task,
            offer.amount_sats,
            &offer.buyer_pubkey,
            &seller_pubkey,
            &integrity,
            &kind,
            turn_creq.as_deref(),
        );
        crate::seller_node::privacy::bind_receipt_keyed(
            home,
            &seller_pubkey,
            r.job_id,
            &mut preimage,
            answer.as_deref(),
            Some(r.fid),
        )
        .map_err(|error| delivery_failed(error.to_string()))?;
        let seller_sig = match self
            .node
            .signer()
            .sign_receipt_hash(preimage.digest_hex())
            .await
        {
            Ok(Ok(sig)) => sig,
            Ok(Err(error)) => {
                return Err(delivery_failed(format!("receipt sign refused ({error})")));
            }
            Err(error) => return Err(delivery_failed(format!("signer actor gone ({error})"))),
        };
        let exec_metadata =
            seller_exec_metadata(r.agent_command, r.agent_label, wall_time_ms, usage.as_ref());
        let mut draft = match (&commit, &answer) {
            (Some(commit), _) => {
                // A git turn's question is the result content (§7.2).
                let content = match (r.status, message.as_deref()) {
                    (TurnStatus::NeedsInput, Some(question)) => {
                        format!("{question}\n\ndelivery commit {commit}")
                    }
                    _ => format!("delivery commit {commit}"),
                };
                let mut draft = git_result_draft(
                    r.job_id,
                    &offer.buyer_pubkey,
                    result_output(offer),
                    r.git_remote,
                    r.branch,
                    commit,
                    offer.amount_sats,
                    &preimage.job_hash,
                    &seller_sig,
                    content,
                    &exec_metadata,
                );
                if let Some(extra) = contribution_result_envelope_tags(
                    store,
                    self.node.signer(),
                    r.job_id,
                    &seller_pubkey,
                    r.git_remote,
                    r.branch,
                    commit,
                )
                .await
                .map_err(delivery_failed)?
                {
                    draft.tags.extend(extra);
                }
                draft
            }
            (None, Some(answer)) => gateway::inline_result_draft(
                r.job_id,
                &offer.buyer_pubkey,
                result_output(offer),
                offer.amount_sats,
                &preimage.job_hash,
                &seller_sig,
                answer,
                &exec_metadata,
            ),
            (None, None) => return Err(delivery_failed("nothing to deliver".into())),
        };
        session::append_result_tags(
            &mut draft,
            Some(r.fid),
            r.status,
            r.context_reset,
            turn_creq.as_deref(),
        );
        let draft = crate::seller_node::privacy::project_keyed(
            home,
            &seller_pubkey,
            r.job_id,
            draft,
            None,
            Some(r.fid),
        )
        .map_err(|error| delivery_failed(error.to_string()))?;
        let now = now_unix();
        store
            .deliver_turn_and_enqueue(
                r.fid,
                r.job_id,
                r.turn,
                commit.as_deref(),
                &draft,
                now,
                now + RESULT_PUBLISH_WINDOW_SECS,
                now,
            )
            .map_err(|error| delivery_failed(error.to_string()))?;
        let key = format!("result:{}", r.fid);
        self.publish_now(&key).await;
        if let Ok(Some((_, _, Some(result_id)))) = store.outbox_row(&key) {
            let _ = store.set_turn_result(r.fid, &result_id);
        }
        opline!(
            "seller node session delivered job_id={} turn={} status={} {}",
            r.job_id,
            r.turn,
            r.status.as_str(),
            commit
                .map(|c| format!("commit={c}"))
                .unwrap_or_else(|| "inline".into())
        );
        Ok(())
    }

    /// Turn 1 ended normally on a session claim (§5.2 step 4): write the session record with the
    /// holder's `StartedAt`, the container's image and the orchestrator's protocol, then measure.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn park_first_turn(
        &self,
        job_id: &str,
        buyer: &str,
        container: &str,
        holder: Option<String>,
        proxy_host: Option<String>,
        harness: &str,
        outcome: &orch::SessionOutcome,
        base_oid: Option<&str>,
    ) {
        let (probe_holder, probe_container) = (holder.clone(), container.to_owned());
        let observed = tokio::task::spawn_blocking(move || {
            let started = probe_holder.map(|h| session_gc::observe_holder(&mut RealDockerCli, &h));
            let image = RealDockerCli
                .run(&crate::seller_exec::container_image_argv(&probe_container))
                .map(|out| out.trim().to_owned())
                .unwrap_or_default();
            (started, image)
        })
        .await;
        let (holder_started_at, image_id) = match observed {
            Ok((Some(session::HolderObservation::Running { started_at }), image)) => {
                (Some(started_at), image)
            }
            Ok((_, image)) => (None, image),
            Err(_) => (None, String::new()),
        };
        let record = SessionRecord {
            job_id: job_id.to_owned(),
            buyer_pubkey: buyer.to_owned(),
            container: container.to_owned(),
            holder,
            holder_started_at,
            image_id,
            orchestrator_version: outcome.session_protocol.to_string(),
            acp_session_id: outcome.acp_session_id.clone(),
            harness: harness.to_owned(),
            model: None,
            turn: 1,
            last_turn_at: now_secs(),
            disk_bytes: 0,
            state: SessionState::Parked,
            proxy_host,
            base_oid: base_oid.map(str::to_owned),
        };
        if let Err(error) = self.node.store().put_session(&record) {
            opline!(
                "seller node session job_id={job_id}: record write failed ({error}); the parked container is left to the sweep"
            );
            return;
        }
        opline!("seller node session job_id={job_id}: parked after turn 1 container={container}");
        if let Some(gc) = self.session_gc() {
            let (job, container) = (job_id.to_owned(), container.to_owned());
            let _ = tokio::task::spawn_blocking(move || {
                gc.measure_and_record(&mut RealDockerCli, &job, &container)
            })
            .await;
        }
    }

    /// Seller restart (§5.5): a session left `running` is stopped and parked; each unfinished
    /// follow-up is re-driven while its deadline is live, else answered `execution_failed`.
    pub(super) async fn recover_sessions(self: &Arc<Self>) {
        if !self.sessions_enabled() {
            return;
        }
        let store = self.node.store();
        for record in store.sessions().unwrap_or_default() {
            if record.state == SessionState::Running {
                let name = record.container.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    RealDockerCli.run(&session_gc::stop_argv(&name))
                })
                .await;
                let _ = store.set_session_state(&record.job_id, SessionState::Parked, now_secs());
                opline!(
                    "seller node session resume: job_id={} stopped and parked",
                    record.job_id
                );
            }
        }
        let now = now_secs();
        for turn in store.unfinished_turns().unwrap_or_default() {
            if now >= turn.deadline_unix
                && matches!(turn.state, TurnState::Reviewing | TurnState::Running)
            {
                let _ =
                    store.set_turn_state(&turn.followup_id, TurnState::Refused, None, None, now);
                if let Ok(Some(offer)) = store.offer_row(&turn.job_id) {
                    self.turn_feedback(
                        &turn.job_id,
                        &offer.buyer_pubkey,
                        &turn.followup_id,
                        ReasonCode::ExecutionFailed,
                    )
                    .await;
                }
                continue;
            }
            if matches!(turn.state, TurnState::Reviewing | TurnState::Running) {
                let _ =
                    store.set_turn_state(&turn.followup_id, TurnState::Received, None, None, now);
            }
            opline!(
                "seller node session resume: re-driving job_id={} turn={}",
                turn.job_id,
                turn.turn
            );
            self.spawn_turn(turn.followup_id);
        }
    }
}

enum TurnDelivery {
    Git(ContainerDelivery),
    Inline {
        answer: String,
        usage: Option<crate::driver::UsageMetadata>,
        wall_time_ms: u64,
    },
}

struct TurnResult<'a> {
    job_id: &'a str,
    fid: &'a str,
    turn: u32,
    offer: &'a Offer,
    git_remote: &'a str,
    branch: &'a str,
    agent_command: &'a [String],
    agent_label: Option<&'a str>,
    status: TurnStatus,
    context_reset: bool,
    delivery: TurnDelivery,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mx-session-release-{tag}-{}", std::process::id()));
        orch::create_exchange_dir(&dir).expect("exchange dir");
        dir
    }

    fn files_contain(dir: &std::path::Path, needle: &str) -> bool {
        std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .any(|entry| {
                std::fs::read(entry.path())
                    .map(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
                    .unwrap_or(false)
            })
    }

    /// Failure test 5 (host half): a refused review writes `abort` and never the buyer's text;
    /// nothing at all is written before the verdict arrives.
    #[tokio::test]
    async fn review_refusal_never_writes_the_buyer_text_into_the_exchange_dir() {
        let dir = exchange("refused");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut gate = ReleaseGate::new(rx);
        gate.poll(&dir, "nonce-1", "job").await;
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "nothing before the verdict"
        );
        let _ = tx.send(Release::Abort("review refused".into()));
        gate.poll(&dir, "nonce-1", "job").await;
        assert!(dir.join(orch::SESSION_ABORT_FILE).exists());
        assert!(!dir.join(orch::SESSION_PROMPT_FILE).exists());
        assert!(!files_contain(&dir, "SECRET BUYER TEXT"));
        assert!(matches!(gate.verdict, Some(Release::Abort(_))));
        assert_eq!(
            orch::await_session_release(
                &dir,
                "nonce-1",
                Duration::from_secs(1),
                Duration::from_millis(10)
            )
            .unwrap(),
            orch::SessionRelease::Abort
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A passed review releases the prompt exactly once; a later poll writes nothing more.
    #[tokio::test]
    async fn passed_review_releases_the_prompt_once() {
        let dir = exchange("passed");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut gate = ReleaseGate::new(rx);
        let prompt = orch::SessionPrompt {
            nonce: "nonce-2".into(),
            prompt: "SECRET BUYER TEXT".into(),
            reset_prompt: "reset".into(),
        };
        let _ = tx.send(Release::Prompt(prompt.clone()));
        gate.poll(&dir, "nonce-2", "job").await;
        assert!(dir.join(orch::SESSION_PROMPT_FILE).exists());
        assert!(!dir.join(orch::SESSION_ABORT_FILE).exists());
        assert_eq!(
            orch::await_session_release(
                &dir,
                "nonce-2",
                Duration::from_secs(1),
                Duration::from_millis(10)
            )
            .unwrap(),
            orch::SessionRelease::Prompt(prompt)
        );
        gate.poll(&dir, "nonce-2", "job").await;
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "released once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A review task that dies without a verdict withholds the prompt.
    #[tokio::test]
    async fn a_lost_review_withholds_the_prompt() {
        let dir = exchange("lost");
        let (tx, rx) = tokio::sync::oneshot::channel::<Release>();
        let mut gate = ReleaseGate::new(rx);
        drop(tx);
        gate.poll(&dir, "nonce-3", "job").await;
        assert!(dir.join(orch::SESSION_ABORT_FILE).exists());
        assert!(!dir.join(orch::SESSION_PROMPT_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
