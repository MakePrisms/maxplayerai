//! Session jobs, buyer side (docs/specs/session-jobs.md §8.1, §8.4, §10): `reply`, the turn view
//! `get_job.session`, turn collection, `no_response` release and the seller's per-turn feedback.
//! Every entry point is inert unless `[sessions] enabled = true`.
use super::store::BuyerTurn;
use super::*;
use crate::session::{FollowUp, FollowUpStatus, ReplyPreflight};
use nostr_sdk::JsonUtil;

/// A turn the seller may still answer or the buyer still owes a decision on.
fn live(turn: &BuyerTurn) -> bool {
    !matches!(turn.state.as_str(), "refused" | "no_response")
}

/// What the buyer knows about a session, gathered from the store, the relay view and the turn-1 bind.
#[derive(Debug, Default, Clone)]
pub(super) struct Facts {
    pub claim_session: bool,
    pub turn1_delivered: bool,
    pub turn1_collected: bool,
    pub turn1_result: Option<String>,
    pub turns: Vec<BuyerTurn>,
    /// Follow-up id → the result that answers it (relay view, awarded seller only).
    pub delivered: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Next {
    /// A signed follow-up was persisted but never published: re-send THOSE bytes (write-once).
    Resend(BuyerTurn),
    New {
        turn: u32,
        prev_result: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Progress {
    pub uncollected: Option<u32>,
    pub in_flight: bool,
    pub next: Option<Next>,
}

pub(super) fn progress(f: &Facts) -> Progress {
    let p = |uncollected, in_flight, next| Progress {
        uncollected,
        in_flight,
        next,
    };
    if !f.turn1_delivered {
        return p(None, true, None);
    }
    if !f.turn1_collected {
        return p(Some(1), false, None);
    }
    match f.turns.iter().filter(|t| live(t)).max_by_key(|t| t.turn) {
        None => p(
            None,
            false,
            f.turn1_result.clone().map(|prev_result| Next::New {
                turn: 2,
                prev_result,
            }),
        ),
        Some(t) if t.state == "collected" => p(
            None,
            false,
            t.result_id.clone().map(|prev_result| Next::New {
                turn: t.turn + 1,
                prev_result,
            }),
        ),
        Some(t) if t.state == "pending" => p(None, false, Some(Next::Resend(t.clone()))),
        Some(t) if t.state == "delivered" || f.delivered.contains_key(&t.followup_id) => {
            p(Some(t.turn), false, None)
        }
        Some(_) => p(None, true, None),
    }
}

/// The money side of the preflight (§8.1).
#[derive(Debug, Clone, Copy)]
pub(super) struct Money {
    pub price: u64,
    pub per_job_cap: u64,
    pub committed: u64,
    pub cap: u64,
    pub live: bool,
}

/// `reply` in order (§8.1, failure test 4): preflight (refuses with nothing signed, reserved or
/// published) → sign once → persist the exact bytes (write-once) → reserve keyed by the follow-up
/// id → publish → `sent`. A refused reservation frees the turn number; a failed publish leaves the
/// turn `pending` so a retry re-sends the same bytes.
pub(super) async fn advance<S, R, RF, P, PF>(
    store: &BuyerStore,
    job_id: &str,
    facts: &Facts,
    money: Money,
    deadline_unix: u64,
    now: i64,
    sign: S,
    reserve: R,
    publish: P,
) -> Result<BuyerTurn, String>
where
    S: FnOnce(u32, &str) -> Result<(String, String), String>,
    R: FnOnce(String) -> RF,
    RF: std::future::Future<Output = Result<(), String>>,
    P: FnOnce(BuyerTurn) -> PF,
    PF: std::future::Future<Output = Result<(), String>>,
{
    let p = progress(facts);
    let resend = matches!(p.next, Some(Next::Resend(_)));
    crate::session::reply_preflight(&ReplyPreflight {
        claim_supports_sessions: facts.claim_session,
        session_live: money.live,
        uncollected_turn: p.uncollected,
        turn_in_flight: p.in_flight,
        // A re-send is already counted in `committed`; it adds nothing.
        price_sats: if resend { 0 } else { money.price },
        per_job_cap_sats: money.per_job_cap,
        session_committed_sats: money.committed,
        session_cap_sats: money.cap,
    })
    .map_err(|refusal| refusal.to_string())?;
    let row = match p
        .next
        .ok_or("session state has no previous result to continue")?
    {
        Next::Resend(row) => row,
        Next::New { turn, prev_result } => {
            let (followup_id, event_json) = sign(turn, &prev_result)?;
            store
                .put_turn(
                    &followup_id,
                    job_id,
                    turn,
                    &event_json,
                    money.price,
                    deadline_unix,
                    now,
                )
                .map_err(|e| e.to_string())?
        }
    };
    if row.amount_sats > 0 {
        if let Err(error) = reserve(row.followup_id.clone()).await {
            let _ = store.set_turn_state(&row.followup_id, "refused", None, now);
            return Err(error);
        }
    }
    publish(row.clone()).await?;
    store
        .set_turn_state(&row.followup_id, "sent", None, now)
        .map_err(|e| e.to_string())?;
    Ok(row)
}

/// Release every turn whose deadline passed with no result (`no_response`, §10): the
/// reservation is freed exactly as a feedback-driven release, and `reply` may retry the turn.
/// The caller holds the money lock.
pub(super) fn sweep_no_response(
    store: &BuyerStore,
    facts: &Facts,
    now: i64,
    _money_lock: &tokio::sync::MutexGuard<'_, ()>,
) {
    for turn in &facts.turns {
        if matches!(turn.state.as_str(), "pending" | "sent")
            && now >= turn.deadline_unix as i64
            && !facts.delivered.contains_key(&turn.followup_id)
        {
            let _ = store.set_turn_state(&turn.followup_id, "no_response", None, now);
            let _ = store.release(&turn.followup_id, now);
            crate::opline!(
                "buyer: session {} turn {} got no result by its deadline; reservation released",
                turn.job_id,
                turn.turn
            );
        }
    }
}

/// Gather [`Facts`] for an awarded job. Also records relay-seen deliveries on their turn rows.
pub(super) fn facts(
    context: &BuyerContext,
    job_id: &str,
    award: &store::AwardRecord,
    view: &job_lifecycle::JobView,
) -> Result<Facts, String> {
    let store = &context.store;
    let claim_session = view
        .claims
        .iter()
        .any(|c| c.claim_id == award.claim_id && c.session);
    let ours = |r: &&job_lifecycle::ResultView| r.seller_pubkey == award.seller_pubkey;
    let turn1_delivered = view
        .results
        .iter()
        .filter(ours)
        .any(|r| r.is_turn_one_delivery());
    let bind =
        job_lifecycle::load_accepted_bind(&context.home, job_id).map_err(|e| e.to_string())?;
    let turn1_collected = bind.as_ref().is_some_and(|bind| {
        if bind.payment_mode.is_free() {
            collect::free_collect_record_path(&context.home, job_id).exists()
        } else {
            matches!(
                store.reservation(job_id),
                Ok(Some((reservations::ReservationState::Spent, _)))
            )
        }
    });
    let delivered: BTreeMap<String, String> = view
        .results
        .iter()
        .filter(ours)
        .filter(|r| r.has_delivery())
        .filter_map(|r| r.session.reply.clone().map(|f| (f, r.result_id.clone())))
        .collect();
    let now = now_unix();
    let mut turns = store.session_turns(job_id).map_err(|e| e.to_string())?;
    for turn in &mut turns {
        if turn.state == "sent" {
            if let Some(result) = delivered.get(&turn.followup_id) {
                let _ = store.set_turn_state(&turn.followup_id, "delivered", Some(result), now);
                turn.state = "delivered".into();
                turn.result_id = Some(result.clone());
            }
        }
    }
    Ok(Facts {
        claim_session,
        turn1_delivered,
        turn1_collected,
        turn1_result: bind.map(|b| b.result_id),
        turns,
        delivered,
    })
}

#[derive(Debug, Deserialize)]
struct ReplyParams {
    job_id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    deadline_secs: Option<u64>,
    #[serde(default)]
    max_session_sats: Option<u64>,
    #[serde(default)]
    close: bool,
}

pub(super) fn disabled() -> String {
    "sessions are disabled on this buyer ([sessions] enabled = false); post a new job".into()
}

pub(super) async fn reply(context: &Arc<BuyerContext>, id: Value, params: Value) -> Response {
    if !context.home.config.sessions.enabled {
        return Response::err(id, CODE_REFUSED, disabled());
    }
    let params: ReplyParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Response::err(id, CODE_METHOD_NOT_FOUND, format!("reply params: {error}"));
        }
    };
    match reply_inner(context, params).await {
        Ok(body) => Response::ok(id, body),
        Err(error) => Response::err(id, CODE_REFUSED, error),
    }
}

/// The signed job chain a follow-up is projected against: `Some` on a private v2 job.
struct Private {
    offer: nostr_sdk::Event,
    claim: nostr_sdk::Event,
    award: nostr_sdk::Event,
}

/// Privacy is read from turn 1's accept-bind (the signed offer it retained), never guessed: a
/// private job's follow-up must never go out as public `i` text.
fn private_chain(home: &MaxplayerHome, job_id: &str) -> Result<Option<Private>, String> {
    let bind = job_lifecycle::load_accepted_bind(home, job_id)
        .map_err(|e| e.to_string())?
        .ok_or("collect turn 1 first")?;
    Ok(bind.private_evidence.and_then(|e| {
        (!crate::private_content::public_v2::is_public(&e.offer)).then_some(Private {
            offer: e.offer,
            claim: e.claim,
            award: e.award,
        })
    }))
}

async fn reply_inner(context: &Arc<BuyerContext>, params: ReplyParams) -> Result<Value, String> {
    let home = &context.home;
    let job_id = params.job_id.as_str();
    let guard = context.money_lock.lock().await;
    let award = context
        .store
        .award_record(job_id)
        .map_err(|e| e.to_string())?
        .ok_or("no award for this job; reply continues an awarded session job")?;
    let keys = buyer_keys(home)?;
    let keys = &keys;
    let now = now_unix();
    let view = job_lifecycle::fetch_job_view_async(home, keys, job_id, RELAY_TIMEOUT, now as u64)
        .await
        .map_err(|e| e.to_string())?;
    let offer = view.offer.as_ref().ok_or("offer not found on the relay")?;
    let facts = facts(context, job_id, &award, &view)?;
    sweep_no_response(&context.store, &facts, now, &guard);
    let facts = self::facts(context, job_id, &award, &view)?;
    if params.close {
        drop(guard);
        send_close(context, job_id, &award.seller_pubkey).await?;
        return Ok(json!({"job_id": job_id, "session": "closed"}));
    }
    let text = params
        .text
        .filter(|t| !t.trim().is_empty())
        .ok_or("reply needs non-empty text (or close: true)")?;
    let price = if offer.payment_mode.is_free() {
        0
    } else {
        offer.amount_sats
    };
    let row = context
        .store
        .session_row(job_id)
        .map_err(|e| e.to_string())?;
    let cap = params
        .max_session_sats
        .or(row.as_ref().map(|(cap, _)| *cap))
        .unwrap_or_else(|| home.config.sessions.cap_for(None, price));
    let money = Money {
        price,
        per_job_cap: if price == 0 {
            u64::MAX
        } else {
            BudgetGate::from_home(home)
                .map_err(|e| e.to_string())?
                .per_job_cap()
        },
        committed: context
            .store
            .session_committed_sats(job_id)
            .map_err(|e| e.to_string())?,
        cap,
        live: row.as_ref().is_none_or(|(_, state)| state == "open"),
    };
    let deadline = now as u64
        + params
            .deadline_secs
            .unwrap_or(crate::session::DEFAULT_TURN_DEADLINE_SECS);
    let private = if facts.turn1_collected {
        private_chain(home, job_id)?
    } else {
        None
    };
    let quoted = claim_creq_mints(&view, &award.claim_id);
    let seller = award.seller_pubkey.clone();
    let sign = |turn: u32, prev: &str| -> Result<(String, String), String> {
        let followup = FollowUp {
            offer_id: job_id.to_owned(),
            seller_pubkey: seller.clone(),
            status: FollowUpStatus::Turn,
            prev_result_id: Some(prev.to_owned()),
            turn: Some(turn),
            deadline_unix: Some(deadline),
            text: private.is_none().then(|| text.clone()),
        };
        let draft = crate::session::followup_draft(&followup).map_err(|e| e.to_string())?;
        let Some(chain) = &private else {
            let event = sign_draft(&keys, &draft)?;
            return Ok((event.id.to_hex(), event.as_json()));
        };
        use crate::private_content as pc;
        let mut ctx = pc::channel::ContentContext::open(home, &keys.public_key().to_hex())
            .map_err(|e| e.to_string())?;
        let content = ctx
            .prepare(
                &format!("followup:{job_id}:{turn}:{deadline}"),
                &chain.offer,
                Some(&chain.claim),
                Some(&chain.award),
                pc::ContentType::FollowUp,
                text.clone(),
                None,
            )
            .map_err(|e| e.to_string())?;
        let draft = pc::carriers::project(
            &chain.offer,
            &draft,
            Some(&chain.award),
            Some(&content),
            &ctx.policy.host,
        )
        .map_err(|e| e.to_string())?;
        let event = sign_draft(&keys, &draft)?;
        ctx.enqueue(
            &event,
            &chain.offer,
            Some(&chain.claim),
            Some(&chain.award),
            None,
            &content,
        )
        .map_err(|e| e.to_string())?;
        Ok((event.id.to_hex(), event.as_json()))
    };
    let reserve = |followup_id: String| async move {
        let ceiling = award_ceiling(context, &followup_id, &quoted, price).await?;
        context
            .store
            .reserve_at(&followup_id, price, &ceiling, now_unix())
            .map(|_| ())
            .map_err(|refused| refused.to_string())
    };
    let is_private = private.is_some();
    let publish = |row: BuyerTurn| async move {
        let event = nostr_sdk::Event::from_json(&row.event_json).map_err(|e| e.to_string())?;
        if is_private {
            // The carrier and its encrypted copies sit in the durable content outbox; a failed
            // flush here is retried by the content worker, never re-signed.
            flush_private(home, &keys).await;
            return Ok(());
        }
        match context.relay.publish(event).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(format!("follow-up publish: {error}")),
            Err(error) => Err(format!("follow-up publish: {error}")),
        }
    };
    let turn = advance(
        &context.store,
        job_id,
        &facts,
        money,
        deadline,
        now,
        sign,
        reserve,
        publish,
    )
    .await?;
    context
        .store
        .open_session(job_id, cap, now)
        .map_err(|e| e.to_string())?;
    if params.max_session_sats.is_some() {
        context
            .store
            .set_session_cap(job_id, cap, now)
            .map_err(|e| e.to_string())?;
    }
    drop(guard);
    Ok(json!({
        "job_id": job_id,
        "followup_id": turn.followup_id,
        "turn": turn.turn,
        "deadline_unix": turn.deadline_unix,
        "reserved_sats": turn.amount_sats,
        "session_cap_sats": cap,
    }))
}

fn sign_draft(
    keys: &nostr_sdk::Keys,
    draft: &crate::gateway::EventDraft,
) -> Result<nostr_sdk::Event, String> {
    crate::gateway::nostr::event_builder(draft)
        .map_err(|e| e.to_string())?
        .sign_with_keys(keys)
        .map_err(|e| e.to_string())
}

async fn flush_private(home: &MaxplayerHome, keys: &nostr_sdk::Keys) {
    use crate::private_content as pc;
    let Ok(mut ctx) = pc::channel::ContentContext::open(home, &keys.public_key().to_hex()) else {
        return;
    };
    if let Ok(mut relay) =
        pc::session::AuthenticatedContentRelay::connect(keys, &home.config.relay_url).await
    {
        let _ = pc::session::flush(&mut ctx.store, keys, &mut relay, 64).await;
        relay.disconnect().await;
    }
}

/// The `close` follow-up (§2.1): no turn fields, never paid; the session closes locally at once.
pub(super) async fn send_close(
    context: &BuyerContext,
    job_id: &str,
    seller: &str,
) -> Result<(), String> {
    let home = &context.home;
    let keys = buyer_keys(home)?;
    let draft = crate::session::followup_draft(&FollowUp {
        offer_id: job_id.to_owned(),
        seller_pubkey: seller.to_owned(),
        status: FollowUpStatus::Close,
        prev_result_id: None,
        turn: None,
        deadline_unix: None,
        text: None,
    })
    .map_err(|e| e.to_string())?;
    let draft = match private_chain(home, job_id)? {
        Some(chain) => {
            let policy = crate::private_content::runtime::Policy::from_home(home)
                .map_err(|e| e.to_string())?;
            crate::private_content::carriers::project(
                &chain.offer,
                &draft,
                Some(&chain.award),
                None,
                &policy.host,
            )
            .map_err(|e| e.to_string())?
        }
        None => draft,
    };
    let event = sign_draft(&keys, &draft)?;
    context
        .store
        .close_session(job_id, now_unix())
        .map_err(|e| e.to_string())?;
    match context.relay.publish(event).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!(
            "close published nowhere ({error}); the session is closed locally"
        )),
        Err(error) => Err(format!(
            "close published nowhere ({error}); the session is closed locally"
        )),
    }
}

/// `get_job.session` (§10). `None` unless sessions are enabled and the job is a session job.
pub(super) async fn view(context: &BuyerContext, view: &job_lifecycle::JobView) -> Option<Value> {
    if !context.home.config.sessions.enabled {
        return None;
    }
    let job_id = view.job_id.as_str();
    let award = context.store.award_record(job_id).ok().flatten()?;
    let row = context.store.session_row(job_id).ok().flatten();
    let mut facts = facts(context, job_id, &award, view).ok()?;
    if !facts.claim_session && row.is_none() {
        return Some(json!({"supported": false}));
    }
    let now = now_unix();
    if facts
        .turns
        .iter()
        .any(|t| matches!(t.state.as_str(), "pending" | "sent") && now >= t.deadline_unix as i64)
    {
        let guard = context.money_lock.lock().await;
        sweep_no_response(&context.store, &facts, now, &guard);
        facts = self::facts(context, job_id, &award, view).ok()?;
    }
    let latest = facts.turns.iter().max_by_key(|t| (t.turn, t.deadline_unix));
    let progress = progress(&facts);
    let status = match row.as_ref().map(|(_, s)| s.as_str()) {
        Some("closed") => "closed",
        Some("expired") => "expired",
        _ if progress.in_flight => "running",
        _ if latest.is_some_and(|t| t.state == "no_response") => "no_response",
        _ => "parked",
    };
    let turn = latest
        .filter(|t| live(t) || t.state == "no_response")
        .map_or(1, |t| t.turn);
    let result_id = latest
        .filter(|t| live(t))
        .and_then(|t| facts.delivered.get(&t.followup_id).cloned())
        .or(facts.turn1_result.clone());
    let result = view.results.iter().find(|r| {
        Some(&r.result_id) == result_id.as_ref()
            || (result_id.is_none()
                && r.seller_pubkey == award.seller_pubkey
                && r.is_turn_one_delivery())
    });
    let turn_status = result
        .and_then(|r| r.session.turn_status.clone())
        .unwrap_or_else(|| "done".into());
    let question = result
        .filter(|_| turn_status == crate::session::TurnStatus::NeedsInput.as_str())
        .and_then(|r| r.inline_answer.clone());
    Some(json!({
        "supported": facts.claim_session,
        "turn": turn,
        "status": status,
        "turn_status": turn_status,
        "question": question,
        "context_reset": result.is_some_and(|r| r.session.context_reset),
        "uncollected_turn": progress.uncollected,
        // The SELLER's lifetime decides; this is the default counted from the last delivery.
        "expires_hint": result.map(|r| r.created_at + crate::home::DEFAULT_SESSION_LIFETIME_SECS),
    }))
}

/// The turn a `collect` call settles (§8.4): `turn` when named (≥ 2), else the newest delivered,
/// uncollected turn once turn 1 is collected. `None` ⇒ turn 1, today's path.
pub(super) async fn collect_target(
    context: &BuyerContext,
    job_id: &str,
    turn: Option<u32>,
) -> Result<Option<BuyerTurn>, String> {
    if !context.home.config.sessions.enabled || turn == Some(1) {
        return Ok(None);
    }
    let turns = context
        .store
        .session_turns(job_id)
        .map_err(|e| e.to_string())?;
    if let Some(n) = turn {
        return turns
            .into_iter()
            .find(|t| t.turn == n && live(t))
            .map(Some)
            .ok_or_else(|| format!("no live turn {n} for job {job_id}"));
    }
    let Some(award) = context
        .store
        .award_record(job_id)
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let keys = buyer_keys(&context.home)?;
    let view = job_lifecycle::fetch_job_view_async(
        &context.home,
        &keys,
        job_id,
        RELAY_TIMEOUT,
        now_unix() as u64,
    )
    .await
    .map_err(|e| e.to_string())?;
    let facts = facts(context, job_id, &award, &view)?;
    if !facts.turn1_collected {
        return Ok(None);
    }
    Ok(facts
        .turns
        .into_iter()
        .filter(|t| t.state == "delivered")
        .max_by_key(|t| t.turn))
}

/// Settle one turn ≥ 2 under the money lock: pay-then-flip on the follow-up's reservation, exactly
/// as turn 1 (`settle_after_pay`), then mark it collected.
pub(super) async fn settle_turn(
    context: &BuyerContext,
    row: &BuyerTurn,
) -> Result<collect::CollectOutcome, String> {
    let _guard = context.money_lock.lock().await;
    let earlier: Vec<BuyerTurn> = context
        .store
        .session_turns(&row.job_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|t| t.turn < row.turn && t.state == "collected")
        .collect();
    if earlier.len() + 2 != row.turn as usize {
        return Err(format!("collect turn {} first", earlier.len() + 2));
    }
    let earlier: Vec<String> = earlier.into_iter().map(|t| t.followup_id).collect();
    let mut gate = if row.amount_sats == 0 {
        None
    } else {
        Some(BudgetGate::from_home(&context.home).map_err(|e| e.to_string())?)
    };
    let (outcome, _) = lifecycle::settle_after_pay(
        &context.store,
        &row.followup_id,
        now_unix(),
        || {
            collect::collect_turn_async(
                &context.home,
                gate.as_mut(),
                &row.job_id,
                &row.followup_id,
                row.turn,
                &earlier,
            )
        },
        |outcome| outcome.pay.amount_sats(),
    )
    .await
    .map_err(|error| match error {
        SettleError::Pay(error) => error.to_string(),
        SettleError::Store(error) => error.to_string(),
    })?;
    let result = job_lifecycle::load_accepted_bind(&context.home, &row.followup_id)
        .ok()
        .flatten()
        .map(|b| b.result_id);
    context
        .store
        .set_turn_state(&row.followup_id, "collected", result.as_deref(), now_unix())
        .map_err(|e| e.to_string())?;
    invalidate_reconcile_snapshot(context).await;
    Ok(outcome)
}

/// A seller FEEDBACK that names one of our turns (`["e", followup, "", "reply"]`): a refused or
/// failed turn frees its reservation and its number; `session_expired` also ends the session.
/// Author-gated on the awarded seller. Returns true when the event named a turn.
pub(super) async fn on_turn_feedback(context: &BuyerContext, event: &nostr_sdk::Event) -> bool {
    if !context.home.config.sessions.enabled {
        return false;
    }
    let tag = |name: &str| {
        event.tags.iter().find_map(|t| {
            let p = t.as_slice();
            (p.first().map(String::as_str) == Some(name))
                .then(|| p.get(1).cloned())
                .flatten()
        })
    };
    let Some(followup) = event.tags.iter().find_map(|t| {
        let p = t.as_slice();
        (p.first().map(String::as_str) == Some("e")
            && p.get(3).map(String::as_str) == Some("reply"))
        .then(|| p.get(1).cloned())
        .flatten()
    }) else {
        return false;
    };
    let Ok(Some(turn)) = context.store.turn(&followup) else {
        return false;
    };
    let seller_ok = matches!(context.store.award_record(&turn.job_id), Ok(Some(a)) if a.seller_pubkey == event.pubkey.to_hex());
    let failure = matches!(tag("status").as_deref(), Some("error" | "refusal"));
    if !seller_ok
        || !failure
        || !job_lifecycle::event_references_job(event, &turn.job_id)
        || !live(&turn)
    {
        return true;
    }
    let _guard = context.money_lock.lock().await;
    let now = now_unix();
    let _ = context
        .store
        .set_turn_state(&followup, "refused", None, now);
    let _ = context.store.release(&followup, now);
    let expired =
        tag("reason_code").as_deref() == Some(crate::gateway::ReasonCode::SessionExpired.as_str());
    if expired {
        let _ = context.store.expire_session(&turn.job_id, now);
    }
    crate::opline!(
        "buyer: session {} turn {} refused by the seller{}; reservation released",
        turn.job_id,
        turn.turn,
        if expired { " (session expired)" } else { "" }
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn store() -> (tempfile::TempDir, BuyerStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(&dir.path().join("buyer.sqlite")).unwrap();
        (dir, store)
    }
    fn ready() -> Facts {
        Facts {
            claim_session: true,
            turn1_delivered: true,
            turn1_collected: true,
            turn1_result: Some("r1".into()),
            ..Facts::default()
        }
    }
    const MONEY: Money = Money {
        price: 100,
        per_job_cap: 1000,
        committed: 100,
        cap: 1000,
        live: true,
    };

    /// Failure test 4: over the session cap, over the per-job cap, or with an uncollected turn,
    /// `reply` refuses with nothing signed, nothing reserved, nothing published.
    #[tokio::test]
    async fn refused_reply_signs_reserves_and_publishes_nothing() {
        let mut uncollected = ready();
        uncollected.turn1_collected = false;
        let mut no_session = ready();
        no_session.claim_session = false;
        let cases = [
            (ready(), Money { cap: 150, ..MONEY }, "session cap"),
            (
                ready(),
                Money {
                    per_job_cap: 50,
                    ..MONEY
                },
                "per-job cap",
            ),
            (uncollected, MONEY, "collect first"),
            (no_session, MONEY, "does not support sessions"),
            (
                ready(),
                Money {
                    live: false,
                    ..MONEY
                },
                "closed or expired",
            ),
        ];
        for (facts, money, why) in cases {
            let (_dir, store) = store();
            let touched = Cell::new(0);
            let error = advance(
                &store,
                "job",
                &facts,
                money,
                99,
                1,
                |_, _| {
                    touched.set(touched.get() + 1);
                    Ok(("f".into(), "{}".into()))
                },
                |_| {
                    touched.set(touched.get() + 1);
                    async { Ok(()) }
                },
                |_| {
                    touched.set(touched.get() + 1);
                    async { Ok(()) }
                },
            )
            .await
            .unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
            assert_eq!(
                touched.get(),
                0,
                "{why}: something was signed, reserved or published"
            );
            assert!(store.session_turns("job").unwrap().is_empty());
            assert!(store.reservation("f").unwrap().is_none());
        }
    }

    /// The order is sign → persist → reserve → publish; a refused reservation frees the turn
    /// number and publishes nothing; a failed publish leaves the SAME bytes for the retry.
    #[tokio::test]
    async fn reply_persists_before_reserving_and_resends_the_same_bytes() {
        let (_dir, store) = store();
        let published = std::cell::RefCell::new(Vec::new());
        let error = advance(
            &store,
            "job",
            &ready(),
            MONEY,
            99,
            1,
            |turn, prev| {
                assert_eq!((turn, prev), (2, "r1"));
                Ok(("f1".into(), "bytes-1".into()))
            },
            |_| async { Err("200 sat exceeds available 0 sat".to_owned()) },
            |row: BuyerTurn| {
                published.borrow_mut().push(row.event_json);
                async { Ok(()) }
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("exceeds available"));
        assert!(published.borrow().is_empty());
        assert_eq!(store.turn("f1").unwrap().unwrap().state, "refused");

        let mut facts = ready();
        facts.turns = store.session_turns("job").unwrap();
        let error = advance(
            &store,
            "job",
            &facts,
            MONEY,
            99,
            2,
            |turn, _| {
                assert_eq!(turn, 2, "a refused turn frees its number");
                Ok(("f2".into(), "bytes-2".into()))
            },
            |_| async { Ok(()) },
            |_| async { Err("relay down".to_owned()) },
        )
        .await
        .unwrap_err();
        assert!(error.contains("relay down"));
        assert_eq!(store.turn("f2").unwrap().unwrap().state, "pending");

        facts.turns = store.session_turns("job").unwrap();
        let row = advance(
            &store,
            "job",
            &facts,
            Money {
                committed: 200,
                cap: 200,
                ..MONEY
            },
            99,
            3,
            |_, _| panic!("a pending turn is re-sent, never re-signed"),
            |_| async { Ok(()) },
            |row: BuyerTurn| {
                published.borrow_mut().push(row.event_json);
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!((row.followup_id.as_str(), row.turn), ("f2", 2));
        assert_eq!(published.borrow().as_slice(), ["bytes-2"]);
        assert_eq!(store.turn("f2").unwrap().unwrap().state, "sent");
    }

    #[test]
    fn progress_tracks_the_newest_live_turn() {
        let turn = |id: &str, n: u32, state: &str, result: Option<&str>| BuyerTurn {
            followup_id: id.into(),
            job_id: "job".into(),
            turn: n,
            event_json: String::new(),
            amount_sats: 1,
            deadline_unix: 9,
            result_id: result.map(str::to_owned),
            state: state.into(),
        };
        let mut f = ready();
        f.turn1_delivered = false;
        assert!(progress(&f).in_flight, "turn 1 still running");
        let mut f = ready();
        f.turns = vec![
            turn("a", 2, "collected", Some("r2")),
            turn("b", 3, "no_response", None),
        ];
        assert_eq!(
            progress(&f).next,
            Some(Next::New {
                turn: 3,
                prev_result: "r2".into()
            })
        );
        f.turns.push(turn("c", 3, "sent", None));
        assert!(progress(&f).in_flight);
        f.delivered.insert("c".into(), "r3".into());
        assert_eq!(progress(&f).uncollected, Some(3));
    }

    /// `no_response` (§10): a turn past its deadline with no result is released; a delivered one
    /// past its deadline is kept for collect.
    #[test]
    fn deadline_without_result_releases_the_turn() {
        let (_dir, store) = store();
        for (id, n) in [("late", 2), ("done", 3)] {
            store.put_turn(id, "job", n, "{}", 10, 5, 1).unwrap();
            store.set_turn_state(id, "sent", None, 1).unwrap();
            store
                .reserve_at(id, 10, &reservations::MintCeiling::pooled(100), 1)
                .unwrap();
        }
        let mut facts = ready();
        facts.turns = store.session_turns("job").unwrap();
        facts.delivered.insert("done".into(), "r".into());
        let lock = tokio::sync::Mutex::new(());
        let guard = lock.try_lock().unwrap();
        sweep_no_response(&store, &facts, 6, &guard);
        assert_eq!(store.turn("late").unwrap().unwrap().state, "no_response");
        assert!(matches!(
            store.reservation("late").unwrap(),
            Some((reservations::ReservationState::Released, _))
        ));
        assert!(matches!(
            store.reservation("done").unwrap(),
            Some((reservations::ReservationState::Reserved, _))
        ));
    }
}
