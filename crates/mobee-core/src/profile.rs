//! Buyer kind-0 (NIP-01 metadata) publish + best-effort read.
//!
//! **Composition rule:** kind-0 `name` is untrusted display metadata. It must never
//! feed targeting, accept-bind, D2 tip-match, or budget decisions — those stay keyed
//! on hex pubkey alone. This module is intentionally separate from
//! `authorize_pay` / `budget` / `delivery` / `payment`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::home::{self, HomeError, MobeeHome, ProfileConfig};

const DEFAULT_FETCH_TIMEOUT_SECS: u64 = 8;
/// Cap hostile kind-0 payloads (same order as web network parser).
const PROFILE_CONTENT_MAX: usize = 64 * 1024;
const PROFILE_NAME_MAX: usize = 128;
const PROFILE_ABOUT_MAX: usize = 512;

/// Inputs for [`set_profile`]. Omitted fields leave existing config values alone;
/// call with both `None` to re-publish from config as-is.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SetProfileRequest {
    pub name: Option<String>,
    pub about: Option<String>,
}

/// Outcome of a successful `set_profile` (never includes the secret key).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetProfileOutcome {
    pub ok: bool,
    pub pubkey: String,
    pub name: Option<String>,
    pub about: Option<String>,
    pub event_id: String,
    pub relay_url: String,
}

/// Kind-0 + NIP-89 announce ids from seller discoverability publish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SellerDiscoverabilityOutcome {
    pub pubkey: String,
    pub kind0_event_id: String,
    pub nip89_event_id: String,
    pub name: Option<String>,
    pub relay_url: String,
}

/// NIP-89 handler information (kind 31990) — parameterized replaceable via `d`.
const NIP89_HANDLER_KIND: u16 = 31990;
const NIP89_HANDLER_D: &str = "mobee-seller";

#[derive(Debug)]
pub enum ProfileError {
    Input(String),
    Home(HomeError),
    Relay(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "profile input: {message}"),
            Self::Home(error) => write!(formatter, "{error}"),
            Self::Relay(message) => write!(formatter, "profile relay: {message}"),
        }
    }
}

impl std::error::Error for ProfileError {}

impl From<HomeError> for ProfileError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

/// Write optional name/about into `[profile]`, then publish/replace buyer kind-0.
///
/// For callers already on a Tokio runtime (MCP dispatch). Never echoes the secret key.
pub async fn set_profile_async(
    home: &mut MobeeHome,
    request: SetProfileRequest,
) -> Result<SetProfileOutcome, ProfileError> {
    home::reload_config(home)?;

    let name = match &request.name {
        Some(name) => Some(clamp_field(name, PROFILE_NAME_MAX).ok_or_else(|| {
            ProfileError::Input("name must be a non-empty string (max 128 chars)".into())
        })?),
        None => None,
    };
    let about = match &request.about {
        Some(about) => Some(clamp_field(about, PROFILE_ABOUT_MAX).ok_or_else(|| {
            ProfileError::Input("about must be a non-empty string (max 512 chars)".into())
        })?),
        None => None,
    };

    home::save_config(home, |config| {
        // Ensure the section exists even when re-publishing empties (idempotent replace).
        let profile = config.profile.get_or_insert_with(ProfileConfig::default);
        if let Some(name) = name {
            profile.name = Some(name);
        }
        if let Some(about) = about {
            profile.about = Some(about);
        }
    })?;

    let profile = home.config.profile.clone().unwrap_or_default();
    let keys = buyer_keys(home)?;
    // Fail-closed read-merge-write: never blind-overwrite a replaceable kind-0.
    let event_id = publish_metadata_merged_async(home, &keys, &profile).await?;

    Ok(SetProfileOutcome {
        ok: true,
        pubkey: keys.public_key().to_hex(),
        name: profile.name,
        about: profile.about,
        event_id,
        relay_url: home.config.relay_url.clone(),
    })
}

/// Seller start: publish clobber-safe kind-0 + idempotent NIP-89 (d=`mobee-seller`).
///
/// Kind-0: fetch → merge name/about → publish; **abort on fetch failure**.
/// NIP-89: same `d` tag every launch (parameterized replaceable — not spam).
pub fn publish_seller_discoverability(
    home: &mut MobeeHome,
) -> Result<SellerDiscoverabilityOutcome, ProfileError> {
    crate::runtime_guard::refuse_nested_block_on("publish_seller_discoverability")
        .map_err(ProfileError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ProfileError::Relay(error.to_string()))?;
    runtime.block_on(publish_seller_discoverability_async(home))
}

/// Async twin of [`publish_seller_discoverability`].
pub async fn publish_seller_discoverability_async(
    home: &mut MobeeHome,
) -> Result<SellerDiscoverabilityOutcome, ProfileError> {
    home::reload_config(home)?;
    let seller = home.config.seller.as_ref().ok_or_else(|| {
        ProfileError::Input("missing [seller] config for discoverability publish".into())
    })?;
    let rate_sats = seller.rate_sats;
    let claim_open_pool = seller.claim_open_pool;
    let agent = seller.agent.clone();

    // Ensure a display name exists (config or short-hex default).
    let pubkey = home::public_key_hex(home)?;
    let short = &pubkey[..8.min(pubkey.len())];
    if home
        .config
        .profile
        .as_ref()
        .and_then(|p| p.name.as_ref())
        .map(|n| n.trim().is_empty())
        .unwrap_or(true)
    {
        let name = format!("mobee-seller-{short}");
        home::save_config(home, |config| {
            config.profile.get_or_insert_with(ProfileConfig::default).name = Some(name);
        })?;
    }
    if home
        .config
        .profile
        .as_ref()
        .and_then(|p| p.about.as_ref())
        .is_none()
    {
        let agent_label = agent.as_deref().unwrap_or("agent");
        let about = format!("mobee seller · {agent_label} · {rate_sats} sat/job · testnut");
        home::save_config(home, |config| {
            config.profile.get_or_insert_with(ProfileConfig::default).about = Some(about);
        })?;
    }

    let profile = home.config.profile.clone().unwrap_or_default();
    let keys = buyer_keys(home)?;
    let kind0_event_id = publish_metadata_merged_async(home, &keys, &profile).await?;
    let nip89_event_id = publish_nip89_announce_async(
        home,
        &keys,
        &profile,
        rate_sats,
        claim_open_pool,
        agent.as_deref(),
    )
    .await?;

    Ok(SellerDiscoverabilityOutcome {
        pubkey: keys.public_key().to_hex(),
        kind0_event_id,
        nip89_event_id,
        name: profile.name,
        relay_url: home.config.relay_url.clone(),
    })
}

/// Best-effort resolve of kind-0 `name` per pubkey. Missing/unparseable → `None`.
///
/// Returns a map keyed by lowercase hex pubkey. Never used for payment decisions.
pub fn resolve_display_names(
    home: &MobeeHome,
    pubkeys: impl IntoIterator<Item = impl AsRef<str>>,
) -> HashMap<String, Option<String>> {
    let mut unique = HashSet::new();
    for key in pubkeys {
        let hex = key.as_ref().trim().to_ascii_lowercase();
        if hex.len() == 64 && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            unique.insert(hex);
        }
    }
    if unique.is_empty() {
        return HashMap::new();
    }

    match fetch_names(home, &unique) {
        Ok(map) => map,
        Err(_) => unique.into_iter().map(|k| (k, None)).collect(),
    }
}

fn clamp_field(raw: &str, max: usize) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cut = if trimmed.len() > max {
        trimmed.chars().take(max).collect()
    } else {
        trimmed.to_owned()
    };
    Some(cut)
}

fn buyer_keys(home: &MobeeHome) -> Result<nostr_sdk::Keys, ProfileError> {
    let secret = home::read_secret_key_hex(home)?;
    nostr_sdk::Keys::parse(&secret)
        .map_err(|error| ProfileError::Home(HomeError::Key(format!("buyer key parse: {error}"))))
}

/// Fail-closed read-merge-write for replaceable kind-0 (never blind-overwrite).
async fn publish_metadata_merged_async(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    profile: &ProfileConfig,
) -> Result<String, ProfileError> {
    use nostr_sdk::prelude::{Client, EventBuilder, Filter, Kind, Metadata};

    let client = Client::new(keys.clone());
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| ProfileError::Relay(format!("add relay: {error}")))?;
    client.connect().await;

    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Metadata)
        .limit(1);
    let timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);
    let fetched = client.fetch_events(filter, timeout).await;
    let fetched = match fetched {
        Ok(events) => events,
        Err(error) => {
            client.disconnect().await;
            return Err(ProfileError::Relay(format!(
                "kind-0 fetch failed (fail-closed, refuse blind overwrite): {error}"
            )));
        }
    };

    use nostr_sdk::JsonUtil;
    let mut metadata = Metadata::new();
    // Preserve existing fields when present; local config wins for name/about.
    if let Some(existing) = fetched.into_iter().next() {
        if let Ok(parsed) = Metadata::from_json(&existing.content) {
            metadata = parsed;
        } else {
            // Defensive fallback: at least keep name/about if content is partial JSON.
            if let Some(name) = parse_kind0_name(&existing.content) {
                metadata = metadata.name(name);
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&existing.content) {
                if let Some(about) = value
                    .get("about")
                    .and_then(|v| v.as_str())
                    .and_then(|a| clamp_field(a, PROFILE_ABOUT_MAX))
                {
                    metadata = metadata.about(about);
                }
            }
        }
    }
    if let Some(name) = &profile.name {
        metadata = metadata.name(name);
    }
    if let Some(about) = &profile.about {
        metadata = metadata.about(about);
    }

    let event = EventBuilder::metadata(&metadata)
        .sign_with_keys(keys)
        .map_err(|error| {
            // disconnect best-effort before returning
            ProfileError::Relay(format!("sign kind-0: {error}"))
        })?;
    let output = client
        .send_event_to([&home.config.relay_url], &event)
        .await;
    client.disconnect().await;
    let output = output.map_err(|error| ProfileError::Relay(format!("send kind-0: {error}")))?;
    if output.success.is_empty() {
        let failed: Vec<String> = output
            .failed
            .into_iter()
            .map(|(url, err)| format!("{url}: {err}"))
            .collect();
        return Err(ProfileError::Relay(format!(
            "no relay accepted kind-0 ({})",
            failed.join("; ")
        )));
    }
    Ok(output.val.to_hex())
}

async fn publish_nip89_announce_async(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    profile: &ProfileConfig,
    rate_sats: u64,
    claim_open_pool: bool,
    agent: Option<&str>,
) -> Result<String, ProfileError> {
    use nostr_sdk::prelude::{EventBuilder, Kind, Tag};

    let content = serde_json::json!({
        "name": profile.name,
        "about": profile.about,
        "rate_sats": rate_sats,
        "claim_open_pool": claim_open_pool,
        "agent": agent,
        "mint": "testnut",
        "protocol": "mobee-seller",
    })
    .to_string();

    // NIP-89 handler advertises the mobee kinds this seller handles: the OFFER it consumes and the
    // RESULT it produces.
    let k_offer = Tag::parse(["k", &crate::gateway::JOB_OFFER_KIND.to_string()])
        .map_err(|error| ProfileError::Relay(format!("NIP-89 k tag: {error}")))?;
    let k_result = Tag::parse(["k", &crate::gateway::JOB_RESULT_KIND.to_string()])
        .map_err(|error| ProfileError::Relay(format!("NIP-89 k tag: {error}")))?;
    let event = EventBuilder::new(Kind::Custom(NIP89_HANDLER_KIND), content)
        .tags([Tag::identifier(NIP89_HANDLER_D), k_offer, k_result])
        .sign_with_keys(keys)
        .map_err(|error| ProfileError::Relay(format!("sign NIP-89: {error}")))?;

    send_signed_event(home, keys, &event, "NIP-89").await
}

/// NIP-34 kind-30617 announce for the seller delivery remote (required before push).
///
/// Parameterized replaceable via `d=<repo_id>` — idempotent across launches.
pub fn announce_seller_delivery_repo(
    home: &MobeeHome,
    remote_url: &str,
) -> Result<String, ProfileError> {
    crate::runtime_guard::refuse_nested_block_on("announce_seller_delivery_repo")
        .map_err(ProfileError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ProfileError::Relay(error.to_string()))?;
    runtime.block_on(announce_seller_delivery_repo_async(home, remote_url))
}

/// Async twin of [`announce_seller_delivery_repo`].
pub async fn announce_seller_delivery_repo_async(
    home: &MobeeHome,
    remote_url: &str,
) -> Result<String, ProfileError> {
    use nostr_sdk::nips::nip34::GitRepositoryAnnouncement;
    use nostr_sdk::prelude::{EventBuilder, Url};

    // Run the SAME transport allowlist the delivery path enforces on any seller-supplied locator
    // (https + relay-git only; `ext:`/`file:`/`ssh:`/scp forms and URLs embedding credentials are
    // refused). The refusal messages never echo the raw URL, so a credential-bearing remote does
    // not leak into logs.
    crate::delivery_transport::assert_allowed_repo_locator(remote_url)
        .map_err(|refuse| ProfileError::Input(refuse.to_string()))?;

    // Errors below deliberately do NOT interpolate the raw URL (redacted) — the allowlist above
    // already rejected credentials-in-URL, but keep secrets out of error strings regardless.
    let repo_id = home::relay_git_repo_id(remote_url).ok_or_else(|| {
        ProfileError::Input(
            "cannot derive NIP-34 repo id from the configured git-remote (redacted)".into(),
        )
    })?;
    let clone = Url::parse(remote_url)
        .map_err(|_| ProfileError::Input("git-remote URL failed to parse (redacted)".into()))?;
    let name = home
        .config
        .profile
        .as_ref()
        .and_then(|p| p.name.clone())
        .unwrap_or_else(|| repo_id.clone());
    let announcement = GitRepositoryAnnouncement {
        id: repo_id,
        name: Some(name),
        description: Some("mobee seller delivery".into()),
        web: Vec::new(),
        clone: vec![clone],
        relays: Vec::new(),
        euc: None,
        maintainers: Vec::new(),
    };
    let keys = buyer_keys(home)?;
    let event = EventBuilder::git_repository_announcement(announcement)
        .map_err(|error| ProfileError::Relay(format!("build NIP-34: {error}")))?
        .sign_with_keys(&keys)
        .map_err(|error| ProfileError::Relay(format!("sign NIP-34: {error}")))?;
    send_signed_event(home, &keys, &event, "NIP-34").await
}

async fn send_signed_event(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    event: &nostr_sdk::Event,
    label: &str,
) -> Result<String, ProfileError> {
    use nostr_sdk::prelude::Client;

    let client = Client::new(keys.clone());
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| ProfileError::Relay(format!("add relay: {error}")))?;
    client.connect().await;
    let output = client
        .send_event_to([&home.config.relay_url], event)
        .await;
    client.disconnect().await;
    let output =
        output.map_err(|error| ProfileError::Relay(format!("send {label}: {error}")))?;
    if output.success.is_empty() {
        let failed: Vec<String> = output
            .failed
            .into_iter()
            .map(|(url, err)| format!("{url}: {err}"))
            .collect();
        return Err(ProfileError::Relay(format!(
            "no relay accepted {label} ({})",
            failed.join("; ")
        )));
    }
    Ok(output.val.to_hex())
}

fn fetch_names(
    home: &MobeeHome,
    pubkeys: &HashSet<String>,
) -> Result<HashMap<String, Option<String>>, ProfileError> {
    // Sync entry only — must not be called from inside an existing Tokio runtime
    // (nested block_on panics). Async callers use [`fetch_names_async`].
    crate::runtime_guard::refuse_nested_block_on("fetch_names")
        .map_err(ProfileError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ProfileError::Relay(error.to_string()))?;
    runtime.block_on(fetch_names_async(home, pubkeys))
}

/// Async kind-0 name fetch for callers already on a Tokio runtime (e.g. `get_job`).
pub async fn fetch_names_async(
    home: &MobeeHome,
    pubkeys: &HashSet<String>,
) -> Result<HashMap<String, Option<String>>, ProfileError> {
    use nostr_sdk::prelude::{Client, Filter, Kind, PublicKey};

    let keys = buyer_keys(home)?;
    let authors: Result<Vec<PublicKey>, ProfileError> = pubkeys
        .iter()
        .map(|hex| {
            PublicKey::from_hex(hex)
                .map_err(|error| ProfileError::Input(format!("pubkey {hex}: {error}")))
        })
        .collect();
    let authors = authors?;

    let client = Client::new(keys);
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| ProfileError::Relay(format!("add relay: {error}")))?;
    client.connect().await;

    let filter = Filter::new().authors(authors).kind(Kind::Metadata);
    let timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);
    let events = client.fetch_events(filter, timeout).await;
    client.disconnect().await;
    let events =
        events.map_err(|error| ProfileError::Relay(format!("fetch kind-0: {error}")))?;

    // Newest replaceable kind-0 wins per author.
    let mut newest: HashMap<String, (u64, String)> = HashMap::new();
    for event in events {
        let author = event.pubkey.to_hex().to_ascii_lowercase();
        let created = event.created_at.as_secs();
        if newest
            .get(&author)
            .map(|(prev, _)| created > *prev)
            .unwrap_or(true)
        {
            newest.insert(author, (created, event.content.clone()));
        }
    }

    let mut out = HashMap::new();
    for hex in pubkeys {
        let name = newest
            .get(hex)
            .and_then(|(_, content)| parse_kind0_name(content));
        out.insert(hex.clone(), name);
    }
    Ok(out)
}

/// Defensive kind-0 content parse — `name` only (cosmetic).
fn parse_kind0_name(content: &str) -> Option<String> {
    let raw = if content.len() > PROFILE_CONTENT_MAX {
        // Truncate on a char boundary — a plain `content[..PROFILE_CONTENT_MAX]` byte slice
        // panics when a multibyte char straddles the cap. Walk down to the nearest boundary
        // (a byte-cap over-fetch only feeds the JSON parser, which fails closed on garbage).
        let mut end = PROFILE_CONTENT_MAX;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        &content[..end]
    } else {
        content
    };
    let parsed: Kind0Content = serde_json::from_str(raw).ok()?;
    clamp_field(parsed.name.as_deref()?, PROFILE_NAME_MAX)
}

#[derive(Debug, Deserialize)]
struct Kind0Content {
    #[serde(default)]
    name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kind0_name_reads_name_field() {
        assert_eq!(
            parse_kind0_name(r#"{"name":"seller-a","about":"x"}"#).as_deref(),
            Some("seller-a")
        );
        assert_eq!(parse_kind0_name(r#"{"about":"only"}"#), None);
        assert_eq!(parse_kind0_name("not-json"), None);
        assert_eq!(parse_kind0_name(r#"{"name":"   "}"#), None);
    }

    // Finding D: parse_kind0_name must not panic when the PROFILE_CONTENT_MAX byte cap falls in
    // the middle of a multibyte char. A plain `content[..MAX]` byte slice panics on that boundary.
    #[test]
    fn parse_kind0_name_survives_multibyte_char_on_byte_cap() {
        // '😀' is 4 bytes; placing it so it starts at PROFILE_CONTENT_MAX-1 makes byte index
        // PROFILE_CONTENT_MAX land INSIDE the char (not a char boundary).
        let mut content = "a".repeat(PROFILE_CONTENT_MAX - 1);
        content.push('😀');
        content.push_str("bbbb");
        assert!(!content.is_char_boundary(PROFILE_CONTENT_MAX));
        // Must return without panicking (the over-cap content is not valid JSON → None).
        assert_eq!(parse_kind0_name(&content), None);
    }

    // Finding D: the delivery-repo announce runs the transport allowlist BEFORE any publish and
    // never leaks the raw locator (which could embed credentials) into error strings.
    #[test]
    fn announce_delivery_repo_refuses_bad_locators_without_leaking_url() {
        let root = std::env::temp_dir().join(format!(
            "mobee-announce-allowlist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");

        // Forbidden scheme (ext::) refuses at the allowlist, before any relay I/O.
        let err = announce_seller_delivery_repo(&home, "ext::sh -c evil").expect_err("ext refused");
        assert!(err.to_string().contains("refused"), "got: {err}");

        // Credentials-in-URL refuses AND the secret never appears in the error string.
        let err = announce_seller_delivery_repo(&home, "https://user:sup3rsecret@example.invalid/repo.git")
            .expect_err("credentials refused");
        let message = err.to_string();
        assert!(message.contains("credentials"), "got: {message}");
        assert!(
            !message.contains("sup3rsecret") && !message.contains("user:"),
            "error leaked the credential-bearing URL: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_display_names_skips_invalid_hex_without_relay() {
        let root = std::env::temp_dir().join(format!(
            "mobee-profile-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let map = resolve_display_names(&home, ["not-a-key", ""]);
        assert!(map.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_profile_writes_config_without_inventing_on_bootstrap() {
        let root = std::env::temp_dir().join(format!(
            "mobee-profile-set-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        assert!(home.config.profile.is_none());

        // Persist a profile through the file-only edit view (skips relay publish).
        home::save_config(&mut home, |config| {
            config.profile = Some(ProfileConfig {
                name: Some("buyer-x".into()),
                about: Some("about-x".into()),
            });
        })
        .expect("save");
        home::reload_config(&mut home).expect("reload");
        let profile = home.config.profile.expect("present");
        assert_eq!(profile.name.as_deref(), Some("buyer-x"));
        assert_eq!(profile.about.as_deref(), Some("about-x"));
        let _ = std::fs::remove_dir_all(&root);
    }


    #[tokio::test(flavor = "current_thread")]
    async fn fetch_names_sync_refuses_inside_runtime() {
        let root = std::env::temp_dir().join(format!(
            "mobee-fetch-names-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut pubkeys = HashSet::new();
        pubkeys.insert("aa".repeat(32));
        let err = fetch_names(&home, &pubkeys).expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        assert!(
            err.to_string().contains("fetch_names"),
            "op name missing: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
