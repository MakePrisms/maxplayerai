//! The buzz wire dialect: the kind numbers, tag names and filter shapes a buzz relay speaks.
//!
//! Everything in this file is a fact about buzz's protocol, transcribed from the deployed relay's
//! own source at rev `e18020a63f8e78d38405a47a16dc9a5babb5a186` (worktree
//! `/srv/forge/workspaces/buzz/.claude/worktrees/agent-acbdfdb3498227c70`). Each constant carries
//! the file and line it came from so a future reader can re-derive it rather than trust it.
//!
//! It is transcribed rather than imported on purpose. The buzz crates that define these
//! (`buzz-core`, `buzz-sdk`) are unpublished workspace members, so depending on them would put
//! mobee's static release build behind the reachability of a second repository — a cost the
//! packaging surface pays, for a handful of integers. The trade reverses the moment a slice needs
//! buzz's *event builders*: this module is read-only wire knowledge, and the first slice that
//! posts buzz events should re-open the `buzz-sdk` dependency question rather than hand-copy
//! builder logic.
//!
//! This is the only file in `participation` that knows the word "buzz". Everything above it works
//! in terms of [`Dialect`], so a second relay vocabulary is a new implementation here, not a
//! rewrite upstairs.

use nostr_sdk::prelude::{Alphabet, Filter, Kind, PublicKey, SingleLetterTag, Timestamp};

/// Relay-signed notification: this pubkey was added to a channel.
///
/// ★ This event IS the invite. Buzz has no invite handshake to answer — kind 9009
/// (`KIND_NIP29_CREATE_INVITE`) is explicitly unimplemented on the relay
/// (`crates/buzz-relay/src/.../side_effects.rs:157-160`). A member publishes a kind-9000
/// add naming your pubkey, and the relay emits this. "Accepting" is nothing more than
/// subscribing to the channel it names.
///
/// Source: `crates/buzz-core/src/kind.rs:396` (`KIND_MEMBER_ADDED_NOTIFICATION`).
/// Stored globally (no channel scope), `p`-tag = the target, `h`-tag = the channel UUID.
pub const KIND_MEMBER_ADDED: u16 = 44100;

/// Relay-signed notification: this pubkey was removed from a channel. Same tag shape as
/// [`KIND_MEMBER_ADDED`]; the correct response is to unsubscribe and drain, never to retry.
///
/// Source: `crates/buzz-core/src/kind.rs:400` (`KIND_MEMBER_REMOVED_NOTIFICATION`).
pub const KIND_MEMBER_REMOVED: u16 = 44101;

/// A channel message. Carries `h` (channel), NIP-10 `e` markers for threading, and `p` tags for
/// mentions — a mention is a `p`-tag and nothing else, so we never parse content to find one.
///
/// Source: `crates/buzz-core/src/kind.rs` channel section; message semantics in
/// `crates/buzz-acp/src/relay.rs` (the `#h`/`#p` REQ shapes reproduced below).
pub const KIND_CHANNEL_MESSAGE: u16 = 9;

/// The tag naming the channel a message belongs to (NIP-29-style group id, a UUID string).
pub const TAG_CHANNEL: Alphabet = Alphabet::H;

/// The tag naming a mentioned pubkey. Also the tag the relay p-gates its notifications on.
pub const TAG_PUBKEY: Alphabet = Alphabet::P;

/// Seconds subtracted from a stored cursor when re-subscribing.
///
/// A relay's clock and ours disagree by some small amount, and `since` is evaluated against the
/// event's `created_at` — which the *author's* clock stamped. Re-asking from exactly where we
/// left off therefore risks stepping over an event authored a second or two "before" our cursor.
/// Overlapping is safe because ingest is idempotent on event id; a gap is not recoverable.
///
/// Matches buzz-acp's own reconnect skew (`crates/buzz-acp/src/relay.rs`, `SINCE_SKEW_SECS`).
pub const SINCE_SKEW_SECS: u64 = 60;

/// A relay's answer to a subscription request, reduced to what the caller must actually *do*.
///
/// Buzz sends these as NIP-01 `CLOSED` frames with a machine-readable prefix. The distinction
/// that matters is not the wording but the blast radius: two of these end one channel, one ends
/// the socket, and one ends nothing at all.
///
/// Semantics transcribed from `crates/buzz-acp/src/relay.rs:3498-3520` (the fixed handling — an
/// earlier revision hot-looped by tearing down the socket for a per-channel refusal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosedVerdict {
    /// `restricted: …` / `… access revoked` — we are not (or no longer) a member of THAT channel.
    /// Drop the one channel and keep the socket: the connection is healthy and every other
    /// subscription on it is still good.
    DropChannel,
    /// `rate-limited: …` — ★ the subscription never existed server-side. This is not a subscription
    /// that failed later; the REQ was refused outright, so anything we believe is queued behind it
    /// is queued behind nothing. Park for the hinted interval, then RE-SEND the REQ; a drain that
    /// merely waits will wait forever.
    RetryAfter { hint_secs: Option<u64> },
    /// `auth-required: …` / `insufficient-scope: …` — the socket's authentication is the problem,
    /// so nothing on it is trustworthy. Reconnect and re-authenticate before re-subscribing.
    Reauthenticate,
    /// A `CLOSED` with an empty reason is the relay acknowledging OUR OWN close, not refusing us.
    /// Reacting to it as a refusal invents a failure that never happened.
    Acknowledged,
    /// Anything else: surfaced verbatim rather than guessed at.
    Unknown(String),
}

/// Classify a NIP-01 `CLOSED` reason string.
///
/// Prefix-matched, not equality-matched: the relay appends detail after the machine-readable
/// prefix (`"restricted: not a channel member"`), and the detail is for humans.
pub fn classify_closed(reason: &str) -> ClosedVerdict {
    let reason = reason.trim();
    if reason.is_empty() {
        return ClosedVerdict::Acknowledged;
    }
    if reason.starts_with("restricted:") || reason.contains("access revoked") {
        return ClosedVerdict::DropChannel;
    }
    if reason.starts_with("rate-limited:") {
        return ClosedVerdict::RetryAfter {
            hint_secs: parse_retry_hint(reason),
        };
    }
    if reason.starts_with("auth-required:") || reason.starts_with("insufficient-scope:") {
        return ClosedVerdict::Reauthenticate;
    }
    ClosedVerdict::Unknown(reason.to_string())
}

/// Pull the first bare integer out of a rate-limit reason (`"rate-limited: retry in 30s"` ⇒ 30).
/// The hint is advisory — absent or unparseable means "back off by our own default", never "retry
/// immediately".
fn parse_retry_hint(reason: &str) -> Option<u64> {
    reason
        .split(|c: char| !c.is_ascii_digit())
        .find(|piece| !piece.is_empty())
        .and_then(|piece| piece.parse().ok())
}

/// The global membership filter: every add/remove notification addressed to us, across all
/// channels, on this relay.
///
/// ★ Half of the entire wake surface. Buzz p-gates 44100/44101 (`P_GATED_KINDS`,
/// `crates/buzz-core/src/kind.rs:146-155`), so the `#p` term is not an optimisation — a REQ for
/// these kinds without it is refused outright.
///
/// Shape matches `send_membership_subscribe` (`crates/buzz-acp/src/relay.rs:3225-3252`).
pub fn membership_filter(me: PublicKey, since: Option<u64>) -> Filter {
    let filter = Filter::new()
        .kinds([
            Kind::from(KIND_MEMBER_ADDED),
            Kind::from(KIND_MEMBER_REMOVED),
        ])
        .custom_tags(SingleLetterTag::lowercase(TAG_PUBKEY), [me.to_hex()]);
    apply_since(filter, since)
}

/// The per-channel filter: messages in one channel that mention us.
///
/// The other half of the wake surface. Deliberately mention-scoped — a node that subscribed to
/// every message in every channel it belongs to would wake on all of them, which is a firehose,
/// not a wake surface. Ambient reading is a separate, explicitly-requested subscription.
///
/// Shape matches `send_subscribe` with `require_mention` (`crates/buzz-acp/src/relay.rs:3151-3224`).
pub fn channel_mention_filter(channel: &str, me: PublicKey, since: Option<u64>) -> Filter {
    let filter = Filter::new()
        .kind(Kind::from(KIND_CHANNEL_MESSAGE))
        .custom_tags(SingleLetterTag::lowercase(TAG_CHANNEL), [channel])
        .custom_tags(SingleLetterTag::lowercase(TAG_PUBKEY), [me.to_hex()]);
    apply_since(filter, since)
}

/// Apply a stored cursor to a filter, backdated by [`SINCE_SKEW_SECS`].
///
/// No cursor means this filter has never run. It asks from *now* rather than from the beginning of
/// time: a first subscribe that replays a channel's whole history would flood the inbox with
/// mentions that were answered months ago.
fn apply_since(filter: Filter, since: Option<u64>) -> Filter {
    let from = match since {
        Some(cursor) => cursor.saturating_sub(SINCE_SKEW_SECS),
        None => Timestamp::now().as_secs(),
    };
    filter.since(Timestamp::from_secs(from))
}

/// Read the channel a `44100`/`44101`/channel-message event refers to, from its `h` tag.
pub fn channel_of(event: &nostr_sdk::Event) -> Option<String> {
    event
        .tags
        .find(nostr_sdk::TagKind::SingleLetter(
            SingleLetterTag::lowercase(TAG_CHANNEL),
        ))
        .and_then(|tag| tag.content())
        .map(str::to_string)
}

/// Whether the event `p`-tags us — the whole of what "mentions me" means on this wire. Buzz does
/// not parse message text for names, and neither do we.
pub fn mentions(event: &nostr_sdk::Event, me: PublicKey) -> bool {
    event.tags.public_keys().any(|pubkey| *pubkey == me)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_per_channel_refusal_does_not_condemn_the_socket() {
        assert_eq!(
            classify_closed("restricted: not a channel member"),
            ClosedVerdict::DropChannel
        );
        assert_eq!(
            classify_closed("channel access revoked"),
            ClosedVerdict::DropChannel
        );
    }

    #[test]
    fn an_auth_failure_does_condemn_the_socket() {
        assert_eq!(
            classify_closed("auth-required: we can't serve DMs to unauthenticated users"),
            ClosedVerdict::Reauthenticate
        );
        assert_eq!(
            classify_closed("insufficient-scope: agent token required"),
            ClosedVerdict::Reauthenticate
        );
    }

    #[test]
    fn a_rate_limit_carries_its_hint_and_never_reads_as_zero() {
        assert_eq!(
            classify_closed("rate-limited: retry in 30 seconds"),
            ClosedVerdict::RetryAfter {
                hint_secs: Some(30)
            }
        );
        // No number to find ⇒ None, meaning "use our own backoff". It must not collapse to
        // Some(0), which would spin.
        assert_eq!(
            classify_closed("rate-limited: slow down"),
            ClosedVerdict::RetryAfter { hint_secs: None }
        );
    }

    #[test]
    fn an_empty_reason_is_our_own_close_coming_back_not_a_refusal() {
        assert_eq!(classify_closed(""), ClosedVerdict::Acknowledged);
        assert_eq!(classify_closed("   "), ClosedVerdict::Acknowledged);
    }

    #[test]
    fn an_unrecognised_reason_is_surfaced_verbatim_not_guessed_at() {
        assert_eq!(
            classify_closed("pow: 20 bits required"),
            ClosedVerdict::Unknown("pow: 20 bits required".into())
        );
    }
}
