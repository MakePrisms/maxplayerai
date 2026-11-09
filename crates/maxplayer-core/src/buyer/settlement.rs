//! `get_job.settlement`: who settles a delivery and where it stands, in one machine-readable object.
//!
//! Why it exists: `get_job wait_for=result` returns the moment the result event lands, a few
//! seconds BEFORE the delivery watcher ([`super::settle_awarded`]) has accepted, paid and
//! materialized it. At that instant the response showed `reservation_state: reserved`,
//! `awarded_delivery_pending: true` and (on a session job) `uncollected_turn: 1`, and nothing said
//! the daemon was already settling it, so a buyer agent asked its human "collect and close the
//! job?" about a job the daemon paid seconds later. This object says it outright.
//!
//! Read-only, and NEVER a money input: it reports the reservation ledger, the free-collect record,
//! the last refused settle ([`super::BuyerContext::settle_failures`]) and the review hold the
//! watcher itself honours. Nothing reads it back to decide anything.
use super::*;
use reservations::ReservationState;

/// The daemon settles this on its own (the delivery watcher covers it).
pub(super) const MODE_AUTO: &str = "auto";
/// The daemon does NOT settle this on its own: an explicit `collect` does.
pub(super) const MODE_MANUAL: &str = "manual";

/// Everything the hint is derived from, gathered by [`facts`]. Pure data so [`hint`] is testable
/// without a relay, a wallet or a daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Facts {
    /// A local award row exists (written only once the 3405 is public).
    pub awarded: bool,
    /// The job's reservation row; `None` = no row.
    pub reservation: Option<ReservationState>,
    /// A free collect record exists (`collects/<job_id>.json`).
    pub free_collected: bool,
    /// The awarded seller delivered (`awarded_delivery_pending`) or an accept-bind already exists.
    pub delivered: bool,
    /// `results/<job_id>` when that directory exists.
    pub results_path: Option<String>,
    /// The last refused settle of the job (watcher or `collect`), cleared by a successful one.
    pub failure: Option<String>,
    /// The review hold the watcher skips on: `(state, detail)` from `review-job-status`.
    pub review_hold: Option<(String, String)>,
    /// The newest session turn row, when sessions are enabled and a follow-up exists.
    pub latest_turn: Option<Turn>,
}

/// A session turn ≥ 2 as the hint needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Turn {
    pub turn: u32,
    /// The `session_turns.state`: pending | sent | delivered | collected | refused | no_response.
    pub state: String,
    pub failure: Option<String>,
    /// `results/<job_id>-t<n>` when that directory exists.
    pub results_path: Option<String>,
    /// The turn's price is still held (reserved, or a free turn): inside the watcher's work set
    /// (`BuyerStore::unsettled_turn_job_ids`, per row `session_reply::turn_funds_held`).
    pub funds_held: bool,
    /// The review hold the watcher's turn pass skips on ([`turn_review_hold`]).
    pub review_hold: Option<(String, String)>,
}

fn body(
    mode: &str,
    state: &str,
    turn: u32,
    results_path: Option<&str>,
    error: Option<&str>,
    note: String,
) -> Value {
    let mut value = json!({ "mode": mode, "state": state, "turn": turn, "note": note });
    if let Some(path) = results_path {
        value["results_path"] = json!(path);
    }
    if let Some(error) = error {
        value["error"] = json!(error);
    }
    value
}

const NOT_A_HUMAN_DECISION: &str =
    "Do not ask the human whether to collect: paying was decided when the claim was awarded.";

/// The `settlement` object for one job. See the `get_job` tool description for the contract.
pub(super) fn hint(f: &Facts) -> Value {
    let retry = DELIVERY_RECHECK_INTERVAL.as_secs();
    let settled = f.free_collected || f.reservation == Some(ReservationState::Spent);
    if settled {
        if let Some(turn) = f.latest_turn.as_ref().filter(|t| t.turn >= 2) {
            return turn_hint(turn);
        }
        return settled_turn_one(f);
    }
    if !f.awarded {
        if f.reservation == Some(ReservationState::Released) {
            return released();
        }
        return body(
            MODE_AUTO,
            "none",
            1,
            None,
            None,
            "Nothing to settle yet: no award is committed. The daemon auto-awards a payable claim, \
             then settles the delivery on its own; poll get_job."
                .to_owned(),
        );
    }
    match f.reservation {
        Some(ReservationState::Released) => released(),
        // The watcher's work set is awards JOIN reservations WHERE state='reserved'
        // (`BuyerStore::awarded_unsettled_job_ids`): an award with no reservation row is outside it.
        None if f.delivered => body(
            MODE_MANUAL,
            "collect_required",
            1,
            None,
            None,
            format!(
                "Delivered, but the daemon holds no reservation for this award, so its delivery \
                 watcher does not settle it. Call collect for this job yourself. \
                 {NOT_A_HUMAN_DECISION}"
            ),
        ),
        None => body(
            MODE_MANUAL,
            "awaiting_delivery",
            1,
            None,
            None,
            "Awarded, but the daemon holds no reservation for this award, so its delivery watcher \
             will not settle it: once get_job shows the delivery, call collect."
                .to_owned(),
        ),
        // Returned above; kept total so a refactor can never panic the daemon here.
        Some(ReservationState::Spent) => settled_turn_one(f),
        Some(ReservationState::Reserved) => {
            if let Some(error) = f.failure.as_deref() {
                return body(
                    MODE_AUTO,
                    "failed",
                    1,
                    None,
                    Some(error),
                    format!(
                        "The daemon's last attempt to settle this delivery was refused (see \
                         error). It retries on each new relay event and at least every {retry}s. \
                         Call collect to retry now and read the full refusal; collect never pays \
                         twice. If it keeps failing, read the maxplayer-debug-buying skill."
                    ),
                );
            }
            if let Some((state, detail)) = f.review_hold.as_ref() {
                if state == "pending" {
                    return body(
                        MODE_AUTO,
                        "settling",
                        1,
                        None,
                        None,
                        format!(
                            "Delivered; the delivery review is in flight. The daemon settles it on \
                             its own once the review passes (on its next pass, within {retry}s). \
                             Re-check with get_job shortly. {NOT_A_HUMAN_DECISION}"
                        ),
                    );
                }
                return body(MODE_AUTO, "failed", 1, None, Some(detail), review_note(state));
            }
            if f.delivered {
                return body(
                    MODE_AUTO,
                    "settling",
                    1,
                    None,
                    None,
                    format!(
                        "Delivered. The buyer daemon is settling this on its own (accept, verify, \
                         pay, materialize into results/<job_id>), normally within seconds of the \
                         delivery. Re-check with get_job shortly; collect is not needed. \
                         {NOT_A_HUMAN_DECISION}"
                    ),
                );
            }
            body(
                MODE_AUTO,
                "awaiting_delivery",
                1,
                None,
                None,
                "Awarded; waiting for the seller's delivery. The daemon settles it on its own when \
                 it lands (poll get_job wait_for=result); collect is not needed."
                    .to_owned(),
            )
        }
    }
}

/// The note for a turn-1 delivery whose review did not pass. `collect` re-runs the review (accept
/// reviews a delivery that has no bind yet), so it retries a review that did not FINISH (`error`:
/// reviewer down, timed out, failed) but can never override one that refused the delivery.
fn review_note(state: &str) -> String {
    match state {
        "error" => format!(
            "The delivery review did not finish ({state}: the reviewer was unreachable, timed out \
             or failed; see error), and the daemon does not retry that on its own. Call collect to \
             retry the review and the settle now; it never pays twice. If it keeps failing, see the \
             maxplayer-debug-buying skill. {NOT_A_HUMAN_DECISION}"
        ),
        _ => format!(
            "The delivery review refused this delivery ({state}; see error). collect re-runs the \
             review, so it cannot override a refusal; nothing is paid unless a review passes. See \
             the maxplayer-debug-buying skill."
        ),
    }
}

fn settled_turn_one(f: &Facts) -> Value {
    let mode = if f.awarded { MODE_AUTO } else { MODE_MANUAL };
    body(
        mode,
        "settled",
        1,
        f.results_path.as_deref(),
        None,
        "Settled: the delivery was accepted, verified, paid (a free job pays nothing) and \
         materialized; the files are under results_path (absent only when an earlier collect used \
         out=). commit_oid, files and pay here are what collect would return, so collect is not \
         needed: it is idempotent and would only repeat them, without paying again."
            .to_owned(),
    )
}

fn released() -> Value {
    body(
        MODE_AUTO,
        "none",
        1,
        None,
        None,
        "Nothing to settle: this job's reservation was released (no payable delivery: a seller \
         failure report, a missed deadline, or an award that never completed). Nothing will be \
         paid, and there is nothing to collect."
            .to_owned(),
    )
}

/// A session turn ≥ 2. The delivery watcher settles these too (`session_reply::settle_delivered_turns`,
/// strictly in turn order, same `settle_turn` path as `collect turn=<n>`), so the mode is `auto`
/// unless the turn's price is not held (then nothing the daemon holds can pay it: `manual`).
fn turn_hint(turn: &Turn) -> Value {
    let n = turn.turn;
    let retry = DELIVERY_RECHECK_INTERVAL.as_secs();
    match turn.state.as_str() {
        "collected" => body(
            MODE_AUTO,
            "settled",
            n,
            turn.results_path.as_deref(),
            None,
            format!(
                "Turn {n} is settled: accepted, verified, paid (a free job pays nothing) and \
                 materialized; its files are under results_path. collect is not needed: it never \
                 pays twice."
            ),
        ),
        "delivered" if !turn.funds_held => body(
            MODE_MANUAL,
            "collect_required",
            n,
            None,
            turn.failure.as_deref(),
            format!(
                "Turn {n} was delivered, but the daemon holds no reservation for it, so its \
                 delivery watcher does not settle it. Call collect with turn={n} yourself. \
                 {NOT_A_HUMAN_DECISION}"
            ),
        ),
        "delivered" => {
            if let Some(error) = turn.failure.as_deref() {
                return body(
                    MODE_AUTO,
                    "failed",
                    n,
                    None,
                    Some(error),
                    format!(
                        "The daemon's last attempt to settle turn {n} was refused (see error). It \
                         retries on each new relay event and at least every {retry}s. Call collect \
                         with turn={n} to retry now and read the full refusal; collect never pays \
                         twice. If it keeps failing, read the maxplayer-debug-buying skill."
                    ),
                );
            }
            if let Some((state, detail)) = turn.review_hold.as_ref() {
                if state == "pending" {
                    return body(
                        MODE_AUTO,
                        "settling",
                        n,
                        None,
                        None,
                        format!(
                            "Turn {n} was delivered; its delivery review is in flight. The daemon \
                             settles it on its own once the review passes (within {retry}s). \
                             Re-check with get_job shortly. {NOT_A_HUMAN_DECISION}"
                        ),
                    );
                }
                return body(
                    MODE_AUTO,
                    "failed",
                    n,
                    None,
                    Some(detail),
                    format!(
                        "The delivery review of turn {n} did not pass ({state}; see error), and the \
                         daemon does not retry that on its own. Fix the cause, then call collect \
                         with turn={n} to retry explicitly; see the maxplayer-debug-buying skill."
                    ),
                );
            }
            body(
                MODE_AUTO,
                "settling",
                n,
                None,
                None,
                format!(
                    "Turn {n} was delivered. The buyer daemon is settling it on its own (accept, \
                     verify, pay this turn's reserved price, materialize into \
                     results/<job_id>-t{n}), normally within seconds. Re-check with get_job \
                     shortly; collect is not needed. {NOT_A_HUMAN_DECISION}"
                ),
            )
        }
        "refused" | "no_response" => body(
            MODE_AUTO,
            "none",
            n,
            None,
            None,
            format!(
                "Turn {n} ended without a payable result ({}); its reservation was released and \
                 there is nothing to collect for it.",
                turn.state
            ),
        ),
        _ => body(
            MODE_AUTO,
            "awaiting_delivery",
            n,
            None,
            None,
            format!(
                "Turn {n} is in flight; poll get_job wait_for=result. The daemon settles it on its \
                 own when it lands; collect is not needed."
            ),
        ),
    }
}

/// The review hold the delivery watcher skips a job on: a delivery review that is pending or did
/// not pass, for a job with no accept-bind yet, when buyer delivery review is enabled for its
/// seller. Shared by the watcher and [`facts`] so the hint can never disagree with what the watcher
/// actually does.
pub(super) fn auto_settle_review_hold(
    context: &BuyerContext,
    job_id: &str,
) -> Result<Option<crate::review::state::Status>, String> {
    let review_enabled = context
        .store
        .award_record(job_id)
        .ok()
        .flatten()
        .map(|award| {
            context
                .home
                .config
                .review
                .enabled(crate::kinds::JOB_RESULT_KIND, &award.seller_pubkey)
                .unwrap_or(true)
        })
        .unwrap_or(context.home.config.review.buyer_delivery);
    if !review_enabled
        || !matches!(
            job_lifecycle::load_accepted_bind(&context.home, job_id),
            Ok(None)
        )
    {
        return Ok(None);
    }
    crate::review::state::blocked_job(&context.home.root, job_id)
}

/// The review hold on session turn `followup_id` of `job_id`: the turn-≥ 2 twin of
/// [`auto_settle_review_hold`], shared by the watcher's turn pass and [`facts`].
///
/// Keying: a turn's buyer delivery review runs in `accept_turn_async` with subject `{offer: job_id,
/// event: <turn result id>}`, and `review::state::write` files every RESULT-kind status under the
/// OFFER (`review-job-status/<job_id>.json`), the same file turn 1 uses. So the hold is read off the
/// job's file, then attributed: a status whose subject is turn 1's or an already settled turn's
/// result is history, not this turn's hold. Any other blocking status is this turn's, because
/// `reply` refuses to start turn n + 1 while turn n is unsettled, so at most one turn is ever
/// delivered and unsettled per job. No hold once the turn's accept-bind exists (its review passed:
/// the pass is recorded before the bind is written), or when delivery review is off for the seller.
pub(super) fn turn_review_hold(
    context: &BuyerContext,
    job_id: &str,
    followup_id: &str,
) -> Result<Option<crate::review::state::Status>, String> {
    let review_enabled = context
        .store
        .award_record(job_id)
        .ok()
        .flatten()
        .map(|award| {
            context
                .home
                .config
                .review
                .enabled(crate::kinds::JOB_RESULT_KIND, &award.seller_pubkey)
                .unwrap_or(true)
        })
        .unwrap_or(context.home.config.review.buyer_delivery);
    if !review_enabled
        || !matches!(
            job_lifecycle::load_accepted_bind(&context.home, followup_id),
            Ok(None)
        )
    {
        return Ok(None);
    }
    let Some(status) = crate::review::state::blocked_job(&context.home.root, job_id)? else {
        return Ok(None);
    };
    let turn_one = job_lifecycle::load_accepted_bind(&context.home, job_id)
        .ok()
        .flatten()
        .map(|bind| bind.result_id);
    let settled = context
        .store
        .session_turns(job_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|t| t.state == "collected" && t.followup_id != followup_id)
        .filter_map(|t| t.result_id);
    let history = turn_one
        .into_iter()
        .chain(settled)
        .any(|result| result == status.subject.event);
    Ok((!history).then_some(status))
}

/// Record (`Some`) or clear (`None`) the last refused settle under `key` — the job id, or a turn's
/// follow-up id.
pub(super) fn note_refusal(context: &BuyerContext, key: &str, refusal: Option<String>) {
    let mut failures = context
        .settle_failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match refusal {
        Some(error) => {
            failures.insert(key.to_owned(), error);
        }
        None => {
            failures.remove(key);
        }
    }
}

fn last_refusal(context: &BuyerContext, key: &str) -> Option<String> {
    context
        .settle_failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .cloned()
}

fn existing_dir(home: &MaxplayerHome, name: &str) -> Option<String> {
    collect::results_dest(home, name, None)
        .ok()
        .filter(|path| path.is_dir())
        .map(|path| path.display().to_string())
}

/// Gather [`Facts`] for `job_id`. `Err` when the reservation ledger cannot be read: the hint is then
/// `null`, never a guess (an unreadable ledger is unknown, not "nothing reserved").
pub(super) fn facts(
    context: &BuyerContext,
    job_id: &str,
    awarded: bool,
    delivered: bool,
) -> Result<Facts, String> {
    let reservation = context
        .store
        .reservation(job_id)
        .map_err(|e| e.to_string())?
        .map(|(state, _)| state);
    let review_hold = if awarded {
        match auto_settle_review_hold(context, job_id) {
            Ok(status) => status.map(|s| (s.state, s.detail)),
            // The watcher skips a job whose hold it cannot read, so the hint reports it as held.
            Err(error) => Some(("error".to_owned(), error)),
        }
    } else {
        None
    };
    let latest_turn = if context.home.config.sessions.enabled {
        context
            .store
            .session_turns(job_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .max_by_key(|t| (t.turn, t.deadline_unix))
            .map(|t| Turn {
                turn: t.turn,
                failure: last_refusal(context, &t.followup_id),
                results_path: existing_dir(
                    &context.home,
                    &crate::session::collect_dir_name(job_id, t.turn),
                ),
                funds_held: session_reply::turn_funds_held(context, &t),
                review_hold: (t.state == "delivered")
                    .then(|| match turn_review_hold(context, job_id, &t.followup_id) {
                        Ok(status) => status.map(|s| (s.state, s.detail)),
                        // The watcher skips a turn whose hold it cannot read: report it as held.
                        Err(error) => Some(("error".to_owned(), error)),
                    })
                    .flatten(),
                state: t.state,
            })
    } else {
        None
    };
    Ok(Facts {
        awarded,
        reservation,
        free_collected: collect::free_collect_record_path(&context.home, job_id).exists(),
        delivered,
        results_path: existing_dir(&context.home, job_id),
        failure: last_refusal(context, job_id),
        review_hold,
        latest_turn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reserved_award() -> Facts {
        Facts {
            awarded: true,
            reservation: Some(ReservationState::Reserved),
            ..Default::default()
        }
    }

    fn state(value: &Value) -> (&str, &str, u64) {
        (
            value["mode"].as_str().expect("mode"),
            value["state"].as_str().expect("state"),
            value["turn"].as_u64().expect("turn"),
        )
    }

    fn note(value: &Value) -> &str {
        value["note"].as_str().expect("note")
    }

    // The incident shape: awarded, reserved, delivered, nothing recorded. The response must say
    // the daemon is settling it, and must tell the agent not to put collect to the human.
    #[test]
    fn a_fresh_delivery_reads_settling_and_forbids_asking_the_human() {
        let hint = hint(&Facts {
            delivered: true,
            ..reserved_award()
        });
        assert_eq!(state(&hint), ("auto", "settling", 1));
        assert!(note(&hint).contains("settling this on its own"), "{hint}");
        assert!(
            note(&hint).contains("Do not ask the human whether to collect"),
            "{hint}"
        );
        assert!(
            note(&hint).contains("Re-check with get_job shortly"),
            "{hint}"
        );
        assert!(hint.get("results_path").is_none() && hint.get("error").is_none());
    }

    #[test]
    fn a_spent_reservation_reads_settled_with_the_results_path() {
        let hint = hint(&Facts {
            awarded: true,
            reservation: Some(ReservationState::Spent),
            delivered: true,
            results_path: Some("/h/results/j".into()),
            // A stale refusal from before the settle must not outrank the ledger.
            failure: Some("old".into()),
            ..Default::default()
        });
        assert_eq!(state(&hint), ("auto", "settled", 1));
        assert_eq!(hint["results_path"], json!("/h/results/j"));
        assert!(note(&hint).contains("collect is not needed"), "{hint}");
        assert!(note(&hint).contains("without paying again"), "{hint}");
        assert!(hint.get("error").is_none());
    }

    // A free job: the watcher flips its zero reservation to spent; the free record is the
    // independent evidence when no reservation row exists (an externally accepted free job).
    #[test]
    fn a_free_collect_record_reads_settled() {
        let hint = hint(&Facts {
            awarded: false,
            reservation: None,
            free_collected: true,
            results_path: Some("/h/results/j".into()),
            ..Default::default()
        });
        assert_eq!(state(&hint), ("manual", "settled", 1));
        assert_eq!(hint["results_path"], json!("/h/results/j"));
    }

    #[test]
    fn a_recorded_refusal_reads_failed_and_points_at_collect() {
        let hint = hint(&Facts {
            delivered: true,
            failure: Some("wallet holds no funds".into()),
            ..reserved_award()
        });
        assert_eq!(state(&hint), ("auto", "failed", 1));
        assert_eq!(hint["error"], json!("wallet holds no funds"));
        assert!(note(&hint).contains("Call collect to retry now"), "{hint}");
        assert!(note(&hint).contains("maxplayer-debug-buying"), "{hint}");
        assert!(note(&hint).contains("never pays twice"), "{hint}");
    }

    #[test]
    fn a_review_hold_reads_failed_unless_the_review_is_still_pending() {
        let held = hint(&Facts {
            delivered: true,
            review_hold: Some(("error".into(), "review timed out".into())),
            ..reserved_award()
        });
        assert_eq!(state(&held), ("auto", "failed", 1));
        assert_eq!(held["error"], json!("review timed out"));
        assert!(
            note(&held).contains("does not retry that on its own"),
            "{held}"
        );
        assert!(note(&held).contains("Call collect to retry the review"), "{held}");

        // A refusal is not retryable by collect: it re-runs the review, it does not override it.
        for refused in ["policy_refused", "input_refused"] {
            let hint = hint(&Facts {
                delivered: true,
                review_hold: Some((refused.into(), "threshold".into())),
                ..reserved_award()
            });
            assert_eq!(state(&hint), ("auto", "failed", 1));
            assert!(note(&hint).contains("cannot override a refusal"), "{hint}");
            assert!(!note(&hint).contains("Call collect to retry"), "{hint}");
        }

        let pending = hint(&Facts {
            delivered: true,
            review_hold: Some(("pending".into(), "Waiting".into())),
            ..reserved_award()
        });
        assert_eq!(state(&pending), ("auto", "settling", 1));
        assert!(note(&pending).contains("Do not ask the human"), "{pending}");
    }

    #[test]
    fn nothing_to_settle_reads_none_or_awaiting_delivery() {
        assert_eq!(state(&hint(&Facts::default())), ("auto", "none", 1));
        let released = hint(&Facts {
            awarded: true,
            reservation: Some(ReservationState::Released),
            delivered: true,
            ..Default::default()
        });
        assert_eq!(state(&released), ("auto", "none", 1));
        assert!(note(&released).contains("nothing to collect"), "{released}");
        assert_eq!(
            state(&hint(&reserved_award())),
            ("auto", "awaiting_delivery", 1)
        );
    }

    // Outside the watcher's work set (awards JOIN reservations): an award with no reservation row.
    #[test]
    fn an_award_without_a_reservation_is_manual() {
        let awarded = Facts {
            awarded: true,
            ..Default::default()
        };
        assert_eq!(state(&hint(&awarded)), ("manual", "awaiting_delivery", 1));
        let delivered = hint(&Facts {
            delivered: true,
            ..awarded
        });
        assert_eq!(state(&delivered), ("manual", "collect_required", 1));
    }

    // Turns ≥ 2 are auto-settled by the delivery watcher, exactly like turn 1: the hint says so,
    // still forbids asking the human, and falls back to manual only when no price is held.
    #[test]
    fn session_turns_two_and_later_are_auto() {
        let base = Facts {
            awarded: true,
            reservation: Some(ReservationState::Spent),
            ..Default::default()
        };
        let with = |turn: Turn| {
            hint(&Facts {
                latest_turn: Some(Turn { turn: 2, ..turn }),
                ..base.clone()
            })
        };
        let held = |state: &str| Turn {
            state: state.into(),
            funds_held: true,
            ..Default::default()
        };
        let due = with(held("delivered"));
        assert_eq!(state(&due), ("auto", "settling", 2));
        assert!(note(&due).contains("settling it on its own"), "{due}");
        assert!(note(&due).contains("collect is not needed"), "{due}");
        assert!(note(&due).contains("Do not ask the human"), "{due}");
        assert!(!note(&due).contains("does NOT settle"), "{due}");

        let refused = with(Turn {
            failure: Some("integrity".into()),
            ..held("delivered")
        });
        assert_eq!(state(&refused), ("auto", "failed", 2));
        assert_eq!(refused["error"], json!("integrity"));
        assert!(note(&refused).contains("collect with turn=2"), "{refused}");

        let review_failed = with(Turn {
            review_hold: Some(("error".into(), "review timed out".into())),
            ..held("delivered")
        });
        assert_eq!(state(&review_failed), ("auto", "failed", 2));
        assert_eq!(review_failed["error"], json!("review timed out"));
        assert!(
            note(&review_failed).contains("does not retry that on its own"),
            "{review_failed}"
        );
        let review_pending = with(Turn {
            review_hold: Some(("pending".into(), "Waiting".into())),
            ..held("delivered")
        });
        assert_eq!(state(&review_pending), ("auto", "settling", 2));

        let done = with(Turn {
            results_path: Some("/h/results/j-t2".into()),
            ..held("collected")
        });
        assert_eq!(state(&done), ("auto", "settled", 2));
        assert_eq!(done["results_path"], json!("/h/results/j-t2"));

        assert_eq!(state(&with(held("sent"))), ("auto", "awaiting_delivery", 2));
        assert_eq!(
            state(&with(Turn {
                state: "no_response".into(),
                ..Default::default()
            })),
            ("auto", "none", 2)
        );

        // No price held (outside the watcher's work set): collect is required, still not a human
        // decision.
        let unheld = with(Turn {
            state: "delivered".into(),
            ..Default::default()
        });
        assert_eq!(state(&unheld), ("manual", "collect_required", 2));
        assert!(note(&unheld).contains("collect with turn=2"), "{unheld}");
        assert!(note(&unheld).contains("Do not ask the human"), "{unheld}");
    }
}
