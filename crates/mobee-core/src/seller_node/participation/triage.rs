//! Inbox triage: deciding what an inbound event IS, before anything acts on it.
//!
//! Kept as a pure function over an event so the whole decision table is testable without a relay,
//! a socket, or a clock. The caller performs the effect; this module only names it.
//!
//! The rule that shapes everything here: **triage never answers.** The strongest thing it can say
//! about a message addressed to the node is "this is owed a response", which is a row in a ledger.
//! Answering arrives with the mind, in a later slice.

use nostr_sdk::prelude::{Event, PublicKey};

use crate::kinds::{
    JOB_AWARD_KIND, JOB_CLAIM_KIND, JOB_FEEDBACK_KIND, JOB_OFFER_KIND, JOB_RECEIPT_KIND,
    JOB_RESULT_KIND,
};

use super::dialect;

/// What an inbound event is, and therefore who handles it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Triage {
    /// A relay-signed membership grant. ★ This IS the invite — there is no handshake to answer;
    /// accepting means subscribing.
    ChannelJoined { channel_id: String },
    /// A relay-signed membership revocation: unsubscribe and drain.
    ChannelLeft { channel_id: String },
    /// Someone addressed the node. Becomes a debt in the owed ledger.
    ///
    /// A buzz DM arrives as exactly this — the relay models a DM as a hidden channel with a
    /// generated id, so a direct message and a channel mention are the same event shape reaching us
    /// through the same filter. Nothing downstream needs to tell them apart, and nothing here
    /// pretends the DM is end-to-end encrypted: its privacy is the relay's access control.
    AddressedToUs {
        channel_id: String,
        counterparty: String,
        kind: u16,
    },
    /// A mobee trade-path event. Belongs to the gateway's job path, which this module does not
    /// touch, reimplement, or wrap.
    TradePath { kind: u16 },
    /// A channel message that is not about us. Read-only: it may inform, it may not summon.
    Ambient { channel_id: String },
    /// Nothing we have a rule for. Ignored, and named so that "ignored" is a decision in the table
    /// rather than a gap in it.
    Unhandled { kind: u16 },
}

/// The mobee trade-path kinds. An event carrying one of these is the gateway's, no matter which
/// relay or channel it arrived on.
const TRADE_PATH_KINDS: [u16; 6] = [
    JOB_RECEIPT_KIND,
    JOB_OFFER_KIND,
    JOB_CLAIM_KIND,
    JOB_RESULT_KIND,
    JOB_FEEDBACK_KIND,
    JOB_AWARD_KIND,
];

/// Classify one inbound event for a node whose pubkey is `me`.
///
/// Order matters and is deliberate:
///
/// 1. Membership first — a 44100 also p-tags us, so testing "mentions me" before "is a membership
///    notification" would file every invite as a message owed a reply.
/// 2. Trade path second — a mobee event that happens to p-tag the seller is the job path's, not a
///    conversation. Reversing this would open a second, unauthenticated route into work handling.
/// 3. Addressed-to-us third.
/// 4. Everything else is ambient or unhandled.
pub fn triage(event: &Event, me: PublicKey) -> Triage {
    let kind = event.kind.as_u16();

    if kind == dialect::KIND_MEMBER_ADDED || kind == dialect::KIND_MEMBER_REMOVED {
        // ★ A membership notification that does not p-tag US describes somebody else's membership.
        // The subscription filter already pins `#p`, so reaching here without our tag means the
        // relay served something we did not ask for — and acting on it would let one relay move
        // this node in and out of channels at will. Checked here too: the filter is the relay's
        // promise, this is our own verification of it, and only one of the two is ours to trust.
        if !dialect::mentions(event, me) {
            return Triage::Unhandled { kind };
        }
        // A membership notification with no channel tag is malformed: it names no channel to join
        // or leave, so there is no action it could describe. Surfaced rather than guessed at.
        return match dialect::channel_of(event) {
            Some(channel_id) if kind == dialect::KIND_MEMBER_ADDED => {
                Triage::ChannelJoined { channel_id }
            }
            Some(channel_id) => Triage::ChannelLeft { channel_id },
            None => Triage::Unhandled { kind },
        };
    }

    if TRADE_PATH_KINDS.contains(&kind) {
        return Triage::TradePath { kind };
    }

    if kind == dialect::KIND_CHANNEL_MESSAGE {
        let Some(channel_id) = dialect::channel_of(event) else {
            return Triage::Unhandled { kind };
        };
        // ★ A mention is a p-tag. We never read the message text looking for our name: content
        // matching would make the node summonable by anyone who can type, on a wire where the
        // p-tag is the relay-enforced addressing primitive.
        return if dialect::mentions(event, me) {
            Triage::AddressedToUs {
                channel_id,
                counterparty: event.pubkey.to_hex(),
                kind,
            }
        } else {
            Triage::Ambient { channel_id }
        };
    }

    Triage::Unhandled { kind }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::prelude::{EventBuilder, Keys, Kind, Tag, TagKind};

    fn tag_channel(channel: &str) -> Tag {
        Tag::custom(TagKind::custom("h"), [channel])
    }

    fn tag_pubkey(pubkey: PublicKey) -> Tag {
        Tag::public_key(pubkey)
    }

    fn signed(keys: &Keys, kind: u16, tags: Vec<Tag>) -> Event {
        EventBuilder::new(Kind::from(kind), "")
            .tags(tags)
            .sign_with_keys(keys)
            .expect("sign")
    }

    fn me_and_them() -> (Keys, Keys) {
        (Keys::generate(), Keys::generate())
    }

    #[test]
    fn a_membership_grant_is_an_invite_not_a_message_owed_a_reply() {
        let (me, relay) = me_and_them();
        // The relay p-tags us on a 44100 — which is exactly why membership must be tested first.
        let event = signed(
            &relay,
            dialect::KIND_MEMBER_ADDED,
            vec![tag_channel("chan-1"), tag_pubkey(me.public_key())],
        );
        assert_eq!(
            triage(&event, me.public_key()),
            Triage::ChannelJoined {
                channel_id: "chan-1".into()
            }
        );
    }

    #[test]
    fn a_membership_revocation_is_a_leave() {
        let (me, relay) = me_and_them();
        let event = signed(
            &relay,
            dialect::KIND_MEMBER_REMOVED,
            vec![tag_channel("chan-1"), tag_pubkey(me.public_key())],
        );
        assert_eq!(
            triage(&event, me.public_key()),
            Triage::ChannelLeft {
                channel_id: "chan-1".into()
            }
        );
    }

    #[test]
    fn a_membership_notification_naming_no_channel_is_not_acted_on() {
        let (me, relay) = me_and_them();
        let event = signed(
            &relay,
            dialect::KIND_MEMBER_ADDED,
            vec![tag_pubkey(me.public_key())],
        );
        assert_eq!(
            triage(&event, me.public_key()),
            Triage::Unhandled {
                kind: dialect::KIND_MEMBER_ADDED
            }
        );
    }

    #[test]
    fn a_p_tag_is_what_addresses_us() {
        let (me, them) = me_and_them();
        let event = signed(
            &them,
            dialect::KIND_CHANNEL_MESSAGE,
            vec![tag_channel("chan-1"), tag_pubkey(me.public_key())],
        );
        assert_eq!(
            triage(&event, me.public_key()),
            Triage::AddressedToUs {
                channel_id: "chan-1".into(),
                counterparty: them.public_key().to_hex(),
                kind: dialect::KIND_CHANNEL_MESSAGE,
            }
        );
    }

    #[test]
    fn our_name_in_the_text_does_not_address_us() {
        let (me, them) = me_and_them();
        // No p-tag, and the content mentions us by every name a human might use. Still ambient:
        // being summonable by content would let anyone conjure the node into a conversation.
        let event = EventBuilder::new(
            Kind::from(dialect::KIND_CHANNEL_MESSAGE),
            format!("hey {} — mobee seller, are you there?", me.public_key()),
        )
        .tags(vec![tag_channel("chan-1")])
        .sign_with_keys(&them)
        .expect("sign");
        assert_eq!(
            triage(&event, me.public_key()),
            Triage::Ambient {
                channel_id: "chan-1".into()
            }
        );
    }

    #[test]
    fn a_message_p_tagging_someone_else_is_ambient() {
        let (me, them) = me_and_them();
        let third = Keys::generate();
        let event = signed(
            &them,
            dialect::KIND_CHANNEL_MESSAGE,
            vec![tag_channel("chan-1"), tag_pubkey(third.public_key())],
        );
        assert_eq!(
            triage(&event, me.public_key()),
            Triage::Ambient {
                channel_id: "chan-1".into()
            }
        );
    }

    #[test]
    fn a_dm_is_the_same_shape_as_a_channel_mention() {
        let (me, them) = me_and_them();
        // Buzz models a DM as a relay-managed hidden channel: same kind 9, same h-tag, same p-tag.
        // The only difference is that the channel id belongs to a channel humans cannot browse.
        let event = signed(
            &them,
            dialect::KIND_CHANNEL_MESSAGE,
            vec![tag_channel("dm-4f1c-hidden"), tag_pubkey(me.public_key())],
        );
        assert_eq!(
            triage(&event, me.public_key()),
            Triage::AddressedToUs {
                channel_id: "dm-4f1c-hidden".into(),
                counterparty: them.public_key().to_hex(),
                kind: dialect::KIND_CHANNEL_MESSAGE,
            }
        );
    }

    #[test]
    fn every_trade_kind_stays_with_the_job_path_even_when_it_p_tags_us() {
        let (me, buyer) = me_and_them();
        for kind in TRADE_PATH_KINDS {
            let event = signed(
                &buyer,
                kind,
                vec![tag_channel("chan-1"), tag_pubkey(me.public_key())],
            );
            assert_eq!(
                triage(&event, me.public_key()),
                Triage::TradePath { kind },
                "kind {kind} must route to the job path, never the conversation ledger"
            );
        }
    }

    #[test]
    fn an_unknown_kind_is_named_as_ignored_rather_than_falling_through() {
        let (me, them) = me_and_them();
        let event = signed(&them, 31337, vec![tag_channel("chan-1")]);
        assert_eq!(
            triage(&event, me.public_key()),
            Triage::Unhandled { kind: 31337 }
        );
    }
}
