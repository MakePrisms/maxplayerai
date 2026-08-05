//! The seller node's **buzz persona**: the node enrolls as an inhabitant of a buzz relay under
//! its EXISTING protocol identity, publishes a NIP-01 kind-0 persona carrying a human-readable
//! rate card, and maintains a live presence heartbeat while it is up.
//!
//! One identity, one signer. The persona is signed by the same seller key the rest of the
//! protocol uses, and that key never leaves the [`signer`](super::signer) actor — this module
//! reaches it only through a [`SignerHandle`], including for the NIP-42 auth the relay may ask of
//! the presence connection (via [`NodeNostrSigner`], a nostr-sdk signer that delegates every sign
//! to the actor). No new keys, no key material in this module.
//!
//! Boundaries (charter slice 1):
//!  * **Config off ⇒ inert.** With no `[buzz]` section the node opens no connection and publishes
//!    nothing (see [`super::SellerNode::start_buzz`]).
//!  * **Clobber guard.** kind-0 is one-per-key replaceable, so a publish CLOBBERS any existing
//!    kind-0 on the key. Before the first publish the node fetches the key's current kind-0; if one
//!    exists that this node did not write (no maxplayer marker) it REFUSES rather than overwrite a
//!    foreign persona ([`clobber_decision`]).
//!  * **Presence.** Deployed-relay presence is a live WS connection + a Redis TTL (~90s), refreshed
//!    by a periodic ephemeral kind-20001 `"online"` status (30s cadence) — NOT stored events. A
//!    clean shutdown disconnects the socket (presence clears immediately); a crash lets the relay
//!    expire it within the TTL.
//!
//! This is discovery/identity context only — nothing here feeds the pay gate, the journal, or the
//! receipt bind.

use std::time::Duration;

use nostr_sdk::prelude::{
    BoxedFuture, Client, Event, EventBuilder, Filter, Kind, Metadata, NostrSigner, PublicKey,
    SignerBackend, SignerError, Tag, UnsignedEvent,
};

use crate::home::BuzzConfig;

use super::signer::SignerHandle;

/// Ephemeral kind the deployed buzz relay consumes as a presence signal. In NIP-01's ephemeral
/// range (`20000..=29999`) so the relay never stores it — presence lives in the relay's WS +
/// Redis-TTL layer, not as a persisted event.
pub const PRESENCE_KIND: u16 = 20001;

/// The deployed relay reads the kind-20001 **content** as a bare status string. `"online"`
/// registers/refreshes presence for the authed connection's own pubkey (the relay IGNORES tags and
/// keys on the authenticated pubkey); the relay expires it on its ~60s TTL, refreshed by the 30s
/// heartbeat. `"offline"` clears it immediately (a clean WS disconnect also clears immediately).
pub const PRESENCE_STATUS_ONLINE: &str = "online";
/// Explicit clear status (see [`PRESENCE_STATUS_ONLINE`]).
pub const PRESENCE_STATUS_OFFLINE: &str = "offline";

/// NIP-42 client-authentication event kind. The relay challenges the presence connection and the
/// signer actor answers it (via [`NodeNostrSigner`]) — so it is on the raw-sign allowlist.
pub const NIP42_AUTH_KIND: u16 = 22242;

/// The buzz signing allowlist is OWNED BY THE ACTOR ([`super::signer::UNSIGNED_SIGN_ALLOWLIST`]) so
/// the deny is enforced default-deny inside the actor for every caller, not just this wrapper.
/// Re-exported here under the buzz name for the wrapper's belt-and-suspenders check + the seam's
/// single source of truth — kind-0 persona, kind-20001 presence, kind-22242 NIP-42 auth, nothing
/// else (enumerate-entry-points doctrine).
pub use super::signer::{
    unsigned_sign_kind_allowed as buzz_signing_kind_allowed,
    UNSIGNED_SIGN_ALLOWLIST as BUZZ_SIGNING_ALLOWLIST,
};

/// Marker tag stamped on every kind-0 this node publishes, so a later boot can tell ITS OWN
/// persona (safe to replace) from a FOREIGN kind-0 on the same key (must not be clobbered). A
/// buzz client renders kind-0 by its metadata content and ignores this tag.
pub const MOBEE_MARKER_TAG: &str = "maxplayer_persona";
/// The marker tag's value.
pub const MOBEE_MARKER_VALUE: &str = "seller";

/// How long to wait for the relay's stored kind-0 before the clobber check (bounded; the relay
/// terminates the fetch on EOSE, so this is an upper bound, not a fixed wait).
const KIND0_FETCH_TIMEOUT_SECS: u64 = 8;
/// How long to wait for the relay connection before publishing.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// A buzz-persona failure. Never carries key material.
#[derive(Debug)]
pub enum BuzzError {
    /// Config was rejected (e.g. an unparseable relay URL or pubkey).
    Config(String),
    /// A relay operation failed (add/connect/fetch/publish).
    Relay(String),
    /// The clobber guard refused to overwrite a foreign kind-0 already on the key.
    Clobber(String),
    /// The signer actor is gone or refused to sign.
    Signer(String),
}

impl std::fmt::Display for BuzzError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(message) => write!(f, "buzz config: {message}"),
            Self::Relay(message) => write!(f, "buzz relay: {message}"),
            Self::Clobber(message) => write!(f, "buzz kind-0 clobber guard: {message}"),
            Self::Signer(message) => write!(f, "buzz signer: {message}"),
        }
    }
}

impl std::error::Error for BuzzError {}

/// What the clobber guard decides given the key's current kind-0 on the buzz relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClobberDecision {
    /// No kind-0 on the key yet — the first publish is safe.
    FirstPublish,
    /// The existing kind-0 carries our marker — it is ours from a prior run, safe to replace.
    OursReplace,
    /// A kind-0 exists WITHOUT our marker — refuse; publishing would clobber a foreign persona.
    ForeignRefuse,
}

/// Decide whether the node may publish its kind-0, given the marker state of the key's current
/// kind-0: `None` ⇒ no kind-0 exists; `Some(true)` ⇒ one exists carrying our marker; `Some(false)`
/// ⇒ one exists WITHOUT our marker (foreign). Pure, so the money-safe refusal is unit-testable.
pub fn clobber_decision(existing_marker: Option<bool>) -> ClobberDecision {
    match existing_marker {
        None => ClobberDecision::FirstPublish,
        Some(true) => ClobberDecision::OursReplace,
        Some(false) => ClobberDecision::ForeignRefuse,
    }
}

/// True when a fetched kind-0 event carries this node's maxplayer marker tag.
pub fn event_has_marker(event: &Event) -> bool {
    event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.first().map(String::as_str) == Some(MOBEE_MARKER_TAG)
            && parts.get(1).map(String::as_str) == Some(MOBEE_MARKER_VALUE)
    })
}

/// Assemble the human-readable rate card shown in the persona's kind-0 `about`. Pure over its
/// inputs so the wording is unit-testable. `seller_rate_sats` is the `[seller]` rate used when the
/// `[buzz]` section does not set its own.
pub fn rate_card_about(cfg: &BuzzConfig, seller_rate_sats: Option<u64>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(about) = cfg.about.as_deref() {
        let trimmed = about.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_owned());
        }
    }
    if let Some(rate) = cfg.rate_sats.or(seller_rate_sats) {
        parts.push(format!("rate {rate} sat/job"));
    }
    if !cfg.capabilities.is_empty() {
        parts.push(format!("does: {}", cfg.capabilities.join(", ")));
    }
    let mint = cfg.mint.as_deref().unwrap_or("testnut");
    parts.push(format!("pays via {mint}"));
    parts.push("hire me on maxplayer".to_owned());
    parts.join(" · ")
}

/// Build the persona metadata (kind-0 content) for the config. `name` is the display handle; the
/// rate card is the `about`.
fn persona_metadata(cfg: &BuzzConfig, seller_rate_sats: Option<u64>) -> Metadata {
    Metadata::new()
        .name(cfg.name.clone())
        .about(rate_card_about(cfg, seller_rate_sats))
}

/// A nostr-sdk signer that delegates every operation to the seller node's [`signer`](super::signer)
/// actor, so the buzz client can NIP-42 auth and publish without ever holding the seller key. Only
/// event signing + public key are supported (all the persona and the auth path need); NIP-04/44
/// are unsupported (the persona never encrypts).
#[derive(Debug, Clone)]
pub struct NodeNostrSigner {
    signer: SignerHandle,
    pubkey: PublicKey,
}

impl NodeNostrSigner {
    /// Build the adapter from the node's signer handle. Fails only if the actor's cached public key
    /// is not parseable (it always is — it is derived from the key at spawn).
    pub fn new(signer: SignerHandle) -> Result<Self, BuzzError> {
        let pubkey = PublicKey::parse(signer.public_key_hex())
            .map_err(|error| BuzzError::Config(format!("seller pubkey parse: {error}")))?;
        Ok(Self { signer, pubkey })
    }
}

impl NostrSigner for NodeNostrSigner {
    fn backend(&self) -> SignerBackend<'_> {
        SignerBackend::Custom(std::borrow::Cow::Borrowed("mobee-seller-node-signer-actor"))
    }

    fn get_public_key(&self) -> BoxedFuture<'_, Result<PublicKey, SignerError>> {
        let pubkey = self.pubkey;
        Box::pin(async move { Ok(pubkey) })
    }

    fn sign_event(&self, unsigned: UnsignedEvent) -> BoxedFuture<'_, Result<Event, SignerError>> {
        let signer = self.signer.clone();
        Box::pin(async move {
            // ALLOWLIST GATE (enumerate-entry-points doctrine): the buzz seam is the ONLY caller of
            // the actor through this adapter, and it may only ever sign identity/presence/auth. Any
            // other kind — a trade-path event or anything else — is refused + logged so the seam can
            // never become a generic sign-anything oracle for the protocol key.
            let kind = unsigned.kind.as_u16();
            if !buzz_signing_kind_allowed(kind) {
                eprintln!(
                    "buzz signer REFUSED to sign kind-{kind}: not on the buzz allowlist {BUZZ_SIGNING_ALLOWLIST:?} \
                     (only kind-0 persona, kind-{PRESENCE_KIND} presence, and kind-{NIP42_AUTH_KIND} NIP-42 auth are permitted)"
                );
                return Err(SignerError::from(format!(
                    "buzz signing refused: kind-{kind} is not on the buzz allowlist"
                )));
            }
            match signer.sign_unsigned(unsigned).await {
                Ok(Ok(event)) => Ok(event),
                Ok(Err(message)) => Err(SignerError::from(message)),
                Err(gone) => Err(SignerError::from(gone.to_string())),
            }
        })
    }

    fn nip04_encrypt<'a>(
        &'a self,
        _public_key: &'a PublicKey,
        _content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        Box::pin(async move { Err(SignerError::from("nip04 unsupported for the buzz persona")) })
    }

    fn nip04_decrypt<'a>(
        &'a self,
        _public_key: &'a PublicKey,
        _encrypted_content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        Box::pin(async move { Err(SignerError::from("nip04 unsupported for the buzz persona")) })
    }

    fn nip44_encrypt<'a>(
        &'a self,
        _public_key: &'a PublicKey,
        _content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        Box::pin(async move { Err(SignerError::from("nip44 unsupported for the buzz persona")) })
    }

    fn nip44_decrypt<'a>(
        &'a self,
        _public_key: &'a PublicKey,
        _payload: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        Box::pin(async move { Err(SignerError::from("nip44 unsupported for the buzz persona")) })
    }
}

/// A live buzz persona: the connected relay client plus the presence heartbeat task. Hold it for
/// as long as the node is up; drop it (or call [`BuzzHandle::shutdown`]) to clear presence.
pub struct BuzzHandle {
    client: Client,
    pubkey: PublicKey,
    /// The published kind-0 event id (identity/discovery — never money state).
    pub kind0_event_id: String,
    presence_task: tokio::task::JoinHandle<()>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl BuzzHandle {
    /// The persona's public key (hex) — the seller identity.
    pub fn pubkey_hex(&self) -> String {
        self.pubkey.to_hex()
    }

    /// Stop the presence heartbeat and cleanly disconnect the relay socket. The clean disconnect is
    /// what clears deployed presence immediately (rather than waiting out the ~90s TTL).
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        // Wake the heartbeat loop out of its sleep so it observes the stop promptly.
        self.presence_task.abort();
        let _ = self.presence_task.await;
        self.client.disconnect().await;
    }
}

/// Bring up the buzz persona: connect to the relay, run the kind-0 clobber guard, publish the
/// persona, and start the presence heartbeat. The seller key stays in the signer actor throughout
/// (via [`NodeNostrSigner`]). Returns a [`BuzzHandle`] that owns the live connection + heartbeat.
pub async fn start(
    signer: SignerHandle,
    cfg: &BuzzConfig,
    seller_rate_sats: Option<u64>,
) -> Result<BuzzHandle, BuzzError> {
    let adapter = NodeNostrSigner::new(signer)?;
    let pubkey = adapter.pubkey;
    // Every buzz-path publish (kind-0, presence) and the NIP-42 auth all go through this ONE adapter
    // — the single signing choke point where the allowlist is enforced. Keep a clone for our own
    // publishes; the other is consumed by the client for auth.
    let publish_signer = adapter.clone();

    let client = Client::new(adapter);
    client.automatic_authentication(true);
    client
        .add_relay(&cfg.relay_url)
        .await
        .map_err(|error| BuzzError::Relay(format!("add relay {}: {error}", cfg.relay_url)))?;
    client.connect().await;
    client
        .wait_for_connection(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .await;

    // Clobber guard BEFORE the first publish: never overwrite a foreign kind-0 on the key.
    match fetch_kind0_marker(&client, pubkey).await {
        Ok(marker) => {
            if let ClobberDecision::ForeignRefuse = clobber_decision(marker) {
                client.disconnect().await;
                return Err(BuzzError::Clobber(format!(
                    "a kind-0 already exists on this key ({}) that this node did not write \
                     (missing the maxplayer marker); refusing to clobber a foreign buzz persona — \
                     use a fresh key for the seller or clear the existing kind-0 first",
                    pubkey.to_hex()
                )));
            }
        }
        Err(error) => {
            // Fail closed: if we cannot read the current kind-0 we cannot prove we are not
            // clobbering a foreign one, so refuse rather than blind-overwrite.
            client.disconnect().await;
            return Err(BuzzError::Clobber(format!(
                "could not read the key's current kind-0 to check for a foreign persona \
                 (fail-closed, refusing to publish): {error}"
            )));
        }
    }

    // Publish the persona kind-0 (signed via the allowlisted adapter → actor).
    let kind0 = build_kind0(&publish_signer, pubkey, cfg, seller_rate_sats).await?;
    let kind0_event_id = kind0.id.to_hex();
    send_event(&client, &kind0).await?;

    // First presence beat immediately, then the periodic heartbeat.
    let first = build_presence(&publish_signer, pubkey).await?;
    send_event(&client, &first).await?;

    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let interval = Duration::from_secs(cfg.heartbeat_secs.max(1));
    let presence_task =
        spawn_presence_heartbeat(client.clone(), publish_signer, pubkey, interval, stop_rx);

    Ok(BuzzHandle {
        client,
        pubkey,
        kind0_event_id,
        presence_task,
        stop,
    })
}

/// Fetch the key's current kind-0 and classify it for the clobber guard: `Ok(None)` ⇒ no kind-0,
/// `Ok(Some(true/false))` ⇒ one exists with/without our marker.
async fn fetch_kind0_marker(client: &Client, pubkey: PublicKey) -> Result<Option<bool>, BuzzError> {
    let filter = Filter::new().author(pubkey).kind(Kind::Metadata).limit(1);
    let events = client
        .fetch_events(filter, Duration::from_secs(KIND0_FETCH_TIMEOUT_SECS))
        .await
        .map_err(|error| BuzzError::Relay(format!("fetch kind-0: {error}")))?;
    // Newest replaceable kind-0 wins.
    let newest = events.into_iter().max_by_key(|event| event.created_at);
    Ok(newest.map(|event| event_has_marker(&event)))
}

/// Build + sign (via the allowlisted adapter) the persona's kind-0 metadata event with the maxplayer
/// marker tag.
async fn build_kind0(
    adapter: &NodeNostrSigner,
    pubkey: PublicKey,
    cfg: &BuzzConfig,
    seller_rate_sats: Option<u64>,
) -> Result<Event, BuzzError> {
    let metadata = persona_metadata(cfg, seller_rate_sats);
    let marker = Tag::parse([MOBEE_MARKER_TAG, MOBEE_MARKER_VALUE])
        .map_err(|error| BuzzError::Config(format!("marker tag: {error}")))?;
    let unsigned = EventBuilder::metadata(&metadata).tag(marker).build(pubkey);
    sign_via_adapter(adapter, unsigned).await
}

/// Build + sign (via the allowlisted adapter) one presence heartbeat event. The deployed relay
/// keys presence on the authenticated connection's pubkey and reads only the content status — no
/// tags (they are ignored). The event is signed by (and authored as) the persona itself.
async fn build_presence(adapter: &NodeNostrSigner, pubkey: PublicKey) -> Result<Event, BuzzError> {
    let unsigned =
        EventBuilder::new(Kind::Custom(PRESENCE_KIND), PRESENCE_STATUS_ONLINE).build(pubkey);
    sign_via_adapter(adapter, unsigned).await
}

/// Sign an unsigned buzz event through the allowlisted adapter (which routes to the signer actor).
/// A non-allowlisted kind is refused by the adapter before it ever reaches the actor.
async fn sign_via_adapter(
    adapter: &NodeNostrSigner,
    unsigned: UnsignedEvent,
) -> Result<Event, BuzzError> {
    adapter
        .sign_event(unsigned)
        .await
        .map_err(|error| BuzzError::Signer(error.to_string()))
}

async fn send_event(client: &Client, event: &Event) -> Result<(), BuzzError> {
    let output = client
        .send_event(event)
        .await
        .map_err(|error| BuzzError::Relay(format!("send kind-{}: {error}", event.kind.as_u16())))?;
    if output.success.is_empty() {
        let failed: Vec<String> = output
            .failed
            .into_iter()
            .map(|(url, err)| format!("{url}: {err}"))
            .collect();
        return Err(BuzzError::Relay(format!(
            "no relay accepted kind-{} ({})",
            event.kind.as_u16(),
            failed.join("; ")
        )));
    }
    Ok(())
}

/// Spawn the presence heartbeat loop: republish an ephemeral kind-20001 `"online"` status every
/// `interval` until `stop_rx` flips true. A publish failure is logged and retried on the next tick
/// (presence recovers within the relay TTL). The task exits promptly on stop OR on abort.
fn spawn_presence_heartbeat(
    client: Client,
    adapter: NodeNostrSigner,
    pubkey: PublicKey,
    interval: Duration,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    match build_presence(&adapter, pubkey).await {
                        Ok(event) => {
                            if let Err(error) = send_event(&client, &event).await {
                                eprintln!("buzz presence heartbeat publish failed (will retry): {error}");
                            }
                        }
                        Err(error) => {
                            eprintln!("buzz presence heartbeat build failed (will retry): {error}");
                        }
                    }
                }
                changed = stop_rx.changed() => {
                    // Sender dropped or told us to stop — exit the loop; the handle disconnects.
                    if changed.is_err() || *stop_rx.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::BuzzConfig;

    fn cfg() -> BuzzConfig {
        BuzzConfig {
            relay_url: "wss://buzz.example".to_owned(),
            name: "Rocky".to_owned(),
            about: Some("Rust reviewer".to_owned()),
            rate_sats: Some(50),
            capabilities: vec!["code".to_owned(), "test".to_owned()],
            mint: None,
            heartbeat_secs: 30,
        }
    }

    #[test]
    fn clobber_refuses_only_a_foreign_kind0() {
        assert_eq!(clobber_decision(None), ClobberDecision::FirstPublish);
        assert_eq!(clobber_decision(Some(true)), ClobberDecision::OursReplace);
        assert_eq!(clobber_decision(Some(false)), ClobberDecision::ForeignRefuse);
    }

    #[test]
    fn rate_card_carries_rate_caps_and_mint() {
        let about = rate_card_about(&cfg(), None);
        assert!(about.contains("Rust reviewer"), "about: {about}");
        assert!(about.contains("50 sat/job"), "about: {about}");
        assert!(about.contains("code, test"), "about: {about}");
        assert!(about.contains("testnut"), "about: {about}");
    }

    #[test]
    fn rate_card_falls_back_to_seller_rate() {
        let mut c = cfg();
        c.rate_sats = None;
        let about = rate_card_about(&c, Some(7));
        assert!(about.contains("7 sat/job"), "about: {about}");
    }

    #[test]
    fn rate_card_honours_explicit_mint() {
        let mut c = cfg();
        c.mint = Some("https://real.mint".to_owned());
        let about = rate_card_about(&c, None);
        assert!(about.contains("https://real.mint"), "about: {about}");
    }

    #[test]
    fn presence_is_in_the_ephemeral_range() {
        assert!((20000..=29999).contains(&PRESENCE_KIND));
    }

    #[test]
    fn allowlist_is_exactly_persona_presence_auth() {
        assert!(buzz_signing_kind_allowed(0), "kind-0 persona allowed");
        assert!(buzz_signing_kind_allowed(PRESENCE_KIND), "presence allowed");
        assert!(buzz_signing_kind_allowed(NIP42_AUTH_KIND), "NIP-42 auth allowed");
        // Trade-path + arbitrary kinds are NOT signable through the buzz seam.
        for kind in [3400u16, 3401, 3402, 3403, 3404, 3405, 30340, 1, 4, 9734] {
            assert!(!buzz_signing_kind_allowed(kind), "kind-{kind} must be refused");
        }
    }

    // TOOTH (enumerate-entry-points): the adapter is the ONLY buzz path to the actor, and it must
    // sign ONLY the three allowlisted kinds. A trade-path kind requested through it is refused —
    // the seam is not a sign-anything oracle for the protocol key. Signing goes through the real
    // actor to prove the gate sits in front of it, not merely in a pure helper.
    #[tokio::test(flavor = "current_thread")]
    async fn adapter_signs_only_allowlisted_kinds() {
        use nostr_sdk::prelude::{EventBuilder, Kind};

        let root = std::env::temp_dir().join(format!(
            "maxplayer-buzz-allowlist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = crate::home::bootstrap(&root).expect("bootstrap");
        let signer = crate::seller_node::signer::spawn(&home).expect("spawn signer");
        let adapter = NodeNostrSigner::new(signer).expect("adapter");
        let pubkey = adapter.pubkey;

        // Each allowlisted kind signs cleanly.
        for kind in [0u16, PRESENCE_KIND, NIP42_AUTH_KIND] {
            let unsigned = EventBuilder::new(Kind::Custom(kind), "")
                .allow_self_tagging()
                .build(pubkey);
            let signed = adapter.sign_event(unsigned).await;
            assert!(signed.is_ok(), "kind-{kind} must sign through the buzz seam: {signed:?}");
        }

        // A trade-path kind (an offer) is refused BEFORE the actor signs it.
        let offer = EventBuilder::new(Kind::Custom(crate::gateway::JOB_OFFER_KIND), "")
            .build(pubkey);
        let refused = adapter.sign_event(offer).await;
        assert!(refused.is_err(), "a trade-path kind must be refused by the buzz seam");
        assert!(
            refused.unwrap_err().to_string().contains("allowlist"),
            "refusal must name the allowlist"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
