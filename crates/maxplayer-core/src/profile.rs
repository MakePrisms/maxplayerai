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

use crate::home::{self, HomeError, MaxplayerHome, ProfileConfig};

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

/// Kind-0 identity publish from seller start. Kind 0 carries the seat's NAME (§4.1); its
/// capability rides the kind-30340 announcement (§4.2) and is published by the heartbeat, not here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SellerDiscoverabilityOutcome {
    pub pubkey: String,
    pub kind0_event_id: String,
    pub name: Option<String>,
    pub relay_url: String,
}

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
    home: &mut MaxplayerHome,
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
        apply_profile_updates(profile, name, about);
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

/// Seller start: publish the clobber-safe kind-0 identity. Fetch → merge name/about → publish;
/// **abort on fetch failure**.
///
/// Kind-0 is the ONLY event this publishes. Issue #645 retired the kind-31990 handler announce
/// that used to ride alongside it: every capability it carried (mints, rate, harness,
/// `claim_open_pool`) is now on the kind-30340 seat announcement, which the heartbeat republishes
/// each beat with live values — where the 31990 was written once at boot and then went stale.
pub fn publish_seller_discoverability(
    home: &mut MaxplayerHome,
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
    home: &mut MaxplayerHome,
) -> Result<SellerDiscoverabilityOutcome, ProfileError> {
    home::reload_config(home)?;
    let seller = home.config.seller.as_ref().ok_or_else(|| {
        ProfileError::Input("missing [seller] config for discoverability publish".into())
    })?;
    let rate_sats = seller.rate_sats;
    // Only the default `about` sentence reads these. The seat's ADVERTISED rate and harness roster
    // are published by the kind-30340 heartbeat from live state; kind-0 `about` is display prose a
    // reader must never target, pay, or dispatch on (§4.1).
    let agent = seller.agents.first().cloned();

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
        let name = format!("maxplayer-seller-{short}");
        home::save_config(home, |config| {
            config.profile.get_or_insert_with(ProfileConfig::default).name = Some(name);
        })?;
    }
    let existing_about = home.config.profile.as_ref().and_then(|p| p.about.as_deref());
    let existing_generated = home.config.profile.as_ref().and_then(|p| p.about_generated);
    let live_about = default_seller_about(agent.as_deref(), rate_sats, &home.config.accepted_mints);
    if let Some((about, about_generated)) =
        generated_about_update(existing_about, existing_generated, live_about)
    {
        home::save_config(home, |config| {
            let profile = config.profile.get_or_insert_with(ProfileConfig::default);
            profile.about = Some(about);
            profile.about_generated = Some(about_generated);
        })?;
    }

    let profile = home.config.profile.clone().unwrap_or_default();
    let keys = buyer_keys(home)?;
    let kind0_event_id = publish_metadata_merged_async(home, &keys, &profile).await?;

    Ok(SellerDiscoverabilityOutcome {
        pubkey: keys.public_key().to_hex(),
        kind0_event_id,
        name: profile.name,
        relay_url: home.config.relay_url.clone(),
    })
}

/// Best-effort resolve of kind-0 `name` per pubkey. Missing/unparseable → `None`.
///
/// Returns a map keyed by lowercase hex pubkey. Never used for payment decisions.
pub fn resolve_display_names(
    home: &MaxplayerHome,
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

fn buyer_keys(home: &MaxplayerHome) -> Result<nostr_sdk::Keys, ProfileError> {
    let secret = home::read_secret_key_hex(home)?;
    nostr_sdk::Keys::parse(&secret)
        .map_err(|error| ProfileError::Home(HomeError::Key(format!("buyer key parse: {error}"))))
}

/// Fail-closed read-merge-write for replaceable kind-0 (never blind-overwrite).
async fn publish_metadata_merged_async(
    home: &MaxplayerHome,
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

fn apply_profile_updates(profile: &mut ProfileConfig, name: Option<String>, about: Option<String>) {
    if let Some(name) = name {
        profile.name = Some(name);
    }
    if let Some(about) = about {
        profile.about = Some(about);
        profile.about_generated = Some(false);
    }
}

/// Return a generated `about` update only for an empty profile or text previously stamped as
/// generated. Unknown provenance is protected; issue #625 showed content-based inference cannot
/// distinguish stale generated prose from intentional operator prose.
///
/// Issue #678: when `about_generated = Some(true)` and the live text is unchanged, return `None`
/// so boot does not rewrite `config.toml`. Comparison is byte-equal on already-clamped text:
/// - live: `default_seller_about` clamps to `PROFILE_ABOUT_MAX` before calling here
/// - stored: the only writer that stamps `about_generated = Some(true)` is this path (or the
///   prior unconditional arm), which always persists that same clamped `live_about`; operator
///   `set_profile_async` clamps too but stamps `Some(false)`. So both sides are comparable
///   without re-clamping the stored value.
fn generated_about_update(
    existing_about: Option<&str>,
    existing_generated: Option<bool>,
    live_about: String,
) -> Option<(String, bool)> {
    match (existing_about, existing_generated) {
        (Some(current), Some(true)) if current == live_about => None,
        (None, _) | (Some(_), Some(true)) => Some((live_about, true)),
        (Some(_), Some(false) | None) => None,
    }
}

/// Default kind-0 / profile `about` when the operator has not set one. Mint label lists every
/// accepted mint (issue #625: a multi-mint seat must not advertise only its first) shortened to
/// its host. The fallback is the honest `"no-mint"`, never a placeholder mint (#453).
fn default_seller_about(agent: Option<&str>, rate_sats: u64, accepted_mints: &[String]) -> String {
    let agent_label = agent.unwrap_or("agent");
    let mint_label = if accepted_mints.is_empty() {
        "no-mint".to_owned()
    } else {
        accepted_mints.iter().map(|mint| mint_host_label(mint)).collect::<Vec<_>>().join(", ")
    };
    let about = format!("maxplayer seller · {agent_label} · {rate_sats} sat/job · {mint_label}");
    clamp_field(&about, PROFILE_ABOUT_MAX).unwrap_or(about)
}

/// Shorten a mint URL to the host a short ad can carry. Values that do not parse as HTTP(S)-style
/// URLs pass through untouched, preserving the deleted buzz persona's labeling behavior.
fn mint_host_label(mint: &str) -> String {
    let rest = mint
        .strip_prefix("https://")
        .or_else(|| mint.strip_prefix("http://"))
        .unwrap_or(mint);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if host.is_empty() { mint.to_owned() } else { host.to_owned() }
}

/// NIP-34 kind-30617 announce for the seller delivery remote (required before push).
///
/// Parameterized replaceable via `d=<repo_id>` — idempotent across launches.
pub fn announce_seller_delivery_repo(
    home: &MaxplayerHome,
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
    home: &MaxplayerHome,
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
        description: Some("maxplayer seller delivery".into()),
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
    home: &MaxplayerHome,
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
    home: &MaxplayerHome,
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
    home: &MaxplayerHome,
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
            "maxplayer-announce-allowlist-{}-{}",
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
            "maxplayer-profile-resolve-{}-{}",
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
            "maxplayer-profile-set-{}-{}",
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
                about_generated: None,
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
            "maxplayer-fetch-names-nested-{}-{}",
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

    /// Issue #209: the default `about` derives its mint label from `accepted_mints` — never a
    /// hard-coded `"testnut"`. This guard outlived the kind-31990 announce it was written beside
    /// (#645 retired that); `about` is the last config-derived mint label kind-0 still carries.
    #[test]
    fn default_about_mint_label_comes_from_config_not_testnut() {
        const REAL_MINT: &str = "https://mint.minibits.cash/Bitcoin";
        let about = default_seller_about(Some("grok-4.5"), 21, &[REAL_MINT.to_owned()]);
        assert!(
            !about.contains("testnut"),
            "about fallback must not hard-code testnut; got: {about}"
        );
        assert!(
            about.contains("mint.minibits.cash"),
            "about fallback must include the configured mint; got: {about}"
        );

        // A seat with no configured mint gets an honest placeholder, not a plausible wrong mint.
        let none = default_seller_about(None, 21, &[]);
        assert!(none.contains("no-mint"), "got: {none}");
    }

    #[test]
    fn generated_about_regenerates_when_live_config_changes() {
        let first = default_seller_about(
            Some("claude"), 2, &["https://testnut.cashu.space".to_owned()],
        );
        let (first, generated) =
            generated_about_update(None, None, first).expect("first boot generates");
        assert!(generated);
        let changed = default_seller_about(
            Some("claude"), 100, &["https://mint.cubabitcoin.org/path".to_owned()],
        );
        let (changed, generated) = generated_about_update(Some(&first), Some(true), changed)
            .expect("generated text refreshes");
        assert!(generated);
        assert!(changed.contains("100 sat/job"), "got: {changed}");
        assert!(changed.contains("mint.cubabitcoin.org"), "got: {changed}");
        assert!(!changed.contains("testnut.cashu.space"), "got: {changed}");
    }

    /// Issue #678: identical generated about must not force a config.toml rewrite every boot.
    #[test]
    fn generated_about_is_noop_when_unchanged() {
        let about = default_seller_about(
            Some("claude"), 2, &["https://testnut.cashu.space".to_owned()],
        );
        assert_eq!(
            generated_about_update(Some(&about), Some(true), about.clone()),
            None
        );
    }

    /// Trailing TOML comment `write_config` will drop. Surviving bytes prove `save_config` was not
    /// reached; disappearance proves it was. Not mtime — coarse, and a same-content rewrite still
    /// answers the wrong question.
    const CONFIG_REWRITE_CANARY: &str = "\n# issue-726-rewrite-canary\n";

    fn profile_test_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-profile-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn wiring_seller(rate_sats: u64) -> home::SellerConfig {
        home::SellerConfig {
            takes_no_payment: false,
            agent_command: vec!["claude".into()],
            rate_sats,
            git_remote: "https://example.invalid/repo.git".into(),
            job_timeout_secs: None,
            agents: vec!["claude".into()],
            claim_open_pool: false,
            accept_open_targeted: false,
            accept_offers_only_from: Vec::new(),
            offer_backfill_secs: home::default_offer_backfill_secs(),
            contribution_enabled: true,
            slots: home::default_slots(),
            claim_award_timeout_secs: None,
        }
    }

    fn inject_rewrite_canary(path: &std::path::Path) -> Vec<u8> {
        let mut bytes = std::fs::read(path).expect("read config.toml");
        bytes.extend_from_slice(CONFIG_REWRITE_CANARY.as_bytes());
        std::fs::write(path, &bytes).expect("inject rewrite canary");
        bytes
    }

    /// Issue #726: the #678 `None` must actually skip `home::save_config` on the seller
    /// discoverability **publish path**, not merely in the pure decision function.
    ///
    /// Drives `publish_seller_discoverability_async` twice against an unchanged generated about,
    /// then once more after a live rate change. Asserts on `config.toml` bytes (a trailing comment
    /// `write_config` strips) — never mtime.
    ///
    /// RED-PROVE (wiring): move the `save_config` call in `publish_seller_discoverability_async`
    /// outside the `if let Some(...)` and the unchanged pass goes red (canary stripped). Disable
    /// the generated-about update entirely and the changed pass goes red (stale about stays on disk).
    /// The existing `generated_about_is_noop_when_unchanged` test stays green in both sabotages.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generated_about_noop_is_wired_into_the_publish_path() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        let root = profile_test_root("about-rewrite-726");
        let mut home = home::bootstrap(&root).expect("home");
        let rate_sats = 2u64;
        let live_about = default_seller_about(
            Some("claude"),
            rate_sats,
            &home.config.accepted_mints,
        );
        home::save_config(&mut home, |config| {
            config.relay_url = relay_url;
            config.seller = Some(wiring_seller(rate_sats));
            config.profile = Some(ProfileConfig {
                name: Some("wired-seller".into()),
                about: Some(live_about),
                about_generated: Some(true),
            });
        })
        .expect("persist generated about + fixture relay");

        let config_path = home.root.join("config.toml");

        // Pass 1: the publish path runs against an already-stamped generated about.
        publish_seller_discoverability_async(&mut home)
            .await
            .expect("first publish");

        // Canary after pass 1 so a same-content `write_config` is still visible: pretty-printed
        // TOML of an unchanged profile would otherwise byte-match, which is the adjacent question.
        let after_first = inject_rewrite_canary(&config_path);

        // Pass 2: unchanged live config — `generated_about_update` returns `None`, and that `None`
        // must skip `save_config`. Byte identity (canary included) is the wiring assertion.
        publish_seller_discoverability_async(&mut home)
            .await
            .expect("second publish (unchanged)");
        let after_second = std::fs::read(&config_path).expect("read after unchanged publish");
        assert_eq!(
            after_second, after_first,
            "unchanged generated about must not rewrite config.toml; save_config outside the \
             if-let would strip the canary even when the typed profile is identical"
        );

        // Other direction: a genuinely changed live config must still rewrite. A test that would
        // pass with the update disabled entirely proves nothing.
        home::save_config(&mut home, |config| {
            config.seller.as_mut().expect("seller").rate_sats = 100;
        })
        .expect("change live rate");
        let after_rate_change = inject_rewrite_canary(&config_path);
        publish_seller_discoverability_async(&mut home)
            .await
            .expect("third publish (rate changed)");
        let after_changed = std::fs::read(&config_path).expect("read after changed publish");
        assert_ne!(
            after_changed, after_rate_change,
            "a changed generated about must rewrite config.toml"
        );
        let after_changed_text = String::from_utf8(after_changed).expect("config.toml utf-8");
        assert!(
            !after_changed_text.contains("issue-726-rewrite-canary"),
            "save_config must have run (canary stripped); got:\n{after_changed_text}"
        );
        assert!(
            after_changed_text.contains("100 sat/job"),
            "rewritten about must carry the new rate; got:\n{after_changed_text}"
        );
        assert!(
            after_changed_text.contains("about_generated = true"),
            "provenance must stay generated; got:\n{after_changed_text}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn operator_about_update_marks_protected_and_is_preserved() {
        let mut profile = ProfileConfig::default();
        apply_profile_updates(&mut profile, None, Some("operator prose · 2 sat/job".into()));
        assert_eq!(profile.about_generated, Some(false));
        let replacement = default_seller_about(
            Some("claude"), 100, &["https://mint.cubabitcoin.org".to_owned()],
        );
        assert_eq!(
            generated_about_update(profile.about.as_deref(), profile.about_generated, replacement),
            None
        );
        assert_eq!(profile.about.as_deref(), Some("operator prose · 2 sat/job"));
    }

    #[test]
    fn pre_upgrade_about_with_unknown_provenance_is_preserved() {
        let profile = ProfileConfig {
            name: None,
            about: Some("old text".into()),
            about_generated: None,
        };
        let replacement = default_seller_about(Some("claude"), 100, &[]);
        assert_eq!(
            generated_about_update(profile.about.as_deref(), profile.about_generated, replacement),
            None
        );
        assert_eq!(profile.about.as_deref(), Some("old text"));
        assert_eq!(profile.about_generated, None);
    }

    #[test]
    fn default_about_names_every_accepted_mint_in_order() {
        let about = default_seller_about(
            Some("claude"),
            100,
            &[
                "https://mint.one.example/Bitcoin".to_owned(),
                "http://mint.two.example/path".to_owned(),
            ],
        );
        assert!(about.ends_with("mint.one.example, mint.two.example"), "got: {about}");
    }

    #[test]
    fn mint_host_label_reduces_a_url_to_its_host() {
        assert_eq!(
            mint_host_label("https://mint.minibits.cash/Bitcoin"),
            "mint.minibits.cash"
        );
        assert_eq!(mint_host_label("http://cashu.example/path?q=1"), "cashu.example");
        assert_eq!(mint_host_label("custom-mint-label"), "custom-mint-label");
    }
}
