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
