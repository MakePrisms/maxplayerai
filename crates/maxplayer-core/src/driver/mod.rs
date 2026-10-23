pub mod acp;
#[cfg(feature = "acp")]
pub mod acp_driver;
pub mod mock;

use std::error::Error;
use std::fmt::{self, Display};

pub use crate::event::RuntimeId;

pub use acp::{
    Artifact, Caps, ContentBlock, EnvVariable, ExtMethod, HttpTransport, Initialize,
    InitializeResult, McpServer, McpServerHttp, McpServerStdio, PermissionOutcome,
    PermissionRequest, PromptTurn, SessionConfig, SessionId, SessionUpdate, StopReason,
    UpdateStream,
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

impl Error for DriverError {}

/// Fires a session's cancel from OUTSIDE the driver while a prompt is in flight (session jobs, §4
/// wrap-up). `prompt` holds `&mut self` until the turn answers, so the cancel cannot be a method
/// call on the driver; it is a handle cloned before the prompt starts.
pub type CancelTrigger = std::sync::Arc<dyn Fn() + Send + Sync>;

#[allow(async_fn_in_trait)]
pub trait Driver {
    fn id(&self) -> RuntimeId;
    async fn ready(&mut self) -> Result<Readiness, DriverError>;
    async fn start_session(&mut self, cfg: SessionConfig) -> Result<SessionId, DriverError>;
    /// Whether `initialize` advertised `agentCapabilities.loadSession` (session jobs, §6.3). Read
    /// only after [`Self::ready`]. Default: no.
    fn supports_load_session(&self) -> bool {
        false
    }
    /// Reload a session the agent saved earlier (ACP `session/load`, docs/specs/session-jobs.md
    /// §6.3). Called only when [`Self::supports_load_session`] is true; any error makes the caller
    /// fall back to [`Self::start_session`] and mark the turn `context_reset`. Default: unsupported.
    async fn load_session(
        &mut self,
        cfg: SessionConfig,
        session_id: &SessionId,
    ) -> Result<SessionId, DriverError> {
        let _ = (cfg, session_id);
        Err(DriverError::Other(
            "session/load is not supported by this driver".into(),
        ))
    }
    async fn prompt(
        &mut self,
        session_id: &SessionId,
        turn: PromptTurn,
    ) -> Result<UpdateStream, DriverError>;
    async fn on_permission(&mut self, req: PermissionRequest) -> PermissionOutcome;
    async fn artifacts(&self, session_id: &SessionId) -> Result<Vec<Artifact>, DriverError>;
    async fn cancel(&mut self, session_id: &SessionId) -> Result<(), DriverError>;
    /// A [`CancelTrigger`] for `session_id`, usable while a prompt is in flight. Default: none, and
    /// a session turn on such a driver simply runs to its deadline.
    fn cancel_trigger(&self, session_id: &SessionId) -> Option<CancelTrigger> {
        let _ = session_id;
        None
    }
    async fn shutdown(&mut self) -> Result<(), DriverError>;

    /// Execution usage captured from the most recent prompt, if the harness surfaced any.
    /// Default `None` keeps **absent-stays-absent** for drivers that expose nothing — only a
    /// driver that actually reads usage overrides this.
    fn usage(&self) -> Option<UsageMetadata> {
        None
    }
}
