pub mod acp;
#[cfg(feature = "acp")]
pub mod acp_driver;
pub mod mock;

use std::error::Error;
use std::fmt::{self, Display};

pub use crate::event::RuntimeId;

pub use acp::{
    Artifact, Caps, ContentBlock, ExtMethod, Initialize, InitializeResult, McpServer,
    PermissionOutcome, PermissionRequest, PromptTurn, SessionConfig, SessionId, SessionUpdate,
    StopReason, UpdateStream,
};
#[cfg(feature = "acp")]
pub use acp_driver::{AcpDriver, AgentCommand};
pub use mock::{MockDriver, ScriptedSession};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Readiness {
    pub runtime_id: RuntimeId,
    pub protocol_version: u32,
}

/// A real monetary cost with its accounting basis (the `cost` tag).
/// `amount` is the exact string as reported by the harness (kept as text so it is
/// byte-exact and never zero/float-mangled); `basis` ∈ {harness-reported-usd,
/// harness-reported-notional}. Unit is locked to USD.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageCost {
    pub amount: String,
    pub basis: String,
}

/// Per-run execution usage a driver surfaced for a job (seller-claimed).
///
/// Every field is OPTIONAL so **absent-stays-absent** is representable end to end: a field
/// the harness did not report is `None` and is NEVER zero-filled downstream. `total_tokens`
/// is intentionally NOT stored — it is DERIVED (`input + output + reasoning`) at tag-emission
/// so a partial capture can never masquerade as a complete total.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageMetadata {
    pub model: Option<String>,
    /// Non-cached input tokens.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Reasoning tokens — absent means UNKNOWN (not zero).
    pub reasoning_tokens: Option<u64>,
    /// Cache-read sibling — evidence only, NEVER summed into the total.
    pub cache_read_tokens: Option<u64>,
    /// Cache-write/creation sibling — evidence only, NEVER summed into the total.
    pub cache_write_tokens: Option<u64>,
    pub cost: Option<UsageCost>,
}

impl UsageMetadata {
    /// True when no field carries a value — nothing real was captured.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.reasoning_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_write_tokens.is_none()
            && self.cost.is_none()
    }

    /// Derived `total_tokens = input + output + reasoning`. Returns `None` unless BOTH `input`
    /// and `output` are present (a
    /// partial capture must never be reported as a total); `reasoning` is added when present
    /// and treated as unknown-not-zero when absent. Cache siblings are never folded in.
    pub fn total_tokens(&self) -> Option<u64> {
        match (self.input_tokens, self.output_tokens) {
            (Some(input), Some(output)) => {
                Some(input + output + self.reasoning_tokens.unwrap_or(0))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverError {
    NotReady,
    SessionNotFound(SessionId),
    SessionCancelled(SessionId),
    ScriptExhausted,
    /// The driver's response timer expired while waiting for this request.
    ///
    /// Typed because the caller knows what supplied the timer. In the seller job path it is the
    /// job's absolute deadline; flattening it into [`Self::Other`] would discard that attribution
    /// and make a healthy harness look broken.
    ResponseTimeout { request_id: u64 },
    /// This driver has no model-selection route, so a model request cannot be honoured.
    ///
    /// Typed rather than folded into [`Self::Other`] because the caller must distinguish "this
    /// harness cannot select a model at all" from "the route exists and failed". The first is a
    /// dispatch fact about the harness; the second is a fault. Collapsing them would let a
    /// harness that never had the capability look like one that broke.
    ModelSelectionUnsupported,
    /// The harness advertises no model selector, so there is no option id to write to.
    ///
    /// Distinct from [`Self::ModelSelectionUnsupported`], which is about OUR driver: this one says
    /// the driver can write config options but THIS session published nothing identifying a model
    /// selector. Absence of a selector is not a fault to retry, it is a harness that cannot serve
    /// a model request.
    ModelSelectorAbsent,
    /// The harness's model selector does not offer the requested model, so it is refused unwritten.
    ///
    /// This is the guard for the substitution the post-write read-back CANNOT see. `codex-acp`
    /// accepts an unrecognised model verbatim and forwards it, so a read-back echoes the nonsense
    /// straight back and an exact comparison PASSES on a model the harness never had. Checking
    /// membership first is a different question from "did the write take", and both are needed.
    ModelNotOffered { requested: String },
    Other(String),
}

impl Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReady => write!(f, "driver is not ready"),
            Self::SessionNotFound(session_id) => write!(f, "session not found: {session_id}"),
            Self::SessionCancelled(session_id) => write!(f, "session is cancelled: {session_id}"),
            Self::ScriptExhausted => write!(f, "mock driver script exhausted"),
            Self::ResponseTimeout { request_id } => {
                write!(f, "ACP request {request_id} timed out waiting for response")
            }
            Self::ModelSelectionUnsupported => {
                write!(f, "driver cannot select a session model")
            }
            Self::ModelSelectorAbsent => {
                write!(f, "harness advertises no model selector for this session")
            }
            Self::ModelNotOffered { requested } => {
                write!(f, "harness does not offer model {requested}")
            }
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

impl Error for DriverError {}

#[allow(async_fn_in_trait)]
pub trait Driver {
    fn id(&self) -> RuntimeId;
    async fn ready(&mut self) -> Result<Readiness, DriverError>;
    async fn start_session(&mut self, cfg: SessionConfig) -> Result<SessionId, DriverError>;
    async fn prompt(
        &mut self,
        session_id: &SessionId,
        turn: PromptTurn,
    ) -> Result<UpdateStream, DriverError>;
    async fn on_permission(&mut self, req: PermissionRequest) -> PermissionOutcome;
    async fn artifacts(&self, session_id: &SessionId) -> Result<Vec<Artifact>, DriverError>;
    async fn cancel(&mut self, session_id: &SessionId) -> Result<(), DriverError>;
    async fn shutdown(&mut self) -> Result<(), DriverError>;

    /// Ask the harness to bind `model` for this session, and return the model the harness then
    /// REPORTS as bound — `None` when it reports nothing usable.
    ///
    /// ⚠ This returns a REPORT, never a verdict. A driver must not decide whether the binding was
    /// acceptable, because the harness's own success is not evidence that the requested model was
    /// bound: `codex-acp` accepts an unrecognised model verbatim and forwards it (rejecting only
    /// the empty string), and `claude-agent-acp` fuzzy-resolves aliases like `opus` onto a
    /// canonical id and returns success. Both paths return OK having bound something other than
    /// what was named. The comparison is therefore the caller's, in one place, where a driver bug
    /// cannot bypass it — see `engine::bind_session_model`.
    ///
    /// Default: [`DriverError::ModelSelectionUnsupported`]. Fail-closed on purpose. A default that
    /// returned `Ok(None)`, or echoed the request back, would read downstream as a successful
    /// binding and would make every driver that never implemented this silently claim to honour
    /// model requests.
    async fn select_model(
        &mut self,
        session_id: &SessionId,
        model: &str,
    ) -> Result<Option<String>, DriverError> {
        let _ = (session_id, model);
        Err(DriverError::ModelSelectionUnsupported)
    }

    /// Execution usage captured from the most recent prompt, if the harness surfaced any.
    /// Default `None` keeps **absent-stays-absent** for drivers that expose nothing — only a
    /// driver that actually reads usage overrides this.
    fn usage(&self) -> Option<UsageMetadata> {
        None
    }
}
