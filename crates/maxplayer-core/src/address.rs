//! Relay-owned NIP-05 handles. Registry discovery never trusts kind-0 identity claims.
use crate::home::MaxplayerHome;
use nostr_sdk::{EventBuilder, JsonUtil, Keys, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub fn normalize_name(raw: &str) -> Result<String, String> {
    let name = raw.to_ascii_lowercase();
    let b = name.as_bytes();
    if !(3..=30).contains(&b.len())
        || !b[0].is_ascii_alphanumeric()
        || !b
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
    {
        return Err("invalid handle: expected ^[a-z0-9][a-z0-9._-]{2,29}$".into());
    }
    Ok(name)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DisplayName {
    pub handle: Option<String>,
    pub address: Option<String>,
}
impl DisplayName {
    pub fn new(handle: Option<String>, domain: &str) -> Self {
        let address = handle
            .as_ref()
            .map(|n| format!("{n}@{}", domain.to_ascii_lowercase()));
        Self { handle, address }
    }
}

pub struct Registry {
    client: reqwest::Client,
    base: Url,
    pub domain: String,
}
#[derive(Deserialize)]
struct Availability {
    available: bool,
    reason: Option<String>,
}
impl Registry {
    pub fn new(relay: &str, domain: &str) -> Result<Self, String> {
        let mut base = Url::parse(relay).map_err(|e| e.to_string())?;
        let scheme = match base.scheme() {
            "wss" | "https" => "https",
            "ws" | "http" => "http",
            _ => return Err("unsupported relay scheme".into()),
        };
        base.set_scheme(scheme).map_err(|_| "invalid relay URL")?;
        if !base.username().is_empty() || base.password().is_some() || base.host_str().is_none() {
            return Err("relay URL must have a host and no credentials".into());
        }
        if domain.is_empty()
            || domain
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        {
            return Err("invalid nip05_domain".into());
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| e.to_string())?,
            base,
            domain: domain.to_ascii_lowercase(),
        })
    }
    pub fn for_home(home: &MaxplayerHome) -> Result<Self, String> {
        Self::new(&home.config.relay_url, &home.config.nip05_domain)
    }
    fn url(&self, path: &str) -> Url {
        self.base.join(path).expect("constant absolute API path")
    }
    async fn response(response: reqwest::Response) -> Result<serde_json::Value, String> {
        let status = response.status();
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|_| format!("handle API HTTP {status}: invalid JSON"))?;
        if !status.is_success() {
            let reason = value
                .get("reason")
                .or_else(|| value.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("request refused");
            return Err(format!("handle API HTTP {status}: {reason}"));
        }
        Ok(value)
    }
    pub async fn lookup(&self, keys: &[String]) -> Result<HashMap<String, String>, String> {
        let mut out = HashMap::new();
        for batch in keys.chunks(500) {
            let mut url = self.url("/api/handles");
            url.query_pairs_mut()
                .append_pair("pubkeys", &batch.join(","));
            let response = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let rows: HashMap<String, String> =
                serde_json::from_value(Self::response(response).await?)
                    .map_err(|e| e.to_string())?;
            for (key, name) in rows {
                if batch.contains(&key) && normalize_name(&name).as_ref() == Ok(&name) {
                    out.insert(key, name);
                }
            }
        }
        Ok(out)
    }
    pub async fn claim(&self, keys: &Keys, raw: &str) -> Result<String, String> {
        let name = normalize_name(raw)?;
        let mut url = self.url("/api/handles/available");
        url.query_pairs_mut().append_pair("name", &name);
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let available: Availability =
            serde_json::from_value(Self::response(response).await?).map_err(|e| e.to_string())?;
        // Availability is anonymous: only authenticated claim can decide own reclaim/idempotency.
        if !available.available
            && !matches!(available.reason.as_deref(), Some("cooldown" | "taken"))
        {
            return Err(format!(
                "handle unavailable: {}",
                available.reason.as_deref().unwrap_or("unknown")
            ));
        }
        let body = serde_json::to_vec(&serde_json::json!({"name":name})).unwrap();
        let result = self.mutate(keys, "/api/handles/claim", body).await?;
        if result["name"].as_str() != Some(name.as_str())
            || result["pubkey"].as_str() != Some(keys.public_key().to_hex().as_str())
        {
            return Err("claim response does not match requested handle/owner".into());
        }
        Ok(name)
    }
    pub async fn release(&self, keys: &Keys) -> Result<(), String> {
        let result = self
            .mutate(keys, "/api/handles/release", Vec::new())
            .await?;
        if result["released"] != true {
            return Err("invalid release response".into());
        }
        Ok(())
    }
    async fn mutate(
        &self,
        keys: &Keys,
        path: &str,
        body: Vec<u8>,
    ) -> Result<serde_json::Value, String> {
        let url = self.url(path);
        let auth = authorization(&url, keys, &body)?;
        let response = self
            .client
            .post(url)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        Self::response(response).await
    }
}
fn authorization(url: &Url, keys: &Keys, body: &[u8]) -> Result<String, String> {
    use base64::Engine;
    use nostr_sdk::nips::nip98::{HttpData, HttpMethod};
    let digest = hex::encode(Sha256::digest(body))
        .parse()
        .map_err(|_| "invalid payload hash")?;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| "nonce generation failed")?;
    let event =
        EventBuilder::http_auth(HttpData::new(url.clone(), HttpMethod::POST).payload(digest))
            .tag(nostr_sdk::Tag::parse(["nonce", &hex::encode(nonce)]).map_err(|e| e.to_string())?)
            .sign_with_keys(keys)
            .map_err(|e| e.to_string())?;
    Ok(format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(event.as_json())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::io::{Read, Write};

    #[test]
    fn handle_name_validation() {
        for raw in [
            "ab", "", "_abc", "a+b", "a@b", " alice", "álîce", "a/b", "a\nb",
        ] {
            assert!(normalize_name(raw).is_err(), "{raw}");
        }
        assert!(normalize_name(&"a".repeat(31)).is_err());
        for raw in ["ABC", "a._", "0-a", &"x".repeat(30)] {
            assert_eq!(normalize_name(raw).unwrap(), raw.to_ascii_lowercase());
        }
    }
    fn verify_auth(header: &str, url: &str, body: &[u8], key: &Keys) {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(header.strip_prefix("Nostr ").unwrap())
            .unwrap();
        let event: nostr_sdk::Event = serde_json::from_slice(&raw).unwrap();
        event.verify().unwrap();
        assert_eq!(event.pubkey, key.public_key());
        assert_eq!(event.kind.as_u16(), 27235);
        for (tag, value) in [
            ("u", url.to_owned()),
            ("method", "POST".into()),
            ("payload", hex::encode(Sha256::digest(body))),
        ] {
            assert!(event.tags.iter().any(|t| t.as_slice() == [tag, &value]));
        }
        assert!(
            event
                .created_at
                .as_secs()
                .abs_diff(nostr_sdk::Timestamp::now().as_secs())
                <= 60
        );
    }
    #[test]
    fn nip98_binds_exact_url_method_and_raw_payload_including_empty() {
        let keys = Keys::generate();
        let url = Url::parse("http://127.0.0.1:1234/api/handles/claim?x=y").unwrap();
        for body in [b"".as_slice(), br#"{"name":"alice"}"#, b"{}"] {
            verify_auth(
                &authorization(&url, &keys, body).unwrap(),
                url.as_str(),
                body,
                &keys,
            );
        }
    }
    // Actual local HTTP fixture: records method, target, headers and exact raw body.
    fn server(
        responses: Vec<(u16, serde_json::Value)>,
        keys: Keys,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = base.clone();
        let task = std::thread::spawn(move || {
            let mut targets = Vec::new();
            for (status, response) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0];
                while !bytes.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let headers = String::from_utf8(bytes).unwrap();
                let lines: Vec<_> = headers.lines().collect();
                let request: Vec<_> = lines[0].split_whitespace().collect();
                targets.push(format!("{} {}", request[0], request[1]));
                let length: usize = lines
                    .iter()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .map(|n| n.parse().unwrap())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                if request[0] == "POST" {
                    let auth = lines
                        .iter()
                        .find_map(|l| {
                            l.split_once(':')
                                .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                                .map(|(_, v)| v.trim())
                        })
                        .unwrap();
                    verify_auth(auth, &format!("{url}{}", request[1]), &body, &keys);
                    if request[1].ends_with("claim") {
                        assert_eq!(
                            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["name"],
                            "alice"
                        );
                    } else {
                        assert!(body.is_empty());
                    }
                }
                let response = response.to_string();
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
            targets
        });
        (base, task)
    }
    #[tokio::test]
    async fn handle_http_claim_available_release_and_lookup() {
        let keys = Keys::generate();
        let hex = keys.public_key().to_hex();
        let (url, task) = server(
            vec![
                (200, serde_json::json!({"available":true,"reason":null})),
                (200, serde_json::json!({"name":"alice","pubkey":hex})),
                (200, serde_json::json!({hex.clone():"alice"})),
                (200, serde_json::json!({"released":true})),
            ],
            keys.clone(),
        );
        let registry = Registry::new(&url, "example.org").unwrap();
        assert_eq!(registry.claim(&keys, "ALIce").await.unwrap(), "alice");
        assert_eq!(
            registry.lookup(&[hex.clone()]).await.unwrap()[&hex],
            "alice"
        );
        registry.release(&keys).await.unwrap();
        let requests = task.join().unwrap();
        assert_eq!(requests[0], "GET /api/handles/available?name=alice");
        assert_eq!(requests[1], "POST /api/handles/claim");
        assert_eq!(requests[2], format!("GET /api/handles?pubkeys={hex}"));
        assert_eq!(requests[3], "POST /api/handles/release");
    }
    #[tokio::test]
    async fn handle_http_availability_reasons_and_claim_conflicts() {
        for reason in ["taken", "reserved", "cooldown", "invalid"] {
            let keys = Keys::generate();
            let mut replies = vec![(200, serde_json::json!({"available":false,"reason":reason}))];
            if matches!(reason, "taken" | "cooldown") {
                replies.push((409, serde_json::json!({"available":false,"reason":reason})));
            }
            let (url, task) = server(replies, keys.clone());
            assert!(
                Registry::new(&url, "maxplayer.ai")
                    .unwrap()
                    .claim(&keys, "alice")
                    .await
                    .unwrap_err()
                    .contains(reason)
            );
            task.join().unwrap();
        }
        // Availability can race: every server-side reason is surfaced after an available check.
        for reason in ["taken", "reserved", "cooldown", "invalid"] {
            let keys = Keys::generate();
            let (url, task) = server(
                vec![
                    (200, serde_json::json!({"available":true})),
                    (409, serde_json::json!({"reason":reason})),
                ],
                keys.clone(),
            );
            assert!(
                Registry::new(&url, "maxplayer.ai")
                    .unwrap()
                    .claim(&keys, "alice")
                    .await
                    .unwrap_err()
                    .contains(reason)
            );
            task.join().unwrap();
        }
    }
    #[tokio::test]
    async fn handle_http_unauthorized_mutations_and_available_failure() {
        for operation in ["claim", "release", "available"] {
            let keys = Keys::generate();
            let mut replies = Vec::new();
            if operation == "claim" {
                replies.push((200, serde_json::json!({"available":true})));
            }
            replies.push((401, serde_json::json!({"error":"unauthorized"})));
            let (url, task) = server(replies, keys.clone());
            let registry = Registry::new(&url, "maxplayer.ai").unwrap();
            let err = if operation == "release" {
                registry.release(&keys).await.unwrap_err()
            } else {
                registry.claim(&keys, "alice").await.unwrap_err()
            };
            assert!(err.contains("401"));
            task.join().unwrap();
        }
    }
    #[tokio::test]
    async fn handle_http_cooldown_owner_can_reclaim() {
        let keys = Keys::generate();
        let (url, task) = server(
            vec![
                (
                    200,
                    serde_json::json!({"available":false,"reason":"cooldown"}),
                ),
                (
                    200,
                    serde_json::json!({"name":"alice","pubkey":keys.public_key().to_hex()}),
                ),
            ],
            keys.clone(),
        );
        assert_eq!(
            Registry::new(&url, "maxplayer.ai")
                .unwrap()
                .claim(&keys, "alice")
                .await
                .unwrap(),
            "alice"
        );
        task.join().unwrap();
    }
    #[test]
    fn mcp_display_name_shape_is_registry_only() {
        assert_eq!(
            serde_json::to_value(DisplayName::new(Some("alice".into()), "maxplayer.ai")).unwrap(),
            serde_json::json!({"handle":"alice","address":"alice@maxplayer.ai"})
        );
        assert_eq!(
            serde_json::to_value(DisplayName::new(None, "maxplayer.ai")).unwrap(),
            serde_json::json!({"handle":null,"address":null})
        );
    }
}
