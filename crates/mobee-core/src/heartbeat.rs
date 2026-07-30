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
use crate::seller_agents::AGENT_TAG;

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

/// Env override for the relay-stall watchdog threshold (missed heartbeat intervals). Takes
/// precedence over `[seller_heartbeat] stall_missed_intervals`; intended for tests that cannot
/// wait several 5-minute intervals for the watchdog to trip.
pub const HEARTBEAT_STALL_MISSED_INTERVALS_ENV: &str = "MOBEE_HEARTBEAT_STALL_MISSED_INTERVALS";

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
    /// The agent harnesses this seller can run, in preference order. Empty ⇒ the seller states no
    /// harness and the tag is omitted entirely (an unlabelled `agent_command` seller has no honest
    /// name to publish).
    pub agents: Vec<String>,
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
            agents: Vec::new(),
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

    /// Advertise `agents` (preference order) on this heartbeat.
    pub fn with_agents(mut self, agents: Vec<String>) -> Self {
        self.agents = agents;
        self
    }

    pub fn to_event_draft(&self) -> EventDraft {
        let accepting = if self.accepting { "y" } else { "n" };
        let queue_depth = self.queue_depth.to_string();
        let rate = self.rate_sats.to_string();
        // `protocol_versions` carries every spoken version as extra tag positions
        // (`["protocol_versions", "1", ...]`), matching the multi-value tag convention.
        let mut protocol_tag = vec!["protocol_versions".to_owned()];
        protocol_tag.extend(self.protocol_versions.iter().cloned());

        let mut tags = vec![
            TagSpec::new(["d", SELLER_HEARTBEAT_D]),
            TagSpec::new(["t", MOBEE_TAG]),
            TagSpec::new(["accepting", accepting]),
            TagSpec::new(["queue_depth", &queue_depth]),
            TagSpec::new(["rate", &rate]),
            TagSpec(protocol_tag),
        ];
        if let Some(tag) = agent_tag(&self.agents) {
            tags.push(tag);
        }
        EventDraft::new(SELLER_HEARTBEAT_KIND, tags, "")
    }
}

/// The `["mobee_agent", …]` advertisement tag, or `None` for a seller that states no harness (the
/// tag is then omitted rather than emitted empty — absent means "unstated", never "none").
pub fn agent_tag(agents: &[String]) -> Option<TagSpec> {
    if agents.is_empty() {
        return None;
    }
    let mut tag = vec![AGENT_TAG.to_owned()];
    tag.extend(agents.iter().cloned());
    Some(TagSpec(tag))
}

/// Read a `["mobee_agent", …]` advertisement off any event's tags. Absent ⇒ empty.
pub fn agents_from_tags(tags: &[TagSpec]) -> Vec<String> {
    first_tag(tags, AGENT_TAG)
        .map(|tag| tag.0[1..].to_vec())
        .unwrap_or_default()
}

/// Build the heartbeat for a seller's live state. `accepting` is `y` only when the seat has a free
/// slot AND something is actually serving: a busy seller is not taking new work, and neither is a
/// seat that has dropped every harness. `agents` is what the live roster advertises. This is the
/// single mapping the daemon loop uses, factored out so the flip is unit-testable without a relay.
///
/// ⚠ **`in_flight` is a COUNT, and the type is load-bearing.** This parameter was a `bool`, which
/// destroyed the count at the signature — the `queue_depth` on the wire could then only ever be 0 or
/// 1 no matter what the caller knew, while this doc still claimed it was the in-flight count. The
/// caller then supplied that bool as `COUNT(*) FROM jobs > 0`, a LIFETIME row count, so a seat
/// published `accepting=n` permanently from its first job onward (#313).
/// ★ The 0/1 cast is why it survived: a seat holding 5 finished jobs advertised `1`, which reads as
/// plausible. A literal `5` on an idle seat would have looked absurd to anyone. **A lossy encoding of
/// a quantity hides a wrong answer inside a believable one** — so keep this a count, and let the wire
/// carry a number a reader can sanity-check.
///
/// ⚠ `anything_serving` is NOT derivable from `agents`. The roster advertises NAMES, and the
/// unlabelled `--agent-argv` hatch has none — so a seat serving only the hatch advertises an empty
/// list while being perfectly able to work, and reading darkness off that list would take it off the
/// market for lacking a label. The signal comes from the roster's own dispatch predicate.
///
/// WHY `accepting` rather than a marker on the agents tag: an ABSENT tag already means "unstated",
/// so there is no spare state there to mean "none" without a protocol change every reader would have
/// to learn. `accepting` has no unstated value, so the truth fits in a field that already exists.
pub fn heartbeat_for_state(
    in_flight: u32,
    anything_serving: bool,
    rate_sats: u64,
    agents: Vec<String>,
) -> HeartbeatDraft {
    HeartbeatDraft::v1(in_flight == 0 && anything_serving, in_flight, rate_sats).with_agents(agents)
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
    /// Advertised harnesses, preference order. Empty ⇒ the seller stated none (the tag was
    /// absent) — NOT a claim that it can run nothing.
    pub agents: Vec<String>,
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
        agents: agents_from_tags(&event.tags),
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

/// Effective relay-stall watchdog threshold (missed heartbeat intervals): env override
/// ([`HEARTBEAT_STALL_MISSED_INTERVALS_ENV`]) wins over the `[seller_heartbeat]
/// stall_missed_intervals` config. A `0` or unparseable value is ignored (falls back to config).
/// Clamped to at least 1 so a misconfiguration can never make the watchdog trip on the first tick.
pub fn resolve_stall_missed_intervals(config: &crate::home::SellerHeartbeatConfig) -> u32 {
    let configured = match std::env::var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV) {
        Ok(raw) => match raw.trim().parse::<u32>() {
            Ok(n) if n > 0 => n,
            _ => config.stall_missed_intervals,
        },
        Err(_) => config.stall_missed_intervals,
    };
    configured.max(1)
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
    fn advertises_every_harness_in_preference_order() {
        let draft = heartbeat_for_state(0, true, 5, vec!["claude".into(), "codex".into()])
            .to_event_draft();
        let tag = first_tag(&draft.tags, "mobee_agent").expect("mobee_agent tag");
        assert_eq!(tag.0, vec!["mobee_agent", "claude", "codex"]);
        // The reader gets the same ordered list back.
        let parsed = parse_heartbeat(&draft).expect("round-trip");
        assert_eq!(parsed.agents, vec!["claude", "codex"]);
    }

    #[test]
    fn a_seller_stating_no_harness_emits_a_byte_identical_heartbeat() {
        // Compat, the byte-identity half: a raw `agent_command` seller has no preset label, so it
        // advertises nothing and its heartbeat is EXACTLY the pre-registry event — no empty tag.
        // It IS serving (hence `true`), which is why an unstated list must never read as dark.
        let stated_none = heartbeat_for_state(0, true, 5, Vec::new()).to_event_draft();
        let before_registry = HeartbeatDraft::v1(true, 0, 5).to_event_draft();
        assert_eq!(stated_none, before_registry);
        assert!(
            first_tag(&stated_none.tags, "mobee_agent").is_none(),
            "an unstated harness list must omit the tag, never emit it empty"
        );
        assert!(parse_heartbeat(&stated_none).expect("parse").agents.is_empty());
    }

    #[test]
    fn accepting_flips_with_in_flight_state() {
        let idle = heartbeat_for_state(0, true, 5, Vec::new());
        assert!(idle.accepting);
        assert_eq!(idle.queue_depth, 0);
        assert_eq!(
            first_tag_value(&idle.to_event_draft().tags, "accepting"),
            Some("y")
        );

        let busy = heartbeat_for_state(1, true, 5, Vec::new());
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

    /// The whole point of the change: an idle seat with nothing serving is NOT accepting.
    ///
    /// Written as the full truth table so every row is pinned, not just the one that motivated the
    /// change. Transposing the two arguments no longer even compiles — `in_flight` is a `u32` and
    /// `anything_serving` a `bool` — which is a stronger guard than the assertion below; the table
    /// stays because it pins the four OUTPUTS, which the types cannot.
    #[test]
    fn accepting_requires_a_free_slot_and_something_serving() {
        let accepting_of = |in_flight, serving| {
            let draft = heartbeat_for_state(in_flight, serving, 5, Vec::new()).to_event_draft();
            (
                first_tag_value(&draft.tags, "accepting")
                    .expect("accepting tag")
                    .to_owned(),
                first_tag_value(&draft.tags, "queue_depth")
                    .expect("queue_depth tag")
                    .to_owned(),
            )
        };

        assert_eq!(accepting_of(0, true), ("y".into(), "0".into()), "idle + serving");
        assert_eq!(accepting_of(1, true), ("n".into(), "1".into()), "busy");
        // The row this change adds. Before it, a fully dark seat published `y` and kept drawing work
        // it could only decline.
        assert_eq!(accepting_of(0, false), ("n".into(), "0".into()), "idle + dark");
        assert_eq!(accepting_of(1, false), ("n".into(), "1".into()), "busy + dark");

        // And dark is DISTINGUISHABLE from busy, which the pair could not express before: both say
        // "not taking work", and `queue_depth` says which reason.
        assert_ne!(accepting_of(0, false), accepting_of(1, true));
    }

    /// `queue_depth` must carry the DEPTH, not a busy flag.
    ///
    /// This is the assertion a `bool` parameter made unwriteable, and its absence is what let #313
    /// live: the wire reported `1` for a seat holding five finished jobs, and `1` reads as plausible.
    /// Any depth above 1 is therefore the discriminator — it cannot be produced by a flag.
    #[test]
    fn queue_depth_is_the_depth_not_a_busy_flag() {
        for depth in [2_u32, 3, 17] {
            let draft = heartbeat_for_state(depth, true, 5, Vec::new()).to_event_draft();
            assert_eq!(
                first_tag_value(&draft.tags, "queue_depth"),
                Some(depth.to_string().as_str()),
                "queue_depth must publish the count itself, not a 0/1 cast of it"
            );
            assert_eq!(
                first_tag_value(&draft.tags, "accepting"),
                Some("n"),
                "any non-zero depth means the seat is occupied"
            );
        }

        // And the boundary that #313 got wrong in the field: nothing in flight ⇒ available, no
        // matter how much this seat has done in the past. The store-side half of this is
        // `a_store_holding_only_terminal_jobs_reports_none_in_flight`.
        let free = heartbeat_for_state(0, true, 5, Vec::new()).to_event_draft();
        assert_eq!(first_tag_value(&free.tags, "accepting"), Some("y"));
        assert_eq!(first_tag_value(&free.tags, "queue_depth"), Some("0"));
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
            ..SellerHeartbeatConfig::default()
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
            ..SellerHeartbeatConfig::default()
        };
        assert!(resolve_enabled(&enabled_cfg));
        unsafe { std::env::set_var(HEARTBEAT_ENABLED_ENV, "0") };
        assert!(!resolve_enabled(&enabled_cfg));
        unsafe { std::env::set_var(HEARTBEAT_ENABLED_ENV, "true") };
        assert!(resolve_enabled(&enabled_cfg));
        unsafe { std::env::remove_var(HEARTBEAT_ENABLED_ENV) };
    }

    #[test]
    fn stall_missed_intervals_respects_env_and_clamps() {
        // SAFETY (edition 2024): serialized by ENV_LOCK; see `interval_respects_config`.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        unsafe { std::env::remove_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV) };

        // Default is 3.
        let default_cfg = SellerHeartbeatConfig::default();
        assert_eq!(default_cfg.stall_missed_intervals, 3);
        assert_eq!(resolve_stall_missed_intervals(&default_cfg), 3);

        // Config override (no env) is honoured.
        let custom = SellerHeartbeatConfig {
            stall_missed_intervals: 5,
            ..SellerHeartbeatConfig::default()
        };
        assert_eq!(resolve_stall_missed_intervals(&custom), 5);

        // Env override wins over config.
        unsafe { std::env::set_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV, "2") };
        assert_eq!(resolve_stall_missed_intervals(&custom), 2);
        // Zero/garbage env falls back to config.
        unsafe { std::env::set_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV, "0") };
        assert_eq!(resolve_stall_missed_intervals(&custom), 5);
        unsafe { std::env::set_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV, "nonsense") };
        assert_eq!(resolve_stall_missed_intervals(&custom), 5);
        unsafe { std::env::remove_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV) };

        // A config of 0 is clamped up to 1 (never trips on the first tick).
        let zero = SellerHeartbeatConfig {
            stall_missed_intervals: 0,
            ..SellerHeartbeatConfig::default()
        };
        assert_eq!(resolve_stall_missed_intervals(&zero), 1);
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
