//! Bounded, destination-bound private repository provisioning. This is storage setup,
//! not a READY/ACK phase. Tokens are signed in-process and redirects are forbidden.
use super::{
    Error, Result,
    wire::{self, HostPolicy},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use nostr_sdk::prelude::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

pub struct ProvisionRequest {
    url: String,
    body: Vec<u8>,
    repo: String,
    job: String,
}
impl ProvisionRequest {
    pub fn new(
        offer: &Event,
        claim: Option<&Event>,
        award: Option<&Event>,
        host: &HostPolicy,
    ) -> Result<Self> {
        let tags = wire::validate_private(offer, host)?;
        if offer.kind.as_u16() != 3401 || claim.is_some() != award.is_some() {
            return Err(Error("invalid private provisioning context"));
        }
        if let (Some(claim), Some(award)) = (claim, award) {
            let c = wire::validate_private(claim, host)?;
            let a = wire::validate_private(award, host)?;
            let buyer = offer.pubkey.to_hex();
            let seller = claim.pubkey.to_hex();
            let parties = std::collections::BTreeSet::from([buyer.clone(), seller.clone()]);
            if claim.kind.as_u16() != 3402
                || award.kind.as_u16() != 3405
                || award.pubkey != offer.pubkey
                || c.get("root") != Some(offer.id.to_hex().as_str())
                || a.get("root") != c.get("root")
                || c.get("job") != tags.get("job")
                || a.get("job") != tags.get("job")
                || a.get("claim") != Some(claim.id.to_hex().as_str())
                || a.participants != parties
                || !c.participants.contains(&buyer)
                || c.participants.iter().any(|p| !parties.contains(p))
                || (!tags.participants.is_empty() && !tags.participants.contains(&seller))
            {
                return Err(Error("invalid private provisioning award"));
            }
        } else if tags.get("discovery") != Some("targeted") {
            return Err(Error("open-pool repo requires selected award"));
        }
        let job = tags.required("job")?.to_string();
        let repo = host.job_repo(&offer.pubkey.to_hex(), &job)?;
        let mut url = Url::parse(&repo).map_err(|_| Error("invalid hosting URL"))?;
        url.set_path(&format!("/api/jobs/private/{job}"));
        let mut value = serde_json::json!({"signed_offer":offer});
        if let Some(claim) = claim {
            value["signed_claim"] =
                serde_json::to_value(claim).map_err(|_| Error("claim encoding failed"))?;
        }
        if let Some(award) = award {
            value["signed_award"] =
                serde_json::to_value(award).map_err(|_| Error("award encoding failed"))?;
        }
        let body = serde_json::to_vec(&value).map_err(|_| Error("provision encoding failed"))?;
        if body.len() > 128 * 1024 {
            return Err(Error("provision request too large"));
        }
        Ok(Self {
            url: url.into(),
            body,
            repo,
            job,
        })
    }
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub async fn send(&self, authorization: &str) -> Result<()> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| Error("private hosting client unavailable"))?;
        let mut response = client
            .put(&self.url)
            .header("Authorization", authorization)
            .header("Content-Type", "application/json")
            .body(self.body.clone())
            .send()
            .await
            .map_err(|_| Error("private repository provisioning unavailable"))?;
        if !response.status().is_success() {
            return Err(Error("private repository provisioning refused"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error("private hosting response incomplete"))?
        {
            if bytes.len() + chunk.len() > 4096 {
                return Err(Error("private hosting response too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        super::strict_json::validate(&bytes)?;
        let reply: ProvisionReply = serde_json::from_slice(&bytes)
            .map_err(|_| Error("invalid private hosting response"))?;
        if reply.repo != self.repo
            || reply.job_id != self.job
            || reply.max_file_bytes != super::MAX_FILE_BYTES
            || reply.max_repository_bytes != super::MAX_REPO_BYTES
            || reply.max_files != super::MAX_FILES
        {
            return Err(Error("private hosting binding or limits mismatch"));
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionReply {
    repo: String,
    job_id: String,
    max_file_bytes: u64,
    max_repository_bytes: u64,
    max_files: usize,
}
/// Re-mint for each attempt. A nonce distinguishes otherwise-identical retries in the
/// same second, while the server's replay guard rejects reuse of a particular auth ID.
pub fn auth_header(keys: &Keys, url: &str, body: &[u8]) -> Result<String> {
    if body.len() > 128 * 1024 {
        return Err(Error("provision request too large"));
    }
    let parsed = Url::parse(url).map_err(|_| Error("invalid provisioning URL"))?;
    let job = parsed
        .path()
        .strip_prefix("/api/jobs/private/")
        .ok_or(Error("invalid provisioning route"))?;
    super::require_hex(job, 32)?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(Error("unsafe provisioning URL"));
    }
    let digest = hex::encode(Sha256::digest(body));
    let event = EventBuilder::http_auth(
        nostr_sdk::nostr::nips::nip98::HttpData::new(
            parsed,
            nostr_sdk::nostr::nips::nip98::HttpMethod::PUT,
        )
        .payload(
            digest
                .parse()
                .map_err(|_| Error("invalid payload digest"))?,
        ),
    )
    .tag(
        Tag::parse(["nonce", super::random_id()?.as_str()])
            .map_err(|_| Error("auth nonce failed"))?,
    )
    .sign_with_keys(keys)
    .map_err(|_| Error("provision signing failed"))?;
    Ok(format!("Nostr {}", STANDARD.encode(event.as_json())))
}
