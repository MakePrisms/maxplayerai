//! Closed v2 coordination schema. Validating syntax never substitutes for checking the
//! signed root offer, claim, award and carrier author. The context supplies trusted hosts.
use super::{Contribution, Dispatch, Error, Result, require_hex};
use nostr::prelude::{Event, JsonUtil, Url};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const VERSION: &str = "2";
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Private,
    Public,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Output {
    Text,
    Code,
    Image,
    Audio,
    Video,
    Data,
    Archive,
    Other,
}
impl Output {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Code => "code",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Data => "data",
            Self::Archive => "archive",
            Self::Other => "other",
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicTask {
    pub schema: String,
    pub text: String,
    pub requested_output: String,
    pub dispatch: Dispatch,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contribution: Option<Contribution>,
}
impl PublicTask {
    pub fn parse(json: &str, job: &str) -> Result<Self> {
        if json.len() > super::MAX_BODY_BYTES {
            return Err(Error("public task too large"));
        }
        super::strict_json::validate(json.as_bytes())?;
        let task: Self = serde_json::from_str(json).map_err(|_| Error("invalid public task"))?;
        if task.schema != "maxplayer.public-task.v2"
            || task.requested_output.is_empty()
            || task.text.trim().is_empty()
        {
            return Err(Error("invalid public task domain or output"));
        }
        task.dispatch.validate()?;
        if let Some(c) = &task.contribution {
            c.validate(job)?;
            if c.input.is_some() {
                return Err(Error("open offer cannot require confidential inputs"));
            }
        }
        Ok(task)
    }
}

/// A deployment-owned URL template, never one supplied by a message.
pub struct HostPolicy {
    pub git_prefix: String,
    pub accepted_mints: Vec<String>,
}
impl HostPolicy {
    pub fn job_repo(&self, buyer: &str, job: &str) -> Result<String> {
        require_hex(buyer, 32)?;
        require_hex(job, 32)?;
        let locator = format!("{}{buyer}/{job}", self.git_prefix);
        self.repo(&locator)?;
        Ok(locator)
    }
    pub fn repo(&self, locator: &str) -> Result<()> {
        let rest = locator
            .strip_prefix(&self.git_prefix)
            .ok_or(Error("untrusted private Git host"))?;
        let parts: Vec<_> = rest.split('/').collect();
        if !self.git_prefix.ends_with('/') || parts.len() != 2 {
            return Err(Error("invalid private repo route"));
        }
        require_hex(parts[0], 32)?;
        require_hex(parts[1].strip_suffix(".git").unwrap_or(parts[1]), 32)?;
        secure_url(locator)?;
        Ok(())
    }
    pub fn mint(&self, mint: &str) -> Result<()> {
        secure_url(mint)?;
        if mint.len() > 2048 || !self.accepted_mints.iter().any(|v| v == mint) {
            return Err(Error("unapproved mint"));
        }
        Ok(())
    }
}
fn secure_url(s: &str) -> Result<()> {
    let u = Url::parse(s).map_err(|_| Error("invalid service URL"))?;
    if u.scheme() != "https"
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(Error("unprotected service URL"));
    }
    Ok(())
}
pub fn decimal(v: &str) -> Result<u64> {
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) || (v.len() > 1 && v.starts_with('0'))
    {
        return Err(Error("invalid canonical integer"));
    }
    v.parse().map_err(|_| Error("integer overflow"))
}
fn member(v: &str, set: &[&str]) -> Result<()> {
    if set.contains(&v) {
        Ok(())
    } else {
        Err(Error("unknown public enum"))
    }
}
fn list(v: &[String], set: &[&str]) -> Result<()> {
    if v.is_empty() || !super::sorted_unique(v) {
        return Err(Error("invalid public enum set"));
    }
    for s in v {
        member(s, set)?;
    }
    Ok(())
}
const AGENTS: &[&str] = &["claude", "codex", "cursor"];
const FAMILIES: &[&str] = &["claude-code", "codex", "cursor", "goose"];
const CAPABILITIES: &[&str] = &["node", "python", "rust"];
const OUTPUTS: &[&str] = &[
    "text", "code", "image", "audio", "video", "data", "archive", "other",
];
const FEEDBACK_REASONS: &[&str] = &[
    "below_rate",
    "unsupported_version",
    "mint_incompatible",
    "at_capacity",
    "execution_failed",
    "delivery_failed",
    "no_sentinel",
    "other",
];
const REJECT_REASONS: &[&str] = &[
    "verify_not_descendant",
    "verify_tip_mismatch",
    "verify_content_refused",
    "verify_no_sentinel",
    "verify_reserved_path",
    "verify_attestation_missing",
    "verify_attestation_mismatch",
    "checks_failed",
    "other",
];

pub fn parse_signed(json: &str) -> Result<Event> {
    if json.len() > 128 * 1024 {
        return Err(Error("coordination event too large"));
    }
    super::strict_json::validate(json.as_bytes())?;
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|_| Error("invalid signed event"))?;
    let o = v.as_object().ok_or(Error("invalid signed event"))?;
    let allowed = [
        "id",
        "pubkey",
        "created_at",
        "kind",
        "tags",
        "content",
        "sig",
    ];
    if o.len() != allowed.len() || o.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(Error("unknown event fields"));
    }
    let e = Event::from_json(json).map_err(|_| Error("invalid signed event"))?;
    e.verify().map_err(|_| Error("invalid event signature"))?;
    Ok(e)
}
/// Checked private coordination tags; values retain signed bytes, never sanitized copies.
pub struct Tags {
    rows: BTreeMap<String, Vec<String>>,
    pub participants: BTreeSet<String>,
}
impl Tags {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.rows
            .get(key)
            .and_then(|v| v.first())
            .map(String::as_str)
    }
    pub fn values(&self, key: &str) -> Option<&[String]> {
        self.rows.get(key).map(Vec::as_slice)
    }
    pub fn required(&self, key: &str) -> Result<&str> {
        self.get(key).ok_or(Error("missing public field"))
    }
    pub fn has(&self, key: &str) -> bool {
        self.rows.contains_key(key)
    }
}
pub fn validate_private(event: &Event, host: &HostPolicy) -> Result<Tags> {
    event
        .verify()
        .map_err(|_| Error("invalid event signature"))?;
    let kind = event.kind.as_u16();
    if !(3400..=3407).contains(&kind) || !event.content.is_empty() {
        return Err(Error("invalid private coordination content"));
    }
    let mut out = Tags {
        rows: BTreeMap::new(),
        participants: BTreeSet::new(),
    };
    for tag in event.tags.iter() {
        let row = tag.as_slice();
        if row.len() < 2 {
            return Err(Error("invalid tag shape"));
        }
        let key = row[0].as_str();
        let v = &row[1];
        let (name, values) = match key {
            "p" => {
                if row.len() != 2 {
                    return Err(Error("invalid participant tag"));
                }
                require_hex(v, 32)?;
                if !out.participants.insert(v.clone()) {
                    return Err(Error("duplicate participant"));
                }
                continue;
            }
            "e" => {
                require_hex(v, 32)?;
                let name = match row.len() {
                    2 => "claim",
                    4 if row[2].is_empty() && row[3] == "root" => "root",
                    4 if row[2].is_empty() && row[3] == "reply" => "reply",
                    _ => return Err(Error("invalid event reference")),
                };
                (name.to_owned(), vec![v.clone()])
            }
            "param" | "sig" | "tokens" => {
                if key == "tokens" {
                    if row.len() != 3 {
                        return Err(Error("invalid tokens row"));
                    }
                    decimal(v)?;
                    member(&row[2], &["input", "output", "total"])?;
                    (format!("tokens:{}", row[2]), vec![v.clone()])
                } else {
                    if row.len() < 3 {
                        return Err(Error("invalid compound tag"));
                    }
                    (format!("{key}:{v}"), row[2..].to_vec())
                }
            }
            "amount" | "wall_time" => {
                if row.len() != 3 || row[2] != if key == "amount" { "sat" } else { "ms" } {
                    return Err(Error("invalid numeric unit"));
                }
                decimal(v)?;
                (key.into(), vec![v.clone()])
            }
            "agents" | "harness_family" | "capabilities" => (key.into(), row[1..].to_vec()),
            _ => {
                if row.len() != 2 {
                    return Err(Error("extra public tag fields"));
                }
                (key.into(), vec![v.clone()])
            }
        };
        if out.rows.insert(name, values).is_some() {
            return Err(Error("duplicate public tag"));
        }
    }
    let common = ["t", "v", "job"];
    let allowed: &[&str] = match kind {
        3401 => &[
            "visibility",
            "discovery",
            "output",
            "amount",
            "param:deadline",
            "param:payment",
            "param:accepts-delivery",
            "param:agent",
            "param:harness_family",
            "param:capability",
            "job-class",
            "delivery",
            "repo",
            "branch",
            "i",
            "content-id",
            "content-commitment",
        ],
        3402 => &[
            "root",
            "status",
            "creq",
            "payment",
            "agents",
            "harness_family",
            "capabilities",
            "content-id",
            "content-commitment",
        ],
        3405 | 3406 => &["root", "claim", "status"],
        3404 => &[
            "root",
            "status",
            "reason_code",
            "award",
            "content-id",
            "content-commitment",
        ],
        3403 => &[
            "root",
            "award",
            "output",
            "amount",
            "job-hash",
            "sig:seller",
            "sig:seller-contribution",
            "delivery",
            "repo",
            "branch",
            "commit",
            "content-id",
            "content-commitment",
            "wall_time",
            "tokens:input",
            "tokens:output",
            "tokens:total",
            "metadata_trust",
        ],
        3407 => &[
            "root",
            "reply",
            "reason_code",
            "status",
            "commit",
            "content-id",
            "content-commitment",
        ],
        3400 => &[
            "root",
            "reply",
            "job-hash",
            "amount",
            "mint",
            "sig:seller",
            "sig:buyer",
            "creq-hash",
            "delivery_kind",
            "delivery_integrity_hash",
            "wall_time",
            "tokens:input",
            "tokens:output",
            "tokens:total",
            "metadata_trust",
        ],
        _ => unreachable!(),
    };
    for (key, values) in &out.rows {
        if !common.contains(&key.as_str()) && !allowed.contains(&key.as_str()) {
            return Err(Error("unlisted private public field"));
        }
        let v = &values[0];
        match key.as_str() {
            "param:accepts-delivery" => list(values, &["git", "inline"])?,
            "param:capability" | "capabilities" => list(values, CAPABILITIES)?,
            "agents" => list(values, AGENTS)?,
            "harness_family" => list(values, FAMILIES)?,
            _ if values.len() != 1 => return Err(Error("extra compound tag values")),
            "t" => member(v, &["maxplayer"])?,
            "v" => member(v, &[VERSION])?,
            "visibility" => member(v, &["private"])?,
            "job" | "root" | "reply" | "claim" | "award" | "content-id" | "content-commitment"
            | "job-hash" | "creq-hash" => require_hex(v, 32)?,
            "sig:seller" | "sig:buyer" | "sig:seller-contribution" => require_hex(v, 64)?,
            "commit" => require_hex(v, 20)?,
            "param:deadline" => {
                decimal(v)?;
            }
            "param:payment" | "payment" => member(v, &["none"])?,
            "param:agent" => member(v, AGENTS)?,
            "param:harness_family" => member(v, FAMILIES)?,
            "output" => member(v, OUTPUTS)?,
            "discovery" => member(v, &["targeted", "open"])?,
            "delivery" => member(v, &["git", "inline"])?,
            "delivery_kind" => member(v, &["fork", "inline"])?,
            "job-class" => member(v, &["contribution"])?,
            "metadata_trust" => member(v, &["seller-claimed"])?,
            "repo" => host.repo(v)?,
            "mint" => host.mint(v)?,
            "branch" => require_hex(
                v.strip_prefix("refs/heads/delivery/")
                    .ok_or(Error("invalid delivery ref"))?,
                32,
            )?,
            "creq" => {
                if v.len() > 16 * 1024 {
                    return Err(Error("invoice too large"));
                }
            }
            _ => {}
        }
    }
    for k in common {
        out.required(k)?;
    }
    let required: &[&str] = match kind {
        3401 => &[
            "visibility",
            "discovery",
            "output",
            "amount",
            "param:deadline",
        ],
        3402 => &["root", "status"],
        3405 | 3406 => &["root", "claim", "status"],
        3404 => &["root", "status", "reason_code"],
        3403 => &[
            "root",
            "award",
            "output",
            "amount",
            "job-hash",
            "sig:seller",
            "delivery",
        ],
        3407 => &["root", "reply", "reason_code", "status", "commit"],
        3400 => &[
            "root",
            "reply",
            "job-hash",
            "amount",
            "mint",
            "sig:seller",
            "sig:buyer",
            "creq-hash",
            "delivery_kind",
            "delivery_integrity_hash",
        ],
        _ => unreachable!(),
    };
    for k in required {
        out.required(k)?;
    }
    if out.has("content-id") != out.has("content-commitment") {
        return Err(Error("incomplete content reference"));
    }
    if let Some(s) = out.get("status") {
        member(
            s,
            match kind {
                3402 => &["processing"],
                3405 | 3406 => &["accepted"],
                3407 => &["rejected"],
                3404 => &["progress", "claim_released", "refusal", "error"],
                _ => &[],
            },
        )?;
    }
    if let Some(r) = out.get("reason_code") {
        member(
            r,
            if kind == 3407 {
                REJECT_REASONS
            } else {
                FEEDBACK_REASONS
            },
        )?;
    }
    if kind == 3401 {
        if out.get("param:payment") == Some("none") && out.get("amount") != Some("0") {
            return Err(Error("free offer carries nonzero amount"));
        }
        if out.get("discovery") == Some("targeted") {
            if out.participants.len() != 1 || out.has("i") || !out.has("content-id") {
                return Err(Error("invalid targeted private offer"));
            }
        } else {
            if !out.participants.is_empty() || out.has("content-id") {
                return Err(Error("invalid open discovery offer"));
            }
            let task = PublicTask::parse(out.required("i")?, out.required("job")?)?;
            if task.contribution.is_some() != out.has("job-class") {
                return Err(Error("contribution marker mismatch"));
            }
            check_dispatch(&out, &task.dispatch)?;
        }
        let n = ["delivery", "repo", "branch"]
            .iter()
            .filter(|k| out.has(k))
            .count();
        if n != 0 && (n != 3 || out.get("delivery") != Some("git")) {
            return Err(Error("incomplete bound delivery"));
        }
        if let Some(repo) = out.get("repo") {
            if repo.strip_suffix(".git").unwrap_or(repo)
                != host.job_repo(&event.pubkey.to_hex(), out.required("job")?)?
            {
                return Err(Error("bound repo is not this buyer's job"));
            }
        }
    } else if out.participants.is_empty() || out.participants.len() > 2 {
        return Err(Error("invalid participant count"));
    }
    if kind == 3402 && out.has("creq") == out.has("payment") {
        return Err(Error("ambiguous claim payment"));
    }
    if kind == 3403 {
        if out.get("delivery") == Some("git") {
            for k in ["repo", "branch", "commit"] {
                out.required(k)?;
            }
        } else if !out.has("content-id") || ["repo", "branch", "commit"].iter().any(|k| out.has(k))
        {
            return Err(Error("invalid private inline result"));
        }
    }
    if kind == 3400 {
        require_hex(
            out.required("delivery_integrity_hash")?,
            if out.get("delivery_kind") == Some("fork") {
                20
            } else {
                32
            },
        )?;
    }
    let numeric = out.has("wall_time") || out.rows.keys().any(|k| k.starts_with("tokens:"));
    if numeric != out.has("metadata_trust") {
        return Err(Error("numeric metadata trust mismatch"));
    }
    Ok(out)
}
pub fn check_dispatch(tags: &Tags, d: &Dispatch) -> Result<()> {
    for (key, value, set) in [
        ("param:agent", d.agent.as_deref(), AGENTS),
        (
            "param:harness_family",
            d.harness_family.as_deref(),
            FAMILIES,
        ),
    ] {
        let expected = value.filter(|v| set.contains(v));
        if tags.get(key) != expected {
            return Err(Error("dispatch tag mismatch"));
        }
    }
    if tags.rows.get("param:capability") != d.capabilities.as_ref() {
        return Err(Error("capability tag mismatch"));
    }
    Ok(())
}
