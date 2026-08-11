//! Seat announcement — addressable kind-30340 capability + liveness (protocol-v1 §4.2).
//!
//! A running seller republishes an **addressable** (NIP-01 parameterized-replaceable) event,
//! `d="maxplayer-seller"`, on a ~5-minute cadence. It carries every seat-level fact a buyer needs
//! before it trades: whether the seat is `accepting` new work, its `queue_depth`, its `rate`, the
//! `accepted_mints` it can be paid on, and the `agents` it can run. Every fact is current as of
//! that beat.
//!
//! **This is the seat's only capability surface.** Issue #645 retired the kind-31990 handler
//! announce that used to carry the mints and the harness label; a reader must take capability from
//! here and nowhere else. Old 31990 events persist on relays as residue (replaceable events are
//! never deleted) — they are not live capability.
//!
//! None of this feeds the pay gate, journal, or receipt bind: it is pre-trade discovery. The
//! payable mint for one trade is the one carried by that trade's `creq`.
//!
//! **Resolve by `(pubkey, d)`, never by event id.** An addressable event is superseded in place,
//! so a superseded id goes empty and a by-id lookup would read as "seller gone." Consumers must
//! always resolve the latest heartbeat by author + `d`. See [`HeartbeatKey`].

use serde::Serialize;

use crate::gateway::{EventDraft, MAXPLAYER_TAG, PROTOCOL_VERSION, TagSpec};
use crate::seller_agents::{AGENT_TAG, LEGACY_AGENT_TAG};

pub use crate::kinds::SELLER_HEARTBEAT_KIND;

/// The addressable `d` identifier for the seller heartbeat.
pub const SELLER_HEARTBEAT_D: &str = "maxplayer-seller";

/// Env override for the heartbeat cadence (seconds). Takes precedence over `[seller_heartbeat]
/// interval_secs`; intended for tests that cannot wait 5 minutes.
pub const HEARTBEAT_INTERVAL_ENV: &str = "MAXPLAYER_HEARTBEAT_INTERVAL_SECS";

/// Env override for heartbeat enablement (`0`/`false`/`no` disable, `1`/`true`/`yes` enable).
/// Takes precedence over `[seller_heartbeat] enabled`; intended for tests.
pub const HEARTBEAT_ENABLED_ENV: &str = "MAXPLAYER_HEARTBEAT_ENABLED";

/// Env override for the relay-stall watchdog threshold (missed heartbeat intervals). Takes
/// precedence over `[seller_heartbeat] stall_missed_intervals`; intended for tests that cannot
/// wait several 5-minute intervals for the watchdog to trip.
pub const HEARTBEAT_STALL_MISSED_INTERVALS_ENV: &str = "MAXPLAYER_HEARTBEAT_STALL_MISSED_INTERVALS";

/// Wire tag listing every mint the seat accepts payment on (§4.2). Multi-value, order preserved.
pub const ACCEPTED_MINTS_TAG: &str = "accepted_mints";

/// A heartbeat ready to sign + publish. Build from live daemon state via [`heartbeat_for_state`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeartbeatDraft {
    /// Is the seller taking new work right now (`y`/`n`).
    pub accepting: bool,
    /// Current in-flight job count.
    pub queue_depth: u32,
    /// The seller's advertised rate (sats).
    pub rate_sats: u64,
    /// Every mint this seat accepts payment on, in config order. §4.2 requires at least one: a
    /// buyer can pay this seat only on a mint in this list, so a seat stating none is unpayable.
    pub accepted_mints: Vec<String>,
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
        accepted_mints: Vec<String>,
    ) -> Self {
        Self {
            accepting,
            queue_depth,
            rate_sats,
            accepted_mints,
            agents: Vec::new(),
        }
    }

    /// Advertise `agents` (preference order) on this heartbeat.
    pub fn with_agents(mut self, agents: Vec<String>) -> Self {
        self.agents = agents;
        self
    }

    /// The §4.2 tag set, in the order the spec table lists it: `d`, `t`, `v`, `rate`, `accepting`,
    /// `queue_depth`, `accepted_mints`, and `agents` when the seat states a roster.
    pub fn to_event_draft(&self) -> EventDraft {
        let accepting = if self.accepting { "y" } else { "n" };
        let queue_depth = self.queue_depth.to_string();
        let rate = self.rate_sats.to_string();

        let mut tags = vec![
            TagSpec::new(["d", SELLER_HEARTBEAT_D]),
            TagSpec::new(["t", MAXPLAYER_TAG]),
            TagSpec::new(["v", PROTOCOL_VERSION]),
            TagSpec::new(["rate", &rate]),
            TagSpec::new(["accepting", accepting]),
            TagSpec::new(["queue_depth", &queue_depth]),
            multi_value_tag(ACCEPTED_MINTS_TAG, &self.accepted_mints),
        ];
        tags.extend(agent_tags(&self.agents));
        EventDraft::new(SELLER_HEARTBEAT_KIND, tags, "")
    }
}

/// The roster advertisement tags, or empty for a seller that states no harness (the tags are then
/// omitted rather than emitted empty — absent means "unstated", never "none").
///
/// Emits `["agents", …]` AND, for one transition release, the pre-#645 `["mobee_agent", …]` with
/// the identical value list. Both emit sites use this, so a seat cannot advertise the roster under
/// one spelling and not the other. See [`LEGACY_AGENT_TAG`] for why and for the removal checklist.
pub fn agent_tags(agents: &[String]) -> Vec<TagSpec> {
    if agents.is_empty() {
        return Vec::new();
    }
    vec![
        multi_value_tag(AGENT_TAG, agents),
        multi_value_tag(LEGACY_AGENT_TAG, agents),
    ]
}

/// Read a roster advertisement off any event's tags. Absent ⇒ empty.
///
/// Prefers `["agents", …]`; falls back to the pre-#645 `["mobee_agent", …]` so this build can read
/// a seat that has not upgraded yet. The fallback is the read half of the same transition window —
/// it goes when [`LEGACY_AGENT_TAG`] goes.
pub fn agents_from_tags(tags: &[TagSpec]) -> Vec<String> {
    let agents = tag_values(tags, AGENT_TAG);
    if agents.is_empty() {
        return tag_values(tags, LEGACY_AGENT_TAG);
    }
    agents
}

/// Read the `["accepted_mints", …]` list off a seat announcement's tags. Absent ⇒ empty, which
/// [`parse_heartbeat`] rejects — §4.2 requires at least one mint.
pub fn accepted_mints_from_tags(tags: &[TagSpec]) -> Vec<String> {
    tag_values(tags, ACCEPTED_MINTS_TAG)
}

/// `["<name>", v0, v1, …]` — the multi-value tag convention both list tags use.
fn multi_value_tag(name: &str, values: &[String]) -> TagSpec {
    let mut tag = vec![name.to_owned()];
    tag.extend(values.iter().cloned());
    TagSpec(tag)
}

fn tag_values(tags: &[TagSpec], name: &str) -> Vec<String> {
    first_tag(tags, name)
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
    accepted_mints: Vec<String>,
    agents: Vec<String>,
) -> HeartbeatDraft {
    HeartbeatDraft::new(
        in_flight == 0 && anything_serving,
        in_flight,
        rate_sats,
        accepted_mints,
    )
    .with_agents(agents)
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
    /// Every mint this seat accepts payment on. Never empty — [`parse_heartbeat`] rejects a seat
    /// that states none.
    pub accepted_mints: Vec<String>,
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

/// Reasons a kind-30340 event fails to parse as a maxplayer seller heartbeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeartbeatParseError {
    WrongKind(u16),
    MissingMaxplayerTag,
    /// The `d` tag is absent or not `maxplayer-seller`.
    WrongDTag(Option<String>),
    /// The `v` tag is absent or names a protocol major this reader does not speak (§2.1).
    WrongVersion(Option<String>),
    MissingTag(&'static str),
    InvalidAccepting(String),
    InvalidQueueDepth(String),
    InvalidRate(String),
    /// The `accepted_mints` tag is absent or lists no mint. §4.2 requires at least one — a seat a
    /// buyer cannot pay is not a tradeable seat, so this is a rejection rather than an empty list.
    MissingAcceptedMints,
}

impl std::fmt::Display for HeartbeatParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongKind(kind) => {
                write!(f, "expected kind {SELLER_HEARTBEAT_KIND}, got {kind}")
            }
            Self::MissingMaxplayerTag => write!(f, "missing t={MAXPLAYER_TAG} tag"),
            Self::WrongDTag(d) => write!(
                f,
                "expected d={SELLER_HEARTBEAT_D}, got {}",
                d.as_deref().unwrap_or("<none>")
            ),
            Self::WrongVersion(version) => write!(
                f,
                "expected v={PROTOCOL_VERSION}, got {}",
                version.as_deref().unwrap_or("<none>")
            ),
            Self::MissingTag(name) => write!(f, "missing {name} tag"),
            Self::InvalidAccepting(value) => {
                write!(f, "accepting must be y/n, got {value}")
            }
            Self::InvalidQueueDepth(value) => write!(f, "invalid queue_depth: {value}"),
            Self::InvalidRate(value) => write!(f, "invalid rate: {value}"),
            Self::MissingAcceptedMints => write!(
                f,
                "missing {ACCEPTED_MINTS_TAG}: a seat must state at least one payable mint"
            ),
        }
    }
}

impl std::error::Error for HeartbeatParseError {}

/// Parse a kind-30340 event into a [`ParsedHeartbeat`] — the buyer-side seat reader. Rejects a
/// wrong kind, a missing `t=maxplayer` guard, a `d` other than `maxplayer-seller`, a `v` other than
/// the protocol major this build speaks, or a seat that states no payable mint.
///
/// This is the ONLY source of a seat's capability. Before #645 the mints and the harness label came
/// off the kind-31990 handler content, so a reader that still consulted 31990 would read residue —
/// a replaceable event a seat stopped republishing does not disappear from the relay.
pub fn parse_heartbeat(event: &EventDraft) -> Result<ParsedHeartbeat, HeartbeatParseError> {
    if event.kind != SELLER_HEARTBEAT_KIND {
        return Err(HeartbeatParseError::WrongKind(event.kind));
    }
    if !has_tag_value(&event.tags, "t", MAXPLAYER_TAG) {
        return Err(HeartbeatParseError::MissingMaxplayerTag);
    }
    let d = first_tag_value(&event.tags, "d");
    if d != Some(SELLER_HEARTBEAT_D) {
        return Err(HeartbeatParseError::WrongDTag(d.map(str::to_owned)));
    }
    let version = first_tag_value(&event.tags, "v");
    if version != Some(PROTOCOL_VERSION) {
        return Err(HeartbeatParseError::WrongVersion(version.map(str::to_owned)));
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

    let accepted_mints = accepted_mints_from_tags(&event.tags);
    if accepted_mints.is_empty() {
        return Err(HeartbeatParseError::MissingAcceptedMints);
    }

    Ok(ParsedHeartbeat {
        d: SELLER_HEARTBEAT_D.to_owned(),
        accepting,
        queue_depth,
        rate_sats,
        accepted_mints,
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

    /// The mints a test seat states. §4.2 makes the tag required, so every draft carries one.
    fn mints() -> Vec<String> {
        vec!["https://testnut.example/Bitcoin".to_owned()]
    }

    fn draft(accepting: bool, queue_depth: u32, rate_sats: u64) -> HeartbeatDraft {
        HeartbeatDraft::new(accepting, queue_depth, rate_sats, mints())
    }

    fn tag_names(event: &EventDraft) -> Vec<&str> {
        let mut names: Vec<&str> = event.tags.iter().filter_map(TagSpec::first).collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn heartbeat_addressable() {
        // Kind is in NIP-01's addressable range so the relay replaces it in place by (pubkey, d).
        assert!((30000..=39999).contains(&SELLER_HEARTBEAT_KIND));
        assert_eq!(SELLER_HEARTBEAT_KIND, 30340);

        // Keyed by (pubkey, d), never by event id.
        let parsed =
            parse_heartbeat(&draft(true, 0, 5).to_event_draft()).expect("parse own draft");
        let key = parsed.key("seller-pubkey-hex");
        assert_eq!(key.pubkey, "seller-pubkey-hex");
        assert_eq!(key.d, SELLER_HEARTBEAT_D);
        // The same author with the same d always resolves to one identity regardless of the
        // (superseded) event that carried it.
        assert_eq!(key, parsed.key("seller-pubkey-hex"));
    }

    /// RED-PROOF (#645): the announcement carries EXACTLY the §4.2 tag set — no more, no less,
    /// plus the ONE deliberate transition tag.
    ///
    /// Set equality, not a list of presence checks, because a presence check cannot fail on a tag
    /// that should have LEFT. `protocol_versions` and `mobee_agent` satisfied every presence
    /// assertion this file used to make, and they are precisely the two tags #645 removed.
    ///
    /// ⚠ `mobee_agent` is back ON PURPOSE and TEMPORARILY. Shipping #645's rename in one step made
    /// a harness-targeted claim hang forever against a buyer on the previous release — field-proven
    /// on the `ember` seat, see [`LEGACY_AGENT_TAG`]. It rides alongside `agents` for exactly one
    /// release. `protocol_versions` stays retired and is still asserted absent below.
    ///
    /// REMOVAL: when the transition window closes, drop `mobee_agent` from both expected sets here
    /// and move it back into the retired loop. That edit turning this test red is the point.
    #[test]
    fn the_announcement_carries_exactly_the_spec_4_2_tag_set() {
        // No roster stated: the roster tags are the optional ones (§4.2 cardinality 0..1).
        let bare = draft(true, 0, 7).to_event_draft();
        assert_eq!(
            tag_names(&bare),
            ["accepted_mints", "accepting", "d", "queue_depth", "rate", "t", "v"]
        );

        let with_roster = draft(true, 0, 7)
            .with_agents(vec!["claude".into()])
            .to_event_draft();
        assert_eq!(
            tag_names(&with_roster),
            [
                "accepted_mints",
                "accepting",
                "agents",
                "d",
                "mobee_agent",
                "queue_depth",
                "rate",
                "t",
                "v"
            ]
        );

        // Named individually so a revert says WHICH tag came back rather than only diffing a list.
        for retired in ["protocol_versions"] {
            assert!(
                with_roster.tags.iter().all(|tag| tag.first() != Some(retired)),
                "#645 retired {retired} from the seat announcement"
            );
        }

        // …and every tag carries the value §4.2 specifies.
        assert_eq!(with_roster.kind, SELLER_HEARTBEAT_KIND);
        assert_eq!(first_tag_value(&with_roster.tags, "d"), Some(SELLER_HEARTBEAT_D));
        assert_eq!(first_tag_value(&with_roster.tags, "t"), Some(MAXPLAYER_TAG));
        assert_eq!(first_tag_value(&with_roster.tags, "v"), Some(PROTOCOL_VERSION));
        assert_eq!(first_tag_value(&with_roster.tags, "rate"), Some("7"));
        assert_eq!(first_tag_value(&with_roster.tags, "accepting"), Some("y"));
        assert_eq!(first_tag_value(&with_roster.tags, "queue_depth"), Some("0"));
        assert_eq!(accepted_mints_from_tags(&with_roster.tags), mints());
        assert_eq!(agents_from_tags(&with_roster.tags), vec!["claude"]);
        assert!(bare.content.is_empty(), "capability rides tags, never content");
    }

    /// RED-PROOF (transition): a reader that knows ONLY the pre-#645 spelling still finds the
    /// roster on what this build emits. This is the half that was missing when #645 shipped, and
    /// its absence hung a real targeted job — the old buyer's award filter looked for
    /// `mobee_agent`, the new seat published only `agents`, and the claim sat pending with no
    /// error on either side.
    ///
    /// Asserted on BOTH emit sites, because a compat tag on one and not the other still hangs:
    /// the claim (§6.2) drives the award filter, the announcement (§4.2) drives discovery.
    #[test]
    fn a_transition_old_reader_finds_the_roster_on_both_emit_sites() {
        let roster = vec!["claude".to_owned(), "codex".to_owned()];

        // What an OLD reader does: look up the legacy key, verbatim.
        let old_reader = |tags: &[TagSpec]| -> Vec<String> {
            first_tag(tags, LEGACY_AGENT_TAG)
                .map(|tag| tag.0[1..].to_vec())
                .unwrap_or_default()
        };

        let announcement = draft(true, 0, 7).with_agents(roster.clone()).to_event_draft();
        assert_eq!(
            old_reader(&announcement.tags),
            roster,
            "a pre-#645 buyer must still resolve the roster off the kind-30340 announcement"
        );

        let claim = crate::gateway::claim_draft("offer", "buyerpk", "sellerpk", "creq", &roster);
        assert_eq!(
            old_reader(&claim.tags),
            roster,
            "a pre-#645 award filter must still resolve the roster off the kind-3402 claim"
        );

        // Both spellings carry the SAME list — a seat must not advertise two different rosters.
        assert_eq!(agents_from_tags(&announcement.tags), roster);
        assert_eq!(old_reader(&announcement.tags), agents_from_tags(&announcement.tags));
        assert_eq!(old_reader(&claim.tags), agents_from_tags(&claim.tags));
    }

    /// RED-PROOF (transition): the read half — this build resolves a roster from a seat that has
    /// NOT upgraded yet and publishes only the legacy spelling. Without this, the break is simply
    /// mirrored: new buyer, old seat, same silent hang.
    #[test]
    fn a_transition_new_reader_finds_the_roster_from_a_legacy_only_seat() {
        let legacy_only = vec![TagSpec::new(["mobee_agent", "claude", "codex"])];
        assert_eq!(agents_from_tags(&legacy_only), vec!["claude", "codex"]);

        // The new spelling still WINS when both are present (they always agree today, but the
        // fallback must never override a seat that has stated `agents` explicitly).
        let both = vec![
            TagSpec::new(["agents", "claude"]),
            TagSpec::new(["mobee_agent", "stale"]),
        ];
        assert_eq!(agents_from_tags(&both), vec!["claude"]);

        // Neither spelling ⇒ unstated, never a phantom roster.
        assert!(agents_from_tags(&[TagSpec::new(["d", "x"])]).is_empty());
    }

    /// RED-PROOF (#645): the buyer-side seat reader takes mints AND roster off the kind-30340
    /// announcement. Before #645 both lived in the kind-31990 handler content, which a seat no
    /// longer republishes — a reader still sourcing them there would read relay residue, because a
    /// replaceable event a seat stops publishing does not disappear.
    #[test]
    fn the_buyer_reader_resolves_mints_and_agents_from_the_announcement() {
        let announced = vec![
            "https://testnut.example/Bitcoin".to_owned(),
            "https://second.example/Bitcoin".to_owned(),
        ];
        let event = HeartbeatDraft::new(true, 0, 21, announced.clone())
            .with_agents(vec!["claude".into(), "codex".into()])
            .to_event_draft();

        let seat = parse_heartbeat(&event).expect("a buyer parses the seat announcement");
        // Order is preserved on both lists: entry 0 is the seat's own preference, and a reader
        // that reordered would pay on — or dispatch to — something the seat ranked lower.
        assert_eq!(seat.accepted_mints, announced);
        assert_eq!(seat.agents, vec!["claude", "codex"]);
        assert_eq!(seat.rate_sats, 21);
        assert!(seat.accepting);
        assert_eq!(seat.queue_depth, 0);
    }

    /// A seat that names no payable mint is REJECTED, never read as "pays on anything".
    #[test]
    fn a_seat_stating_no_mint_is_not_a_resolvable_seat() {
        let mut absent = draft(true, 0, 5).to_event_draft();
        absent.tags.retain(|tag| tag.first() != Some(ACCEPTED_MINTS_TAG));
        assert_eq!(
            parse_heartbeat(&absent),
            Err(HeartbeatParseError::MissingAcceptedMints)
        );

        // Present-but-valueless is the same rejection: the seat still named nothing payable.
        let mut valueless = draft(true, 0, 5).to_event_draft();
        for tag in valueless.tags.iter_mut() {
            if tag.first() == Some(ACCEPTED_MINTS_TAG) {
                tag.0.truncate(1);
            }
        }
        assert_eq!(
            parse_heartbeat(&valueless),
            Err(HeartbeatParseError::MissingAcceptedMints)
        );
    }

    #[test]
    fn advertises_every_harness_in_preference_order() {
        let draft = heartbeat_for_state(0, true, 5, mints(), vec!["claude".into(), "codex".into()])
            .to_event_draft();
        let tag = first_tag(&draft.tags, "agents").expect("agents tag");
        assert_eq!(tag.0, vec!["agents", "claude", "codex"]);
        // The reader gets the same ordered list back.
        let parsed = parse_heartbeat(&draft).expect("round-trip");
        assert_eq!(parsed.agents, vec!["claude", "codex"]);
    }

    #[test]
    fn a_seller_stating_no_harness_omits_the_roster_tag() {
        // A raw `agent_command` seller has no preset label, so it advertises no roster and the tag
        // is omitted rather than emitted empty. It IS serving (hence `true`), which is why an
        // unstated list must never read as dark.
        let stated_none = heartbeat_for_state(0, true, 5, mints(), Vec::new()).to_event_draft();
        assert_eq!(stated_none, draft(true, 0, 5).to_event_draft());
        assert!(
            first_tag(&stated_none.tags, AGENT_TAG).is_none(),
            "an unstated harness list must omit the tag, never emit it empty"
        );
        assert!(parse_heartbeat(&stated_none).expect("parse").agents.is_empty());
    }

    #[test]
    fn accepting_flips_with_in_flight_state() {
        let idle = heartbeat_for_state(0, true, 5, mints(), Vec::new());
        assert!(idle.accepting);
        assert_eq!(idle.queue_depth, 0);
        assert_eq!(
            first_tag_value(&idle.to_event_draft().tags, "accepting"),
            Some("y")
        );

        let busy = heartbeat_for_state(1, true, 5, mints(), Vec::new());
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
            let draft = heartbeat_for_state(in_flight, serving, 5, mints(), Vec::new()).to_event_draft();
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
            let draft = heartbeat_for_state(depth, true, 5, mints(), Vec::new()).to_event_draft();
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
        let free = heartbeat_for_state(0, true, 5, mints(), Vec::new()).to_event_draft();
        assert_eq!(first_tag_value(&free.tags, "accepting"), Some("y"));
        assert_eq!(first_tag_value(&free.tags, "queue_depth"), Some("0"));
    }

    #[test]
    fn reader_round_trip() {
        let announced = vec!["https://a.example/x".to_owned(), "https://b.example/y".to_owned()];
        let draft = HeartbeatDraft::new(false, 3, 21, announced.clone());
        let parsed = parse_heartbeat(&draft.to_event_draft()).expect("round-trip parse");
        assert_eq!(parsed.d, SELLER_HEARTBEAT_D);
        assert!(!parsed.accepting);
        assert_eq!(parsed.queue_depth, 3);
        assert_eq!(parsed.rate_sats, 21);
        assert_eq!(parsed.accepted_mints, announced);
    }

    #[test]
    fn parse_rejects_wrong_kind_and_missing_guards() {
        let mut wrong_kind = draft(true, 0, 5).to_event_draft();
        wrong_kind.kind = 30341;
        assert_eq!(
            parse_heartbeat(&wrong_kind),
            Err(HeartbeatParseError::WrongKind(30341))
        );

        // Drop the t=maxplayer guard.
        let mut no_maxplayer = draft(true, 0, 5).to_event_draft();
        no_maxplayer.tags.retain(|tag| tag.first() != Some("t"));
        assert_eq!(
            parse_heartbeat(&no_maxplayer),
            Err(HeartbeatParseError::MissingMaxplayerTag)
        );

        // Wrong d.
        let mut wrong_d = draft(true, 0, 5).to_event_draft();
        for tag in wrong_d.tags.iter_mut() {
            if tag.first() == Some("d") {
                tag.0[1] = "not-maxplayer-seller".to_owned();
            }
        }
        assert_eq!(
            parse_heartbeat(&wrong_d),
            Err(HeartbeatParseError::WrongDTag(Some(
                "not-maxplayer-seller".to_owned()
            )))
        );

        // A foreign protocol major, and an announcement with no `v` at all. #645 put the tag on
        // this event; gating on it here is what stops it from being decoration (§2.1).
        let mut wrong_version = draft(true, 0, 5).to_event_draft();
        for tag in wrong_version.tags.iter_mut() {
            if tag.first() == Some("v") {
                tag.0[1] = "2".to_owned();
            }
        }
        assert_eq!(
            parse_heartbeat(&wrong_version),
            Err(HeartbeatParseError::WrongVersion(Some("2".to_owned())))
        );

        let mut no_version = draft(true, 0, 5).to_event_draft();
        no_version.tags.retain(|tag| tag.first() != Some("v"));
        assert_eq!(
            parse_heartbeat(&no_version),
            Err(HeartbeatParseError::WrongVersion(None))
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
