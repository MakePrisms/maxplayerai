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

/// A provisioning failure: the fixed [`Error`] text, plus the HTTP cause when there is one
/// (`private repository provisioning unavailable: error sending request for url (..): client
/// error (Connect): tunnel error: proxy authorization required`). The leading text is unchanged,
/// so a match on it still works. The cause is [`crate::error_chain::chain`]: credentials in any
/// URL are redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError {
    pub kind: Error,
    cause: Option<String>,
}
impl SendError {
    fn http(kind: &'static str, error: &reqwest::Error) -> Self {
        Self {
            kind: Error(kind),
            cause: Some(crate::error_chain::chain(error)),
        }
    }
    /// A Git transport failure: the fixed text, then the transport's own cause
    /// ([`crate::git_transport::operator_cause`]: no server text, credentials redacted).
    pub(crate) fn transport(
        kind: &'static str,
        error: &crate::git_transport::TransportError,
    ) -> Self {
        Self {
            kind: Error(kind),
            cause: Some(crate::git_transport::operator_cause(error)),
        }
    }
}
impl From<Error> for SendError {
    fn from(kind: Error) -> Self {
        Self { kind, cause: None }
    }
}
impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.cause {
            Some(cause) => write!(f, "{}: {cause}", self.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}
impl std::error::Error for SendError {}

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

    pub async fn send(&self, authorization: &str) -> std::result::Result<(), SendError> {
        self.send_via(reqwest::Client::builder(), authorization)
            .await
    }

    /// [`Self::send`] on a given client builder (tests pass one that ignores ambient proxy
    /// variables).
    async fn send_via(
        &self,
        builder: reqwest::ClientBuilder,
        authorization: &str,
    ) -> std::result::Result<(), SendError> {
        let client = builder
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| SendError::http("private hosting client unavailable", &e))?;
        let mut response = client
            .put(&self.url)
            .header("Authorization", authorization)
            .header("Content-Type", "application/json")
            .body(self.body.clone())
            .send()
            .await
            .map_err(|e| SendError::http("private repository provisioning unavailable", &e))?;
        if !response.status().is_success() {
            return Err(SendError {
                kind: Error("private repository provisioning refused"),
                cause: Some(format!("HTTP {}", response.status())),
            });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| SendError::http("private hosting response incomplete", &e))?
        {
            if bytes.len() + chunk.len() > 4096 {
                return Err(Error("private hosting response too large").into());
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
            || reply.max_objects != super::MAX_OBJECTS
            || reply.max_pack_bytes == 0
            || reply.max_compressed_repository_bytes == 0
        {
            return Err(Error("private hosting binding or limits mismatch").into());
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
    max_objects: usize,
    max_pack_bytes: u64,
    max_compressed_repository_bytes: u64,
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

#[cfg(test)]
mod send_error_tests {
    use super::*;

    // The buyer saw only "private repository provisioning unavailable" while every request failed
    // at its proxy. The fixed text must stay first and the HTTP cause must follow it.
    fn send_to(url: String) -> SendError {
        let request = ProvisionRequest {
            url,
            body: b"{}".to_vec(),
            repo: String::new(),
            job: String::new(),
        };
        // No system proxy: an ambient HTTP(S)_PROXY must not turn a refused connect into a
        // proxy error, or route the request away from the test server.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(request.send_via(reqwest::Client::builder().no_proxy(), "Nostr x"))
            .unwrap_err()
    }

    // A non-2xx answer names its status and never echoes the response body.
    #[test]
    fn refused_names_the_status_and_never_the_body() {
        use std::io::{Read as _, Write as _};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).unwrap();
            let body = "token=secret-in-the-body";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .unwrap();
        });
        let error = send_to(format!("http://{addr}/api/jobs/private/{}", "a".repeat(64)));
        thread.join().unwrap();
        assert_eq!(error.kind, Error("private repository provisioning refused"));
        assert_eq!(
            error.to_string(),
            "private repository provisioning refused: HTTP 403 Forbidden"
        );
    }

    #[test]
    fn unavailable_names_the_http_cause_after_the_fixed_text() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let error = send_to(format!(
            "http://{closed}/api/jobs/private/{}",
            "a".repeat(64)
        ));
        assert_eq!(
            error.kind,
            Error("private repository provisioning unavailable")
        );
        let text = error.to_string();
        assert!(
            text.starts_with("private repository provisioning unavailable: error sending request"),
            "{text}"
        );
        assert!(text.to_ascii_lowercase().contains("refused"), "{text}");
        // A bare static error still reads exactly as before.
        assert_eq!(
            SendError::from(Error("private hosting response too large")).to_string(),
            "private hosting response too large"
        );
    }
}
