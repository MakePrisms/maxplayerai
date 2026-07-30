//! Participation: the node's READ arm on a social relay.
//!
//! The seller node already speaks two things on the wire: the mobee trade path (offers, claims,
//! results, receipts) and a buzz persona (kind-0 plus a presence heartbeat). Both are outbound.
//! This module is what lets the node be *reached* — admitted to channels, mentioned, and asked.
//!
//! # What it does not do
//!
//! It never originates a post. Outbound stays exactly what it already was: presence, persona, and
//! the trade path. The only frames this module adds to the wire are `REQ`s — and on buzz, a `REQ`
//! is literally how membership is accepted, because [there is no invite handshake to
//! answer](dialect::KIND_MEMBER_ADDED).
//!
//! That is a property of the design rather than a rule to remember. Nothing here builds an event:
//! the access probe rides the presence beat the node was going to publish anyway, and triage's
//! strongest verdict is a row in a ledger. There is no code path from an inbound message to an
//! outbound one, so a slice that wants to answer has to add the ability, visibly.
//!
//! # Shape
//!
//! - [`dialect`] — the buzz wire vocabulary, transcribed from buzz's own source with citations.
//!   The only module that knows the word "buzz"; a second relay vocabulary is a new dialect here,
//!   not a change upstairs.
//! - [`relays`] — the relay roster and the per-relay access state.
//! - [`probe`] — how that access state is established: publish a carrier the caller supplies, then
//!   read it back off the wire. ★ Requires two clients; see its module note before touching it.
//! - [`triage`] — what an inbound event is, as a pure function.
//! - [`engine`] — what to do about it: durable effects, plus the actions the caller applies to the
//!   socket. Spawns nothing, so it composes with the node's `!Send` run loop.
//!
//! Durable state (per-filter cursors, channel membership, the owed-response ledger) lives in
//! [`super::store::SellerStore`] with the rest of the node's state.

pub mod dialect;
pub mod engine;
pub mod probe;
pub mod relays;
pub mod runtime;
pub mod triage;

pub use crate::home::ParticipationConfig;

/// The stable identity of the global membership filter, for the cursor table.
pub const MEMBERSHIP_FILTER_ID: &str = "membership";

/// The stable identity of one channel's mention filter, for the cursor table.
pub fn channel_filter_id(channel_id: &str) -> String {
    format!("channel:{channel_id}")
}

/// Why nothing was sent, or `None` if at least one relay took the message.
///
/// ★ A pool `Ok` means ACCEPTED, NOT SENT. `subscribe_with_id` and `send_event_to` return `Ok` whenever
/// the pool holds the named relays at all; a relay that would not take the message — never connected
/// (`NotReady`), banned, asleep, or with a full send channel — is recorded in `output.failed` and NOWHERE
/// ELSE. The `Result` cannot express it, so `.map(|_| ())` discards the only evidence there was.
///
/// Every client here holds exactly one relay, so an empty `success` set means our only relay took nothing
/// and there is no message in flight for anything to answer.
///
/// This is load-bearing, not tidiness. `note_retry_attempt` arms an attribution token, and a resync clears
/// the pending marker that owes it, both on the strength of "the REQ went out" — reading that from a value
/// which cannot say it arms a token for a message nobody sent. Same defect as arming it too early, one
/// layer further down.
pub(super) fn undelivered<T: std::fmt::Debug>(
    output: &nostr_sdk::prelude::Output<T>,
) -> Option<String> {
    if !output.success.is_empty() {
        return None;
    }
    let reasons: Vec<String> = output
        .failed
        .iter()
        .map(|(url, why)| format!("{url}: {why}"))
        .collect();
    Some(if reasons.is_empty() {
        "no relay accepted it".to_string()
    } else {
        reasons.join("; ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_ids_do_not_collide_across_the_two_filter_shapes() {
        // Both cursors share one table keyed by (relay, filter_id); a channel literally named
        // "membership" must not be able to overwrite the global cursor.
        assert_ne!(channel_filter_id("membership"), MEMBERSHIP_FILTER_ID);
    }

    #[test]
    fn an_absent_config_participates_nowhere() {
        let config = ParticipationConfig::default();
        assert!(config.relays.is_empty());
        assert!(relays::RelayRoster::new(config.relays).is_empty());
    }
}
