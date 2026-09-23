//! Shared closed private-content v2 schemas. No wallet, network or lifecycle runtime.
pub mod strict_json;
pub mod wire;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const BODY_SCHEMA: &str = "maxplayer.content.v2";
pub const ENVELOPE_SCHEMA: &str = "maxplayer.content-envelope.v2";
pub const MAX_BODY_BYTES: usize = 16 * 1024;
pub const MAX_ENVELOPE_BYTES: usize = 23 * 1024;
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_REPO_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_FILES: usize = 1000;

/// Errors intentionally contain no untrusted content, keys, paths, or decoded message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

pub fn is_hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes * 2
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn require_hex(value: &str, bytes: usize) -> Result<()> {
    if is_hex(value, bytes) {
        Ok(())
    } else {
        Err(Error("invalid fixed-length lowercase hex"))
    }
}
pub fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error("randomness unavailable"))?;
    Ok(hex::encode(bytes))
}
pub fn job_hash(offer_id: &str) -> Result<String> {
    require_hex(offer_id, 32)?;
    let mut h = Sha256::new();
    h.update(b"maxplayer/job/v2\0");
    h.update(hex::decode(offer_id).map_err(|_| Error("invalid offer id"))?);
    Ok(hex::encode(h.finalize()))
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    Task,
    ClaimDetails,
    Progress,
    Answer,
    Feedback,
    Rejection,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Dispatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
}
impl Dispatch {
    pub fn validate(&self) -> Result<()> {
        if self.agent.as_ref().is_some_and(|s| s.is_empty())
            || self.harness_model.as_ref().is_some_and(|s| s.is_empty())
        {
            return Err(Error("empty dispatch preference"));
        }
        if self
            .harness_family
            .as_ref()
            .is_some_and(|s| !["claude-code", "codex", "cursor", "goose"].contains(&s.as_str()))
        {
            return Err(Error("unknown harness family"));
        }
        if let Some(v) = &self.capabilities {
            if v.is_empty()
                || !sorted_unique(v)
                || v.iter()
                    .any(|s| !["node", "python", "rust"].contains(&s.as_str()))
            {
                return Err(Error("invalid capability set"));
            }
        }
        Ok(())
    }
}
fn sorted_unique(values: &[String]) -> bool {
    values.windows(2).all(|w| w[0] < w[1])
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub repo_id: String,
    pub commit_oid: String,
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
}
impl Attachment {
    pub fn validate(&self, job_id: &str) -> Result<()> {
        require_hex(&self.repo_id, 32)?;
        require_hex(&self.commit_oid, 20)?;
        require_hex(&self.sha256, 32)?;
        if self.repo_id != job_id || self.size_bytes > MAX_FILE_BYTES {
            return Err(Error("attachment scope or size"));
        }
        validate_path(&self.path)
    }
}
pub fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains('\\')
        || path.contains(':')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.eq_ignore_ascii_case(".git"))
    {
        return Err(Error("unsafe attachment path"));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Contribution {
    pub target_owner_pubkey: String,
    pub target_clone_url: String,
    pub base_branch: String,
    pub base_oid: String,
    pub accepts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Attachment>,
}
impl Contribution {
    fn validate(&self, job_id: &str) -> Result<()> {
        require_hex(&self.target_owner_pubkey, 32)?;
        require_hex(&self.base_oid, 20)?;
        let url = nostr::prelude::Url::parse(&self.target_clone_url)
            .map_err(|_| Error("invalid contribution URL"))?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.host_str().is_none()
        {
            return Err(Error("unsafe contribution URL"));
        }
        if self.accepts != ["fork"] || !valid_ref(&self.base_branch) {
            return Err(Error("invalid contribution pin"));
        }
        if let Some(input) = &self.input {
            input.validate(job_id)?;
        }
        Ok(())
    }
}
fn valid_ref(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.ends_with('.')
        && !s.contains("..")
        && !s.contains("@{")
        && !s.contains("//")
        && s != "@"
        && !s.chars().any(|c| c.is_control() || " ~^:?*[\\".contains(c))
        && s.split('/')
            .all(|p| !p.starts_with('.') && !p.ends_with(".lock"))
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContentBody {
    pub schema: String,
    pub job_id: String,
    pub offer_id: Option<String>,
    pub award_id: Option<String>,
    pub message_id: String,
    #[serde(rename = "type")]
    pub kind: ContentType,
    pub revision: u64,
    pub supersedes: Option<String>,
    pub author: String,
    pub recipients: Vec<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<Dispatch>,
    pub attachments: Vec<Attachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contribution: Option<Contribution>,
}
// Do not derive Debug for plaintext-bearing structures.
impl ContentBody {
    pub fn validate(&self) -> Result<()> {
        if self.schema != BODY_SCHEMA {
            return Err(Error("wrong content domain"));
        }
        for id in [&self.job_id, &self.message_id, &self.author] {
            require_hex(id, 32)?;
        }
        for id in [&self.offer_id, &self.award_id, &self.supersedes]
            .into_iter()
            .flatten()
        {
            require_hex(id, 32)?;
        }
        if self.recipients.is_empty()
            || self.recipients.len() > 3
            || !sorted_unique(&self.recipients)
        {
            return Err(Error("invalid recipient set"));
        }
        for r in &self.recipients {
            require_hex(r, 32)?;
        }
        if !self.recipients.contains(&self.author) {
            return Err(Error("missing author self-copy"));
        }
        if (self.revision == 0) != self.supersedes.is_none()
            || self.supersedes.as_ref() == Some(&self.message_id)
        {
            return Err(Error("invalid revision"));
        }
        match self.kind {
            ContentType::Task => {
                if self.text.trim().is_empty()
                    || self.offer_id.is_some()
                    || self.award_id.is_some()
                    || self.revision != 0
                    || self.requested_output.as_ref().is_none_or(|s| s.is_empty())
                    || self.dispatch.is_none()
                {
                    return Err(Error("invalid initial task"));
                }
            }
            ContentType::ClaimDetails => {
                if self.offer_id.is_none()
                    || self.award_id.is_some()
                    || self.dispatch.is_none()
                    || !self.text.is_empty()
                    || !self.attachments.is_empty()
                {
                    return Err(Error("invalid claim details"));
                }
            }
            ContentType::Answer | ContentType::Rejection => {
                if self.offer_id.is_none() || self.award_id.is_none() {
                    return Err(Error("missing execution binding"));
                }
            }
            ContentType::Progress | ContentType::Feedback => {
                if self.offer_id.is_none() {
                    return Err(Error("missing offer binding"));
                }
            }
        }
        if self.kind != ContentType::Task
            && (self.requested_output.is_some() || self.contribution.is_some())
        {
            return Err(Error("task fields on non-task content"));
        }
        if !matches!(self.kind, ContentType::Task | ContentType::ClaimDetails)
            && self.dispatch.is_some()
        {
            return Err(Error("dispatch on wrong content type"));
        }
        if let Some(dispatch) = &self.dispatch {
            dispatch.validate()?;
        }
        if let Some(contribution) = &self.contribution {
            contribution.validate(&self.job_id)?;
        }
        if self.attachments.len() > MAX_FILES {
            return Err(Error("too many attachments"));
        }
        let mut paths = BTreeSet::new();
        for attachment in &self.attachments {
            attachment.validate(&self.job_id)?;
            if !paths.insert((
                &attachment.repo_id,
                &attachment.commit_oid,
                &attachment.path,
            )) {
                return Err(Error("duplicate attachment"));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema: String,
    nonce: String,
    body_b64: String,
}

/// Exact immutable bytes. Secret body and nonce never implement Debug or appear in errors.
#[derive(Clone)]
pub struct PreparedContent {
    envelope: String,
    body_bytes: Vec<u8>,
    body: ContentBody,
    commitment: String,
}
impl PreparedContent {
    pub fn new(body: ContentBody) -> Result<Self> {
        let nonce = random_id()?;
        Self::with_nonce(body, &nonce)
    }
    fn with_nonce(body: ContentBody, nonce: &str) -> Result<Self> {
        body.validate()?;
        let bytes = serde_json::to_vec(&body).map_err(|_| Error("content serialization failed"))?;
        let commitment = commit(nonce, &bytes)?;
        let envelope = serde_json::to_string(&Envelope {
            schema: ENVELOPE_SCHEMA.into(),
            nonce: nonce.into(),
            body_b64: STANDARD.encode(&bytes),
        })
        .map_err(|_| Error("envelope serialization failed"))?;
        if envelope.len() > MAX_ENVELOPE_BYTES {
            return Err(Error("envelope too large"));
        }
        Ok(Self {
            envelope,
            body_bytes: bytes,
            body,
            commitment,
        })
    }
    pub fn decode(envelope: &str) -> Result<Self> {
        if envelope.len() > MAX_ENVELOPE_BYTES {
            return Err(Error("envelope too large"));
        }
        strict_json::validate(envelope.as_bytes())?;
        let env: Envelope =
            serde_json::from_str(envelope).map_err(|_| Error("invalid envelope"))?;
        if env.schema != ENVELOPE_SCHEMA {
            return Err(Error("wrong envelope domain"));
        }
        let bytes = STANDARD
            .decode(&env.body_b64)
            .map_err(|_| Error("invalid body encoding"))?;
        if STANDARD.encode(&bytes) != env.body_b64 {
            return Err(Error("noncanonical body encoding"));
        }
        let commitment = commit(&env.nonce, &bytes)?;
        strict_json::validate(&bytes)?;
        let raw: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| Error("invalid body JSON"))?;
        let object = raw.as_object().ok_or(Error("body is not an object"))?;
        for key in [
            "schema",
            "job_id",
            "offer_id",
            "award_id",
            "message_id",
            "type",
            "revision",
            "supersedes",
            "author",
            "recipients",
            "text",
            "attachments",
        ] {
            if !object.contains_key(key) {
                return Err(Error("missing content field"));
            }
        }
        for key in ["requested_output", "dispatch", "contribution"] {
            if object.get(key).is_some_and(serde_json::Value::is_null) {
                return Err(Error("null optional content field"));
            }
        }
        if let Some(dispatch) = object
            .get("dispatch")
            .and_then(serde_json::Value::as_object)
        {
            if dispatch.values().any(serde_json::Value::is_null) {
                return Err(Error("null dispatch preference"));
            }
        }
        let body: ContentBody =
            serde_json::from_slice(&bytes).map_err(|_| Error("invalid content body"))?;
        body.validate()?;
        Ok(Self {
            envelope: envelope.into(),
            body_bytes: bytes,
            body,
            commitment,
        })
    }
    pub fn envelope(&self) -> &str {
        &self.envelope
    }
    pub fn body_bytes(&self) -> &[u8] {
        &self.body_bytes
    }
    pub fn body(&self) -> &ContentBody {
        &self.body
    }
    pub fn commitment(&self) -> &str {
        &self.commitment
    }
    pub fn validate_binding(&self, expected: &Binding<'_>) -> Result<()> {
        let body = &self.body;
        let mut recipients = vec![
            expected.buyer.to_owned(),
            expected.seller.to_owned(),
            expected.service.to_owned(),
        ];
        recipients.sort();
        recipients.dedup();
        for r in &recipients {
            require_hex(r, 32)?;
        }
        if body.recipients != recipients
            || body.author != expected.author
            || body.job_id != expected.job_id
            || body.offer_id.as_deref() != expected.offer_id
            || body.award_id.as_deref() != expected.award_id
            || body.message_id != expected.message_id
            || self.commitment != expected.commitment
            || body.kind != expected.kind
        {
            return Err(Error("content does not match signed carrier"));
        }
        Ok(())
    }
}
pub struct Binding<'a> {
    pub buyer: &'a str,
    pub seller: &'a str,
    pub service: &'a str,
    pub author: &'a str,
    pub job_id: &'a str,
    pub offer_id: Option<&'a str>,
    pub award_id: Option<&'a str>,
    pub message_id: &'a str,
    pub commitment: &'a str,
    pub kind: ContentType,
}
fn commit(nonce: &str, body: &[u8]) -> Result<String> {
    require_hex(nonce, 32)?;
    if body.len() > MAX_BODY_BYTES {
        return Err(Error("body too large"));
    }
    let mut h = Sha256::new();
    h.update(b"maxplayer/content/v2\0");
    h.update(hex::decode(nonce).map_err(|_| Error("invalid nonce"))?);
    h.update((body.len() as u64).to_be_bytes());
    h.update(body);
    Ok(hex::encode(h.finalize()))
}
