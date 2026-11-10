//! #574 platform-contract test: a signature-INVALID nostr event is DROPPED at the client's relay
//! ingest, before it can reach any consumer.
//!
//! WHY THIS EXISTS — the platform assumption three money gates lean on. The buyer's
//! release-on-failure author gate ([`super::release_reservation_on_failure_feedback`]), the seller
//! node's award-author reject, and the pinned-seller delivery filter all trust that `event.pubkey`
//! is authentic — that the event was really signed by that key. Nothing in OUR code re-checks the
//! signature; we rely on the SDK verifying it at ingest. That reliance had no test. If a future
//! nostr-sdk upgrade, an options change, or a custom ingest path ever admitted unverified events,
//! all three gates would silently become spoofable at once and no red would fire. This is the one
//! boundary test that guards the class (#574), rather than one test per consuming site.
//!
//! WHAT IT PINS — and why it is not adjacent:
//! * PROD PATH. The event arrives over a REAL websocket from an in-process `LocalRelay`, so the
//!   client runs the exact inbound path production uses to receive from strfry/buzz
//!   (`nostr-relay-pool` `handle_relay_message` → `event.verify()`), not a server-only path.
//! * PROD CONFIG. The `Client` is built EXACTLY as [`super::relay::spawn`] builds it (`Client::new`
//!   + `automatic_authentication(true)` + `pool().add_relay(url, RelayOptions::default()
//!   .reconnect(true))`), so a config change that disabled verification would fail this test too.
//!   DRIFT GUARD: if `super::relay::spawn` ever gains a verify-affecting option, mirror it here.
//! * FORGERY, NOT DEDUP. `LocalRelay::notify_event` broadcasts straight to subscribers, bypassing
//!   the RELAY server's own verify — so the CLIENT's verify is the sole thing that can drop the
//!   forged event. The forged event carries a DISTINCT id from the control, so a relay/client dedup
//!   could never be the reason it is absent.
//! * SIGNATURE, NOT ID. Only the signature is corrupted (the id — hence content and pubkey — is
//!   left intact), so `verify_id` passes and only `verify_signature` can reject it.
//!
//! The load-bearing red (that this test catches "the SDK silently stops verifying") is produced out
//! of band by no-op'ing `nostr::Event::verify` via a worktree-local `[patch.crates-io]` and
//! observing this test fail at the drop assertion — a throwaway red-prove, never committed.

use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::{
    Client, Event, EventBuilder, Filter, JsonUtil, Keys, Kind, PublicKey, RelayOptions,
    RelayPoolNotification, Tag,
};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A validly-signed kind-3404 FEEDBACK from `author`, on the wire [`crate::gateway::error_draft`]
/// emits (`status=error` + `reason_code`). `offer_id` distinguishes otherwise-identical events so
/// two of them get DISTINCT ids — an id collision would let dedup, not verify, decide the outcome.
fn signed_feedback(offer_id: &str, author: &Keys) -> Event {
    let buyer_hex = Keys::generate().public_key().to_hex();
    let tags = [
        vec!["status".to_owned(), "error".to_owned()],
        vec!["e".to_owned(), offer_id.to_owned(), String::new(), "root".to_owned()],
        vec!["p".to_owned(), buyer_hex],
        vec!["p".to_owned(), author.public_key().to_hex()],
        vec!["reason_code".to_owned(), "delivery_failed".to_owned()],
    ];
    let mut builder = EventBuilder::new(
        Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND),
        format!("feedback for {offer_id}"),
    );
    builder.allow_self_tagging = true;
    for tag in tags {
        builder = builder.tag(Tag::parse(tag).expect("parse tag"));
    }
    builder.sign_with_keys(author).expect("sign feedback")
}

/// Corrupt ONLY the signature of `event`, keeping its id (content + pubkey) intact: the result is a
/// well-formed event whose `pubkey` claims `author` but whose signature `author` never produced —
/// exactly a spoofed-`event.pubkey` forgery. `Event::from_json` does NOT verify, so it happily
/// reparses; `verify_id` still passes and only `verify_signature` fails.
fn with_forged_signature(event: &Event) -> Event {
    let mut value = serde_json::to_value(event).expect("event to json");
    let sig = value["sig"].as_str().expect("sig hex").to_owned();
    // Flip the first hex nibble: a different 64-byte signature, still valid hex so parsing succeeds
    // and only signature verification can reject it.
    let mut chars: Vec<char> = sig.chars().collect();
    chars[0] = if chars[0] == '0' { '1' } else { '0' };
    value["sig"] = serde_json::Value::String(chars.into_iter().collect());
    let json = value.to_string();
    Event::from_json(&json).expect("reparse forged event (from_json does not verify)")
}

/// Collect the id hex of every FEEDBACK event the client SURFACES to consumers.
fn spawn_delivery_collector(client: &Client) -> Arc<Mutex<HashSet<String>>> {
    let seen = Arc::new(Mutex::new(HashSet::new()));
    let sink = Arc::clone(&seen);
    let mut notifications = client.notifications();
    tokio::spawn(async move {
        while let Ok(n) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = n {
                if event.kind == Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND) {
                    sink.lock().unwrap_or_else(|e| e.into_inner()).insert(event.id.to_hex());
                }
            }
        }
    });
    seen
}

fn was_seen(seen: &Arc<Mutex<HashSet<String>>>, id_hex: &str) -> bool {
    seen.lock().unwrap_or_else(|e| e.into_inner()).contains(id_hex)
}

async fn wait_seen(seen: &Arc<Mutex<HashSet<String>>>, id_hex: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if was_seen(seen, id_hex) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signature_invalid_event_is_dropped_at_client_ingest_before_any_consumer() {
    // A real in-process relay, and a client built EXACTLY as the buyer daemon builds it in prod.
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.expect("relay run");
    let url = relay.url().await.to_string();

    let author = Keys::generate(); // the "seller" whose pubkey a forgery would spoof
    let client = Client::new(Keys::generate());
    client.automatic_authentication(true);
    client
        .pool()
        .add_relay(&url, RelayOptions::default().reconnect(true))
        .await
        .expect("add relay");
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let seen = spawn_delivery_collector(&client);
    let author_pk: PublicKey = author.public_key();
    client
        .subscribe(
            Filter::new().kind(Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND)).author(author_pk),
            None,
        )
        .await
        .expect("subscribe");
    // Let the relay register the REQ before we inject onto the broadcast path.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // CONTROL — a validly-signed event delivers: proves the inject → ws → client → consumer path is
    // live, so a later non-arrival is meaningful rather than a dead path.
    let control = signed_feedback("control-offer", &author);
    assert!(relay.notify_event(control.clone()), "relay had a subscriber for the control");
    assert!(
        wait_seen(&seen, &control.id.to_hex(), Duration::from_secs(5)).await,
        "a validly-signed event must reach the consumer over the prod client ingest path"
    );

    // THE PROPERTY — a forged-signature event (distinct id, so dedup can never explain its absence)
    // must be dropped by the client's ingest verify and never surface.
    let base = signed_feedback("forged-offer", &author);
    let forged = with_forged_signature(&base);
    assert_eq!(forged.id, base.id, "forging the signature must not change the id (isolates the sig)");
    assert!(relay.notify_event(forged.clone()), "relay broadcast the forged event to the subscriber");

    // ORDERING BARRIER — not a fixed grace: inject a second valid event AFTER the forgery and wait
    // for IT. The broadcast preserves order and the client processes frames sequentially, so once
    // this later event has surfaced the forged one is already fully processed — its absence is then
    // conclusive, never merely "not yet".
    let tracer = signed_feedback("tracer-offer", &author);
    assert!(relay.notify_event(tracer.clone()), "relay broadcast the tracer");
    assert!(
        wait_seen(&seen, &tracer.id.to_hex(), Duration::from_secs(5)).await,
        "the tracer (injected after the forgery) must arrive — the barrier that makes absence conclusive"
    );
    assert!(
        !was_seen(&seen, &forged.id.to_hex()),
        "a signature-invalid event MUST NOT reach any consumer — the platform assumption #574 guards"
    );

    // SENSITIVITY (non-vacuity) — the SAME content/id/author, re-signed VALIDLY, DOES surface. The
    // only variable that changed between this and the dropped forgery is signature validity, so the
    // drop above is bound to the signature, not to the id, kind, author, or subscription filter.
    assert!(relay.notify_event(base.clone()), "relay broadcast the validly-signed original");
    assert!(
        wait_seen(&seen, &base.id.to_hex(), Duration::from_secs(5)).await,
        "the same event with a VALID signature must surface — proving the drop was the signature alone"
    );

    client.disconnect().await;
    drop(relay);
}

// ── RC3-02 (v0.7.3-rc3 external review): SAME-ID redelivery ─────────────────────────────────────
//
// The test above gives every forgery a DISTINCT id, so it cannot see the hole the review named: the
// SDK's verification cache is keyed by event id, and nostr-relay-pool 0.44.1 skipped verification
// for an id it had already saved. 0.44.3 (pinned by #1253) verifies before the admit policy and
// never caches a failure, but still treats a CACHED id as verified — so a redelivery that reuses a
// genuine event's id with altered content passes `verify_and_cache`, and is then re-emitted on the
// pool's raw `Message` stream (never as a new `Event`, because the id is already saved). The tests
// below pin the two SDK properties our live consumers rely on, and the app-side gates
// (`job_lifecycle::verified_author_hex`) are what close the raw-message / fetch path.

/// Re-serialize `event` with `edit` applied to its JSON; the claimed id and sig are kept unless the
/// edit changes them. `Event::from_json` does not verify.
fn tampered(event: &Event, edit: impl FnOnce(&mut serde_json::Value)) -> Event {
    let mut value = serde_json::to_value(event).expect("event to json");
    edit(&mut value);
    Event::from_json(value.to_string()).expect("reparse tampered event (from_json does not verify)")
}

/// Every FEEDBACK the client surfaces, split by stream: `(events, raw_messages)`, each as
/// `(id_hex, content)` so a same-id content swap is distinguishable from the genuine event.
type Seen = Arc<Mutex<Vec<(String, String)>>>;
fn spawn_split_collector(client: &Client) -> (Seen, Seen) {
    let (events, messages): (Seen, Seen) = Default::default();
    let (ev, msg) = (Arc::clone(&events), Arc::clone(&messages));
    let mut notifications = client.notifications();
    tokio::spawn(async move {
        while let Ok(n) = notifications.recv().await {
            match n {
                RelayPoolNotification::Event { event, .. }
                    if event.kind == Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND) =>
                {
                    ev.lock().unwrap_or_else(|e| e.into_inner()).push((event.id.to_hex(), event.content.clone()));
                }
                RelayPoolNotification::Message {
                    message: nostr_sdk::RelayMessage::Event { event, .. },
                    ..
                } if event.kind == Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND) => {
                    msg.lock().unwrap_or_else(|e| e.into_inner()).push((event.id.to_hex(), event.content.to_string()));
                }
                _ => {}
            }
        }
    });
    (events, messages)
}

fn has_content(seen: &Seen, content: &str) -> bool {
    seen.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(_, c)| c == content)
}

async fn wait_content(seen: &Seen, content: &str) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if has_content(seen, content) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn prod_client(url: &str, author: &Keys) -> Client {
    let client = Client::new(Keys::generate());
    client.automatic_authentication(true);
    client
        .pool()
        .add_relay(url, RelayOptions::default().reconnect(true))
        .await
        .expect("add relay");
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    client
        .subscribe(
            Filter::new().kind(Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND)).author(author.public_key()),
            None,
        )
        .await
        .expect("subscribe");
    client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_id_redelivery_with_altered_content_never_surfaces_as_an_event_nor_authorizes() {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.expect("relay run");
    let url = relay.url().await.to_string();
    let author = Keys::generate();
    let client = prod_client(&url, &author).await;
    let (events, messages) = spawn_split_collector(&client);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The genuine event: verified and cached by id on first sight.
    let genuine = signed_feedback("genuine-offer", &author);
    assert!(relay.notify_event(genuine.clone()));
    assert!(wait_content(&events, &genuine.content).await, "control: the genuine event surfaces");

    // The redelivery: SAME claimed id + signature, different content (retargeted at a victim job).
    let swapped = tampered(&genuine, |v| {
        v["content"] = serde_json::Value::String("forged: release the victim job".into());
    });
    assert_eq!(swapped.id, genuine.id, "the forgery reuses the genuine, already-verified id");
    assert!(swapped.verify().is_err(), "it does not verify on its own");
    assert!(relay.notify_event(swapped.clone()));

    // Ordering barrier: once a later event surfaces, the redelivery is fully processed.
    let tracer = signed_feedback("tracer-offer", &author);
    assert!(relay.notify_event(tracer.clone()));
    assert!(wait_content(&events, &tracer.content).await, "the tracer must arrive");

    // SDK property our LIVE loops rely on (buyer `forward_job_events`, seller work lane): the
    // de-duplicated `Event` stream never carries the swapped content.
    assert!(
        !has_content(&events, &swapped.content),
        "a same-id redelivery must never surface as a new Event notification"
    );
    // The raw `Message` stream (what `fetch_events` and backfills read) MAY carry it — that is the
    // residual SDK behaviour this PR's app-side gates exist for. Whatever surfaces there must fail
    // our gate's author check, so no money decision can be taken on it.
    if has_content(&messages, &swapped.content) {
        eprintln!("note: SDK re-emitted the same-id redelivery on the raw Message stream (cached id)");
    }
    assert_eq!(
        crate::job_lifecycle::verified_author_hex(&swapped),
        None,
        "the app-side gate refuses the same-id redelivery"
    );
    assert_eq!(
        crate::job_lifecycle::verified_author_hex(&genuine),
        Some(author.public_key().to_hex()),
        "and still accepts the genuine event"
    );

    client.disconnect().await;
    drop(relay);
}

// The poisoning direction, which 0.44.1 got wrong: a forgery that ARRIVES FIRST under an id must not
// make the genuine event with that id unreceivable (a failed verification must never be cached or
// saved). Red on nostr-relay-pool 0.44.1's "save before verify" ordering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forgery_arriving_first_does_not_block_the_genuine_event_with_its_id() {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.expect("relay run");
    let url = relay.url().await.to_string();
    let author = Keys::generate();
    let client = prod_client(&url, &author).await;
    let (events, _messages) = spawn_split_collector(&client);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let genuine = signed_feedback("first-forged-offer", &author);
    let forged_first = with_forged_signature(&genuine);
    assert_eq!(forged_first.id, genuine.id);
    assert!(relay.notify_event(forged_first));

    let tracer = signed_feedback("poison-tracer", &author);
    assert!(relay.notify_event(tracer.clone()));
    assert!(wait_content(&events, &tracer.content).await, "the tracer must arrive");
    assert!(
        !events.lock().unwrap().iter().any(|(id, _)| *id == genuine.id.to_hex()),
        "the forgery itself never surfaces"
    );

    assert!(relay.notify_event(genuine.clone()));
    assert!(
        wait_content(&events, &genuine.content).await,
        "the genuine event must still surface after a same-id forgery was dropped"
    );

    client.disconnect().await;
    drop(relay);
}
