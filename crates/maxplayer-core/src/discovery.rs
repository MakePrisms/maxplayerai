//! Buyer-side **specialist discovery**: read the public seat directory off kind-30340
//! announcements so a buyer can find a seat it has never met, read what that seat says it is for,
//! and then target it with the posting path that already exists.
//!
//! ## What this is, and the three things it is NOT
//!
//! It is ONE read. A buyer asks the relay for seat announcements, this module reduces them to the
//! latest live beat per seat, and the caller gets rows to look at. Nothing here posts, awards, pays,
//! or reserves — see [`SellerDirectory`] and the `discovery_never_writes` test for the pinned form
//! of that claim.
//!
//! - **NOT automatic matching.** There is no scoring, no ranking, no keyword predicate. A buyer (or
//!   its agent, or its human) reads [`DiscoveredSeller::specialty`] and decides. The rows come back
//!   in a deliberately merit-free order — see [`SellerDirectory::sellers`].
//! - **NOT a competence assertion.** `specialty` is text the seat's operator typed. It is unverified
//!   by construction and rides the announcement alone, never a claim; see
//!   [`crate::heartbeat::SPECIALTY_TAG`] for why that placement is load-bearing rather than
//!   incidental.
//! - **NOT a capacity or eligibility signal.** ⛔ A FRESH BEAT PROVES NEITHER. `accepting=y` says the
//!   seat is alive and serving, not that it has a free execution slot (`slots` defaults to 3 and the
//!   gate is `SlotGate::try_reserve` at claim time), and the admission fields say who the seat
//!   *advertises* it admits, which is intent and not a guarantee. The authoritative signal that a
//!   seat will take a job remains that the seat CLAIMS one. A buyer that treats a row here as
//!   "will serve me" has read it wrong.
//!
//! ## Shape: a pure reducer plus a thin transport
//!
//! [`reduce_directory`] holds every rule — recency, future-dating, retraction, latest-per-address —
//! and touches no relay, so all of it is testable offline against drafts built by the same
//! [`crate::heartbeat`] emitters a real seat publishes through. [`fetch_directory_async`] is the
//! only part that needs a socket. The split is why the acceptance tests need no live relay and no
//! sats.

use std::collections::HashMap;

use crate::gateway::EventDraft;
use crate::heartbeat::{HeartbeatKey, ParsedHeartbeat, parse_heartbeat};

/// Wire spelling for an admission field the seat did NOT state.
///
/// ⛔ **UNSTATED IS NOT `closed`.** A seat published before the §4.2 admission tags existed states
/// neither half, and rendering that as "closed" would tell a buyer that every seat running today
/// refuses it. It is a third value because it is a third fact: the seat did not say.
pub const ADMISSION_UNSTATED: &str = "unstated";

/// How old a beat may be and still count as a LIVE seat, in seconds.
///
/// Derived from the shipped cadence rather than picked: [`crate::home`]'s heartbeat defaults are a
/// 300 s interval and 3 missed intervals before the seat itself calls a publish stalled, so 900 s is
/// the same patience the seller side already applies to its own beat. A buyer using a different
/// window passes its own through [`DirectoryPolicy::max_age_secs`].
///
/// ⚠ THE WINDOW IS THE ONLY COVER FOR AN UNGRACEFUL EXIT, and that is why it exists at all.
/// kind-30340 is addressable: the relay holds exactly one announcement per `(pubkey, d)`, and a seat
/// killed by SIGKILL, an OOM or a power cut leaves its last `accepting=y` standing as its permanent
/// public answer with no later event to correct it. Waiting produces nothing. Recency filtering is
/// therefore REQUIRED of every consumer and is not a tuning nicety — see
/// [`crate::heartbeat::retraction_for_state`], which covers the graceful case and explicitly does
/// not cover this one.
pub const DEFAULT_MAX_AGE_SECS: u64 = 900;

/// How far into the future a beat's `created_at` may sit before it is discarded, in seconds.
///
/// Relay and seat clocks disagree by seconds in normal operation, so a small tolerance keeps honest
/// seats visible. Beyond it the timestamp is not usable: an addressable event is superseded by
/// `created_at` order, so a far-future beat would outrank every genuine later one and pin a stale
/// row in place until real time caught up. Discarding it costs one seat's visibility; keeping it
/// costs the correctness of the whole ordering rule.
pub const DEFAULT_MAX_CLOCK_SKEW_SECS: u64 = 300;

/// Default cap on how many announcements one directory read asks the relay for.
pub const DEFAULT_DIRECTORY_LIMIT: usize = 500;

/// How long one directory read waits on the relay, in seconds.
///
/// Finite by construction — there is deliberately no unbounded variant, because an MCP tool that
/// never returns is indistinguishable to its caller from a hung buyer. Declared here rather than in
/// the relay leg so a build with no relay features can still state the tool's contract.
pub const DEFAULT_DISCOVERY_TIMEOUT_SECS: u64 = 8;

/// The largest directory-read budget a caller may ask for, in seconds.
///
/// ⚠ THE CEILING EXISTS BECAUSE THE CALLER THAT MATTERS HAS ITS OWN DEADLINE. `maxplayer mcp`
/// caps a `tools/call` at 15 s (`mcp::TOOL_DEADLINE_SECS`) and reports a cap-hit as a tool error
/// with no directory in it, so a budget at or above that ceiling converts every slow relay into
/// "the tool broke" instead of the honest "the relay did not answer" this module goes to some
/// trouble to be able to say. 10 s leaves the daemon room to reply inside the client's window.
///
/// It is REFUSED at the RPC boundary rather than silently clamped, the same choice
/// [`crate::job_lifecycle::WAIT_FOR_CAP_SECS`] makes for the long poll: a caller that asked for
/// 60 s and got 10 would read the empty answer as sixty seconds of evidence.
pub const MAX_DISCOVERY_TIMEOUT_SECS: u64 = 10;

/// One seat as the public directory describes it, at the moment of the read.
///
/// Every field comes off ONE announcement — the latest live beat for this seat's `(pubkey, d)`
/// address — so the row is internally consistent rather than assembled from several events.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DiscoveredSeller {
    /// The seat's pubkey, 64-hex lowercase. **This is the discovery output that matters**: it is the
    /// value a buyer hands to the existing targeted-post parameter (`seller_pubkey`) to hire this
    /// seat. Nothing else here is an identifier.
    pub pubkey: String,
    /// What the seat says it specialises in, or `None` when it stated nothing.
    ///
    /// ⛔ SELLER-DECLARED, NEVER VERIFIED. Read it as an advertisement, not a credential. `None` is
    /// unstated and NOT a claim to be a generalist — a seat published before the field existed and a
    /// seat whose operator declined to describe it are the same value here, and both stay listed.
    pub specialty: Option<String>,
    /// The announcement's `created_at` (unix seconds), as the relay served it.
    pub announced_at: u64,
    /// How long ago that was, at the moment of this read. Carried alongside the timestamp rather
    /// than left to the caller so two callers cannot compute "age" against two different clocks.
    pub age_secs: u64,
    /// The seat's advertised rate floor in sats — the LOWEST it accepts, per §4.2, not a quote.
    pub rate_sats: u64,
    /// The seat states it takes NO payment at all (§4.1). ⚠ Do not substitute `rate_sats == 0`:
    /// that means "any amount ≥ 0", which a buyer holding zero sats cannot act on.
    pub takes_no_payment: bool,
    /// Every mint this seat can be paid on. Never empty — a seat naming none does not parse.
    pub accepted_mints: Vec<String>,
    /// The harnesses the seat advertises, in its preference order. Empty ⇒ stated none, which is not
    /// a claim that it can run nothing (the unlabelled `--agent-argv` hatch has no name to publish).
    pub agents: Vec<String>,
    /// The enum-bound harness families the seat serves. Empty ⇒ unstated.
    pub harness_families: Vec<String>,
    /// Untargeted (open-pool) admission: `open`, `closed`, or [`ADMISSION_UNSTATED`].
    pub admits_pool: String,
    /// Targeted admission: `open`, `named`, `closed`, or [`ADMISSION_UNSTATED`].
    ///
    /// `named` discloses only that an allowlist EXISTS, never who is on it — so a buyer reading it
    /// learns that targeting this seat may be refused, which is exactly the fact a boolean would
    /// have hidden.
    pub admits_targeted: String,
}

/// Why a returned announcement did not become a row. Counts only — a directory read is a diagnostic
/// surface, and naming the pubkeys it dropped would publish a list of dead seats to no purpose.
///
/// It exists so an empty directory can be EXPLAINED. "The relay answered and held 40 beats, all of
/// them stale" and "the relay answered and held nothing" are different facts about the market, and
/// without this they are the same empty list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct DirectorySkips {
    /// Events that are not parseable maxplayer seat announcements at all (wrong kind, missing the
    /// `t=maxplayer` guard, a protocol major this build does not speak, no payable mint).
    pub unparseable: u32,
    /// Seats whose latest beat is older than [`DirectoryPolicy::max_age_secs`].
    pub stale: u32,
    /// Seats whose latest beat is dated further ahead than
    /// [`DirectoryPolicy::max_clock_skew_secs`].
    pub future_dated: u32,
    /// Seats whose latest beat says `accepting=n` — the seat has left the market or is closed. This
    /// is the retraction being HONOURED: the terminal beat superseded the seat's old `accepting=y`
    /// at the same address, and this read resolves the address, so the newer word wins.
    pub retracted: u32,
}

/// The rules one directory read applies. Taken as a value rather than read from globals so every
/// rule is exercisable offline with a fixed clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryPolicy {
    /// The reader's "now", unix seconds. Supplied by the caller so a test can pin it.
    pub now_unix: u64,
    /// Recency window — see [`DEFAULT_MAX_AGE_SECS`].
    pub max_age_secs: u64,
    /// Future-dating tolerance — see [`DEFAULT_MAX_CLOCK_SKEW_SECS`].
    pub max_clock_skew_secs: u64,
}

impl DirectoryPolicy {
    /// The shipped rules against a caller-supplied clock.
    pub fn at(now_unix: u64) -> Self {
        Self {
            now_unix,
            max_age_secs: DEFAULT_MAX_AGE_SECS,
            max_clock_skew_secs: DEFAULT_MAX_CLOCK_SKEW_SECS,
        }
    }
}

/// The result of one directory read.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SellerDirectory {
    /// The live seats, ordered by `pubkey` ascending.
    ///
    /// ⛔ **THE ORDER IS DELIBERATELY MERIT-FREE, AND SORTING BY FRESHNESS WOULD NOT BE.** Any order
    /// this function chooses is the order a caller reads first, so freshness-descending would ship a
    /// ranking policy — "the seat that beat most recently is the best one to hire" — which is a
    /// claim discovery has no standing to make and which rewards beating often. Sorting on the
    /// pubkey is stable, total, and says nothing. `announced_at`/`age_secs` are on every row for a
    /// caller that wants to order by them and owns that decision.
    pub sellers: Vec<DiscoveredSeller>,
    /// **Whether the relay ANSWERED this read.** `fetch_events` resolves `Ok(empty)` on timeout, so
    /// an empty `sellers` cannot by itself tell "no specialists are advertising" from "we stopped
    /// waiting" — the same bytes, and the discriminator has to be asked for. `true` ⇒ the emptiness
    /// is a fact about the market. `false` ⇒ it is a fact about our patience.
    ///
    /// It is the same discipline [`crate::job_lifecycle::JobView::read_confirmed`] applies to offer
    /// reads (#291/#322), and it is `false` on any directory not built from a confirmed read, so the
    /// misleading direction is the one a caller has to opt into. A hard relay failure is an
    /// [`DiscoveryError::Relay`] instead — that is a THIRD outcome, not this flag.
    pub read_confirmed: bool,
    /// What the read saw and dropped. See [`DirectorySkips`].
    pub skipped: DirectorySkips,
    /// How many announcements the relay returned, before any rule was applied. The denominator for
    /// everything above.
    pub events_read: u32,
}

impl SellerDirectory {
    /// An answered read of a market with nothing in it.
    ///
    /// Public rather than crate-private because the DISTINCTION it makes with [`Self::unverified`]
    /// is the module's contract, not an implementation detail: a caller assembling a directory from
    /// its own transport has to be able to state which of the two it got, and a private
    /// constructor would leave it building the struct field-by-field and choosing
    /// `read_confirmed` by hand.
    pub fn empty_confirmed() -> Self {
        Self {
            sellers: Vec::new(),
            read_confirmed: true,
            skipped: DirectorySkips::default(),
            events_read: 0,
        }
    }

    /// A read the relay never answered. Empty AND unconfirmed — see [`Self::read_confirmed`].
    pub fn unverified() -> Self {
        Self {
            sellers: Vec::new(),
            read_confirmed: false,
            skipped: DirectorySkips::default(),
            events_read: 0,
        }
    }
}

/// One announcement as the relay served it: the author, the timestamp, and the event's tag content.
///
/// A plain struct rather than a `nostr_sdk::Event` so [`reduce_directory`] compiles and tests
/// without the gateway feature, and so a test can build one from
/// [`crate::heartbeat::HeartbeatDraft::to_event_draft`] — the very emitter a real seat publishes
/// through, rather than a hand-written tag set made to agree with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnouncedSeat {
    /// The event's author pubkey, 64-hex.
    pub author_pubkey: String,
    /// The event's `created_at`, unix seconds.
    pub created_at: u64,
    /// The event's SIGNED id, 64-hex lowercase — the NIP-01 tie-breaker, carried across the
    /// transport-to-reducer seam because `(author, created_at)` is NOT a total order. Two signed
    /// beats for one address CAN share a timestamp, and if they disagree about `accepting` then
    /// whichever the reducer keeps decides whether the seat is live or retracted. Without the id
    /// that decision falls to iteration luck.
    pub event_id: String,
    /// The event's kind/tags/content.
    pub event: EventDraft,
}

/// Whether the relay actually FINISHED answering the directory request.
///
/// This is the whole of [`SellerDirectory::read_confirmed`], and it is a transport fact the reducer
/// cannot derive: rows alone cannot tell a completed answer from a stream that stopped early.
/// `fetch_events`-style helpers end on either an `EOSE` or a spent deadline WITHOUT distinguishing
/// them, so a caller that infers completion from "we got here with some events" certifies a partial
/// read. Only the directory REQ's own `EOSE` may set [`Self::ConfirmedByEose`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadCompletion {
    /// The directory subscription's own `EOSE` arrived: the relay served everything it holds for
    /// this filter, so an empty row set is a genuinely empty market.
    ConfirmedByEose,
    /// The read ended without its `EOSE` — deadline spent, socket dropped, or stream closed. Rows
    /// already received are kept and reported HONESTLY as unconfirmed; absence proves nothing.
    Unconfirmed,
}

impl ReadCompletion {
    /// `true` only for [`Self::ConfirmedByEose`]. Written once, here, so no call site can decide
    /// what "confirmed" means for itself.
    pub fn is_confirmed(self) -> bool {
        matches!(self, Self::ConfirmedByEose)
    }
}

/// Reduce raw announcements to the live seat directory — **every discovery rule, and no I/O**.
///
/// In order:
///
/// 1. **Parse.** Anything [`parse_heartbeat`] refuses is counted and dropped. A junk event squatting
///    the kind must not become a row a buyer might target.
/// 2. **Resolve the address.** Rows are keyed by `(pubkey, d)` via [`ParsedHeartbeat::key`], NEVER
///    by event id: kind-30340 is addressable and superseded IN PLACE, so an id-keyed reduce would
///    keep a seat's dead announcements alongside its live one. Newest `created_at` wins; an exact
///    timestamp tie is broken by the LOWEST lexical event id, per NIP-01's retention rule for
///    addressable events. Both together are a total order over signed input, so the result does not
///    depend on the order the relay handed the events over.
/// 3. **Discard a future-dated beat**, past the skew tolerance — it would outrank every genuine
///    later beat for this address.
/// 4. **Discard a stale beat**, past the recency window.
/// 5. **Honour a retraction.** A resolved beat with `accepting=n` is the seat's own last word that
///    it is not taking work; it leaves the directory.
///
/// Steps 3–5 apply to the RESOLVED beat, after step 2, and that ordering is the point: a seat's
/// newer retraction must not be filtered out on its own merits and leave the seat's older
/// `accepting=y` standing as the survivor. Resolve the address first, judge the winner second.
pub fn reduce_directory(
    announcements: impl IntoIterator<Item = AnnouncedSeat>,
    policy: DirectoryPolicy,
    completion: ReadCompletion,
) -> SellerDirectory {
    let mut skipped = DirectorySkips::default();
    let mut events_read: u32 = 0;
    // (pubkey, d) -> the winning beat for that address so far: its timestamp, its signed id (the
    // tie-breaker), and the parse.
    let mut newest: HashMap<HeartbeatKey, (u64, String, ParsedHeartbeat)> = HashMap::new();

    for announcement in announcements {
        events_read = events_read.saturating_add(1);
        let Ok(parsed) = parse_heartbeat(&announcement.event) else {
            skipped.unparseable = skipped.unparseable.saturating_add(1);
            continue;
        };
        let pubkey = announcement.author_pubkey.to_ascii_lowercase();
        let key = parsed.key(&pubkey);
        let created = announcement.created_at;
        let id = announcement.event_id.to_ascii_lowercase();
        // NEWER WINS, and on an exact tie the LOWER id wins — NIP-01's own rule for retaining an
        // addressable event. A conforming relay may never serve the conflicting pair, but this
        // reducer is reusable and is handed whatever arrives: two same-address beats sharing a
        // timestamp and disagreeing about `accepting` must resolve the SAME way whichever order
        // they come in, or a seat is live or retracted by luck.
        let supersedes = match newest.get(&key) {
            None => true,
            Some((previous_created, previous_id, _)) => match created.cmp(previous_created) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => id < *previous_id,
            },
        };
        if supersedes {
            newest.insert(key, (created, id, parsed));
        }
    }

    let mut sellers: Vec<DiscoveredSeller> = Vec::with_capacity(newest.len());
    for (key, (created_at, _id, parsed)) in newest {
        if created_at > policy.now_unix.saturating_add(policy.max_clock_skew_secs) {
            skipped.future_dated = skipped.future_dated.saturating_add(1);
            continue;
        }
        // Saturating, so a beat inside the skew tolerance but still ahead of our clock reads as age
        // zero rather than wrapping to a colossal age and being called stale.
        let age_secs = policy.now_unix.saturating_sub(created_at);
        if age_secs > policy.max_age_secs {
            skipped.stale = skipped.stale.saturating_add(1);
            continue;
        }
        if !parsed.accepting {
            skipped.retracted = skipped.retracted.saturating_add(1);
            continue;
        }
        sellers.push(row(key.pubkey, created_at, age_secs, parsed));
    }
    sellers.sort_by(|left, right| left.pubkey.cmp(&right.pubkey));

    SellerDirectory {
        sellers,
        // The TRANSPORT's word, never this function's guess. A reducer handed a partial stream
        // sees perfectly well-formed rows and has no way to know the relay stopped early.
        read_confirmed: completion.is_confirmed(),
        skipped,
        events_read,
    }
}

/// Project one resolved beat into a directory row. Reads the capability off the ALREADY-PARSED
/// [`ParsedHeartbeat`] rather than re-reading tags, so this shares the one reader
/// ([`crate::heartbeat::SeatCapability::from_tags`]) with the claim path and cannot spell a field
/// differently from it.
fn row(
    pubkey: String,
    announced_at: u64,
    age_secs: u64,
    parsed: ParsedHeartbeat,
) -> DiscoveredSeller {
    let (admits_pool, admits_targeted) = match parsed.admission {
        Some(admission) => (
            if admission.pool {
                crate::home::ADMISSION_OPEN.to_owned()
            } else {
                crate::home::ADMISSION_CLOSED.to_owned()
            },
            admission.targeted.as_str().to_owned(),
        ),
        // Unstated on BOTH halves, together: a seat that predates the tags published neither, and
        // guessing one of them would be inventing a policy the seat never advertised.
        None => (ADMISSION_UNSTATED.to_owned(), ADMISSION_UNSTATED.to_owned()),
    };
    DiscoveredSeller {
        pubkey,
        specialty: parsed.capability.specialty,
        announced_at,
        age_secs,
        rate_sats: parsed.rate_sats,
        takes_no_payment: parsed.takes_no_payment,
        accepted_mints: parsed.accepted_mints,
        agents: parsed.agents,
        harness_families: parsed.capability.harness_families,
        admits_pool,
        admits_targeted,
    }
}

/// Why a directory read could not be performed at all.
///
/// ⚠ **A RELAY FAILURE IS NOT AN EMPTY MARKET, AND THAT IS THIS TYPE'S ONLY JOB.** Collapsing the
/// two would tell a buyer "no specialists are advertising" whenever its own network is down — the
/// single most misleading answer discovery can give, because it looks exactly like a true one. The
/// three outcomes are: `Err(_)` (the read failed), `Ok` with `read_confirmed == false` (the relay
/// did not answer in time), and `Ok` with `read_confirmed == true` (whatever `sellers` holds,
/// including nothing, is what the market has).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryError {
    /// The relay could not be added, reached, or served the read.
    Relay(String),
    /// The home has no usable identity to read with.
    Identity(String),
    /// Called from inside a Tokio runtime through the sync entry point.
    Runtime(String),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Relay(detail) => write!(f, "relay: {detail}"),
            Self::Identity(detail) => write!(f, "identity: {detail}"),
            Self::Runtime(detail) => write!(f, "runtime: {detail}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

#[cfg(all(feature = "wallet", feature = "gateway"))]
pub use transport::{fetch_directory, fetch_directory_async};

/// The relay leg. Gated with `job_lifecycle`/`profile` because it needs the buyer identity and the
/// relay client; every RULE lives in [`reduce_directory`], which is ungated.
#[cfg(all(feature = "wallet", feature = "gateway"))]
mod transport {
    use super::{
        AnnouncedSeat, DEFAULT_DIRECTORY_LIMIT, DirectoryPolicy, DiscoveryError, ReadCompletion,
        SellerDirectory, reduce_directory,
    };
    use crate::gateway::{EventDraft, TagSpec};
    use crate::home::MaxplayerHome;
    use std::time::{Duration, Instant};

    /// Our own REQ's subscription id. The completion evidence is scoped to THIS id: an `EOSE` for
    /// anything else — a liveness probe, the buyer's job subscription, another read sharing the
    /// socket — says nothing about whether the directory request finished.
    const DIRECTORY_SUB_ID: &str = "maxplayer-discovery-directory";

    /// Ceiling on the connect leg alone. `connect()` only SPAWNS the connection, so a fetch racing
    /// the handshake burns its whole window and comes back empty — which this module would then
    /// have to report as an unconfirmed read of an apparently dead market. It is a CEILING, not a
    /// floor: the wait is whatever is left of the caller's budget, capped here.
    const RELAY_CONNECT_WAIT: Duration = Duration::from_secs(20);

    /// What is left of the caller's budget. Zero once it is spent — never a negative wrap.
    fn remaining(deadline: Instant) -> Duration {
        deadline.saturating_duration_since(Instant::now())
    }

    /// Read the live seat directory off the home's relay. **Read-only**: it subscribes and reads,
    /// and publishes no event of any kind.
    ///
    /// Completion is read off OUR OWN REQ and nothing else. The subscription is opened with a known
    /// id, the notification receiver is established BEFORE the REQ goes out (an `EOSE` that lands
    /// first would otherwise be missed), and only that id's `EOSE` sets
    /// [`ReadCompletion::ConfirmedByEose`]. A deadline that expires, a socket that drops, or a
    /// stream that ends leaves the read UNCONFIRMED however many rows arrived — a partial answer is
    /// still returned, honestly flagged, because rows already in hand are useful and pretending they
    /// are the whole market is not.
    ///
    /// This is deliberately NOT `fetch_events`. That helper ends on either an `EOSE` or a spent
    /// timeout and returns the same `Ok(events)` for both (`job_lifecycle.rs:202` documents the
    /// same trap), so no caller of it can honestly certify completion. Nor can a preceding liveness
    /// probe stand in: a probe is a DIFFERENT subscription with a different filter, and its `EOSE`
    /// is evidence about the probe.
    ///
    /// A relay that REJECTS our NIP-42 authentication is an error, not silence. That rejection
    /// arrives on this relay's own notification channel and is never forwarded to the pool's
    /// (`relay/inner.rs:417-419`), and a refused relay owes the subscription neither an `EOSE` nor
    /// a `CLOSED` — so a reader watching only the pool waits out its deadline and reports an
    /// unanswered read, throwing away a rejection that has a reason and a fix.
    ///
    /// The REQ goes out through the SINGLE RELAY, whose result is `Result<(), Error>`. The
    /// pool-level call returns `Result<Output<()>>` and folds per-relay send failures into
    /// `output.failed`, returning `Ok(output)` even when nothing succeeded (`pool/mod.rs:955-973`),
    /// so its outer `Err` alone cannot tell a sent REQ from one that reached nobody.
    ///
    /// `budget` bounds the read loop rather than each leg, because a caller (an MCP tool with a
    /// client read-timeout) can only honour a promise about the total. Running out mid-read yields
    /// an UNCONFIRMED directory — an unanswered read, which is what it is — never an empty market
    /// and never an error. Connect and cleanup are bounded separately and are not inside that one
    /// timeout, so this is not a strict wall-clock cap on the whole call.
    pub async fn fetch_directory_async(
        home: &MaxplayerHome,
        policy: DirectoryPolicy,
        limit: usize,
        budget: Duration,
    ) -> Result<SellerDirectory, DiscoveryError> {
        use nostr_sdk::RelayMessage;
        use nostr_sdk::pool::relay::RelayNotification;
        use nostr_sdk::prelude::{Client, Filter, Kind, SubscribeOptions, SubscriptionId};

        let deadline = Instant::now() + budget;
        let secret = crate::home::read_secret_key_hex(home)
            .map_err(|error| DiscoveryError::Identity(error.to_string()))?;
        let keys = nostr_sdk::Keys::parse(&secret)
            .map_err(|error| DiscoveryError::Identity(format!("key parse: {error}")))?;

        let client = Client::new(keys.clone());
        // Same discipline as every other read on this relay: auto-auth on, and WAIT for the socket.
        client.automatic_authentication(true);
        client
            .add_relay(&home.config.relay_url)
            .await
            .map_err(|error| DiscoveryError::Relay(format!("add relay: {error}")))?;
        let relay = client
            .relay(&home.config.relay_url)
            .await
            .map_err(|error| DiscoveryError::Relay(format!("relay handle: {error}")))?;

        // THIS RELAY'S OWN notifications, and opened BEFORE `connect()` — before any authentication
        // activity exists to observe. Two reasons, both load-bearing:
        //
        // 1. `AuthenticationFailed` is emitted on the RELAY's channel and is NOT forwarded to the
        //    pool's (`relay/inner.rs:417-419`). A reader watching only pool notifications sees a
        //    relay that rejected its AUTH as a relay that simply never answered: the rejection has a
        //    reason and a fix, and reporting it as an unanswered read throws both away.
        // 2. The challenge and the negative OK can both land during `connect()`. A receiver opened
        //    afterwards would miss them, which is the same ordering trap the EOSE receiver avoids.
        let mut notifications = relay.notifications();

        client.connect().await;
        relay
            .wait_for_connection(remaining(deadline).min(RELAY_CONNECT_WAIT))
            .await;

        // Scoped to the seat address: the kind, the `#t=maxplayer` namespace guard so a foreign
        // event squatting the kind is never delivered, and the `d` identifier so only seat
        // announcements match.
        let filter = Filter::new()
            .kind(Kind::Custom(crate::heartbeat::SELLER_HEARTBEAT_KIND))
            .hashtag(crate::gateway::MAXPLAYER_TAG)
            .identifier(crate::heartbeat::SELLER_HEARTBEAT_D)
            .limit(if limit == 0 {
                DEFAULT_DIRECTORY_LIMIT
            } else {
                limit
            });

        let sub_id = SubscriptionId::new(DIRECTORY_SUB_ID);

        // Subscribed through the SINGLE RELAY, whose result is `Result<(), Error>` — the failure of
        // THIS relay's REQ, propagated. `Client::subscribe_with_id` returns `Result<Output<()>>`,
        // and the pool collects per-relay failures into `output.failed` and returns `Ok(output)`
        // even when NOTHING succeeded (`pool/mod.rs:955-973`); checking only the outer `Err` there
        // reads a REQ that reached nobody as a REQ that was sent. One relay is the whole market
        // here, so its disposition is the read's disposition.
        if let Err(error) = relay
            .subscribe_with_id(sub_id.clone(), filter, SubscribeOptions::default())
            .await
        {
            client.disconnect().await;
            return Err(DiscoveryError::Relay(format!(
                "subscribe seat directory: {error}"
            )));
        }

        let mut events: Vec<nostr_sdk::Event> = Vec::new();
        let mut refusal: Option<String> = None;
        // A relay that rejected our AUTH. Distinct from `refusal` because the two are different
        // facts with different fixes — "this relay will not serve you" versus "this relay will not
        // serve this subscription" — and a reader that collapses them cannot tell an identity
        // problem from a policy one.
        let mut auth_failed = false;
        // Pessimistic until this subscription's own EOSE says otherwise. Every early exit below
        // leaves it as it is, so "we fell out of the loop somehow" can only ever mean unconfirmed.
        let mut completion = ReadCompletion::Unconfirmed;

        let _ = tokio::time::timeout(remaining(deadline), async {
            loop {
                match notifications.recv().await {
                    Ok(RelayNotification::Event {
                        subscription_id,
                        event,
                    }) if subscription_id == sub_id => events.push((*event).clone()),
                    Ok(RelayNotification::Message {
                        message: RelayMessage::EndOfStoredEvents(id),
                    }) if *id == sub_id => {
                        completion = ReadCompletion::ConfirmedByEose;
                        return;
                    }
                    // A CLOSED naming our subscription is the relay REFUSING this read — a policy
                    // rejection with a reason. Reporting it as "no sellers" would be the worst
                    // possible lie about it.
                    Ok(RelayNotification::Message {
                        message:
                            RelayMessage::Closed {
                                subscription_id,
                                message,
                            },
                    }) if *subscription_id == sub_id => {
                        refusal = Some(message.to_string());
                        return;
                    }
                    // The relay rejected the AUTH we signed for it. Nothing else is coming: a relay
                    // that refuses the identity need send neither EOSE nor CLOSED, and waiting out
                    // the deadline would convert an explicit, actionable rejection into an
                    // unanswered read — the exact downgrade this arm exists to stop.
                    Ok(RelayNotification::AuthenticationFailed) => {
                        auth_failed = true;
                        return;
                    }
                    Ok(RelayNotification::Shutdown) => return,
                    Ok(_) => continue,
                    // The notification stream ending is a lost socket, never a finished read.
                    Err(_) => return,
                }
            }
        })
        .await;

        // Cleanup on EVERY path, including the timeout: drop our REQ before the socket goes, so a
        // relay is not left streaming into a subscription nobody is reading.
        client.unsubscribe(&sub_id).await;
        client.disconnect().await;

        if auth_failed {
            return Err(DiscoveryError::Relay(format!(
                "relay {} rejected our authentication; the seat directory was not read",
                home.config.relay_url
            )));
        }

        if let Some(reason) = refusal {
            return Err(DiscoveryError::Relay(format!(
                "relay refused the seat-directory subscription: {reason}"
            )));
        }

        let announcements = events.into_iter().map(|event| AnnouncedSeat {
            author_pubkey: event.pubkey.to_hex().to_ascii_lowercase(),
            created_at: event.created_at.as_secs(),
            event_id: event.id.to_hex().to_ascii_lowercase(),
            event: EventDraft::new(
                u16::try_from(event.kind.as_u16()).unwrap_or(event.kind.as_u16()),
                event
                    .tags
                    .iter()
                    .map(|tag| TagSpec(tag.clone().to_vec()))
                    .collect(),
                event.content.clone(),
            ),
        });
        Ok(reduce_directory(announcements, policy, completion))
    }

    /// Sync entry point for callers not already on a runtime. `budget` bounds the whole read, as in
    /// [`fetch_directory_async`].
    pub fn fetch_directory(
        home: &MaxplayerHome,
        policy: DirectoryPolicy,
        limit: usize,
        budget: Duration,
    ) -> Result<SellerDirectory, DiscoveryError> {
        crate::runtime_guard::refuse_nested_block_on("discovery::fetch_directory")
            .map_err(DiscoveryError::Runtime)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| DiscoveryError::Runtime(error.to_string()))?;
        runtime.block_on(fetch_directory_async(home, policy, limit, budget))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heartbeat::{
        HeartbeatDraft, SeatCapability, heartbeat_for_state, retraction_for_state,
    };
    use crate::home::{AdmissionPolicy, TargetedAdmission};

    const NOW: u64 = 1_800_000_000;
    const MINT: &str = "https://mint.example/Bitcoin";
    const SEAT_A: &str = "aa11111111111111111111111111111111111111111111111111111111111111";
    const SEAT_B: &str = "bb22222222222222222222222222222222222222222222222222222222222222";

    fn open_policy() -> AdmissionPolicy {
        AdmissionPolicy {
            pool: true,
            targeted: TargetedAdmission::Open,
        }
    }

    /// A beat built through the PRODUCTION emitter, so a discovery test can never pass against a
    /// tag set hand-written to agree with the reader. `specialty` rides the same
    /// `SeatCapability` a real seat's roster read fills.
    fn beat(specialty: Option<&str>, admission: Option<AdmissionPolicy>) -> HeartbeatDraft {
        let capability = SeatCapability {
            harness_families: vec!["claude-code".to_owned()],
            specialty: specialty.map(str::to_owned),
            ..SeatCapability::default()
        };
        match admission {
            Some(admission) => heartbeat_for_state(
                0,
                true,
                12,
                false,
                vec![MINT.to_owned()],
                vec!["claude".to_owned()],
                capability,
                admission,
            ),
            // The pre-§4.2 shape: a seat that states no admission policy at all.
            None => HeartbeatDraft::new(true, 0, 12, vec![MINT.to_owned()])
                .with_agents(vec!["claude".to_owned()])
                .with_capability(capability),
        }
    }

    /// The reducer under a COMPLETED read. Completion is a transport fact, and these tests are
    /// about the rules; the confirmed/unconfirmed distinction has its own tests below and a
    /// behavioral one against a scripted relay in `tests/discovery_relay_behavior.rs`.
    fn reduced(
        announcements: impl IntoIterator<Item = AnnouncedSeat>,
        policy: DirectoryPolicy,
    ) -> SellerDirectory {
        reduce_directory(announcements, policy, ReadCompletion::ConfirmedByEose)
    }

    /// A distinct id per (pubkey, created_at), so the ordinary tests carry signed-shaped ids
    /// without caring what they are. Tie-break tests state their ids explicitly instead.
    fn announced(pubkey: &str, created_at: u64, draft: &HeartbeatDraft) -> AnnouncedSeat {
        let id = format!("{:0>64}", format!("{}{created_at}", &pubkey[..4]));
        announced_with_id(pubkey, created_at, &id, draft)
    }

    fn announced_with_id(
        pubkey: &str,
        created_at: u64,
        event_id: &str,
        draft: &HeartbeatDraft,
    ) -> AnnouncedSeat {
        AnnouncedSeat {
            event_id: event_id.to_owned(),
            ..announced_inner(pubkey, created_at, draft)
        }
    }

    fn announced_inner(pubkey: &str, created_at: u64, draft: &HeartbeatDraft) -> AnnouncedSeat {
        AnnouncedSeat {
            author_pubkey: pubkey.to_owned(),
            created_at,
            event_id: String::new(),
            event: draft.to_event_draft(),
        }
    }

    #[test]
    fn an_equal_created_at_tie_resolves_to_the_lowest_id_in_either_input_order() {
        // F2. Two SIGNED beats for one address, same timestamp, opposite `accepting`: one says the
        // seat is live, the other retracts it. NIP-01 retains the lowest id, so the retraction here
        // (id "11…") must win both times. Before the id crossed the transport seam this resolved by
        // whichever event the relay happened to hand over last — a seat live or dead by luck.
        let live = beat(Some("Rust"), Some(open_policy()));
        let terminal = retraction_for_state(
            0,
            12,
            false,
            vec![MINT.to_owned()],
            vec!["claude".to_owned()],
            SeatCapability {
                harness_families: vec!["claude-code".to_owned()],
                specialty: Some("Rust".to_owned()),
                ..SeatCapability::default()
            },
            open_policy(),
        );
        let low = format!("{:1>64}", "");
        let high = format!("{:f>64}", "");

        for (first, second) in [(&live, &terminal), (&terminal, &live)] {
            let first_id = if std::ptr::eq(first, &live) {
                &high
            } else {
                &low
            };
            let second_id = if std::ptr::eq(second, &live) {
                &high
            } else {
                &low
            };
            let directory = reduced(
                [
                    announced_with_id(SEAT_A, NOW - 30, first_id, first),
                    announced_with_id(SEAT_A, NOW - 30, second_id, second),
                ],
                DirectoryPolicy::at(NOW),
            );
            assert!(
                directory.sellers.is_empty(),
                "the lowest-id event is the retraction, so the seat must be retracted whichever \
                 order it arrives in: {directory:?}"
            );
            assert_eq!(directory.skipped.retracted, 1);
        }

        // And the mirror image: when the LIVE beat holds the lowest id, the seat stays listed in
        // both orders. A tie-break that always dropped the seat would pass the half above.
        for (first, second) in [(&live, &terminal), (&terminal, &live)] {
            let first_id = if std::ptr::eq(first, &live) {
                &low
            } else {
                &high
            };
            let second_id = if std::ptr::eq(second, &live) {
                &low
            } else {
                &high
            };
            let directory = reduced(
                [
                    announced_with_id(SEAT_A, NOW - 30, first_id, first),
                    announced_with_id(SEAT_A, NOW - 30, second_id, second),
                ],
                DirectoryPolicy::at(NOW),
            );
            assert_eq!(
                directory.sellers.len(),
                1,
                "the lowest-id event is the live beat, so the seat must be listed whichever order \
                 it arrives in: {directory:?}"
            );
            assert_eq!(directory.skipped.retracted, 0);
        }
    }

    #[test]
    fn a_newer_timestamp_still_outranks_a_lower_id() {
        // The tie-break must be SECONDARY. An id-first order would let a stale low-id beat outrank
        // the seat's newer word about itself.
        let live = beat(Some("Rust"), Some(open_policy()));
        let terminal = retraction_for_state(
            0,
            12,
            false,
            vec![MINT.to_owned()],
            vec!["claude".to_owned()],
            SeatCapability {
                harness_families: vec!["claude-code".to_owned()],
                specialty: Some("Rust".to_owned()),
                ..SeatCapability::default()
            },
            open_policy(),
        );
        let low = format!("{:1>64}", "");
        let high = format!("{:f>64}", "");

        // Older retraction with the LOW id; newer live beat with the HIGH id. Newer wins.
        let directory = reduced(
            [
                announced_with_id(SEAT_A, NOW - 300, &low, &terminal),
                announced_with_id(SEAT_A, NOW - 30, &high, &live),
            ],
            DirectoryPolicy::at(NOW),
        );
        assert_eq!(directory.sellers.len(), 1, "{directory:?}");

        // And the reverse: newer retraction with the high id beats an older live beat with the low.
        let directory = reduced(
            [
                announced_with_id(SEAT_A, NOW - 300, &low, &live),
                announced_with_id(SEAT_A, NOW - 30, &high, &terminal),
            ],
            DirectoryPolicy::at(NOW),
        );
        assert!(directory.sellers.is_empty(), "{directory:?}");
    }

    #[test]
    fn an_unconfirmed_read_never_reports_itself_as_confirmed_however_many_rows_it_holds() {
        // F1, at the reducer seam. A partial stream carries perfectly well-formed rows; the reducer
        // cannot tell it from a completed one, so completion is passed IN and never inferred.
        let live = beat(Some("Rust"), Some(open_policy()));

        let partial = reduce_directory(
            [announced(SEAT_A, NOW - 30, &live)],
            DirectoryPolicy::at(NOW),
            ReadCompletion::Unconfirmed,
        );
        assert_eq!(partial.sellers.len(), 1, "partial rows are KEPT");
        assert!(
            !partial.read_confirmed,
            "rows in hand are not evidence the relay finished answering"
        );

        // The nastiest shape of the same bug: one stale beat arrives, the stream dies, and the
        // filtered-out row leaves an EMPTY seller list. Confirmed here would read as "the market is
        // empty" on the strength of an answer that never came.
        let stale_partial = reduce_directory(
            [announced(SEAT_A, NOW - DEFAULT_MAX_AGE_SECS - 1, &live)],
            DirectoryPolicy::at(NOW),
            ReadCompletion::Unconfirmed,
        );
        assert!(stale_partial.sellers.is_empty());
        assert!(
            !stale_partial.read_confirmed,
            "an empty list from an unfinished read is not an empty market"
        );
        assert_eq!(
            stale_partial.events_read, 1,
            "and it still says what it saw"
        );

        assert!(ReadCompletion::ConfirmedByEose.is_confirmed());
        assert!(!ReadCompletion::Unconfirmed.is_confirmed());
    }

    #[test]
    fn a_declared_specialty_reaches_the_discovery_output_with_its_pubkey() {
        // The chain scope 1 started, finished: config -> beat -> tag -> parse -> a row a buyer can
        // act on. `pubkey` is the load-bearing field — it is what the targeted post takes.
        let directory = reduced(
            [announced(
                SEAT_A,
                NOW - 30,
                &beat(
                    Some("Rust async runtimes and tokio internals"),
                    Some(open_policy()),
                ),
            )],
            DirectoryPolicy::at(NOW),
        );
        assert_eq!(directory.sellers.len(), 1, "{directory:?}");
        let seat = &directory.sellers[0];
        assert_eq!(seat.pubkey, SEAT_A);
        assert_eq!(
            seat.specialty.as_deref(),
            Some("Rust async runtimes and tokio internals")
        );
        assert_eq!(seat.announced_at, NOW - 30);
        assert_eq!(seat.age_secs, 30);
        assert_eq!(seat.rate_sats, 12);
        assert_eq!(seat.accepted_mints, vec![MINT]);
        assert_eq!(seat.agents, vec!["claude"]);
        assert_eq!(seat.harness_families, vec!["claude-code"]);
        assert_eq!(seat.admits_pool, crate::home::ADMISSION_OPEN);
        assert_eq!(seat.admits_targeted, crate::home::ADMISSION_OPEN);
        assert!(directory.read_confirmed);
        assert_eq!(directory.events_read, 1);
        assert_eq!(directory.skipped, DirectorySkips::default());
    }

    #[test]
    fn a_discovered_pubkey_is_accepted_by_the_unchanged_targeted_post_path() {
        // The handoff, end to end and OFFLINE: the pubkey a discovery row carries goes into the
        // EXISTING targeted-post parameter and comes back out of the parsed offer as the seat the
        // offer addresses. No relay, no post, no payment — only the two ends of the flow the order
        // names, joined by nothing but a 64-hex string.
        //
        // This test exists to catch a whole class of quiet breakage: a row field that renders fine
        // and is not a valid target (padded, truncated, npub-encoded, upper-cased). Discovery's
        // whole purpose is to end at a value `post_job` accepts, so the value is asserted through
        // the real `OfferDraft` -> `to_event_draft` -> `parse_offer` path rather than eyeballed.
        use crate::gateway::{OfferDraft, assert_seller_matches, is_targeted, parse_offer};

        let directory = reduced(
            [announced(
                SEAT_A,
                NOW - 30,
                &beat(Some("Rust async runtimes"), Some(open_policy())),
            )],
            DirectoryPolicy::at(NOW),
        );
        let discovered = directory.sellers[0].pubkey.clone();

        let draft = OfferDraft::new(
            "port a crate to tokio",
            "text/plain",
            1,
            NOW + 600,
            &discovered,
        )
        .to_event_draft();
        let offer =
            parse_offer(&draft).expect("an offer targeted at a discovered pubkey must parse");

        assert!(
            is_targeted(&offer),
            "a discovered pubkey must produce a TARGETED offer, not an open-pool one"
        );
        assert!(offer.seller_matches(&discovered));
        assert_seller_matches(&offer, &discovered)
            .expect("the discovered seat must be the seat the offer addresses");
        assert!(
            !offer.seller_matches(SEAT_B),
            "targeting one discovered seat must not address another"
        );

        // And the row's own specialty is nowhere on the offer. Discovery informed the CHOICE; it
        // did not become a term of the deal.
        assert!(
            !draft
                .tags
                .iter()
                .any(|tag| tag.0.iter().any(|value| value.contains("Rust async"))),
            "specialty text must not ride the offer: {:?}",
            draft.tags
        );
    }

    #[test]
    fn a_seat_with_no_specialty_is_still_discoverable() {
        // The migration property the order names: old sellers without a description must not vanish
        // from the directory. Unstated is a missing FIELD, never a missing SEAT.
        let directory = reduced(
            [announced(
                SEAT_A,
                NOW - 10,
                &beat(None, Some(open_policy())),
            )],
            DirectoryPolicy::at(NOW),
        );
        assert_eq!(directory.sellers.len(), 1);
        assert_eq!(directory.sellers[0].specialty, None);
        assert_eq!(directory.sellers[0].pubkey, SEAT_A);
    }

    #[test]
    fn a_legacy_beat_that_states_no_admission_reads_as_unstated_never_closed() {
        // Rendering unstated as `closed` would tell a buyer that every seat older than the §4.2
        // tags refuses it. The seat did not say; the directory must not say either.
        let directory = reduced(
            [announced(SEAT_A, NOW - 10, &beat(Some("Rust"), None))],
            DirectoryPolicy::at(NOW),
        );
        assert_eq!(directory.sellers.len(), 1, "a legacy seat stays listed");
        assert_eq!(directory.sellers[0].admits_pool, ADMISSION_UNSTATED);
        assert_eq!(directory.sellers[0].admits_targeted, ADMISSION_UNSTATED);
        assert_ne!(
            directory.sellers[0].admits_pool,
            crate::home::ADMISSION_CLOSED
        );
    }

    #[test]
    fn a_newer_retraction_removes_the_seat_even_though_its_older_beat_was_live() {
        // The ordering rule that makes retraction work. The address is resolved FIRST, then the
        // winner is judged: resolve-after-filter would drop the `accepting=n` beat on its own
        // merits and leave the seat's older `accepting=y` standing as the survivor — the seat would
        // stay advertised by the very event that retracted it.
        let live = beat(Some("Rust"), Some(open_policy()));
        let terminal = retraction_for_state(
            0,
            12,
            false,
            vec![MINT.to_owned()],
            vec!["claude".to_owned()],
            SeatCapability {
                harness_families: vec!["claude-code".to_owned()],
                specialty: Some("Rust".to_owned()),
                ..SeatCapability::default()
            },
            open_policy(),
        );
        let directory = reduced(
            [
                announced(SEAT_A, NOW - 600, &live),
                announced(SEAT_A, NOW - 60, &terminal),
            ],
            DirectoryPolicy::at(NOW),
        );
        assert!(
            directory.sellers.is_empty(),
            "a retracted seat must not appear as a live seller: {directory:?}"
        );
        assert_eq!(directory.skipped.retracted, 1);
        assert_eq!(directory.events_read, 2);

        // AND THE OTHER DIRECTION, or the assertion above is satisfied by any rule that drops
        // `accepting=n`: an OLDER retraction must NOT bury a newer live beat.
        let recovered = reduced(
            [
                announced(SEAT_A, NOW - 600, &terminal),
                announced(SEAT_A, NOW - 60, &live),
            ],
            DirectoryPolicy::at(NOW),
        );
        assert_eq!(
            recovered.sellers.len(),
            1,
            "a seat that came back is live again: {recovered:?}"
        );
        assert_eq!(recovered.skipped.retracted, 0);
    }

    #[test]
    fn a_stale_or_future_dated_beat_is_not_a_live_seller() {
        let live = beat(Some("Rust"), Some(open_policy()));
        let stale = reduced(
            [announced(SEAT_A, NOW - DEFAULT_MAX_AGE_SECS - 1, &live)],
            DirectoryPolicy::at(NOW),
        );
        assert!(stale.sellers.is_empty(), "{stale:?}");
        assert_eq!(stale.skipped.stale, 1);

        // Exactly AT the window is still live — the bound is inclusive, so a seat is not dropped by
        // one second of arithmetic it cannot observe.
        let edge = reduced(
            [announced(SEAT_A, NOW - DEFAULT_MAX_AGE_SECS, &live)],
            DirectoryPolicy::at(NOW),
        );
        assert_eq!(edge.sellers.len(), 1, "{edge:?}");

        let future = reduced(
            [announced(
                SEAT_A,
                NOW + DEFAULT_MAX_CLOCK_SKEW_SECS + 1,
                &live,
            )],
            DirectoryPolicy::at(NOW),
        );
        assert!(future.sellers.is_empty(), "{future:?}");
        assert_eq!(future.skipped.future_dated, 1);

        // Inside the skew tolerance a seat stays visible, at age zero rather than a wrapped age.
        let skewed = reduced(
            [announced(SEAT_A, NOW + 10, &live)],
            DirectoryPolicy::at(NOW),
        );
        assert_eq!(skewed.sellers.len(), 1, "{skewed:?}");
        assert_eq!(skewed.sellers[0].age_secs, 0);
    }

    #[test]
    fn an_unparseable_event_is_counted_and_never_becomes_a_row() {
        // A junk event squatting the kind must not become a seat a buyer might target. The count is
        // what lets an empty directory be EXPLAINED rather than merely reported.
        let junk = AnnouncedSeat {
            author_pubkey: SEAT_B.to_owned(),
            created_at: NOW - 10,
            event_id: format!("{:0>64}", "junk"),
            event: EventDraft::new(
                crate::heartbeat::SELLER_HEARTBEAT_KIND,
                vec![crate::gateway::TagSpec::new(["d", "maxplayer-seller"])],
                "",
            ),
        };
        let directory = reduced(
            [
                announced(SEAT_A, NOW - 10, &beat(Some("Rust"), Some(open_policy()))),
                junk,
            ],
            DirectoryPolicy::at(NOW),
        );
        assert_eq!(directory.sellers.len(), 1);
        assert_eq!(directory.sellers[0].pubkey, SEAT_A);
        assert_eq!(directory.skipped.unparseable, 1);
        assert_eq!(directory.events_read, 2);
    }

    #[test]
    fn the_latest_beat_per_address_wins_and_the_order_carries_no_ranking() {
        // Addressable events supersede IN PLACE, so a seat's older beats must not survive alongside
        // its newest — an id-keyed reduce would list the same seat twice at two rates.
        let old = beat(Some("Rust, old text"), Some(open_policy()));
        let new = beat(Some("Rust, current text"), Some(open_policy()));
        let directory = reduced(
            [
                announced(SEAT_B, NOW - 20, &new),
                announced(SEAT_A, NOW - 300, &old),
                announced(SEAT_A, NOW - 5, &new),
            ],
            DirectoryPolicy::at(NOW),
        );
        assert_eq!(
            directory.sellers.len(),
            2,
            "one row per seat: {directory:?}"
        );
        assert_eq!(
            directory.sellers[0].specialty.as_deref(),
            Some("Rust, current text"),
            "the superseded text must not be what a buyer reads"
        );
        // Pubkey order, NOT freshness order: SEAT_B beat more recently than SEAT_A's resolved beat
        // would in a freshness sort, and it still comes second. Freshness-descending would be a
        // ranking policy this slice deliberately does not ship.
        assert_eq!(
            directory
                .sellers
                .iter()
                .map(|seat| seat.pubkey.as_str())
                .collect::<Vec<_>>(),
            vec![SEAT_A, SEAT_B]
        );
    }

    #[test]
    fn an_answered_empty_market_is_not_the_same_value_as_an_unanswered_read() {
        // The distinction the order demands, asserted on the two constructors the transport returns.
        // Both hold zero sellers; only one of them is a statement about the market.
        let answered = SellerDirectory::empty_confirmed();
        let unanswered = SellerDirectory::unverified();
        assert!(answered.sellers.is_empty() && unanswered.sellers.is_empty());
        assert!(
            answered.read_confirmed,
            "an answered empty read is a fact about the market"
        );
        assert!(
            !unanswered.read_confirmed,
            "an unanswered read is a fact about our patience, and must not read as an empty market"
        );
        assert_ne!(answered, unanswered);

        // And a REDUCED directory is always a confirmed read: the reducer only ever runs on events
        // the relay actually served, so the unconfirmed value cannot be produced by this path.
        let reduced = reduced([], DirectoryPolicy::at(NOW));
        assert!(reduced.read_confirmed);
        assert_eq!(reduced, answered);
    }

    #[test]
    fn discovery_never_writes() {
        // A structural check, not a behavioural one: the discovery module must contain no publish,
        // no award, no payment. Asserted against the SOURCE because the property is "this code
        // cannot spend", and a runtime test can only show that one path did not.
        let source = include_str!("discovery.rs");
        // Split the needles so this test's own text does not match them.
        for forbidden in [
            concat!("send_", "event"),
            concat!("send_", "event_to"),
            concat!("publish_", "signed"),
            concat!("EventBuilder", "::"),
            concat!("award_", "claim"),
            concat!("reserve_", "for_award"),
            concat!("pay_", "invoice"),
        ] {
            assert!(
                !source.contains(forbidden),
                "discovery is read-only and must not reference `{forbidden}`"
            );
        }
    }
}
