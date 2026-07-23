//! Seller heartbeat — addressable kind-30340 liveness + capacity signal.
//!
//! A running seller republishes an **addressable** (NIP-01 parameterized-replaceable) event,
//! `d="mobee-seller"`, on a ~5-minute cadence. It advertises whether the seller is `accepting`
//! new work, its `queue_depth`, its `rate`, and the `protocol_versions` it speaks (feeding
//! `min_protocol_version` eligibility). This is diagnostic/discovery context only — it never
//! feeds the pay gate, journal, or receipt bind.
//!
//! **Resolve by `(pubkey, d)`, never by event id.** An addressable event is superseded in place,
//! so a superseded id goes empty and a by-id lookup would read as "seller gone." Consumers must
//! always resolve the latest heartbeat by author + `d`. See [`HeartbeatKey`].

use serde::Serialize;

use crate::gateway::{EventDraft, MOBEE_TAG, PROTOCOL_VERSION, TagSpec};

/// Addressable kind for the seller heartbeat. MUST be in NIP-01's `30000..=39999` addressable
/// range so the relay replaces it in place keyed by `(pubkey, d)` — hence `30340`, not a `34xx`
/// value.
pub const SELLER_HEARTBEAT_KIND: u16 = 30340;

/// The addressable `d` identifier for the seller heartbeat.
pub const SELLER_HEARTBEAT_D: &str = "mobee-seller";

/// Env override for the heartbeat cadence (seconds). Takes precedence over `[seller_heartbeat]
/// interval_secs`; intended for tests that cannot wait 5 minutes.
pub const HEARTBEAT_INTERVAL_ENV: &str = "MOBEE_HEARTBEAT_INTERVAL_SECS";

/// Env override for heartbeat enablement (`0`/`false`/`no` disable, `1`/`true`/`yes` enable).
/// Takes precedence over `[seller_heartbeat] enabled`; intended for tests.
pub const HEARTBEAT_ENABLED_ENV: &str = "MOBEE_HEARTBEAT_ENABLED";

/// A heartbeat ready to sign + publish. Build from live daemon state via [`heartbeat_for_state`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeartbeatDraft {
    /// Is the seller taking new work right now (`y`/`n`).
    pub accepting: bool,
    /// Current in-flight job count.
    pub queue_depth: u32,
    /// The seller's advertised rate (sats).
    pub rate_sats: u64,
    /// The mobee protocol versions this seller speaks.
    pub protocol_versions: Vec<String>,
}

impl HeartbeatDraft {
    pub fn new(
        accepting: bool,
        queue_depth: u32,
        rate_sats: u64,
        protocol_versions: Vec<String>,
    ) -> Self {
        Self {
            accepting,
            queue_depth,
            rate_sats,
            protocol_versions,
        }
    }

    /// Convenience constructor: the heartbeat wire carries protocol version `1`.
    pub fn v1(accepting: bool, queue_depth: u32, rate_sats: u64) -> Self {
        Self::new(
            accepting,
            queue_depth,
            rate_sats,
            vec![PROTOCOL_VERSION.to_owned()],
        )
    }

    pub fn to_event_draft(&self) -> EventDraft {
        let accepting = if self.accepting { "y" } else { "n" };
        let queue_depth = self.queue_depth.to_string();
        let rate = self.rate_sats.to_string();
        // `protocol_versions` carries every spoken version as extra tag positions
        // (`["protocol_versions", "1", ...]`), matching the multi-value tag convention.
        let mut protocol_tag = vec!["protocol_versions".to_owned()];
        protocol_tag.extend(self.protocol_versions.iter().cloned());

        let tags = vec![
            TagSpec::new(["d", SELLER_HEARTBEAT_D]),
            TagSpec::new(["t", MOBEE_TAG]),
            TagSpec::new(["accepting", accepting]),
            TagSpec::new(["queue_depth", &queue_depth]),
            TagSpec::new(["rate", &rate]),
            TagSpec(protocol_tag),
        ];
        EventDraft::new(SELLER_HEARTBEAT_KIND, tags, "")
    }
}

/// Build the heartbeat for a seller's live state. `accepting` is `n` while a job holds the
/// single-flight slot (a busy seller is not taking new work); `queue_depth` is that in-flight
/// count. This is the single mapping the daemon loop uses, factored out so the flip is unit-
/// testable without a live relay.
pub fn heartbeat_for_state(job_in_flight: bool, rate_sats: u64) -> HeartbeatDraft {
    HeartbeatDraft::v1(!job_in_flight, u32::from(job_in_flight), rate_sats)
}

/// A parsed heartbeat's payload. The author pubkey is NOT carried here — combine it with [`d`]
/// via [`ParsedHeartbeat::key`] to get the `(pubkey, d)` identity.
///
/// [`d`]: ParsedHeartbeat::d
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParsedHeartbeat {
    pub d: String,
    pub accepting: bool,
    pub queue_depth: u32,
    pub rate_sats: u64,
    pub protocol_versions: Vec<String>,
}

impl ParsedHeartbeat {
    /// The `(pubkey, d)` key for this heartbeat given its author.
    ///
    /// **Always key a heartbeat by this, never by event id.** An addressable event is superseded
    /// in place, so an old id goes empty and a by-id lookup would read as "seller gone"
    /// (NIP-01).
    pub fn key(&self, author_pubkey: &str) -> HeartbeatKey {
        HeartbeatKey {
            pubkey: author_pubkey.to_owned(),
            d: self.d.clone(),
        }
    }
}

/// Identity of a seller heartbeat: `(pubkey, d)`. This — never the event id — is how consumers
/// resolve the latest heartbeat for a seller (see [`ParsedHeartbeat::key`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HeartbeatKey {
    pub pubkey: String,
    pub d: String,
}

/// Reasons a kind-30340 event fails to parse as a mobee seller heartbeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeartbeatParseError {
    WrongKind(u16),
    MissingMobeeTag,
    /// The `d` tag is absent or not `mobee-seller`.
    WrongDTag(Option<String>),
    MissingTag(&'static str),
    InvalidAccepting(String),
    InvalidQueueDepth(String),
    InvalidRate(String),
}

impl std::fmt::Display for HeartbeatParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongKind(kind) => {
                write!(f, "expected kind {SELLER_HEARTBEAT_KIND}, got {kind}")
            }
            Self::MissingMobeeTag => write!(f, "missing t={MOBEE_TAG} tag"),
            Self::WrongDTag(d) => write!(
                f,
                "expected d={SELLER_HEARTBEAT_D}, got {}",
                d.as_deref().unwrap_or("<none>")
            ),
            Self::MissingTag(name) => write!(f, "missing {name} tag"),
            Self::InvalidAccepting(value) => {
                write!(f, "accepting must be y/n, got {value}")
            }
            Self::InvalidQueueDepth(value) => write!(f, "invalid queue_depth: {value}"),
            Self::InvalidRate(value) => write!(f, "invalid rate: {value}"),
        }
    }
}

impl std::error::Error for HeartbeatParseError {}

/// Parse a kind-30340 event into a [`ParsedHeartbeat`]. Rejects a wrong kind, a missing
/// `t=mobee` guard, or a `d` other than `mobee-seller`.
pub fn parse_heartbeat(event: &EventDraft) -> Result<ParsedHeartbeat, HeartbeatParseError> {
    if event.kind != SELLER_HEARTBEAT_KIND {
        return Err(HeartbeatParseError::WrongKind(event.kind));
    }
    if !has_tag_value(&event.tags, "t", MOBEE_TAG) {
        return Err(HeartbeatParseError::MissingMobeeTag);
    }
    let d = first_tag_value(&event.tags, "d");
    if d != Some(SELLER_HEARTBEAT_D) {
        return Err(HeartbeatParseError::WrongDTag(d.map(str::to_owned)));
    }

    let accepting = match first_tag_value(&event.tags, "accepting") {
        Some("y") => true,
        Some("n") => false,
        Some(other) => return Err(HeartbeatParseError::InvalidAccepting(other.to_owned())),
        None => return Err(HeartbeatParseError::MissingTag("accepting")),
    };

    let queue_raw = first_tag_value(&event.tags, "queue_depth")
        .ok_or(HeartbeatParseError::MissingTag("queue_depth"))?;
    let queue_depth = queue_raw
        .parse()
        .map_err(|_| HeartbeatParseError::InvalidQueueDepth(queue_raw.to_owned()))?;

    let rate_raw =
        first_tag_value(&event.tags, "rate").ok_or(HeartbeatParseError::MissingTag("rate"))?;
    let rate_sats = rate_raw
        .parse()
        .map_err(|_| HeartbeatParseError::InvalidRate(rate_raw.to_owned()))?;

    let protocol_versions = first_tag(&event.tags, "protocol_versions")
        .map(|tag| tag.0[1..].to_vec())
        .ok_or(HeartbeatParseError::MissingTag("protocol_versions"))?;

    Ok(ParsedHeartbeat {
        d: SELLER_HEARTBEAT_D.to_owned(),
        accepting,
        queue_depth,
        rate_sats,
        protocol_versions,
    })
}

/// Effective cadence (seconds): env override ([`HEARTBEAT_INTERVAL_ENV`]) wins over the
/// `[seller_heartbeat] interval_secs` config. A `0` or unparseable env value is ignored.
pub fn resolve_interval_secs(config: &crate::home::SellerHeartbeatConfig) -> u64 {
    match std::env::var(HEARTBEAT_INTERVAL_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => secs,
            _ => config.interval_secs,
        },
        Err(_) => config.interval_secs,
    }
}

/// Effective enablement: env override ([`HEARTBEAT_ENABLED_ENV`]) wins over the
/// `[seller_heartbeat] enabled` config. Unrecognised env values fall back to config.
pub fn resolve_enabled(config: &crate::home::SellerHeartbeatConfig) -> bool {
    match std::env::var(HEARTBEAT_ENABLED_ENV) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => config.enabled,
        },
        Err(_) => config.enabled,
    }
}

fn first_tag<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a TagSpec> {
    tags.iter()
        .find(|tag| tag.0.first().map(String::as_str) == Some(name))
}

fn first_tag_value<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a str> {
    first_tag(tags, name).and_then(TagSpec::value)
}

fn has_tag_value(tags: &[TagSpec], name: &str, value: &str) -> bool {
    tags.iter().any(|tag| {
        tag.0.first().map(String::as_str) == Some(name)
            && tag.0.get(1).map(String::as_str) == Some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::SellerHeartbeatConfig;

    #[test]
    fn heartbeat_addressable() {
        // Kind is in NIP-01's addressable range so the relay replaces it in place by (pubkey, d).
        assert!((30000..=39999).contains(&SELLER_HEARTBEAT_KIND));
        assert_eq!(SELLER_HEARTBEAT_KIND, 30340);

        // Keyed by (pubkey, d), never by event id.
        let parsed = parse_heartbeat(&HeartbeatDraft::v1(true, 0, 5).to_event_draft())
            .expect("parse own draft");
        let key = parsed.key("seller-pubkey-hex");
        assert_eq!(key.pubkey, "seller-pubkey-hex");
        assert_eq!(key.d, SELLER_HEARTBEAT_D);
        // The same author with the same d always resolves to one identity regardless of the
        // (superseded) event that carried it.
        assert_eq!(key, parsed.key("seller-pubkey-hex"));
    }

    #[test]
    fn heartbeat_draft_shape() {
        let draft = HeartbeatDraft::v1(true, 0, 7).to_event_draft();
        assert_eq!(draft.kind, SELLER_HEARTBEAT_KIND);
        assert_eq!(first_tag_value(&draft.tags, "d"), Some(SELLER_HEARTBEAT_D));
        assert_eq!(first_tag_value(&draft.tags, "t"), Some(MOBEE_TAG));
        assert_eq!(first_tag_value(&draft.tags, "accepting"), Some("y"));
        assert_eq!(first_tag_value(&draft.tags, "queue_depth"), Some("0"));
        assert_eq!(first_tag_value(&draft.tags, "rate"), Some("7"));
        assert_eq!(
            first_tag_value(&draft.tags, "protocol_versions"),
            Some(PROTOCOL_VERSION)
        );
        assert!(draft.content.is_empty());
    }

    #[test]
    fn accepting_flips_with_in_flight_state() {
        let idle = heartbeat_for_state(false, 5);
        assert!(idle.accepting);
        assert_eq!(idle.queue_depth, 0);
        assert_eq!(
            first_tag_value(&idle.to_event_draft().tags, "accepting"),
            Some("y")
        );

        let busy = heartbeat_for_state(true, 5);
        assert!(!busy.accepting);
        assert_eq!(busy.queue_depth, 1);
        assert_eq!(
            first_tag_value(&busy.to_event_draft().tags, "accepting"),
            Some("n")
        );
        assert_eq!(
            first_tag_value(&busy.to_event_draft().tags, "queue_depth"),
            Some("1")
        );
    }

    #[test]
    fn reader_round_trip() {
        let draft = HeartbeatDraft::new(false, 3, 21, vec!["1".to_owned(), "2".to_owned()]);
        let parsed = parse_heartbeat(&draft.to_event_draft()).expect("round-trip parse");
        assert_eq!(parsed.d, SELLER_HEARTBEAT_D);
        assert!(!parsed.accepting);
        assert_eq!(parsed.queue_depth, 3);
        assert_eq!(parsed.rate_sats, 21);
        assert_eq!(parsed.protocol_versions, vec!["1", "2"]);
    }

    #[test]
    fn parse_rejects_wrong_kind_and_missing_guards() {
        let mut wrong_kind = HeartbeatDraft::v1(true, 0, 5).to_event_draft();
        wrong_kind.kind = 30341;
        assert_eq!(
            parse_heartbeat(&wrong_kind),
            Err(HeartbeatParseError::WrongKind(30341))
        );

        // Drop the t=mobee guard.
        let mut no_mobee = HeartbeatDraft::v1(true, 0, 5).to_event_draft();
        no_mobee.tags.retain(|tag| tag.first() != Some("t"));
        assert_eq!(
            parse_heartbeat(&no_mobee),
            Err(HeartbeatParseError::MissingMobeeTag)
        );

        // Wrong d.
        let mut wrong_d = HeartbeatDraft::v1(true, 0, 5).to_event_draft();
        for tag in wrong_d.tags.iter_mut() {
            if tag.first() == Some("d") {
                tag.0[1] = "not-mobee-seller".to_owned();
            }
        }
        assert_eq!(
            parse_heartbeat(&wrong_d),
            Err(HeartbeatParseError::WrongDTag(Some(
                "not-mobee-seller".to_owned()
            )))
        );
    }

    #[test]
    fn interval_respects_config() {
        // Serialize env access across the two env-reading tests (process-global env).
        // SAFETY (edition 2024): mutations are serialized by ENV_LOCK and these are the only
        // tests that touch the heartbeat env vars.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        unsafe {
            std::env::remove_var(HEARTBEAT_INTERVAL_ENV);
            std::env::remove_var(HEARTBEAT_ENABLED_ENV);
        }

        // Default cadence is 300s (5 min).
        let default_cfg = SellerHeartbeatConfig::default();
        assert_eq!(default_cfg.interval_secs, 300);
        assert!(default_cfg.enabled);
        assert_eq!(resolve_interval_secs(&default_cfg), 300);

        // Config override (no env) is honoured.
        let custom = SellerHeartbeatConfig {
            enabled: true,
            interval_secs: 42,
        };
        assert_eq!(resolve_interval_secs(&custom), 42);

        // Env override wins over config.
        unsafe { std::env::set_var(HEARTBEAT_INTERVAL_ENV, "3") };
        assert_eq!(resolve_interval_secs(&custom), 3);
        // A zero/garbage env value is ignored (falls back to config).
        unsafe { std::env::set_var(HEARTBEAT_INTERVAL_ENV, "0") };
        assert_eq!(resolve_interval_secs(&custom), 42);
        unsafe { std::env::set_var(HEARTBEAT_INTERVAL_ENV, "nonsense") };
        assert_eq!(resolve_interval_secs(&custom), 42);
        unsafe { std::env::remove_var(HEARTBEAT_INTERVAL_ENV) };
    }

    #[test]
    fn enabled_respects_env_override() {
        // SAFETY (edition 2024): serialized by ENV_LOCK; see `interval_respects_config`.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        unsafe { std::env::remove_var(HEARTBEAT_ENABLED_ENV) };

        let enabled_cfg = SellerHeartbeatConfig {
            enabled: true,
            interval_secs: 300,
        };
        assert!(resolve_enabled(&enabled_cfg));
        unsafe { std::env::set_var(HEARTBEAT_ENABLED_ENV, "0") };
        assert!(!resolve_enabled(&enabled_cfg));
        unsafe { std::env::set_var(HEARTBEAT_ENABLED_ENV, "true") };
        assert!(resolve_enabled(&enabled_cfg));
        unsafe { std::env::remove_var(HEARTBEAT_ENABLED_ENV) };
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
