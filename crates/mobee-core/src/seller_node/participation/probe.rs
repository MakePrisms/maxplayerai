//! The positive access probe: proving a relay will actually serve us, rather than assuming it.
//!
//! # Why silence cannot be read
//!
//! Buzz narrows a global `REQ` to the channels the asker may see. A healthy, authenticated
//! connection with no admission therefore answers `EOSE` and nothing else — byte-identical to a
//! relay that genuinely holds no matching events, and identical again to a relay that holds
//! everything but has revoked us. Any classifier built on "did anything come back" reports the
//! same thing in all three cases, which makes it not a classifier.
//!
//! An `EOSE` round-trip does not close the gap either. It proves the relay accepts and answers our
//! subscriptions on this authenticated session — the write and subscribe paths — and an
//! access-scoped relay passes it while serving us nothing.
//!
//! So the probe asks a question that has exactly one silent answer and one loud one: **publish an
//! event, then ask the relay for that event by id.** Reading our own event back off the wire proves
//! the relay stored it, matched it, and served it *to us*. Nothing else does.
//!
//! # What it publishes: an event it was handed, never one it wrote
//!
//! [`probe_access`] takes the carrier event as an argument. It has no builder, no signer, and no
//! way to author anything — so "this module never originates a post" is enforced by its signature
//! rather than by discipline. The caller hands it the persona (kind-0) the node publishes on every
//! relay it participates on anyway, which makes the probe's write the same write the node already
//! owed that relay.
//!
//! ★ The carrier must be a **stored** kind. The tidier-sounding choice — ride the presence
//! heartbeat — is impossible: presence is kind 20001, inside NIP-01's ephemeral range
//! (20000–29999), and relays do not store ephemeral events. Nobody can read one back, so the round
//! trip cannot close. That is a protocol fact, not a relay quirk, and it is toothed below.
//!
//! # ★ Why the probe needs TWO clients
//!
//! `nostr-sdk` 0.44 saves an event into the publishing client's own database *before* transmitting
//! (`nostr-relay-pool-0.44.1`, `src/pool/mod.rs`), and the inbound handler then skips anything
//! already in that database (`src/relay/inner.rs`, the `DatabaseEventStatus::Saved` arm). **A
//! client cannot observe its own published events** — not on the notification stream, and not
//! through `fetch_events` either. No error, no log, no `CLOSED`: just silence, which on this wire
//! is the one answer that means nothing.
//!
//! So publish and read-back cannot be the same client, and both legs are toothed below. Reading
//! the library source is not enough to establish this — an earlier pass through that source
//! concluded `fetch_events` was unaffected, and the test in this file is what disproved it.
//!
//! ⚠ **Do not consolidate these two clients.** Two connections where one would "obviously" do
//! reads as pure waste, and removing the second one leaves every test green while turning this
//! probe into a function that returns [`ProbeOutcome::EchoMissing`] for relays that are working
//! perfectly. This exact optimisation has already been made once in this codebase, on the seller
//! heartbeat watchdog, and the symptom surfaced far from the diff that caused it.
//!
//! The suite also carries the leg that fails if `fetch_events` ever starts answering from the
//! local database instead of the wire — because on that day this probe would pass for every relay
//! on earth while proving nothing at all.

use std::time::Duration;

use nostr_sdk::prelude::{Client, Event, Filter};

use super::relays::ProbeOutcome;

/// Publish `carrier` through `publisher`, then try to read it back through `reader`.
///
/// Two requirements the caller has to meet, both of which fail silently rather than loudly:
///
/// - `publisher` and `reader` MUST be different [`Client`] instances on the same relay. Passing one
///   client twice compiles, connects, publishes, and then reports [`ProbeOutcome::EchoMissing`]
///   forever.
/// - `carrier` MUST be a stored kind. An ephemeral one (presence, 20001) can never come back.
///
/// [`carrier_is_storable`] checks the second; the first is the module note's ⚠.
///
/// The outcome is deliberately blunt: either the echo arrived ([`ProbeOutcome::EchoObserved`]) or
/// admission is unproven and the relay is not used. "Unproven" is not a claim that the relay is
/// broken or empty — it is the only thing we may act on.
///
/// # The assumption this probe rests on, stated because it is not self-evident
///
/// The carrier is a **kind-0 persona**, but the surface we actually need is the `#p`-gated one
/// (44100/44101 and channel reads). Those are not the same surface, and a relay is free to serve
/// public metadata while refusing p-gated reads — in which case this probe would report `Admitted`
/// for a relay we cannot read a single membership event from.
///
/// It holds on the deployed relay, and that was measured rather than assumed: after the throwaway
/// key's membership row was deleted, a freshly published carrier came back `EchoMissing` — the write
/// was accepted and the read returned nothing, so kind-0 reads there ARE membership-gated. Evidence:
/// `participation-s1-live-artifacts/*-L7-postrevoke.log`.
///
/// It is NOT guaranteed for a relay we have not characterised, so it is a single-relay assumption
/// carried deliberately into a single-relay slice. The stronger shape, for the multi-relay slice, is
/// to promote only on a positive read of the **access-scoped** surface itself — publish nothing and
/// require a `44100`/`44101` for `me` to come back, falling back to this echo only where the roster
/// records that the relay has been characterised.
pub async fn probe_access(
    publisher: &Client,
    relay_url: &str,
    reader: &Client,
    carrier: &Event,
    timeout: Duration,
) -> ProbeOutcome {
    if !carrier_is_storable(carrier) {
        // Refuse rather than run: an ephemeral carrier would deny every relay it touched, and the
        // denial would look exactly like a relay refusing us.
        //
        // ★ THE ONE `Refused` HERE THAT IS NOT THE RELAY SPEAKING, AND IT KEEPS THE TERMINAL VERDICT
        // ANYWAY. Everything else in this function that fails without a word from the relay is a
        // silence, because a silence may pass; this cannot. The carrier is ours, the kind is ours,
        // and no number of retries turns an ephemeral event into a stored one — a bounded retry here
        // would spend the whole attempt budget of every relay in the config to learn the same thing
        // three times. Terminal is the accurate verdict; only the word is borrowed.
        return ProbeOutcome::Refused(format!(
            "probe carrier is kind {} — ephemeral kinds are not stored by relays, so the echo can \
             never arrive; use a stored kind (the persona) as the carrier",
            carrier.kind.as_u16()
        ));
    }

    let event_id = carrier.id;

    // ★ `send_event_to`, NEVER `send_event`. The publisher holds every configured relay, and
    // `send_event` writes to ALL of them — so probing one relay would publish the carrier to relays
    // we have not proven, and to relays already classified denied. The roster gates which relay we
    // ADDRESS, but it cannot gate a client that was handed the whole list; targeting the URL here is
    // what makes "a denied relay gets nothing" true of the publish path too, not just the REQ path.
    match publisher.send_event_to([relay_url], carrier).await {
        // ★ SILENCE, NOT REFUSAL. This is the SDK failing to hand the event to a socket; the relay
        // has not said a word. The label was free while `Refused` and `EchoMissing` differed only in
        // their wording — both meant unproven, therefore unused. Once refusal became TERMINAL and
        // silence became a bounded retry, the same label started costing the relay permanently.
        Err(error) => {
            return ProbeOutcome::Unreachable(format!(
                "the probe publish could not be sent: {error}"
            ));
        }
        // ★ A pool `Ok` is acceptance, not delivery — see [`super::undelivered`]. Both outcomes are
        // fail-closed, so this is not an admission bug; it is a DIAGNOSIS bug, and those are the ones
        // that waste an operator's afternoon. Without it a carrier that never left the process would be
        // reported as `EchoMissing` — "the relay did not retain it" — which blames the relay for our own
        // disconnected socket, after burning the full probe timeout waiting for an echo of nothing.
        Ok(output) => {
            if let Some(why) = super::undelivered(&output) {
                // ★★ SILENCE, AND DELIBERATELY NOT A DISCRIMINATED VERDICT. The tempting split is
                // `output.failed`: non-empty looks like the relay answering `OK: false`, empty like
                // nothing ever taking the event. It does not hold — the pool puts SEND failures in
                // that same map, so `failed` sits NEXT TO "the relay refused us" rather than meaning
                // it, and a socket that died mid-publish would be read as a policy rejection. The
                // property wanted is "did the relay speak?", and nothing reachable here has access
                // to it.
                //
                // So it goes to the side whose mistake is survivable, and the asymmetry is not
                // close: a refusal misread as silence costs THREE bounded attempts and then denies
                // itself anyway, while a silence misread as a refusal costs the relay permanently —
                // and, with `note_new_signal` currently wired to nothing, permanently means for the
                // life of the process. The charter's "never poll a refusal" is kept in substance:
                // this stops, for good, after a bounded count.
                return ProbeOutcome::Unreachable(format!(
                    "the probe publish reached no relay: {why}"
                ));
            }
        }
    }

    match reader
        .fetch_events(Filter::new().id(event_id), timeout)
        .await
    {
        Ok(events) if events.iter().any(|event| event.id == event_id) => ProbeOutcome::EchoObserved,
        Ok(_) => ProbeOutcome::EchoMissing,
        // A failed read is not a denial of access — it is a failure to learn.
        //
        // ★ THAT SENTENCE WAS ALREADY HERE, ABOVE A `Refused`. It was TRUE when it was written: a
        // missing echo and a refusal both meant unproven-therefore-unused, so "lands on the same
        // side" described the code accurately and the word chosen did not matter. Splitting the
        // verdicts falsified it in place, and a comment cannot recompile — so the sentence went on
        // asserting a property the code had lost. A reason we failed to OBTAIN is not a refusal we
        // were GIVEN, and now the label says so.
        Err(error) => ProbeOutcome::Unreachable(format!("could not read the probe back: {error}")),
    }
}

/// Whether a relay will retain this event long enough to answer for it.
///
/// NIP-01 reserves 20000–29999 for ephemeral events, which relays broadcast and drop. Everything
/// else is stored (regular), replaced (10000–19999, 0, 3) or addressable (30000–39999) — all of
/// which remain fetchable, which is all the probe needs.
pub fn carrier_is_storable(event: &Event) -> bool {
    !(20_000..30_000).contains(&event.kind.as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
    use nostr_sdk::prelude::{EventBuilder, Keys, Kind, RelayPoolNotification};

    async fn start_relay() -> (LocalRelay, String) {
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let url = relay.url().await.to_string();
        (relay, url)
    }

    async fn connect(url: &str, keys: Keys) -> Client {
        let client = Client::new(keys);
        client.add_relay(url).await.expect("add relay");
        client.connect().await;
        client.wait_for_connection(Duration::from_secs(5)).await;
        client
    }

    async fn relay_and_client() -> (LocalRelay, Client, Keys) {
        let (relay, url) = start_relay().await;
        let keys = Keys::generate();
        let client = connect(&url, keys.clone()).await;
        (relay, client, keys)
    }

    fn beat(keys: &Keys) -> nostr_sdk::Event {
        EventBuilder::new(
            Kind::Custom(super::super::super::buzz::PRESENCE_KIND),
            "online",
        )
        .sign_with_keys(keys)
        .expect("sign")
    }

    fn persona(keys: &Keys) -> nostr_sdk::Event {
        EventBuilder::new(Kind::Metadata, r#"{"name":"probe"}"#)
            .sign_with_keys(keys)
            .expect("sign")
    }

    /// The mechanism the probe stands on: publish on one client, read it back on another.
    #[tokio::test]
    async fn a_second_client_reads_the_published_event_back_by_id() {
        let (_relay, url) = start_relay().await;
        let keys = Keys::generate();
        let publisher = connect(&url, keys.clone()).await;
        let reader = connect(&url, Keys::generate()).await;
        let event = persona(&keys);

        publisher.send_event(&event).await.expect("publish");
        let found = reader
            .fetch_events(Filter::new().id(event.id), Duration::from_secs(5))
            .await
            .expect("fetch");

        assert!(
            found.iter().any(|seen| seen.id == event.id),
            "a separate reader must see the published event — the whole probe depends on it"
        );
    }

    /// ★ Why the probe does NOT ride the presence heartbeat, despite that being the tidiest
    /// reactive-only story.
    ///
    /// Presence is kind 20001, inside NIP-01's ephemeral range (20000–29999). Relays are specified
    /// not to store ephemeral events, so no reader can ever fetch one back — the round trip is
    /// impossible by protocol, not by bug, and no amount of waiting or reconnecting changes it.
    #[tokio::test]
    async fn the_presence_beat_is_ephemeral_and_can_never_be_read_back() {
        let (_relay, url) = start_relay().await;
        let keys = Keys::generate();
        let publisher = connect(&url, keys.clone()).await;
        let reader = connect(&url, Keys::generate()).await;
        let event = beat(&keys);
        assert!(
            (20_000..30_000).contains(&event.kind.as_u16()),
            "the premise of this test is that presence sits in the ephemeral range"
        );

        publisher.send_event(&event).await.expect("publish");
        let found = reader
            .fetch_events(Filter::new().id(event.id), Duration::from_secs(3))
            .await
            .expect("fetch");

        assert!(
            found.is_empty(),
            "an ephemeral event came back — if relays now store these, presence becomes usable as \
             the probe carrier and this module can stop republishing the persona"
        );
    }

    /// ★ Why the probe takes two clients, kept as an executable statement rather than a comment.
    ///
    /// The publishing client stores the event locally before transmitting and then suppresses it
    /// on the way back in. `fetch_events` does not escape this — an earlier reading of the library
    /// source concluded that it did, and this test is what settled it.
    #[tokio::test]
    async fn the_publishing_client_cannot_read_its_own_event_back() {
        let (_relay, client, keys) = relay_and_client().await;
        let event = beat(&keys);

        client.send_event(&event).await.expect("publish");
        let found = client
            .fetch_events(Filter::new().id(event.id), Duration::from_secs(3))
            .await
            .expect("fetch");

        assert!(
            found.is_empty(),
            "the publisher saw its own event — if nostr-sdk has fixed this, the probe may collapse \
             to a single client, but nothing should assume it until this test says so"
        );
    }

    /// ★ The red leg for the leg above. If `fetch_events` ever answered out of the client's local
    /// database, the probe would return `EchoObserved` for a relay that never received anything —
    /// passing everywhere, proving nothing. So: put an event in the local DB that the relay has
    /// never seen, and require the fetch to come back EMPTY.
    #[tokio::test]
    async fn fetch_reads_the_wire_and_not_the_local_database() {
        let (_relay, client, keys) = relay_and_client().await;
        let event = beat(&keys);

        client
            .database()
            .save_event(&event)
            .await
            .expect("seed the local database only");

        let found = client
            .fetch_events(Filter::new().id(event.id), Duration::from_secs(2))
            .await
            .expect("fetch");

        assert!(
            found.is_empty(),
            "fetch_events answered from the local database — the access probe is now vacuous \
             and every relay would classify as admitted"
        );
    }

    /// The trap itself, kept as a live tooth: the notification stream really does swallow our own
    /// events, so nothing here may be rebuilt on top of it.
    #[tokio::test]
    async fn the_event_notification_stream_never_delivers_our_own_event() {
        let (_relay, client, keys) = relay_and_client().await;
        let event = beat(&keys);
        let mut notifications = client.notifications();

        client
            .subscribe(Filter::new().id(event.id), None)
            .await
            .expect("subscribe");
        client.send_event(&event).await.expect("publish");

        let own_event_seen = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match notifications.recv().await {
                    Ok(RelayPoolNotification::Event { event: seen, .. }) if seen.id == event.id => {
                        return true;
                    }
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);

        assert!(
            !own_event_seen,
            "the Event notification stream delivered our own event — if nostr-sdk has fixed this, \
             the probe can be simplified, but until then nothing may depend on that stream"
        );
    }

    /// A relay that never receives the publish cannot echo it, and the probe must say so rather
    /// than fall back on anything softer.
    #[tokio::test]
    async fn a_relay_that_never_got_the_event_yields_no_echo() {
        let (_relay, client, keys) = relay_and_client().await;
        let unpublished = beat(&keys);

        let found = client
            .fetch_events(Filter::new().id(unpublished.id), Duration::from_secs(2))
            .await
            .expect("fetch");

        assert!(found.is_empty());
    }

    // ── probe_access end to end ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_relay_that_serves_the_carrier_back_is_admitted() {
        let (_relay, url) = start_relay().await;
        let keys = Keys::generate();
        let publisher = connect(&url, keys.clone()).await;
        let reader = connect(&url, Keys::generate()).await;

        let outcome =
            probe_access(&publisher, &url, &reader, &persona(&keys), Duration::from_secs(5)).await;

        assert_eq!(outcome, ProbeOutcome::EchoObserved);
    }

    /// The reader is pointed at a relay that never saw the publish. Nothing refuses us, nothing
    /// errors — the read is simply empty, which is exactly the shape an access-scoped relay
    /// produces, and the probe must call it unproven rather than fine.
    #[tokio::test]
    async fn a_silent_relay_is_unproven_not_admitted() {
        let (_relay_a, url_a) = start_relay().await;
        let (_relay_b, url_b) = start_relay().await;
        let keys = Keys::generate();
        let publisher = connect(&url_a, keys.clone()).await;
        let reader = connect(&url_b, Keys::generate()).await;

        let outcome =
            probe_access(&publisher, &url_a, &reader, &persona(&keys), Duration::from_secs(3)).await;

        assert_eq!(outcome, ProbeOutcome::EchoMissing);
    }

    /// ★★ THE SAFE SIDE OF THE VERDICT SPLIT. `Refused` is terminal and `Unreachable` is a bounded
    /// retry, so every site that reaches a verdict WITHOUT a word from the relay has to reach the
    /// second one. This publisher holds no relay and no socket, so the publish cannot leave the
    /// process — while the relay being judged is healthy, listening, and has refused nothing.
    ///
    /// Reading that as a refusal is how a momentary transport fault cost a relay permanently. It was
    /// harmless for as long as `Refused` and `EchoMissing` differed only in their wording; splitting
    /// them is what put a price on it, and this is the shape a seam in this module actually shipped.
    #[tokio::test]
    async fn a_publish_that_never_left_the_process_is_silence_not_refusal() {
        let (_relay, url) = start_relay().await;
        let keys = Keys::generate();
        // Deliberately NOT `connect`: no relay added, nothing dialled.
        let publisher = Client::new(keys.clone());
        let reader = connect(&url, Keys::generate()).await;

        let outcome =
            probe_access(&publisher, &url, &reader, &persona(&keys), Duration::from_secs(2)).await;

        match outcome {
            ProbeOutcome::Unreachable(_) => {}
            other => panic!(
                "a publish that reached no socket is SILENCE, and silence is retryable — `Refused` \
                 denies this relay for the life of the process over a fault that was ours: {other:?}"
            ),
        }
    }

    /// ★ THE SECOND WAY A PUBLISH FAILS WITHOUT THE RELAY SPEAKING, and it is a different branch —
    /// the test above dies inside `send_event_to` with "no relays", never reaching the delivery
    /// check. Here the pool HOLDS the relay and simply never connected it, so the send returns `Ok`
    /// and the relay lands in `failed`; `Ok` from the pool is acceptance, not delivery.
    ///
    /// Worth its own test because `failed` is the map a discriminator wants to read — non-empty
    /// looks exactly like the relay answering `OK: false`. It is not: send failures land there too,
    /// so the map sits NEXT TO "the relay refused us" rather than meaning it. This is the case that
    /// would be misread, and it must come out retryable.
    #[tokio::test]
    async fn a_publish_the_pool_accepted_but_never_delivered_is_silence_too() {
        let (_relay, url) = start_relay().await;
        let keys = Keys::generate();
        let publisher = Client::new(keys.clone());
        publisher.add_relay(&url).await.expect("add relay");
        // No `connect()`. The pool knows the relay and holds no socket to it.
        let reader = connect(&url, Keys::generate()).await;

        let outcome =
            probe_access(&publisher, &url, &reader, &persona(&keys), Duration::from_secs(2)).await;

        match outcome {
            ProbeOutcome::Unreachable(_) => {}
            other => panic!(
                "an undelivered publish is SILENCE — reading `failed` as a refusal denies a relay \
                 permanently for a socket that was never dialled: {other:?}"
            ),
        }
    }

    /// Guarding the caller mistake that would otherwise deny every relay it touched, while looking
    /// identical to a fleet-wide access revocation.
    ///
    /// ★ AND IT IS THE ONE `Refused` THIS FUNCTION STILL PRODUCES, which is why it is worth keeping
    /// sharp: everything that fails without the relay speaking is now a silence, so if this test ever
    /// goes green for an `Unreachable` the terminal branch has stopped being reachable at all.
    #[tokio::test]
    async fn an_ephemeral_carrier_is_refused_before_anything_goes_on_the_wire() {
        let (_relay, url) = start_relay().await;
        let keys = Keys::generate();
        let publisher = connect(&url, keys.clone()).await;
        let reader = connect(&url, Keys::generate()).await;

        let outcome =
            probe_access(&publisher, &url, &reader, &beat(&keys), Duration::from_secs(3)).await;

        match outcome {
            ProbeOutcome::Refused(reason) => assert!(
                reason.contains("ephemeral"),
                "the refusal must name the real cause, not read as a relay problem: {reason}"
            ),
            other => panic!("an ephemeral carrier must be refused up front, got {other:?}"),
        }
    }

    #[test]
    fn only_the_ephemeral_range_is_unstorable() {
        let keys = Keys::generate();
        assert!(carrier_is_storable(&persona(&keys)));
        assert!(!carrier_is_storable(&beat(&keys)));
    }
}
