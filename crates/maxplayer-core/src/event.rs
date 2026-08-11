use serde::{Deserialize, Serialize};

pub const CURRENT_ENVELOPE_VERSION: u16 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Envelope {
    pub v: u16,
    pub seq: u64,
    pub ts: u64,
    pub payload: Event,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    #[serde(rename = "harness.candidate_found")]
    HarnessCandidateFound { candidate_id: HarnessCandidateId },
    #[serde(rename = "gateway.status_changed")]
    GatewayStatusChanged { status: GatewayStatus },
    #[serde(rename = "wallet.status_changed")]
    WalletStatusChanged { status: WalletStatus },
    #[serde(rename = "job.offered")]
    JobOffered { job_id: JobId },
    #[serde(rename = "driver.ready")]
    DriverReady { runtime_id: RuntimeId },
    #[serde(rename = "job.claimed")]
    JobClaimed { job_id: JobId },
    #[serde(rename = "result.posted")]
    ResultPosted { result_id: ResultId },
    #[serde(rename = "payment.settled")]
    PaymentSettled { payment_id: PaymentId },
    #[serde(rename = "setup.action_required")]
    SetupActionRequired { action_id: SetupActionId },
    #[serde(rename = "job.execution_changed")]
    JobExecutionChanged {
        job_id: JobId,
        status: JobExecutionStatus,
    },
    #[serde(rename = "artifact.produced")]
    ArtifactProduced { artifact_id: ArtifactId },
    #[serde(rename = "agent.message")]
    AgentMessage { job_id: JobId, text: String },
    #[serde(rename = "receipt.signed")]
    ReceiptSigned { receipt_id: ReceiptId },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct HarnessCandidateId(pub String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct JobId(pub String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RuntimeId(pub String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ResultId(pub String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PaymentId(pub String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SetupActionId(pub String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ArtifactId(pub String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ReceiptId(pub String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayStatus {
    Offline,
    Starting,
    Online,
    Degraded,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WalletStatus {
    Unconfigured,
    Locked,
    Ready,
    Degraded,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobExecutionStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}
