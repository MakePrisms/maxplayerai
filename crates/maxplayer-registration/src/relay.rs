//! NIP-43 relay membership commands, sent over the relay's NIP-98 HTTP bridge.
//!
//! buzz-relay handles 9030 (add) and 9031 (remove) in `handlers/relay_admin.rs`: the
//! signer must hold the `admin` or `owner` role, `created_at` must be within ±120 s,
//! and the target is the first `p` tag. An add for an existing member is a silent
//! no-op, so re-sending a 9030 is safe. A remove for a missing member answers
//! "member not found", which this client treats as already done.

use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, Tag, Timestamp};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HOST};

use crate::config::RelayConfig;
use crate::nip98;

pub const KIND_ADD_MEMBER: u16 = 9030;
pub const KIND_REMOVE_MEMBER: u16 = 9031;
pub const KIND_MEMBER_LIST: u16 = 13534;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberAction {
    Add,
    Remove,
}

impl MemberAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "add" => Some(Self::Add),
            "remove" => Some(Self::Remove),
            _ => None,
        }
    }
    pub fn kind(self) -> u16 {
        match self {
            Self::Add => KIND_ADD_MEMBER,
            Self::Remove => KIND_REMOVE_MEMBER,
        }
    }
}

/// `{"kind": 9030|9031, "content": "", "tags": [["-"], ["p", <target hex>]]}`, signed
/// by the relay admin key with `created_at` = now. No `role` tag: buzz defaults to
/// `member`, the only role this service ever grants.
pub fn build_member_command(
    admin: &Keys,
    action: MemberAction,
    target: &PublicKey,
    created_at: Timestamp,
) -> Event {
    EventBuilder::new(Kind::Custom(action.kind()), "")
        .tags([Tag::protected(), Tag::public_key(*target)])
        .custom_created_at(created_at)
        .sign_with_keys(admin)
        .expect("signing with local keys cannot fail")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    Applied {
        event_id: String,
    },
    /// A remove for a key that is not a member.
    AlreadyDone,
}

pub struct RelayClient {
    http: reqwest::Client,
    base: String,
    auth_base: String,
    host_header: Option<String>,
    admin: Keys,
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

impl RelayClient {
    pub fn new(cfg: &RelayConfig, admin: Keys) -> anyhow::Result<Self> {
        let base = cfg.base_url.trim_end_matches('/').to_string();
        let auth_base = cfg.effective_auth_base().to_string();
        let host_header = if auth_base != base {
            let u = url::Url::parse(&auth_base)?;
            let host = u.host_str().unwrap_or_default().to_string();
            Some(match u.port() {
                Some(p) => format!("{host}:{p}"),
                None => host,
            })
        } else {
            None
        };
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            base,
            auth_base,
            host_header,
            admin,
        })
    }

    pub fn admin_pubkey(&self) -> PublicKey {
        self.admin.public_key()
    }

    async fn post(
        &self,
        path: &str,
        body: Vec<u8>,
        now: Timestamp,
    ) -> Result<(reqwest::StatusCode, String), String> {
        let auth = nip98::authorization(
            &self.admin,
            &format!("{}{path}", self.auth_base),
            "POST",
            Some(&body),
            now,
        );
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .header(AUTHORIZATION, auth)
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(host) = &self.host_header {
            req = req.header(HOST, host);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("relay unreachable: {}", e.without_url()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    /// Send one fresh 9030/9031 for `target`.
    pub async fn send_member_command(
        &self,
        action: MemberAction,
        target: &PublicKey,
        now: Timestamp,
    ) -> Result<CommandOutcome, String> {
        let event = build_member_command(&self.admin, action, target, now);
        let (status, text) = self
            .post("/events", event.as_json().into_bytes(), now)
            .await?;
        let json: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        if status.is_success() && json["accepted"].as_bool() == Some(true) {
            return Ok(CommandOutcome::Applied {
                event_id: event.id.to_hex(),
            });
        }
        let msg = json["message"]
            .as_str()
            .filter(|m| !m.is_empty())
            .or_else(|| json["error"].as_str())
            .unwrap_or("")
            .to_string();
        if action == MemberAction::Remove && msg.contains("member not found") {
            return Ok(CommandOutcome::AlreadyDone);
        }
        Err(format!(
            "relay HTTP {}: {}",
            status.as_u16(),
            truncate(&msg, 200)
        ))
    }

    /// The relay's newest kind-13534 member list, signed by `relay_pubkey`:
    /// `(pubkey hex, role)` pairs. `Ok(None)` = the relay has published none.
    pub async fn fetch_member_list(
        &self,
        relay_pubkey: &PublicKey,
        now: Timestamp,
    ) -> Result<Option<Vec<(String, String)>>, String> {
        let filter = serde_json::json!([{
            "kinds": [KIND_MEMBER_LIST],
            "authors": [relay_pubkey.to_hex()],
            "limit": 1
        }]);
        let (status, text) = self
            .post("/query", filter.to_string().into_bytes(), now)
            .await?;
        if !status.is_success() {
            return Err(format!("relay query HTTP {}", status.as_u16()));
        }
        let events: Vec<serde_json::Value> =
            serde_json::from_str(&text).map_err(|_| "relay query: not a JSON array".to_string())?;
        let newest = events
            .iter()
            .filter_map(|v| Event::from_json(v.to_string()).ok())
            .filter(|e| {
                e.kind == Kind::Custom(KIND_MEMBER_LIST)
                    && e.pubkey == *relay_pubkey
                    && e.verify().is_ok()
            })
            .max_by_key(|e| e.created_at);
        Ok(newest.map(|e| parse_member_list(&e)))
    }
}

/// `["member", <hex>, <role>]` tags of a 13534 (buzz format; role defaults to member).
pub fn parse_member_list(event: &Event) -> Vec<(String, String)> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) != Some("member") {
                return None;
            }
            let pk = s.get(1)?.to_ascii_lowercase();
            if pk.len() != 64 || !pk.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            Some((pk, s.get(2).cloned().unwrap_or_else(|| "member".into())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_remove_commands_have_the_nip43_shape() {
        let admin = Keys::generate();
        let target = Keys::generate().public_key();
        for (action, kind) in [(MemberAction::Add, 9030), (MemberAction::Remove, 9031)] {
            let ev = build_member_command(&admin, action, &target, Timestamp::from(1_800_000_000));
            ev.verify().unwrap();
            assert_eq!(ev.kind.as_u16(), kind);
            assert_eq!(ev.pubkey, admin.public_key());
            assert_eq!(ev.content, "");
            assert_eq!(ev.created_at.as_secs(), 1_800_000_000);
            let tags: Vec<Vec<String>> = ev.tags.iter().map(|t| t.as_slice().to_vec()).collect();
            assert_eq!(
                tags,
                vec![
                    vec!["-".to_string()],
                    vec!["p".to_string(), target.to_hex()]
                ]
            );
        }
    }

    #[test]
    fn member_list_parsing_keeps_valid_member_tags_only() {
        let relay = Keys::generate();
        let a = Keys::generate().public_key().to_hex();
        let b = Keys::generate().public_key().to_hex();
        let ev = EventBuilder::new(Kind::Custom(KIND_MEMBER_LIST), "")
            .tags([
                Tag::protected(),
                Tag::parse(["member", &a, "admin"]).unwrap(),
                Tag::parse(["member", &b.to_uppercase()]).unwrap(),
                Tag::parse(["member", "nothex"]).unwrap(),
                Tag::parse(["p", &a]).unwrap(),
            ])
            .sign_with_keys(&relay)
            .unwrap();
        assert_eq!(
            parse_member_list(&ev),
            vec![(a, "admin".into()), (b, "member".into())]
        );
    }

    #[test]
    fn action_round_trips() {
        for a in [MemberAction::Add, MemberAction::Remove] {
            assert_eq!(MemberAction::parse(a.as_str()), Some(a));
        }
        assert_eq!(MemberAction::parse("ban"), None);
    }
}
