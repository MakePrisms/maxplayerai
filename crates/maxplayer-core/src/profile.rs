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
    /// This key's stored kind-0 could not be read: the read did not end in the relay's EOSE (timeout,
    /// refused sign-in, dropped connection), or its content is not a JSON object where a merge needs
    /// one. Nothing was published or saved. `profile set` reports it as an error; seller start and
    /// the automatic label fill skip the kind-0 update with a warning (kind-0 is display-only).
    Unread(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "profile input: {message}"),
            Self::Home(error) => write!(formatter, "{error}"),
            Self::Relay(message) | Self::Unread(message) => {
                write!(formatter, "profile relay: {message}")
            }
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
    let _lock = profile_write_lock(home, PROFILE_LOCK_WAIT).await?;
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
    let event_id = publish_metadata_merged_async(
        home,
        &keys,
        profile.name.as_deref(),
        profile.about.as_deref(),
    )
    .await?
    .event_id;

    Ok(SetProfileOutcome {
        ok: true,
        pubkey: keys.public_key().to_hex(),
        name: profile.name,
        about: profile.about,
        event_id,
        relay_url: home.config.relay_url.clone(),
    })
}

/// Seller start: publish the clobber-safe kind-0 identity. Fetch → merge name/about → publish.
/// A fetch that did not complete, or a stored kind-0 that is not a JSON object, publishes and
/// saves nothing and returns [`ProfileError::Unread`]; seller start logs it and boots on.
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

/// Async twin of [`publish_seller_discoverability`]. A nameless seat may take the
/// `maxplayer-seller-<8hex>` default (see [`publish_seller_discoverability_with`]).
pub async fn publish_seller_discoverability_async(
    home: &mut MaxplayerHome,
) -> Result<SellerDiscoverabilityOutcome, ProfileError> {
    publish_seller_discoverability_with(home, true).await
}

/// Seller start's kind-0 publish. The name is `[profile].name` when set; otherwise the name the
/// relay already holds, kept as published and saved to `[profile].name`; otherwise, only when
/// `allow_default_name`, `maxplayer-seller-<8hex>`, saved once the relay accepted it. With
/// `allow_default_name` false a nameless seat publishes no name and saves none, so its account
/// label can still name it on a later start ([`NameBackfill::allows_default_name`]).
pub async fn publish_seller_discoverability_with(
    home: &mut MaxplayerHome,
    allow_default_name: bool,
) -> Result<SellerDiscoverabilityOutcome, ProfileError> {
    let _lock = profile_write_lock(home, PROFILE_LOCK_WAIT).await?;
    home::reload_config(home)?;
    let seller = home.config.seller.as_ref().ok_or_else(|| {
        ProfileError::Input("missing [seller] config for discoverability publish".into())
    })?;
    let rate_sats = seller.rate_sats;
    // Only the default `about` sentence reads these. The seat's ADVERTISED rate and harness roster
    // are published by the kind-30340 heartbeat from live state; kind-0 `about` is display prose a
    // reader must never target, pay, or dispatch on (§4.1).
    let agent = seller.agents.first().cloned();

    let pubkey = home::public_key_hex(home)?;
    let short = &pubkey[..8.min(pubkey.len())];
    let local_name = configured_name(home);
    let fallback = (local_name.is_none() && allow_default_name)
        .then(|| format!("maxplayer-seller-{short}"));
    let keys = buyer_keys(home)?;
    // Read before anything is saved: a profile that cannot be read leaves config.toml untouched
    // (no generated about, no default name) as well as the relay.
    let (session, mut fields) = open_merge_base(home, &keys, true).await?;

    let existing_about = home.config.profile.as_ref().and_then(|p| p.about.as_deref());
    let existing_generated = home.config.profile.as_ref().and_then(|p| p.about_generated);
    let live_about = default_seller_about(agent.as_deref(), rate_sats, &home.config.accepted_mints);
    if let Some((about, about_generated)) =
        generated_about_update(existing_about, existing_generated, live_about)
    {
        let saved = home::save_config(home, |config| {
            let profile = config.profile.get_or_insert_with(ProfileConfig::default);
            profile.about = Some(about);
            profile.about_generated = Some(about_generated);
        });
        if let Err(error) = saved {
            session.client.disconnect().await;
            return Err(error.into());
        }
    }

    let about = home.config.profile.as_ref().and_then(|p| p.about.clone());
    let name = merge_name_about(
        &mut fields,
        local_name.as_deref(),
        about.as_deref(),
        fallback.as_deref(),
    );
    let event_id = send_metadata(session, home, &keys, fields).await?;
    if local_name.is_none()
        && let Some(name) = &name
    {
        save_profile_name(home, name)?;
    }

    Ok(SellerDiscoverabilityOutcome {
        pubkey: keys.public_key().to_hex(),
        kind0_event_id: event_id,
        name,
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

/// Every kind-0 read-merge-write on a home holds this lock from reading `[profile].name` until the
/// event is sent and the name saved. The buyer daemon and the seller node hold different home locks
/// (`buyer.lock`, `seller.lock`) and can both run on one home, so without it a seller start could read
/// the relay before a buyer's label fill landed and then publish over it.
const PROFILE_LOCK_FILE: &str = "profile.lock";
/// How long a kind-0 write waits for another one on the same home (each is one read and one send).
const PROFILE_LOCK_WAIT: Duration = Duration::from_secs(30);

async fn profile_write_lock(
    home: &MaxplayerHome,
    wait: Duration,
) -> Result<crate::buyer::lock::HomeLock, ProfileError> {
    use crate::buyer::lock::{HomeLock, LockError};
    let path = home.root.join(PROFILE_LOCK_FILE);
    let started = std::time::Instant::now();
    loop {
        match HomeLock::acquire(&path) {
            Ok(lock) => return Ok(lock),
            Err(LockError::Held { .. }) if started.elapsed() < wait => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(LockError::Held { .. }) => {
                return Err(ProfileError::Relay(format!(
                    "another maxplayer process on this home is still publishing its profile \
                     (waited {}s); nothing was published",
                    wait.as_secs()
                )));
            }
            Err(error) => return Err(ProfileError::Relay(format!("profile lock: {error}"))),
        }
    }
}

/// When a kind-0 read started now must have finished (connect included).
fn kind0_read_deadline() -> std::time::Instant {
    std::time::Instant::now() + Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS)
}

/// What [`publish_metadata_merged_async`] published.
struct MergedPublish {
    event_id: String,
}

/// Connect and read this key's kind-0 as the base of a merge: every field as published, or empty
/// when the relay holds none. A read that did not end in EOSE is [`ProfileError::Unread`]; so is a
/// stored kind-0 whose content is not a JSON object when `unreadable_is_error` (seller start, which
/// must not replace what it cannot read). Callers hold [`profile_write_lock`].
async fn open_merge_base(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    unreadable_is_error: bool,
) -> Result<(Kind0Session, serde_json::Map<String, serde_json::Value>), ProfileError> {
    let session = connect_metadata_client(home, keys).await?;
    let fields = match read_own_kind0(&session, keys, kind0_read_deadline()).await {
        Ok(OwnKind0::Fields(fields)) => fields,
        Ok(OwnKind0::Absent) => serde_json::Map::new(),
        Ok(OwnKind0::NotAnObject) if unreadable_is_error => {
            session.client.disconnect().await;
            return Err(not_an_object());
        }
        // An explicit `profile set`: no field can be read out of it, so there is nothing to keep
        // (this publish carries the operator's own name and about).
        Ok(OwnKind0::NotAnObject) => serde_json::Map::new(),
        Err(error) => {
            session.client.disconnect().await;
            return Err(error);
        }
    };
    Ok((session, fields))
}

fn not_an_object() -> ProfileError {
    ProfileError::Unread(
        "this key's kind-0 content is not a JSON object, so it cannot be read or merged; nothing \
         was published"
            .into(),
    )
}

/// Fail-closed read-merge-write for replaceable kind-0 (never blind-overwrite), for `profile set`.
/// Reads this key's kind-0 to the relay's EOSE and republishes every field it holds as published,
/// with the given name and about set. A read that did not end in EOSE publishes nothing.
async fn publish_metadata_merged_async(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    name: Option<&str>,
    about: Option<&str>,
) -> Result<MergedPublish, ProfileError> {
    let (session, mut fields) = open_merge_base(home, keys, false).await?;
    merge_name_about(&mut fields, name, about, None);
    let event_id = send_metadata(session, home, keys, fields).await?;
    Ok(MergedPublish { event_id })
}

/// Set `about` when given and the name to `name`, else leave it as the relay has it, else
/// `fallback_name`. Returns the kind-0 `name` the merged content carries.
fn merge_name_about(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    name: Option<&str>,
    about: Option<&str>,
    fallback_name: Option<&str>,
) -> Option<String> {
    let name = match (name, remote_name(fields)) {
        (Some(name), _) => {
            fields.insert("name".into(), name.into());
            Some(name.to_owned())
        }
        // Kept exactly as published.
        (None, Some(remote)) => Some(remote),
        (None, None) => fallback_name.map(|fallback| {
            fields.insert("name".into(), fallback.into());
            fallback.to_owned()
        }),
    };
    if let Some(about) = about {
        fields.insert("about".into(), about.into());
    }
    name
}

/// One connection for a kind-0 read and the send that follows it.
struct Kind0Session {
    client: nostr_sdk::Client,
    relay: nostr_sdk::prelude::Relay,
    sign_in: crate::relay_auth::SignInWatch,
}

async fn connect_metadata_client(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
) -> Result<Kind0Session, ProfileError> {
    let client = nostr_sdk::Client::new(keys.clone());
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| ProfileError::Relay(format!("add relay: {error}")))?;
    let relay = client
        .relay(&home.config.relay_url)
        .await
        .map_err(|error| ProfileError::Relay(format!("relay handle: {error}")))?;
    // Before connect: buzz refuses an unregistered key's sign-in as soon as the socket opens
    // (#1228), and the read or send below then only says `timeout`.
    let sign_in = crate::relay_auth::SignInWatch::new(&relay);
    client.connect().await;
    Ok(Kind0Session {
        client,
        relay,
        sign_in,
    })
}

/// This key's kind-0 as the relay holds it.
enum OwnKind0 {
    /// The relay sent EOSE without a kind-0 for this key.
    Absent,
    /// The stored kind-0's content, every field as published (types the SDK would reject included).
    Fields(serde_json::Map<String, serde_json::Value>),
    /// A stored kind-0 whose content is not a JSON object.
    NotAnObject,
}

/// Read this key's kind-0 to the relay's EOSE, by `until`. `Client::fetch_events` reports a read
/// that timed out without EOSE as `Ok(empty)`, the same as a key with no kind-0, so a slow relay
/// read that way made a real profile look absent and got it overwritten. Only an EOSE-ended read
/// proves absence; anything else is an error and the caller publishes nothing.
async fn read_own_kind0(
    session: &Kind0Session,
    keys: &nostr_sdk::Keys,
    until: std::time::Instant,
) -> Result<OwnKind0, ProfileError> {
    use crate::seller_node::stored_read::{StoredRead, read_stored};
    use nostr_sdk::prelude::{Filter, Kind};

    let incomplete = |reason: String| {
        ProfileError::Unread(crate::relay_refusal::with_hint(format!(
            "kind-0 read incomplete, so nothing was published (refusing to write over a profile \
             it could not read): {}",
            session.sign_in.annotate(&reason)
        )))
    };
    let left = || until.saturating_duration_since(std::time::Instant::now());
    session.client.wait_for_connection(left()).await;
    if !session.relay.is_connected() {
        return Err(incomplete(format!("no connection to {}", session.relay.url())));
    }
    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Metadata)
        .limit(1);
    let events = match read_stored(&session.relay, vec![filter], left()).await {
        StoredRead::Complete(events) => events,
        StoredRead::Incomplete { reason, .. } => return Err(incomplete(reason)),
    };
    // Newest first.
    let Some(event) = events.into_iter().next() else {
        return Ok(OwnKind0::Absent);
    };
    Ok(
        match serde_json::from_str::<serde_json::Value>(&event.content) {
            Ok(serde_json::Value::Object(fields)) => OwnKind0::Fields(fields),
            _ => OwnKind0::NotAnObject,
        },
    )
}

/// The kind-0 `name` in `fields`, when it is a usable string.
fn remote_name(fields: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    fields
        .get("name")
        .and_then(serde_json::Value::as_str)
        .and_then(|name| clamp_field(name, PROFILE_NAME_MAX))
}

/// Sign and send `fields` as this key's kind-0 content; disconnects either way.
async fn send_metadata(
    session: Kind0Session,
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    fields: serde_json::Map<String, serde_json::Value>,
) -> Result<String, ProfileError> {
    use nostr_sdk::prelude::{EventBuilder, Kind};

    let Kind0Session {
        client, sign_in, ..
    } = session;
    let content = serde_json::Value::Object(fields).to_string();
    let event = match EventBuilder::new(Kind::Metadata, content).sign_with_keys(keys) {
        Ok(event) => event,
        Err(error) => {
            client.disconnect().await;
            return Err(ProfileError::Relay(format!("sign kind-0: {error}")));
        }
    };
    let output = client
        .send_event_to([&home.config.relay_url], &event)
        .await;
    client.disconnect().await;
    let output = output.map_err(|error| ProfileError::Relay(format!("send kind-0: {error}")))?;
    if output.success.is_empty() {
        let failed = crate::relay_auth::failed_reasons(&output, Some(&sign_in));
        return Err(ProfileError::Relay(crate::relay_refusal::with_hint(format!(
            "no relay accepted kind-0 ({failed})"
        ))));
    }
    Ok(output.val.to_hex())
}

/// How [`fill_name_from_label_async`] ended. Only `Published` wrote a kind-0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameFill {
    /// The label was published as kind-0 `name` (other kind-0 fields kept) and saved to
    /// `[profile].name`.
    Published { name: String, event_id: String },
    /// `[profile].name` was already set; nothing was fetched or published.
    KeptLocal(String),
    /// The relay already held a kind-0 `name` for this key. Nothing was published; the name was
    /// saved to `[profile].name` so later republishes (seller start) keep it too.
    KeptRemote(String),
}

impl NameFill {
    pub fn name(&self) -> &str {
        match self {
            Self::Published { name, .. } | Self::KeptLocal(name) | Self::KeptRemote(name) => name,
        }
    }
}

fn configured_name(home: &MaxplayerHome) -> Option<String> {
    home.config
        .profile
        .as_ref()
        .and_then(|profile| profile.name.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn save_profile_name(home: &mut MaxplayerHome, name: &str) -> Result<(), ProfileError> {
    home::save_config(home, |config| {
        config
            .profile
            .get_or_insert_with(ProfileConfig::default)
            .name = Some(name.to_owned());
    })?;
    Ok(())
}

/// Publish the account-link `label` as this key's kind-0 `name`, but only when the key has no name
/// yet: `[profile].name` unset or blank AND the relay's kind-0 has no `name`. An operator-set name
/// is never replaced. The read must end in the relay's EOSE and every other kind-0 field is kept as
/// published; a read that did not finish, or a kind-0 whose content is not a JSON object, publishes
/// nothing ([`ProfileError::Unread`]; callers warn and the next start's backfill tries again). `[profile].name` is written only after the relay accepted the event, so a failure
/// leaves the key eligible for the startup backfill.
pub async fn fill_name_from_label_async(
    home: &mut MaxplayerHome,
    label: &str,
) -> Result<NameFill, ProfileError> {
    fill_name_until(home, label, None).await
}

/// [`fill_name_from_label_async`], giving up before the send once `deadline` has passed. A send that
/// started is never abandoned: the relay accepting the label is always followed by saving it.
async fn fill_name_until(
    home: &mut MaxplayerHome,
    label: &str,
    deadline: Option<std::time::Instant>,
) -> Result<NameFill, ProfileError> {
    let left = || deadline.map(|d| d.saturating_duration_since(std::time::Instant::now()));
    let lock_wait = left().map_or(PROFILE_LOCK_WAIT, |left| left.min(PROFILE_LOCK_WAIT));
    let _lock = profile_write_lock(home, lock_wait).await?;
    home::reload_config(home)?;
    if let Some(name) = configured_name(home) {
        return Ok(NameFill::KeptLocal(name));
    }
    let name = crate::registration::normalize_label(label)
        .ok_or_else(|| ProfileError::Input("the account label is not a usable name".into()))?;
    let keys = buyer_keys(home)?;
    let session = connect_metadata_client(home, &keys).await?;
    let read_until = deadline.map_or_else(kind0_read_deadline, |d| d.min(kind0_read_deadline()));
    let mut fields = match read_own_kind0(&session, &keys, read_until).await {
        Ok(OwnKind0::Absent) => serde_json::Map::new(),
        Ok(OwnKind0::Fields(fields)) => fields,
        Ok(OwnKind0::NotAnObject) => {
            session.client.disconnect().await;
            return Err(not_an_object());
        }
        Err(error) => {
            session.client.disconnect().await;
            return Err(error);
        }
    };
    if let Some(remote) = remote_name(&fields) {
        session.client.disconnect().await;
        save_profile_name(home, &remote)?;
        return Ok(NameFill::KeptRemote(remote));
    }
    if left().is_some_and(|left| left.is_zero()) {
        session.client.disconnect().await;
        return Err(ProfileError::Relay(
            "the start-up time for the public name ran out before publishing; nothing was \
             published"
                .into(),
        ));
    }
    fields.insert("name".into(), name.clone().into());
    let event_id = send_metadata(session, home, &keys, fields).await?;
    save_profile_name(home, &name)?;
    Ok(NameFill::Published { name, event_id })
}

/// Sync twin of [`fill_name_from_label_async`] for the CLI (`account link`).
pub fn fill_name_from_label(
    home: &mut MaxplayerHome,
    label: &str,
) -> Result<NameFill, ProfileError> {
    crate::runtime_guard::refuse_nested_block_on("fill_name_from_label")
        .map_err(ProfileError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ProfileError::Relay(error.to_string()))?;
    runtime.block_on(fill_name_from_label_async(home, label))
}

/// Whether an account label from `registration_url` may name this key on `relay_url`: only when
/// both are on the same host. A registration service vouches for membership on the relay it fronts
/// (`https://relay.maxplayer.ai/account` ↔ `wss://relay.maxplayer.ai`); a link made with a
/// different service says nothing about this relay, and a home pointed at a test or self-hosted
/// relay must not consult the public service at all.
pub fn registration_pairs_with_relay(registration_url: &str, relay_url: &str) -> bool {
    use nostr_sdk::prelude::Url;
    let host = |raw: &str| {
        Url::parse(raw.trim())
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
    };
    matches!((host(registration_url), host(relay_url)), (Some(a), Some(b)) if a == b)
}

/// Upper bound on the startup backfill's wait: the status call, then the kind-0 read. A send that
/// started runs to its end (see [`fill_name_until`]).
pub const NAME_BACKFILL_BUDGET: Duration = Duration::from_secs(15);
/// A key the registration service reported as not linked is asked again after this long (sooner
/// after `account link` on this home, which forgets the answer).
pub const NOT_LINKED_RECHECK: Duration = Duration::from_secs(7 * 24 * 3600);
/// After a failed status call the next start waits this long, doubling per failure up to
/// [`FAILURE_BACKOFF_MAX`].
const FAILURE_BACKOFF_FIRST: Duration = Duration::from_secs(3600);
const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(24 * 3600);
const BACKFILL_STATE_FILE: &str = "name-backfill.json";

/// How a startup backfill ([`backfill_name_from_link_async`]) ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameBackfill {
    /// `[profile].name` is set, or the registration service does not front this home's relay.
    NotNeeded,
    /// Linked with a label and a relay member: the fill ran.
    Filled(NameFill),
    /// The registration service says this key is not linked (now, or at a check remembered for
    /// [`NOT_LINKED_RECHECK`]).
    NotLinked,
    /// Linked, but the relay has not added the key yet: asked again on every start.
    Pending,
    /// Not asked this start: an earlier status call failed and its retry is not due yet.
    Deferred,
    /// This start's attempt failed (status call, time budget, or the kind-0 fill).
    Failed(String),
}

impl NameBackfill {
    /// Whether seller start may give the key the `maxplayer-seller-<8hex>` default now: only when no
    /// account label can name it. A pending membership, a failure or a deferral may still end in
    /// the label, and a saved default would keep it from ever applying.
    pub fn allows_default_name(&self) -> bool {
        matches!(self, Self::NotNeeded | Self::Filled(_) | Self::NotLinked)
    }

    /// Why no default name was chosen, for the seller's start line.
    pub fn reason(&self) -> &str {
        match self {
            Self::NotNeeded | Self::Filled(_) => "named",
            Self::NotLinked => "not linked",
            Self::Pending => "relay membership is still pending",
            Self::Deferred => "the registration service failed earlier; retrying later",
            Self::Failed(reason) => reason,
        }
    }
}

/// What the last status call said, so an unlinked key or a down service is not asked on every
/// start. Keyed by pubkey: a home whose key changed starts over.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct BackfillState {
    pubkey: String,
    #[serde(default)]
    not_linked_until: Option<u64>,
    #[serde(default)]
    failures: u32,
    #[serde(default)]
    retry_after: Option<u64>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn load_backfill_state(home: &MaxplayerHome, pubkey: &str) -> BackfillState {
    std::fs::read(home.root.join(BACKFILL_STATE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BackfillState>(&bytes).ok())
        .filter(|state| state.pubkey == pubkey)
        .unwrap_or_else(|| BackfillState {
            pubkey: pubkey.to_owned(),
            ..BackfillState::default()
        })
}

fn save_backfill_state(home: &MaxplayerHome, state: &BackfillState) {
    let path = home.root.join(BACKFILL_STATE_FILE);
    let tmp = home.root.join(format!("{BACKFILL_STATE_FILE}.tmp"));
    let written = serde_json::to_vec(state)
        .map_err(|error| error.to_string())
        .and_then(|bytes| std::fs::write(&tmp, bytes).map_err(|error| error.to_string()))
        .and_then(|()| std::fs::rename(&tmp, &path).map_err(|error| error.to_string()));
    if let Err(error) = written {
        crate::opline!("warn: public name backfill state not saved: {error}");
    }
}

/// Forget a remembered "not linked" answer and any failure backoff, so the next start asks the
/// registration service again. `account link` calls this after linking.
pub fn forget_name_backfill(home: &MaxplayerHome) {
    let _ = std::fs::remove_file(home.root.join(BACKFILL_STATE_FILE));
}

fn failure_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FAILURE_BACKOFF_FIRST
        .saturating_mul(1 << doublings)
        .min(FAILURE_BACKOFF_MAX)
}

/// Startup backfill for keys linked before `account link` published the label: when
/// `[profile].name` is unset, ask the registration service for this key's link, and if it is linked
/// with a label and is a relay member, fill the name from it. Never an error: a failure is one
/// `warn:` line and [`NameBackfill::Failed`]. A definitive "not linked" is remembered for
/// [`NOT_LINKED_RECHECK`] and a failed status call backs off; a pending membership is asked again
/// on every start.
pub async fn backfill_name_from_link_async(home: &mut MaxplayerHome) -> NameBackfill {
    backfill_name_within(home, NAME_BACKFILL_BUDGET).await
}

async fn backfill_name_within(home: &mut MaxplayerHome, budget: Duration) -> NameBackfill {
    if let Err(error) = home::reload_config(home) {
        crate::opline!("warn: public name backfill skipped: {error}");
        return NameBackfill::Failed(error.to_string());
    }
    if configured_name(home).is_some()
        || !registration_pairs_with_relay(&home.config.registration_url, &home.config.relay_url)
    {
        return NameBackfill::NotNeeded;
    }
    let keys = match buyer_keys(home) {
        Ok(keys) => keys,
        Err(error) => {
            crate::opline!("warn: public name backfill skipped: {error}");
            return NameBackfill::Failed(error.to_string());
        }
    };
    let pubkey = keys.public_key().to_hex();
    let mut state = load_backfill_state(home, &pubkey);
    let now = unix_now();
    if state.not_linked_until.is_some_and(|until| now < until) {
        return NameBackfill::NotLinked;
    }
    if state.retry_after.is_some_and(|at| now < at) {
        return NameBackfill::Deferred;
    }
    let deadline = std::time::Instant::now() + budget;
    let status = match registration_status(home, keys, budget).await {
        Ok(status) => status,
        Err(message) => {
            state.failures = state.failures.saturating_add(1);
            let backoff = failure_backoff(state.failures);
            state.retry_after = Some(now.saturating_add(backoff.as_secs()));
            state.not_linked_until = None;
            save_backfill_state(home, &state);
            crate::opline!(
                "warn: public name backfill skipped: {message} (next try in {})",
                crate::registration::wait_text(backoff.as_secs())
            );
            return NameBackfill::Failed(message);
        }
    };
    match (
        status.registered,
        status.relay_member.as_deref(),
        status.label,
    ) {
        (false, _, _) => {
            save_backfill_state(
                home,
                &BackfillState {
                    pubkey,
                    not_linked_until: Some(now.saturating_add(NOT_LINKED_RECHECK.as_secs())),
                    ..BackfillState::default()
                },
            );
            NameBackfill::NotLinked
        }
        (true, Some("added"), Some(label)) => {
            forget_name_backfill(home);
            match fill_name_until(home, &label, Some(deadline)).await {
                Ok(fill) => NameBackfill::Filled(fill),
                Err(error) => {
                    crate::opline!(
                        "warn: public name backfill skipped, nothing published (retried next \
                         start): {error}"
                    );
                    NameBackfill::Failed(error.to_string())
                }
            }
        }
        _ => {
            forget_name_backfill(home);
            NameBackfill::Pending
        }
    }
}

/// `GET /status` for this key, bounded by `budget`. The registration client is blocking reqwest, so
/// it runs off the async workers with its own request timeout set to `budget`: the thread ends
/// within the budget instead of running on for the client's default 30s after boot moved on.
async fn registration_status(
    home: &MaxplayerHome,
    keys: nostr_sdk::Keys,
    budget: Duration,
) -> Result<crate::registration::Status, String> {
    let base = home.config.registration_url.clone();
    let task = tokio::task::spawn_blocking(move || {
        crate::registration::RegistrationClient::with_timeout(&base, keys, budget)?.status()
    });
    // A backstop only: the request timeout above ends the task first.
    match tokio::time::timeout(budget + Duration::from_secs(1), task).await {
        Err(_) => Err(format!(
            "no answer within {:.1}s",
            budget.as_secs_f32()
        )),
        Ok(Err(error)) => Err(format!("status task: {error}")),
        Ok(Ok(result)) => result.map_err(|error| error.to_string()),
    }
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

pub(crate) async fn send_signed_event(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    event: &nostr_sdk::Event,
    label: &str,
) -> Result<String, ProfileError> {
    let client = crate::relay_auth::connect_authenticated(keys, &home.config.relay_url)
        .await
        .map_err(|error| ProfileError::Relay(error.to_string()))?;
    // A relay that challenges only on the write refuses the sign-in here, not at connect.
    let sign_in = crate::relay_auth::SignInWatch::for_client(&client, &home.config.relay_url).await;
    let output = client
        .send_event_to([&home.config.relay_url], event)
        .await;
    client.disconnect().await;
    let output =
        output.map_err(|error| ProfileError::Relay(format!("send {label}: {error}")))?;
    if output.success.is_empty() {
        let failed = crate::relay_auth::failed_reasons(&output, sign_in.as_ref());
        return Err(ProfileError::Relay(crate::relay_refusal::with_hint(format!(
            "no relay accepted {label} ({failed})"
        ))));
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

    pub(super) fn wiring_seller(rate_sats: u64) -> home::SellerConfig {
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

/// A stand-in for the registration service's `GET /status` (loopback, no auth check): answers every
/// request with the configured link and counts the calls. Shared with the seller-start tests.
#[cfg(test)]
pub(crate) mod fake_status {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) struct FakeStatus {
        base: String,
        calls: Arc<AtomicUsize>,
    }

    impl FakeStatus {
        /// `label`/`relay_member` as `GET /status` reports them for a linked key.
        pub(crate) fn linked(label: &str, relay_member: &str) -> Self {
            Self::answering(serde_json::json!({
                "registered": true,
                "email": "you@example.com",
                "label": label,
                "relay_member": relay_member,
            }))
        }

        /// `GET /status` for a key that is not linked to any email.
        pub(crate) fn unlinked() -> Self {
            Self::answering(serde_json::json!({ "registered": false }))
        }

        fn answering(body: serde_json::Value) -> Self {
            let body = body.to_string();
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake status");
            let base = format!(
                "http://127.0.0.1:{}/account",
                listener.local_addr().unwrap().port()
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0usize;
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                        line.clear();
                    }
                    let mut rest = vec![0u8; length];
                    let _ = reader.read_exact(&mut rest);
                    counter.fetch_add(1, Ordering::SeqCst);
                    let mut stream = stream;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            Self { base, calls }
        }

        pub(crate) fn base_url(&self) -> String {
            self.base.clone()
        }

        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
}

#[cfg(test)]
mod name_fill_tests {
    use super::fake_status::FakeStatus;
    use super::*;
    use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
    use nostr_sdk::prelude::{Client, EventBuilder, Filter, Kind, Metadata};

    struct Fixture {
        _relay: LocalRelay,
        relay_url: String,
        home: MaxplayerHome,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home.root);
        }
    }

    /// nostr-relay-builder 0.44.1 probes a free port, releases it, then binds it again, so a
    /// parallel test can take it in between: retry only that AddrInUse, with a fresh relay.
    async fn start_relay() -> LocalRelay {
        for attempt in 1..=8 {
            let relay = LocalRelay::new(RelayBuilder::default());
            match relay.run().await {
                Ok(()) => return relay,
                Err(nostr_relay_builder::Error::IO(error))
                    if error.kind() == std::io::ErrorKind::AddrInUse && attempt < 8 => {}
                Err(error) => panic!("relay run: {error}"),
            }
        }
        unreachable!("the last attempt returns or panics")
    }

    /// A fresh home on a local relay, with `registration_url` (or an unused loopback port).
    async fn fixture(label: &str, registration_url: Option<String>) -> Fixture {
        let relay = start_relay().await;
        let relay_url = relay.url().await.to_string();
        let root = std::env::temp_dir().join(format!(
            "maxplayer-name-fill-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        let registration_url = registration_url.unwrap_or_else(|| {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            format!("http://127.0.0.1:{port}/account")
        });
        let url = relay_url.clone();
        home::save_config(&mut home, |config| {
            config.relay_url = url;
            config.registration_url = registration_url;
        })
        .expect("persist fixture config");
        Fixture {
            _relay: relay,
            relay_url,
            home,
        }
    }

    fn keys(home: &MaxplayerHome) -> nostr_sdk::Keys {
        buyer_keys(home).expect("keys")
    }

    /// Every kind-0 this key has on the relay (a replaceable event: the relay keeps the newest).
    async fn kind0s(fx: &Fixture) -> Vec<Metadata> {
        use nostr_sdk::JsonUtil;
        let client = Client::default();
        client.add_relay(&fx.relay_url).await.unwrap();
        client.connect().await;
        let filter = Filter::new()
            .author(keys(&fx.home).public_key())
            .kind(Kind::Metadata);
        let events = client
            .fetch_events(filter, Duration::from_secs(5))
            .await
            .expect("read kind-0");
        client.disconnect().await;
        events
            .into_iter()
            .map(|e| Metadata::from_json(&e.content).expect("kind-0 json"))
            .collect()
    }

    async fn seed_kind0(fx: &Fixture, metadata: &Metadata) {
        let keys = keys(&fx.home);
        let client = Client::new(keys.clone());
        client.add_relay(&fx.relay_url).await.unwrap();
        client.connect().await;
        let event = EventBuilder::metadata(metadata)
            .sign_with_keys(&keys)
            .unwrap();
        client.send_event(&event).await.expect("seed kind-0");
        client.disconnect().await;
    }

    fn saved_name(home: &mut MaxplayerHome) -> Option<String> {
        home::reload_config(home).unwrap();
        home.config.profile.as_ref().and_then(|p| p.name.clone())
    }

    /// No local and no remote name: the label is published and saved, and the fail-closed merge
    /// keeps every other kind-0 field.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn label_fill_publishes_when_nameless_and_keeps_other_fields() {
        let mut fx = fixture("publish", None).await;
        seed_kind0(
            &fx,
            &Metadata::new()
                .about("operator about")
                .website(nostr_sdk::Url::parse("https://example.com").unwrap()),
        )
        .await;
        let outcome = fill_name_from_label_async(&mut fx.home, "  my laptop ")
            .await
            .expect("fill");
        assert!(
            matches!(&outcome, NameFill::Published { name, .. } if name == "my laptop"),
            "{outcome:?}"
        );
        let remote = kind0s(&fx).await;
        assert_eq!(remote.len(), 1, "{remote:?}");
        assert_eq!(remote[0].name.as_deref(), Some("my laptop"));
        assert_eq!(remote[0].about.as_deref(), Some("operator about"));
        assert_eq!(remote[0].website.as_deref(), Some("https://example.com/"));
        assert_eq!(saved_name(&mut fx.home).as_deref(), Some("my laptop"));
    }

    /// `[profile].name` set: nothing is read or published, and the name stays.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn label_fill_never_replaces_a_local_name() {
        let mut fx = fixture("local", None).await;
        home::save_config(&mut fx.home, |config| {
            config
                .profile
                .get_or_insert_with(ProfileConfig::default)
                .name = Some("ops-box".into());
        })
        .unwrap();
        let outcome = fill_name_from_label_async(&mut fx.home, "my laptop")
            .await
            .expect("fill");
        assert_eq!(outcome, NameFill::KeptLocal("ops-box".into()));
        assert!(kind0s(&fx).await.is_empty(), "no kind-0 may be published");
        assert_eq!(saved_name(&mut fx.home).as_deref(), Some("ops-box"));
    }

    /// The relay already names this key: no publish, and the remote name is saved locally so a
    /// later republish (seller start) keeps it rather than writing a default over it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn label_fill_never_replaces_a_remote_name() {
        let mut fx = fixture("remote", None).await;
        seed_kind0(&fx, &Metadata::new().name("set-elsewhere").about("a")).await;
        let outcome = fill_name_from_label_async(&mut fx.home, "my laptop")
            .await
            .expect("fill");
        assert_eq!(outcome, NameFill::KeptRemote("set-elsewhere".into()));
        let remote = kind0s(&fx).await;
        assert_eq!(remote.len(), 1);
        assert_eq!(remote[0].name.as_deref(), Some("set-elsewhere"));
        assert_eq!(saved_name(&mut fx.home).as_deref(), Some("set-elsewhere"));
    }

    /// A relay that cannot be read publishes nothing and saves nothing (fail closed), so the
    /// backfill can try again later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn label_fill_fails_closed_on_an_unreachable_relay() {
        let mut fx = fixture("dead-relay", None).await;
        home::save_config(&mut fx.home, |config| {
            config.relay_url = "ws://127.0.0.1:1".into();
        })
        .unwrap();
        assert!(
            fill_name_from_label_async(&mut fx.home, "my laptop")
                .await
                .is_err()
        );
        assert_eq!(saved_name(&mut fx.home), None);
    }

    /// Linked + member + no name: the first start asks `/status` once and fills; the saved name
    /// means the next start makes no status call at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_fills_once_then_skips_the_status_call() {
        let status = FakeStatus::linked("old-buyer", "added");
        let mut fx = fixture("backfill", Some(status.base_url())).await;
        let first = backfill_name_from_link_async(&mut fx.home).await;
        assert!(
            matches!(&first, NameBackfill::Filled(NameFill::Published { name, .. }) if name == "old-buyer"),
            "{first:?}"
        );
        assert_eq!(status.calls(), 1);
        assert_eq!(kind0s(&fx).await[0].name.as_deref(), Some("old-buyer"));
        assert_eq!(saved_name(&mut fx.home).as_deref(), Some("old-buyer"));

        assert_eq!(
            backfill_name_from_link_async(&mut fx.home).await,
            NameBackfill::NotNeeded
        );
        assert_eq!(status.calls(), 1, "a named key never calls /status again");
    }

    /// Membership still queued: no publish (the relay would refuse it), nothing saved, no default
    /// allowed, and the next start asks again (no backoff for `pending`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_waits_for_relay_membership() {
        let status = FakeStatus::linked("old-buyer", "pending");
        let mut fx = fixture("pending", Some(status.base_url())).await;
        let outcome = backfill_name_from_link_async(&mut fx.home).await;
        assert_eq!(outcome, NameBackfill::Pending);
        assert!(!outcome.allows_default_name());
        assert_eq!(status.calls(), 1);
        assert!(kind0s(&fx).await.is_empty());
        assert_eq!(saved_name(&mut fx.home), None);
        assert_eq!(
            backfill_name_from_link_async(&mut fx.home).await,
            NameBackfill::Pending
        );
        assert_eq!(status.calls(), 2, "pending is asked again on every start");
    }

    /// A definitive "not linked" is remembered: the next start makes no status call, until
    /// `account link` forgets the answer. Seller start may then use its default name.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_remembers_a_key_that_is_not_linked() {
        let status = FakeStatus::unlinked();
        let mut fx = fixture("unlinked", Some(status.base_url())).await;
        let outcome = backfill_name_from_link_async(&mut fx.home).await;
        assert_eq!(outcome, NameBackfill::NotLinked);
        assert!(outcome.allows_default_name());
        assert_eq!(status.calls(), 1);
        assert_eq!(
            backfill_name_from_link_async(&mut fx.home).await,
            NameBackfill::NotLinked
        );
        assert_eq!(status.calls(), 1, "a remembered \"not linked\" is not asked again");
        forget_name_backfill(&fx.home);
        assert_eq!(
            backfill_name_from_link_async(&mut fx.home).await,
            NameBackfill::NotLinked
        );
        assert_eq!(status.calls(), 2, "after `account link` the service is asked again");
        assert!(kind0s(&fx).await.is_empty());
    }

    /// A failed status call backs off: the next start is deferred without asking, and only
    /// `account link` (or the backoff running out) asks again. No default name while deferred.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_backs_off_after_a_failed_status_call() {
        let mut fx = fixture("backoff", None).await;
        let first = backfill_name_from_link_async(&mut fx.home).await;
        assert!(matches!(first, NameBackfill::Failed(_)), "{first:?}");
        assert!(!first.allows_default_name());
        let second = backfill_name_from_link_async(&mut fx.home).await;
        assert_eq!(second, NameBackfill::Deferred);
        assert!(!second.allows_default_name());
        forget_name_backfill(&fx.home);
        let third = backfill_name_from_link_async(&mut fx.home).await;
        assert!(matches!(third, NameBackfill::Failed(_)), "{third:?}");
        assert_eq!(failure_backoff(1), FAILURE_BACKOFF_FIRST);
        assert_eq!(failure_backoff(2), FAILURE_BACKOFF_FIRST * 2);
        assert_eq!(failure_backoff(40), FAILURE_BACKOFF_MAX);
    }

    /// An unreachable service fails quietly well inside the budget, and changes nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_status_failure_does_not_block() {
        let mut fx = fixture("status-down", None).await;
        let started = std::time::Instant::now();
        let outcome = backfill_name_from_link_async(&mut fx.home).await;
        assert!(matches!(outcome, NameBackfill::Failed(_)), "{outcome:?}");
        assert!(
            started.elapsed() < NAME_BACKFILL_BUDGET,
            "{:?}",
            started.elapsed()
        );
        assert!(kind0s(&fx).await.is_empty());
        assert_eq!(saved_name(&mut fx.home), None);
    }

    /// A service that never answers is cut off at the budget rather than holding startup, and the
    /// blocking request itself ends there (its timeout is the budget), not 30s later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_gives_up_at_its_budget() {
        // Accepts the connection, then never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/account",
            listener.local_addr().unwrap().port()
        );
        let mut fx = fixture("status-hang", Some(url)).await;
        let started = std::time::Instant::now();
        let outcome = backfill_name_within(&mut fx.home, Duration::from_millis(300)).await;
        assert!(matches!(outcome, NameBackfill::Failed(_)), "{outcome:?}");
        assert!(!outcome.allows_default_name());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        drop(listener);
    }

    /// A home whose relay is not the one the registration service fronts never asks it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_skips_a_registration_service_for_another_relay() {
        let status = FakeStatus::linked("old-buyer", "added");
        let mut fx = fixture(
            "unpaired",
            Some(status.base_url().replace("127.0.0.1", "localhost")),
        )
        .await;
        assert_eq!(
            backfill_name_from_link_async(&mut fx.home).await,
            NameBackfill::NotNeeded
        );
        assert_eq!(status.calls(), 0);
    }

    /// Every kind-0 content this key has on the relay, raw.
    async fn kind0_contents(fx: &Fixture) -> Vec<String> {
        let client = Client::default();
        client.add_relay(&fx.relay_url).await.unwrap();
        client.connect().await;
        let filter = Filter::new()
            .author(keys(&fx.home).public_key())
            .kind(Kind::Metadata);
        let events = client
            .fetch_events(filter, Duration::from_secs(5))
            .await
            .expect("read kind-0");
        client.disconnect().await;
        events.into_iter().map(|e| e.content).collect()
    }

    async fn seed_raw_kind0(fx: &Fixture, content: &str) {
        let keys = keys(&fx.home);
        let client = Client::new(keys.clone());
        client.add_relay(&fx.relay_url).await.unwrap();
        client.connect().await;
        let event = EventBuilder::new(Kind::Metadata, content)
            .sign_with_keys(&keys)
            .unwrap();
        client.send_event(&event).await.expect("seed kind-0");
        client.disconnect().await;
    }

    /// A kind-0 the SDK cannot parse as metadata (a number where it wants a string, a bad URL)
    /// keeps every field exactly as published; it is not rebuilt from name and about.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn label_fill_keeps_fields_the_sdk_cannot_parse() {
        let mut fx = fixture("odd-fields", None).await;
        seed_raw_kind0(
            &fx,
            r#"{"about":"kept","website":"not a url","lud16":5,"custom":{"x":1}}"#,
        )
        .await;
        let outcome = fill_name_from_label_async(&mut fx.home, "my laptop")
            .await
            .expect("fill");
        assert!(matches!(outcome, NameFill::Published { .. }), "{outcome:?}");
        let contents = kind0_contents(&fx).await;
        assert_eq!(contents.len(), 1, "{contents:?}");
        let published: serde_json::Value = serde_json::from_str(&contents[0]).unwrap();
        assert_eq!(
            published,
            serde_json::json!({
                "about": "kept",
                "website": "not a url",
                "lud16": 5,
                "custom": {"x": 1},
                "name": "my laptop",
            })
        );
    }

    /// A kind-0 whose content is not a JSON object cannot say whether the key is named: publish
    /// nothing, save nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn label_fill_publishes_nothing_over_unreadable_content() {
        let mut fx = fixture("not-json", None).await;
        seed_raw_kind0(&fx, "not json at all").await;
        let error = fill_name_from_label_async(&mut fx.home, "my laptop")
            .await
            .expect_err("unreadable content must not be overwritten");
        assert!(matches!(error, ProfileError::Unread(_)), "{error:?}");
        assert!(error.to_string().contains("not a JSON object"), "{error}");
        assert_eq!(kind0_contents(&fx).await, vec!["not json at all".to_owned()]);
        assert_eq!(saved_name(&mut fx.home), None);

        // Seller start skips it the same way: nothing published, nothing saved.
        let seller = super::tests::wiring_seller(1);
        home::save_config(&mut fx.home, |config| config.seller = Some(seller)).unwrap();
        let error = publish_seller_discoverability_with(&mut fx.home, true)
            .await
            .expect_err("seller start must not replace unreadable content");
        assert!(matches!(error, ProfileError::Unread(_)), "{error:?}");
        assert_eq!(kind0_contents(&fx).await, vec!["not json at all".to_owned()]);
        home::reload_config(&mut fx.home).unwrap();
        assert!(fx.home.config.profile.is_none(), "{:?}", fx.home.config.profile);
    }

    /// A home on the EOSE-withholding fixture relay, with a seller section.
    fn pgate_home(label: &str, relay_url: &str) -> MaxplayerHome {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-name-fill-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        let url = relay_url.to_owned();
        home::save_config(&mut home, |config| {
            config.relay_url = url;
            config.seller = Some(super::tests::wiring_seller(1));
        })
        .expect("persist config");
        home
    }

    fn published_kind0_names(events: &[crate::seller_node::p_gate_relay_fixture::PublishedEvent]) -> Vec<Option<String>> {
        events
            .iter()
            .filter(|event| event.kind == 0)
            .map(|event| {
                serde_json::from_str::<serde_json::Value>(&event.content)
                    .ok()
                    .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned))
            })
            .collect()
    }

    /// The finding: `fetch_events` answers a read that never got EOSE with `Ok(empty)`, which read
    /// as "no kind-0" and let the label (or seller start's merge) overwrite the real profile. Now a
    /// read without EOSE publishes nothing on every kind-0 write path, and saves no name.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_kind0_read_without_eose_publishes_nothing() {
        use crate::seller_node::p_gate_relay_fixture::PGateRelay;
        let relay = PGateRelay::start(Duration::from_millis(0)).await;
        relay.withhold_eose_for(&[0]);
        let mut home = pgate_home("no-eose", &relay.url());

        let started = std::time::Instant::now();
        let error = fill_name_from_label_async(&mut home, "my laptop")
            .await
            .expect_err("a read without EOSE must not publish");
        assert!(error.to_string().contains("kind-0 read incomplete"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS + 4),
            "{:?}",
            started.elapsed()
        );
        assert!(matches!(error, ProfileError::Unread(_)), "{error:?}");
        let error = publish_seller_discoverability_with(&mut home, true)
            .await
            .expect_err("seller start must not publish over an unread profile");
        assert!(matches!(error, ProfileError::Unread(_)), "{error:?}");
        assert!(error.to_string().contains("kind-0 read incomplete"), "{error}");
        home::reload_config(&mut home).unwrap();
        assert!(
            home.config.profile.is_none(),
            "seller start saved {:?} without reading the profile",
            home.config.profile
        );
        let error = set_profile_async(
            &mut home,
            SetProfileRequest {
                name: None,
                about: Some("operator".into()),
            },
        )
        .await
        .expect_err("profile set stays an error over an unread profile");
        assert!(matches!(error, ProfileError::Unread(_)), "{error:?}");
        assert!(error.to_string().contains("kind-0 read incomplete"), "{error}");
        assert_eq!(relay.event_kind_count(0).await, 0, "no kind-0 reached the relay");
        assert_eq!(saved_name(&mut home), None);

        // Positive control: the same relay answering with EOSE gets the label.
        relay.withhold_eose_for(&[]);
        let outcome = fill_name_from_label_async(&mut home, "my laptop")
            .await
            .expect("fill once the read completes");
        assert!(matches!(outcome, NameFill::Published { .. }), "{outcome:?}");
        assert_eq!(
            published_kind0_names(&relay.events().await),
            vec![Some("my laptop".to_owned())]
        );
        let _ = std::fs::remove_dir_all(&home.root);
    }

    /// Buyer daemon and seller node hold different home locks, so a buyer label fill and a seller
    /// start can run at once. Seller start waits for the profile lock: it neither reads nor
    /// publishes while a fill holds it, and then keeps the name the fill saved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seller_start_waits_for_a_label_fill_on_the_same_home() {
        let mut fx = fixture("overlap", None).await;
        let seller = super::tests::wiring_seller(1);
        home::save_config(&mut fx.home, |config| config.seller = Some(seller)).unwrap();

        // Stands in for a buyer fill between its kind-0 read and its save.
        let held = profile_write_lock(&fx.home, Duration::from_secs(1))
            .await
            .expect("lock");
        let mut seller_home = fx.home.clone();
        let seller_start = tokio::spawn(async move {
            publish_seller_discoverability_with(&mut seller_home, true).await
        });
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            kind0s(&fx).await.is_empty(),
            "seller start published while another kind-0 write held the home"
        );
        assert!(!seller_start.is_finished());
        // The fill lands (published, then saved) and releases the lock.
        seed_kind0(&fx, &Metadata::new().name("my laptop")).await;
        save_profile_name(&mut fx.home, "my laptop").unwrap();
        drop(held);

        let outcome = seller_start
            .await
            .unwrap()
            .expect("seller publish after the fill");
        assert_eq!(outcome.name.as_deref(), Some("my laptop"));
        let remote = kind0s(&fx).await;
        assert_eq!(remote.len(), 1);
        assert_eq!(remote[0].name.as_deref(), Some("my laptop"));
        assert_eq!(saved_name(&mut fx.home).as_deref(), Some("my laptop"));
    }

    /// Run for real at the same time, whichever goes first, the relay and `[profile].name` agree:
    /// either the label everywhere, or the default everywhere with the fill keeping it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_concurrent_label_fill_and_seller_start_agree_on_one_name() {
        for round in 0..3 {
            let fx = fixture(&format!("race-{round}"), None).await;
            let mut buyer_home = fx.home.clone();
            let seller = super::tests::wiring_seller(1);
            home::save_config(&mut buyer_home, |config| config.seller = Some(seller)).unwrap();
            let mut seller_home = buyer_home.clone();
            let (fill, seller_start) = tokio::join!(
                fill_name_from_label_async(&mut buyer_home, "my laptop"),
                publish_seller_discoverability_with(&mut seller_home, true),
            );
            let fill = fill.expect("fill");
            let seller_start = seller_start.expect("seller start");
            let remote = kind0s(&fx).await;
            assert_eq!(remote.len(), 1);
            let mut reader = fx.home.clone();
            let saved = saved_name(&mut reader);
            assert_eq!(remote[0].name, saved, "round {round}: {fill:?} / {seller_start:?}");
            assert_eq!(saved.as_deref(), Some(fill.name()), "round {round}");
        }
    }

    #[test]
    fn registration_pairs_only_with_its_own_relay_host() {
        assert!(registration_pairs_with_relay(
            home::DEFAULT_REGISTRATION_URL,
            home::DEFAULT_RELAY_URL
        ));
        assert!(registration_pairs_with_relay(
            "http://127.0.0.1:9/account",
            "ws://127.0.0.1:7777"
        ));
        assert!(!registration_pairs_with_relay(
            home::DEFAULT_REGISTRATION_URL,
            "ws://127.0.0.1:7777"
        ));
        assert!(!registration_pairs_with_relay(
            "not a url",
            home::DEFAULT_RELAY_URL
        ));
    }
}
