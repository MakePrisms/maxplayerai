//! Session jobs — one job, many turns (`docs/specs/session-jobs.md`).
//!
//! This module holds the wire shapes (§2) and every pure decision rule the spec names (§4–§10),
//! so each money-path and security invariant is a function with its own failure test. Nothing
//! here performs I/O. Everything is inert unless `[sessions] enabled = true`: [`advertise_on_claim`]
//! is the only place a `session` tag can enter a claim, and it is a no-op when disabled, which is
//! what keeps every claim/offer/result/accept byte identical to today's (failure test 13).
use crate::gateway::{EventDraft, TagSpec};
use crate::home::SessionsConfig;
use crate::kinds::{JOB_ACCEPT_KIND, JOB_CLAIM_KIND, JOB_RESULT_KIND, SESSION_FOLLOWUP_KIND};
use sha2::{Digest, Sha256};

/// `["session","1"]` on a CLAIM: absent ⇒ no session support (fail-closed, §2.2).
pub const SESSION_TAG: &str = "session";
pub const SESSION_TAG_VALUE: &str = "1";
/// Inputs-file protocol between host and in-container orchestrator (§5.5). A parked container
/// whose recorded orchestrator speaks a lower version is expired rather than started.
pub const SESSION_PROTOCOL: u32 = 1;
/// The agent marks a question by beginning its final message with this (§7.2).
pub const NEEDS_INPUT_MARKER: &str = "MAXPLAYER-NEEDS-INPUT-V1";
/// Buyer default per-turn deadline (§4), same as an offer's.
pub const DEFAULT_TURN_DEADLINE_SECS: u64 = 3600;
/// Extra start margin on top of wrap-up + push margin (§4).
pub const RESUME_START_MARGIN_SECS: u64 = 120;
/// `PUSH_MARGIN_SECS` from the delivery orchestrator (§4), duplicated here because that module is
/// behind `git-delivery`; a `git-delivery` test pins the two together.
pub const PUSH_MARGIN_SECS: u64 = 300;
/// Reviewer context bound (`review::MAX_INPUT_BYTES`, §9).
pub const REVIEW_CONTEXT_MAX_BYTES: usize = crate::review::MAX_INPUT_BYTES;
/// `context_reset` prompt history bound (§6.3).
pub const RESET_HISTORY_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    WrongKind(u16),
    Malformed(&'static str),
}
impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongKind(k) => write!(f, "session: not a follow-up (kind {k})"),
            Self::Malformed(m) => write!(f, "session: malformed follow-up ({m})"),
        }
    }
}
impl std::error::Error for SessionError {}

// ───────────────────────────── §2.1 FOLLOW-UP ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUpStatus {
    Turn,
    Close,
}
impl FollowUpStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Close => "close",
        }
    }
}

/// A parsed (or to-be-drafted) follow-up. `text` is the PUBLIC message (`["i", text]`); a private
/// job carries its message as a `ContentType::FollowUp` body instead and leaves this `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    pub offer_id: String,
    pub seller_pubkey: String,
    pub status: FollowUpStatus,
    /// The RESULT this turn continues (required for `turn`).
    pub prev_result_id: Option<String>,
    /// Turn number, `n >= 2` (required for `turn`).
    pub turn: Option<u32>,
    /// This turn's deadline (required for `turn`).
    pub deadline_unix: Option<u64>,
    pub text: Option<String>,
}

/// Kind-3412 draft. Tag order is fixed so the bytes a buyer persists before publishing (write-once,
/// §8.1) are deterministic.
pub fn followup_draft(f: &FollowUp) -> Result<EventDraft, SessionError> {
    validate_shape(f)?;
    let mut tags = vec![TagSpec::new(["e", &f.offer_id, "", "root"])];
    if f.status == FollowUpStatus::Turn {
        tags.push(TagSpec::new([
            "e",
            f.prev_result_id.as_deref().unwrap_or_default(),
            "",
            "reply",
        ]));
    }
    tags.push(TagSpec::new(["status", f.status.as_str()]));
    if f.status == FollowUpStatus::Turn {
        tags.push(TagSpec::new([
            "turn",
            &f.turn.unwrap_or_default().to_string(),
        ]));
        tags.push(TagSpec::new([
            "param",
            "deadline",
            &f.deadline_unix.unwrap_or_default().to_string(),
        ]));
        if let Some(text) = &f.text {
            tags.push(TagSpec::new(["i", text]));
        }
    }
    tags.push(TagSpec::new(["p", &f.seller_pubkey]));
    tags.push(TagSpec::new(["t", crate::gateway::MAXPLAYER_TAG]));
    tags.push(TagSpec::new(["v", crate::gateway::PROTOCOL_VERSION]));
    Ok(EventDraft::new(SESSION_FOLLOWUP_KIND, tags, ""))
}

fn validate_shape(f: &FollowUp) -> Result<(), SessionError> {
    if f.offer_id.is_empty() || f.seller_pubkey.is_empty() {
        return Err(SessionError::Malformed("missing root or seller"));
    }
    match f.status {
        FollowUpStatus::Turn => {
            if f.prev_result_id.as_deref().is_none_or(str::is_empty) {
                return Err(SessionError::Malformed("turn without reply"));
            }
            if f.turn.is_none_or(|n| n < 2) {
                return Err(SessionError::Malformed("turn number must be >= 2"));
            }
            if f.deadline_unix.is_none() {
                return Err(SessionError::Malformed("turn without deadline"));
            }
        }
        FollowUpStatus::Close => {
            if f.turn.is_some() || f.deadline_unix.is_some() || f.text.is_some() {
                return Err(SessionError::Malformed("close carries turn fields"));
            }
        }
    }
    Ok(())
}

/// Parse a kind-3412 draft. Duplicated singleton tags are refused, never resolved by position.
pub fn parse_followup(event: &EventDraft) -> Result<FollowUp, SessionError> {
    if event.kind != SESSION_FOLLOWUP_KIND {
        return Err(SessionError::WrongKind(event.kind));
    }
    let single = |name: &str| -> Result<Option<&TagSpec>, SessionError> {
        let mut it = event.tags.iter().filter(|t| t.first() == Some(name));
        let first = it.next();
        if it.next().is_some() {
            return Err(SessionError::Malformed("duplicate tag"));
        }
        Ok(first)
    };
    let marked = |marker: &str| -> Result<Option<String>, SessionError> {
        let rows: Vec<_> = event
            .tags
            .iter()
            .filter(|t| t.first() == Some("e") && t.0.get(3).map(String::as_str) == Some(marker))
            .collect();
        match rows.as_slice() {
            [] => Ok(None),
            [one] => Ok(one.value().map(str::to_owned)),
            _ => Err(SessionError::Malformed("duplicate e marker")),
        }
    };
    let offer_id = marked("root")?.ok_or(SessionError::Malformed("missing root"))?;
    let status = match single("status")?.and_then(TagSpec::value) {
        Some("turn") => FollowUpStatus::Turn,
        Some("close") => FollowUpStatus::Close,
        _ => return Err(SessionError::Malformed("status")),
    };
    let seller_pubkey = single("p")?
        .and_then(TagSpec::value)
        .ok_or(SessionError::Malformed("missing p"))?
        .to_owned();
    let turn = match single("turn")?.and_then(TagSpec::value) {
        Some(v) => Some(
            v.parse::<u32>()
                .map_err(|_| SessionError::Malformed("turn"))?,
        ),
        None => None,
    };
    let deadlines: Vec<_> = event
        .tags
        .iter()
        .filter(|t| {
            t.first() == Some("param") && t.0.get(1).map(String::as_str) == Some("deadline")
        })
        .collect();
    let deadline_unix = match deadlines.as_slice() {
        [] => None,
        [one] => Some(
            one.0
                .get(2)
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or(SessionError::Malformed("deadline"))?,
        ),
        _ => return Err(SessionError::Malformed("duplicate deadline")),
    };
    let f = FollowUp {
        offer_id,
        seller_pubkey,
        status,
        prev_result_id: marked("reply")?,
        turn,
        deadline_unix,
        text: single("i")?.and_then(TagSpec::value).map(str::to_owned),
    };
    validate_shape(&f)?;
    Ok(f)
}

/// Why a seller ignores a follow-up (§2.1 reader rule, failure test 1). Ignored means nothing
/// starts and nothing is published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ignore {
    NotTheAwardBuyer,
    NotThisSeat,
    OtherJob,
    /// `turn` is not exactly last delivered + 1 (skipped or duplicate).
    TurnOutOfOrder {
        expected: u32,
        got: u32,
    },
    SessionEnded,
}

/// Seller-side admission of a parsed follow-up. `last_delivered_turn` is 1 after the first
/// delivery. A `close` is admitted at any turn from the award's buyer.
pub fn admit_followup(
    f: &FollowUp,
    author: &str,
    award_buyer: &str,
    seat: &str,
    job_offer_id: &str,
    last_delivered_turn: u32,
    session_live: bool,
) -> Result<(), Ignore> {
    if author != award_buyer {
        return Err(Ignore::NotTheAwardBuyer);
    }
    if f.seller_pubkey != seat {
        return Err(Ignore::NotThisSeat);
    }
    if f.offer_id != job_offer_id {
        return Err(Ignore::OtherJob);
    }
    if !session_live {
        return Err(Ignore::SessionEnded);
    }
    if f.status == FollowUpStatus::Turn {
        let expected = last_delivered_turn.saturating_add(1);
        let got = f.turn.unwrap_or_default();
        if got != expected {
            return Err(Ignore::TurnOutOfOrder { expected, got });
        }
    }
    Ok(())
}

// ───────────────────────────── §2.2–§2.4 tags ─────────────────────────────

/// Append `["session","1"]` to a claim draft — ONLY when sessions are enabled and the seat runs
/// docker with container delivery (§2.2, §5.1). With `enabled = false` the draft is untouched.
pub fn advertise_on_claim(
    draft: &mut EventDraft,
    cfg: &SessionsConfig,
    docker_container_seat: bool,
) {
    if draft.kind != JOB_CLAIM_KIND || !cfg.enabled || !docker_container_seat {
        return;
    }
    if claim_supports_sessions(&draft.tags) {
        return;
    }
    // Before the trailing t/v tags, so the claim reads in the usual order.
    let at = draft
        .tags
        .iter()
        .position(|t| t.first() == Some("t"))
        .unwrap_or(draft.tags.len());
    draft
        .tags
        .insert(at, TagSpec::new([SESSION_TAG, SESSION_TAG_VALUE]));
}

/// Exactly one `["session","1"]` row. Anything else — absent, another value, a duplicate — reads
/// as no session support.
pub fn claim_supports_sessions(tags: &[TagSpec]) -> bool {
    let rows: Vec<_> = tags
        .iter()
        .filter(|t| t.first() == Some(SESSION_TAG))
        .collect();
    matches!(rows.as_slice(), [one] if one.0.len() == 2 && one.value() == Some(SESSION_TAG_VALUE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Done,
    NeedsInput,
    OutOfTime,
}
impl TurnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::NeedsInput => "needs_input",
            Self::OutOfTime => "out_of_time",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "done" => Some(Self::Done),
            "needs_input" => Some(Self::NeedsInput),
            "out_of_time" => Some(Self::OutOfTime),
            _ => None,
        }
    }
    /// §7.3: `out_of_time` when the wrap-up cancel fired, `needs_input` from the marker, else done.
    pub fn from_turn(cancelled_for_wrap_up: bool, final_message: &str) -> Self {
        if cancelled_for_wrap_up {
            Self::OutOfTime
        } else if final_message.trim_start().starts_with(NEEDS_INPUT_MARKER) {
            Self::NeedsInput
        } else {
            Self::Done
        }
    }
}

/// The per-turn RESULT tags (§2.3), appended only on a session job. Turn 1 `done` without a reset
/// adds nothing, so a session job's first result is byte-identical to a one-shot result.
pub fn append_result_tags(
    draft: &mut EventDraft,
    followup_id: Option<&str>,
    status: TurnStatus,
    context_reset: bool,
    turn_creq: Option<&str>,
) {
    if draft.kind != JOB_RESULT_KIND {
        return;
    }
    let at = draft
        .tags
        .iter()
        .position(|t| t.first() == Some("t"))
        .unwrap_or(draft.tags.len());
    let mut extra = Vec::new();
    if let Some(id) = followup_id {
        extra.push(TagSpec::new(["e", id, "", "reply"]));
    }
    if status != TurnStatus::Done {
        extra.push(TagSpec::new(["turn_status", status.as_str()]));
    }
    if context_reset {
        extra.push(TagSpec::new(["context", "reset"]));
    }
    if let Some(creq) = turn_creq {
        extra.push(TagSpec::new(["creq", creq]));
    }
    draft.tags.splice(at..at, extra);
}

/// Read a session result's turn status: absent ⇒ `done`; an unknown value is refused.
pub fn result_turn_status(tags: &[TagSpec]) -> Result<TurnStatus, SessionError> {
    match tags.iter().find(|t| t.first() == Some("turn_status")) {
        None => Ok(TurnStatus::Done),
        Some(t) => t
            .value()
            .and_then(TurnStatus::parse)
            .ok_or(SessionError::Malformed("turn_status")),
    }
}

pub fn result_context_reset(tags: &[TagSpec]) -> bool {
    tags.iter().any(|t| t.0 == ["context", "reset"])
}

/// A session ACCEPT binds one turn's RESULT with `["e", result_id, "", "reply"]` (§2.4).
pub fn bind_accept_to_result(draft: &mut EventDraft, result_id: &str) {
    if draft.kind != JOB_ACCEPT_KIND {
        return;
    }
    let at = draft
        .tags
        .iter()
        .position(|t| t.first() == Some("t"))
        .unwrap_or(draft.tags.len());
    draft
        .tags
        .insert(at, TagSpec::new(["e", result_id, "", "reply"]));
}

// ───────────────────────────── §4 deadline ─────────────────────────────

/// The instant the orchestrator sends `session/cancel` (§4).
pub fn wrap_up_at(deadline_unix: u64, wrap_up_secs: u64) -> u64 {
    deadline_unix.saturating_sub(wrap_up_secs)
}

/// Whether a turn may start now (slot freed, review/resume about to run). Below this the seller
/// publishes `at_capacity`, charges nothing, and keeps the session parked.
pub fn turn_can_start(now_unix: u64, deadline_unix: u64, wrap_up_secs: u64) -> bool {
    deadline_unix
        >= now_unix
            .saturating_add(wrap_up_secs)
            .saturating_add(PUSH_MARGIN_SECS)
            .saturating_add(RESUME_START_MARGIN_SECS)
}

// ───────────────────────────── §5 parking ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Parked,
    Closed,
    Expired,
}
impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Parked => "parked",
            Self::Closed => "closed",
            Self::Expired => "expired",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "parked" => Some(Self::Parked),
            "closed" => Some(Self::Closed),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
    pub fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::Parked)
    }
}

/// What `docker inspect` of the netns holder said at resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderObservation {
    Missing,
    NotRunning,
    Running { started_at: String },
}

/// What the holder's network namespace looked like at resume, read by an unprivileged helper that
/// joins it (`--network container:<holder>`), before any job container starts.
///
/// A holder can be running, unrestarted and still carry the firewall rules while its namespace has
/// lost every address and route: measured on runsc seats, where turn 1's gVisor sandbox leaves the
/// parked holder with only `lo`. A turn started into that namespace cannot reach the credential
/// proxy, burns ~4 minutes, and strikes a healthy harness. Only [`HolderNetwork::Up`] starts a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderNetwork {
    /// A non-loopback IPv4 address and a default route are present.
    Up,
    /// No non-loopback IPv4 address in the namespace.
    NoAddress,
    /// An address, but no default route.
    NoDefaultRoute,
    /// The probe could not answer (docker error, timeout, unreadable output). Fail-closed.
    ProbeFailed(String),
    /// runsc only: the namespace lost its address or default route, and putting docker's recorded
    /// values back was refused (no veth, no usable record, filters not as installed) or did not
    /// re-verify afterwards. Fail-closed; the session expires and its holder is removed.
    RestoreFailed(String),
    /// Not probed: the holder already failed its own check, or no probe was run at all.
    NotProbed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeGate {
    Start,
    /// Expire the session and answer the follow-up with `session_expired`; the job container is
    /// never started (failure tests 6 and 14).
    Expire(&'static str),
}

/// §5.5: a holder that is gone, stopped, or restarted (a new `State.StartedAt` is a new network
/// namespace with no firewall rules) expires the session; so does an orchestrator that predates
/// the inputs protocol the host would send. A holder that passes all of that must ALSO still have a
/// network ([`HolderNetwork::Up`]); anything else — including a probe that could not answer or was
/// never run — expires with `holder_network_lost`.
pub fn resume_gate(
    recorded_holder_started_at: Option<&str>,
    observed: &HolderObservation,
    recorded_orchestrator_protocol: u32,
    network: &HolderNetwork,
) -> ResumeGate {
    if recorded_orchestrator_protocol < SESSION_PROTOCOL {
        return ResumeGate::Expire("orchestrator_too_old");
    }
    let Some(recorded) = recorded_holder_started_at.filter(|s| !s.is_empty()) else {
        return ResumeGate::Expire("holder_unrecorded");
    };
    match observed {
        HolderObservation::Missing => ResumeGate::Expire("holder_missing"),
        HolderObservation::NotRunning => ResumeGate::Expire("holder_not_running"),
        HolderObservation::Running { started_at } if started_at != recorded => {
            ResumeGate::Expire("holder_restarted")
        }
        HolderObservation::Running { .. } if *network != HolderNetwork::Up => {
            ResumeGate::Expire("holder_network_lost")
        }
        HolderObservation::Running { .. } => ResumeGate::Start,
    }
}

/// One session row as the GC sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSize {
    pub job_id: String,
    pub state: SessionState,
    pub last_turn_at: u64,
    pub disk_bytes: u64,
}

/// §5.5 lifetime: parked sessions idle past `lifetime_secs`.
pub fn expired_by_lifetime(
    sessions: &[SessionSize],
    now_unix: u64,
    lifetime_secs: u64,
) -> Vec<String> {
    sessions
        .iter()
        .filter(|s| {
            s.state == SessionState::Parked
                && now_unix >= s.last_turn_at.saturating_add(lifetime_secs)
        })
        .map(|s| s.job_id.clone())
        .collect()
}

/// §5.5 disk budget: while the parked total exceeds the budget, evict the oldest `last_turn_at`
/// first. A running session is never chosen (failure test 12); ties break on job id for a stable
/// order.
pub fn evict_over_budget(sessions: &[SessionSize], budget_bytes: u64) -> Vec<String> {
    let mut parked: Vec<_> = sessions
        .iter()
        .filter(|s| s.state == SessionState::Parked)
        .collect();
    let mut total: u64 = parked.iter().map(|s| s.disk_bytes).sum();
    parked.sort_by(|a, b| {
        a.last_turn_at
            .cmp(&b.last_turn_at)
            .then(a.job_id.cmp(&b.job_id))
    });
    let mut out = Vec::new();
    for s in parked {
        if total <= budget_bytes {
            break;
        }
        total = total.saturating_sub(s.disk_bytes);
        out.push(s.job_id.clone());
    }
    out
}

// ───────────────────────────── §7 delivery ─────────────────────────────

/// Delivery branch per turn (§7.1). Every turn pushes a NEW ref; none is ever updated.
pub fn delivery_branch(
    private: bool,
    offer_id: &str,
    turn: u32,
    followup_id: Option<&str>,
) -> String {
    match (private, turn, followup_id) {
        (true, n, Some(f)) if n >= 2 => format!("refs/heads/delivery/{f}"),
        (true, _, _) => format!("refs/heads/delivery/{offer_id}"),
        (false, n, _) if n >= 2 => format!("maxplayer/{}-t{n}", &offer_id[..8.min(offer_id.len())]),
        (false, _, _) => format!("maxplayer/{}", &offer_id[..8.min(offer_id.len())]),
    }
}

/// Same algorithm as `job_lifecycle::job_hash_for_offer` (pinned by a test) — available without the
/// wallet feature for the seller/orchestrator side.
fn public_job_hash(id: &str, text: &str, amount_sats: u64) -> String {
    let mut h = Sha256::new();
    h.update(id.as_bytes());
    h.update(b"|");
    h.update(text.as_bytes());
    h.update(b"|");
    h.update(amount_sats.to_string().as_bytes());
    hex::encode(h.finalize())
}

/// The turn's `job-hash` (§7.1 as amended): the OFFER's, on every turn. The v2 result validators on
/// both buyer paths require `job-hash == job_hash(offer)` (`private_content::wire::bind_content`,
/// `public_v2::validate_evidence`), so a follow-up-derived hash would get every turn n ≥ 2 refused.
/// Turns stay distinct without it: the receipt preimage binds each turn's own creq hash and
/// delivered commit/answer, payment ids are the follow-up id, and a turn-1 tree cannot pass as
/// turn 2 because a turn-n commit must have the previous turn's commit as its single parent
/// ([`verify_turn_parent`]). `followup` is still required for n ≥ 2 (a turn needs its follow-up).
pub fn turn_job_hash(
    private: bool,
    offer_id: &str,
    offer_task: &str,
    turn: u32,
    followup: Option<(&str, &str)>,
    amount_sats: u64,
) -> Result<String, SessionError> {
    let (id, text) = match (turn, followup) {
        (1, _) | (_, Some(_)) => (offer_id, offer_task),
        _ => return Err(SessionError::Malformed("turn >= 2 without follow-up")),
    };
    if private {
        maxplayer_private_protocol::job_hash(id).map_err(|_| SessionError::Malformed("job id"))
    } else {
        Ok(public_job_hash(id, text, amount_sats))
    }
}

/// Buyer collect check (§8.4, failure test 10): a turn ≥ 2 git commit has exactly one parent, the
/// previous git turn's accepted commit.
pub fn verify_turn_parent(
    parents: &[String],
    previous_git_commit: &str,
) -> Result<(), &'static str> {
    match parents {
        [one] if one == previous_git_commit => Ok(()),
        [_] => Err("turn commit does not descend from the previous turn"),
        _ => Err("turn commit must have exactly one parent"),
    }
}

/// Inline or git for a session turn (§7.2): changed tree ⇒ git; unchanged ⇒ inline with the marked
/// message, or for `out_of_time` the last message. Unchanged and empty ⇒ `None` (refused
/// `no_sentinel`, nothing charged — failure test 9).
pub fn session_delivery_content(
    tree_changed: bool,
    status: TurnStatus,
    final_message: &str,
) -> Option<SessionDelivery> {
    if tree_changed {
        return Some(SessionDelivery::Git);
    }
    let msg = final_message.trim();
    let marked =
        msg.starts_with(crate::engine::INLINE_ANSWER_MARKER) || msg.starts_with(NEEDS_INPUT_MARKER);
    if marked || (status == TurnStatus::OutOfTime && !msg.is_empty()) {
        Some(SessionDelivery::Inline(final_message.to_owned()))
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDelivery {
    Git,
    Inline(String),
}

/// `results/<job_id>` for turn 1, `results/<job_id>-t<n>` after (§8.4).
pub fn collect_dir_name(job_id: &str, turn: u32) -> String {
    if turn <= 1 {
        job_id.to_owned()
    } else {
        format!("{job_id}-t{turn}")
    }
}

// ───────────────────────────── §8 payment ─────────────────────────────

/// Per-turn payment identity (§8.2): turn 1 pays against the job id (claim creq, unchanged); turn
/// n ≥ 2 against the follow-up id (the result's creq). Seller receipts and buyer reservations are
/// keyed by this, so the turn-1 `has_receipt` skip cannot swallow turn 2.
pub fn payment_id<'a>(job_id: &'a str, turn: u32, followup_id: Option<&'a str>) -> &'a str {
    match (turn, followup_id) {
        (n, Some(f)) if n >= 2 => f,
        _ => job_id,
    }
}

/// What a seller receive maps an incoming payload id to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentTarget {
    pub job_id: String,
    pub turn: u32,
    /// Receipt-row key: the job id for turn 1, the follow-up id for n ≥ 2.
    pub receipt_key: String,
}

/// Map a payload id: a follow-up id found in `session_turns` resolves to `(job, turn)`; anything
/// else is the job id itself (turn 1, today's path).
pub fn resolve_payment(
    payload_id: &str,
    lookup_turn: impl Fn(&str) -> Option<(String, u32)>,
) -> PaymentTarget {
    match lookup_turn(payload_id) {
        Some((job_id, turn)) => PaymentTarget {
            job_id,
            turn,
            receipt_key: payload_id.to_owned(),
        },
        None => PaymentTarget {
            job_id: payload_id.to_owned(),
            turn: 1,
            receipt_key: payload_id.to_owned(),
        },
    }
}

/// Seller turn states (`session_turns.state`, §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Received,
    WaitingPayment,
    Reviewing,
    Running,
    Delivered,
    Paid,
    Refused,
}
impl TurnState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::WaitingPayment => "waiting_payment",
            Self::Reviewing => "reviewing",
            Self::Running => "running",
            Self::Delivered => "delivered",
            Self::Paid => "paid",
            Self::Refused => "refused",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "received" => Self::Received,
            "waiting_payment" => Self::WaitingPayment,
            "reviewing" => Self::Reviewing,
            "running" => Self::Running,
            "delivered" => Self::Delivered,
            "paid" => Self::Paid,
            "refused" => Self::Refused,
            _ => return None,
        })
    }
}

/// §8.3 one unpaid turn at most: turn n+1 runs only once turn n is settled — receipted on a priced
/// job, ACCEPT seen on a free one. Before that the follow-up waits (`waiting_payment`), and past
/// its deadline it is dropped uncharged (failure test 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentGate {
    Run,
    Wait,
    DropExpired,
}
pub fn payment_gate(
    previous_turn_settled: bool,
    now_unix: u64,
    followup_deadline_unix: u64,
) -> PaymentGate {
    if previous_turn_settled {
        PaymentGate::Run
    } else if now_unix >= followup_deadline_unix {
        PaymentGate::DropExpired
    } else {
        PaymentGate::Wait
    }
}

// ───────────────────────────── §8.1 buyer reply ─────────────────────────────

/// Why `reply` refuses, LOCALLY, before anything is signed, reserved or published (failure test 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyRefusal {
    NoSessionSupport,
    CollectFirst {
        turn: u32,
    },
    SessionEnded,
    TurnInFlight,
    PerJobCap {
        price: u64,
        cap: u64,
    },
    SessionCap {
        committed: u64,
        price: u64,
        cap: u64,
    },
}
impl std::fmt::Display for ReplyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSessionSupport => write!(f, "seller does not support sessions; post a new job"),
            Self::CollectFirst { turn } => write!(
                f,
                "collect first: turn {turn} is delivered but not collected"
            ),
            Self::SessionEnded => write!(f, "session is closed or expired; post a new job"),
            Self::TurnInFlight => write!(f, "a turn is already in flight; wait for its result"),
            Self::PerJobCap { price, cap } => write!(
                f,
                "turn price {price} sat exceeds the per-job cap {cap} sat"
            ),
            Self::SessionCap {
                committed,
                price,
                cap,
            } => write!(
                f,
                "session cap: {committed} sat committed + {price} sat > cap {cap} sat (raise max_session_sats)"
            ),
        }
    }
}

/// Buyer-side session view the preflight reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyPreflight {
    pub claim_supports_sessions: bool,
    pub session_live: bool,
    /// Newest delivered turn not yet collected, if any.
    pub uncollected_turn: Option<u32>,
    pub turn_in_flight: bool,
    /// Turn price (= job price). 0 on a free job.
    pub price_sats: u64,
    pub per_job_cap_sats: u64,
    /// Spent + reserved over every turn of this session.
    pub session_committed_sats: u64,
    pub session_cap_sats: u64,
}
pub fn reply_preflight(p: &ReplyPreflight) -> Result<(), ReplyRefusal> {
    if !p.claim_supports_sessions {
        return Err(ReplyRefusal::NoSessionSupport);
    }
    if !p.session_live {
        return Err(ReplyRefusal::SessionEnded);
    }
    if let Some(turn) = p.uncollected_turn {
        return Err(ReplyRefusal::CollectFirst { turn });
    }
    if p.turn_in_flight {
        return Err(ReplyRefusal::TurnInFlight);
    }
    if p.price_sats > p.per_job_cap_sats {
        return Err(ReplyRefusal::PerJobCap {
            price: p.price_sats,
            cap: p.per_job_cap_sats,
        });
    }
    if p.session_committed_sats.saturating_add(p.price_sats) > p.session_cap_sats {
        return Err(ReplyRefusal::SessionCap {
            committed: p.session_committed_sats,
            price: p.price_sats,
            cap: p.session_cap_sats,
        });
    }
    Ok(())
}

// ───────────────────────────── §6.3 / §9 context ─────────────────────────────

/// One earlier delivery as the reviewer sees it (§9): a single line, never its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryLine {
    pub turn: u32,
    pub status: TurnStatus,
    pub commit: Option<String>,
}

/// Follow-up review context (§9): the original task and every follow-up in full, each earlier
/// delivery as one line. Only past [`REVIEW_CONTEXT_MAX_BYTES`] are the OLDEST follow-ups replaced
/// by a size note; the current follow-up (the last element) is always in full.
pub fn followup_review_context(
    task: &str,
    followups: &[String],
    deliveries: &[DeliveryLine],
) -> String {
    let numbered: Vec<(u32, String)> = followups
        .iter()
        .enumerate()
        .map(|(i, f)| (i as u32 + 2, f.clone()))
        .collect();
    followup_review_context_numbered(task, &numbered, deliveries)
}

/// [`followup_review_context`] with each follow-up's own turn number: the private lane (§9) may
/// carry only the newest follow-ups (transport bound), so numbering by position would mislabel them.
pub fn followup_review_context_numbered(
    task: &str,
    followups: &[(u32, String)],
    deliveries: &[DeliveryLine],
) -> String {
    let render = |omit: usize| -> String {
        let mut s = format!("ORIGINAL TASK:\n{task}\n");
        for d in deliveries {
            s.push_str(&format!(
                "turn {}: {}, commit {}\n",
                d.turn,
                d.status.as_str(),
                d.commit.as_deref().unwrap_or("inline")
            ));
        }
        for (i, (n, f)) in followups.iter().enumerate() {
            if i < omit && i + 1 < followups.len() {
                s.push_str(&format!(
                    "FOLLOW-UP turn {n}: [omitted, {} bytes]\n",
                    f.len()
                ));
            } else {
                s.push_str(&format!("FOLLOW-UP turn {n}:\n{f}\n"));
            }
        }
        s
    };
    let mut omit = 0;
    loop {
        let s = render(omit);
        if s.len() <= REVIEW_CONTEXT_MAX_BYTES || omit + 1 >= followups.len() {
            return s;
        }
        omit += 1;
    }
}

/// `context_reset` prompt history (§6.3): the original task and buyer follow-ups, newest last, cut
/// at 64 KiB by dropping the oldest follow-ups first (the newest is always kept).
pub fn reset_history(task: &str, followups: &[String]) -> String {
    let mut kept: Vec<&String> = followups.iter().collect();
    loop {
        let mut s = format!("The conversation could not be reloaded. Original task:\n{task}\n");
        let dropped = followups.len() - kept.len();
        if dropped > 0 {
            s.push_str(&format!("({dropped} earlier follow-up(s) omitted)\n"));
        }
        for f in &kept {
            s.push_str(&format!("Buyer follow-up:\n{f}\n"));
        }
        if s.len() <= RESET_HISTORY_MAX_BYTES || kept.len() <= 1 {
            return s;
        }
        kept.remove(0);
    }
}

/// The session section of the composed agent prompt (§7.2).
pub fn prompt_session_section() -> String {
    format!(
        "This job is a session: the buyer may send follow-ups after you deliver. If you need an \
         answer from the buyer before you can continue, end your final message with a line that \
         begins `{NEEDS_INPUT_MARKER}` followed by your question; the turn then ends and the buyer \
         replies in the next turn."
    )
}

#[cfg(test)]
mod tests;
