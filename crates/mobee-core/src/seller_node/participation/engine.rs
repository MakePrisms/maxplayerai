//! The wire engine: what the node does with each thing the relay says.
//!
//! Split deliberately into a decision half and an effect half. [`Engine`] owns the durable state
//! and answers "given this event, what should happen" as a list of [`Action`]s; the caller applies
//! them to a live [`Client`]. That split is what lets every rule below be tested against a store
//! and a fixed event, with no socket, no relay and no clock.
//!
//! The engine is `!Send`-friendly by construction: it holds no task handles and spawns nothing.
//! The seller node's run loop is `!Send` under the `acp` feature (its `AcpDriver` holds a `!Sync`
//! std receiver), so anything that needed its own `tokio::spawn` would fail to compile on the
//! shipped feature combination while building fine on the workspace default. Nothing here spawns.

use nostr_sdk::prelude::{Event, PublicKey};

use super::super::store::{OwedResponse, SellerStore, StoreError};
use super::triage::{Triage, triage};
use super::{MEMBERSHIP_FILTER_ID, channel_filter_id, dialect};

/// Something the caller must do on the wire as a result of an inbound event.
///
/// There is deliberately no `Publish` variant. The engine cannot ask for a post to be sent, so no
/// future edit can quietly turn an inbound message into an outbound one without adding a variant
/// here — which is a visible change to a public enum, not a line buried in a match arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Subscribe to a channel's mention filter, resuming from the stored cursor.
    SubscribeChannel { channel_id: String },
    /// Unsubscribe from a channel and discard anything queued for it.
    UnsubscribeChannel { channel_id: String },
    /// Hand the event to the gateway's existing job path, untouched.
    ForwardToJobPath { kind: u16 },
}

/// What to do about a `CLOSED` frame. Mirrors [`dialect::ClosedVerdict`] but in terms of this
/// node's subscriptions rather than the wire's wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosedAction {
    /// Drop this one channel; the socket and every other subscription on it are fine.
    DropChannel { channel_id: String },
    /// ★ The subscription never existed server-side, so nothing is queued behind it and waiting
    /// will wait forever. Re-send the REQ after the hinted delay.
    ResendAfter {
        channel_id: Option<String>,
        hint_secs: Option<u64>,
    },
    /// The socket's authentication is the problem; reconnect before re-subscribing anything.
    ReconnectRelay,
    /// Nothing to do — either our own close coming back, or a reason we do not act on.
    Ignore,
}

/// Per-relay participation state and the rules over it.
pub struct Engine {
    store: SellerStore,
    relay_url: String,
    me: PublicKey,
}

impl Engine {
    pub fn new(store: SellerStore, relay_url: String, me: PublicKey) -> Self {
        Self {
            store,
            relay_url,
            me,
        }
    }

    /// The channels to subscribe on boot: the ones we were in when we stopped.
    ///
    /// Resuming the membership filter is not enough to rebuild this. The cursor makes that filter
    /// resume from where it left off, which means the `44100` that admitted us is exactly the
    /// event we will not be shown again — membership has to be remembered, not re-derived.
    pub fn channels_to_resume(&self) -> Result<Vec<String>, StoreError> {
        self.store.joined_channels(&self.relay_url)
    }

    /// The stored cursor for the global membership filter.
    pub fn membership_cursor(&self) -> Result<Option<u64>, StoreError> {
        Ok(self
            .store
            .participation_cursor(&self.relay_url, MEMBERSHIP_FILTER_ID)?
            .map(|since| since as u64))
    }

    /// The stored cursor for one channel's mention filter.
    pub fn channel_cursor(&self, channel_id: &str) -> Result<Option<u64>, StoreError> {
        Ok(self
            .store
            .participation_cursor(&self.relay_url, &channel_filter_id(channel_id))?
            .map(|since| since as u64))
    }

    /// Apply one inbound event: record whatever it means durably, and return what the caller must
    /// do on the wire.
    ///
    /// Durable state is written BEFORE the action is returned. A crash between the two replays the
    /// subscribe (idempotent); a crash the other way around would leave us subscribed to a channel
    /// we have no record of, which no restart would ever repair.
    pub fn ingest(&self, event: &Event, now_unix: i64) -> Result<Vec<Action>, StoreError> {
        let actions = match triage(event, self.me) {
            Triage::ChannelJoined { channel_id } => {
                let newly = self.store.record_channel_joined(
                    &self.relay_url,
                    &channel_id,
                    &event.id.to_hex(),
                    now_unix,
                )?;
                // A re-delivered 44100 for a channel we are already in produces no action. This is
                // the ordinary case after a reconnect, not an anomaly.
                if newly {
                    vec![Action::SubscribeChannel { channel_id }]
                } else {
                    Vec::new()
                }
            }
            Triage::ChannelLeft { channel_id } => {
                let was_member = self.store.record_channel_left(
                    &self.relay_url,
                    &channel_id,
                    &event.id.to_hex(),
                    now_unix,
                )?;
                if was_member {
                    vec![Action::UnsubscribeChannel { channel_id }]
                } else {
                    Vec::new()
                }
            }
            Triage::AddressedToUs {
                channel_id,
                counterparty,
                kind,
            } => {
                self.store.record_owed(
                    &OwedResponse {
                        event_id: event.id.to_hex(),
                        relay_url: self.relay_url.clone(),
                        channel_id,
                        counterparty,
                        kind,
                        created_at_unix: event.created_at.as_u64() as i64,
                    },
                    now_unix,
                )?;
                // No action: recording the debt IS the response for now. Answering arrives with
                // the mind.
                Vec::new()
            }
            Triage::TradePath { kind } => vec![Action::ForwardToJobPath { kind }],
            // Ambient traffic is read; it is not acted on and it is not stored. A channel we are
            // in produces a message every few seconds and none of them are ours to answer.
            Triage::Ambient { .. } | Triage::Unhandled { .. } => Vec::new(),
        };

        self.advance_cursor_for(event, now_unix)?;
        Ok(actions)
    }

    /// Move the cursor of whichever filter delivered this event.
    ///
    /// Which filter matters: membership notifications and channel mentions arrive on different
    /// subscriptions that advance at completely different rates, and crediting one filter's
    /// progress to the other is how a quiet channel silently skips its own backlog.
    fn advance_cursor_for(&self, event: &Event, now_unix: i64) -> Result<(), StoreError> {
        let kind = event.kind.as_u16();
        let filter_id =
            if kind == dialect::KIND_MEMBER_ADDED || kind == dialect::KIND_MEMBER_REMOVED {
                MEMBERSHIP_FILTER_ID.to_string()
            } else {
                match dialect::channel_of(event) {
                    Some(channel_id) => channel_filter_id(&channel_id),
                    // An event with no channel came from neither of our two filters; there is no
                    // cursor it belongs to.
                    None => return Ok(()),
                }
            };
        self.store.advance_participation_cursor(
            &self.relay_url,
            &filter_id,
            event.created_at.as_u64() as i64,
            now_unix,
        )
    }

    /// Decide what a `CLOSED` frame means for us.
    ///
    /// `channel_id` is whichever channel the closed subscription belonged to, or `None` for the
    /// global membership subscription.
    pub fn on_closed(
        &self,
        channel_id: Option<String>,
        reason: &str,
        now_unix: i64,
    ) -> Result<ClosedAction, StoreError> {
        Ok(match dialect::classify_closed(reason) {
            dialect::ClosedVerdict::DropChannel => match channel_id {
                Some(channel_id) => {
                    // Losing access is a membership fact, so it is recorded exactly as a 44101 is
                    // — otherwise a restart would cheerfully re-subscribe to a channel the relay
                    // has already told us we cannot read, forever.
                    self.store.record_channel_left(
                        &self.relay_url,
                        &channel_id,
                        &format!("closed:{reason}"),
                        now_unix,
                    )?;
                    ClosedAction::DropChannel { channel_id }
                }
                // A `restricted:` on the GLOBAL membership subscription is not about one channel —
                // it is the relay refusing the p-gated notification feed itself, which is a
                // relay-level access problem.
                None => ClosedAction::ReconnectRelay,
            },
            dialect::ClosedVerdict::RetryAfter { hint_secs } => ClosedAction::ResendAfter {
                channel_id,
                hint_secs,
            },
            dialect::ClosedVerdict::Reauthenticate => ClosedAction::ReconnectRelay,
            dialect::ClosedVerdict::Acknowledged => ClosedAction::Ignore,
            dialect::ClosedVerdict::Unknown(_) => ClosedAction::Ignore,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::prelude::{EventBuilder, Keys, Kind, Tag, TagKind, Timestamp};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    const RELAY: &str = "wss://relay.example";

    fn engine(label: &str) -> (Engine, Keys, std::path::PathBuf) {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "mobee-participation-engine-{label}-{}-{id}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        let me = Keys::generate();
        let engine = Engine::new(store, RELAY.to_string(), me.public_key());
        (engine, me, path)
    }

    fn event(
        author: &Keys,
        kind: u16,
        channel: Option<&str>,
        p: Option<PublicKey>,
        at: u64,
    ) -> Event {
        let mut tags = Vec::new();
        if let Some(channel) = channel {
            tags.push(Tag::custom(TagKind::custom("h"), [channel]));
        }
        if let Some(p) = p {
            tags.push(Tag::public_key(p));
        }
        EventBuilder::new(Kind::from(kind), "")
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(at))
            .sign_with_keys(author)
            .expect("sign")
    }

    #[test]
    fn an_invite_subscribes_the_channel_and_a_replayed_one_does_not() {
        let (engine, me, path) = engine("invite");
        let relay_key = Keys::generate();
        let invite = event(
            &relay_key,
            dialect::KIND_MEMBER_ADDED,
            Some("chan-1"),
            Some(me.public_key()),
            1_000,
        );

        assert_eq!(
            engine.ingest(&invite, 1_001).expect("ingest"),
            vec![Action::SubscribeChannel {
                channel_id: "chan-1".into()
            }]
        );
        // Reconnect replay: same event again. Subscribing twice is not harmful, but reporting it
        // as new work would make every reconnect look like a fresh wave of invites.
        assert_eq!(engine.ingest(&invite, 1_002).expect("replay"), Vec::new());
        assert_eq!(engine.channels_to_resume().expect("resume"), ["chan-1"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_removal_unsubscribes_and_survives_restart_as_not_a_member() {
        let (engine, me, path) = engine("removal");
        let relay_key = Keys::generate();
        engine
            .ingest(
                &event(
                    &relay_key,
                    dialect::KIND_MEMBER_ADDED,
                    Some("chan-1"),
                    Some(me.public_key()),
                    1_000,
                ),
                1_001,
            )
            .expect("join");

        let removal = event(
            &relay_key,
            dialect::KIND_MEMBER_REMOVED,
            Some("chan-1"),
            Some(me.public_key()),
            2_000,
        );
        assert_eq!(
            engine.ingest(&removal, 2_001).expect("leave"),
            vec![Action::UnsubscribeChannel {
                channel_id: "chan-1".into()
            }]
        );
        // The whole point of leg 2: nothing re-subscribes it, so there is no reconnect loop.
        assert!(engine.channels_to_resume().expect("resume").is_empty());
        assert_eq!(engine.ingest(&removal, 2_002).expect("replay"), Vec::new());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_mention_lands_as_a_debt_and_asks_for_nothing_on_the_wire() {
        let (engine, me, path) = engine("mention");
        let them = Keys::generate();
        let mention = event(
            &them,
            dialect::KIND_CHANNEL_MESSAGE,
            Some("chan-1"),
            Some(me.public_key()),
            1_500,
        );

        // ★ No action. The engine has no way to answer, and no variant that could ask for one.
        assert_eq!(engine.ingest(&mention, 1_501).expect("ingest"), Vec::new());
        let owed = engine.store.owed_responses().expect("owed");
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].counterparty, them.public_key().to_hex());
        assert_eq!(owed[0].created_at_unix, 1_500);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_message_redelivered_after_a_restart_is_ingested_exactly_once() {
        let (engine, me, path) = engine("exactly-once");
        let them = Keys::generate();
        let mention = event(
            &them,
            dialect::KIND_CHANNEL_MESSAGE,
            Some("chan-1"),
            Some(me.public_key()),
            1_500,
        );

        engine.ingest(&mention, 1_501).expect("first");
        // A reconnect deliberately re-asks from before the cursor, so this is the normal path, not
        // an edge case.
        engine.ingest(&mention, 9_999).expect("replay");

        assert_eq!(engine.store.owed_responses().expect("owed").len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn each_filter_advances_only_its_own_cursor() {
        let (engine, me, path) = engine("cursors");
        let relay_key = Keys::generate();
        let them = Keys::generate();

        engine
            .ingest(
                &event(
                    &relay_key,
                    dialect::KIND_MEMBER_ADDED,
                    Some("chan-1"),
                    Some(me.public_key()),
                    5_000,
                ),
                5_001,
            )
            .expect("membership");
        engine
            .ingest(
                &event(
                    &them,
                    dialect::KIND_CHANNEL_MESSAGE,
                    Some("chan-1"),
                    Some(me.public_key()),
                    1_000,
                ),
                1_001,
            )
            .expect("mention");

        // The busy membership filter must not drag the channel filter forward past the backlog it
        // has not read yet.
        assert_eq!(engine.membership_cursor().expect("membership"), Some(5_000));
        assert_eq!(
            engine.channel_cursor("chan-1").expect("channel"),
            Some(1_000)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ambient_traffic_moves_the_cursor_and_nothing_else() {
        let (engine, _me, path) = engine("ambient");
        let them = Keys::generate();
        let chatter = event(
            &them,
            dialect::KIND_CHANNEL_MESSAGE,
            Some("chan-1"),
            None,
            3_000,
        );

        assert_eq!(engine.ingest(&chatter, 3_001).expect("ingest"), Vec::new());
        assert!(engine.store.owed_responses().expect("owed").is_empty());
        // Still credited: a message we read but did not act on is still a message we have seen, and
        // a cursor that ignored it would replay the channel's whole chatter on every reconnect.
        assert_eq!(
            engine.channel_cursor("chan-1").expect("cursor"),
            Some(3_000)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_trade_event_goes_to_the_job_path_and_leaves_no_conversational_trace() {
        let (engine, me, path) = engine("trade");
        let buyer = Keys::generate();
        let offer = event(
            &buyer,
            crate::kinds::JOB_OFFER_KIND,
            Some("chan-1"),
            Some(me.public_key()),
            4_000,
        );

        assert_eq!(
            engine.ingest(&offer, 4_001).expect("ingest"),
            vec![Action::ForwardToJobPath {
                kind: crate::kinds::JOB_OFFER_KIND
            }]
        );
        assert!(engine.store.owed_responses().expect("owed").is_empty());
        let _ = std::fs::remove_file(&path);
    }

    // ── CLOSED handling ──────────────────────────────────────────────────────────────────────

    #[test]
    fn a_per_channel_refusal_drops_that_channel_permanently_not_the_socket() {
        let (engine, me, path) = engine("closed-restricted");
        let relay_key = Keys::generate();
        engine
            .ingest(
                &event(
                    &relay_key,
                    dialect::KIND_MEMBER_ADDED,
                    Some("chan-1"),
                    Some(me.public_key()),
                    1_000,
                ),
                1_001,
            )
            .expect("join");

        let action = engine
            .on_closed(
                Some("chan-1".into()),
                "restricted: not a channel member",
                2_000,
            )
            .expect("closed");

        assert_eq!(
            action,
            ClosedAction::DropChannel {
                channel_id: "chan-1".into()
            }
        );
        // Recorded like a removal — otherwise the next restart re-subscribes to a channel the
        // relay has already refused, which is the reconnect loop leg 2 forbids.
        assert!(engine.channels_to_resume().expect("resume").is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_rate_limit_asks_for_a_resend_because_the_subscription_never_existed() {
        let (engine, _me, path) = engine("closed-rate");
        assert_eq!(
            engine
                .on_closed(Some("chan-1".into()), "rate-limited: retry in 30s", 1_000)
                .expect("closed"),
            ClosedAction::ResendAfter {
                channel_id: Some("chan-1".into()),
                hint_secs: Some(30)
            }
        );
        // ★ And the channel stays joined: a rate limit is not a loss of access, and recording it as
        // one would silently drop a channel we are still a member of.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_auth_failure_reconnects_the_relay_rather_than_blaming_a_channel() {
        let (engine, _me, path) = engine("closed-auth");
        assert_eq!(
            engine
                .on_closed(
                    Some("chan-1".into()),
                    "auth-required: authenticate first",
                    1_000
                )
                .expect("closed"),
            ClosedAction::ReconnectRelay
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_restriction_on_the_global_feed_is_a_relay_problem_not_a_channel_one() {
        let (engine, _me, path) = engine("closed-global");
        // No channel id ⇒ the membership subscription itself was refused. Treating this as
        // "drop a channel" would silently do nothing while the node went deaf.
        assert_eq!(
            engine
                .on_closed(None, "restricted: not a channel member", 1_000)
                .expect("closed"),
            ClosedAction::ReconnectRelay
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn our_own_close_coming_back_is_not_treated_as_a_refusal() {
        let (engine, me, path) = engine("closed-empty");
        let relay_key = Keys::generate();
        engine
            .ingest(
                &event(
                    &relay_key,
                    dialect::KIND_MEMBER_ADDED,
                    Some("chan-1"),
                    Some(me.public_key()),
                    1_000,
                ),
                1_001,
            )
            .expect("join");

        assert_eq!(
            engine
                .on_closed(Some("chan-1".into()), "", 2_000)
                .expect("closed"),
            ClosedAction::Ignore
        );
        // Critically, it did NOT drop the channel.
        assert_eq!(engine.channels_to_resume().expect("resume"), ["chan-1"]);
        let _ = std::fs::remove_file(&path);
    }
}
