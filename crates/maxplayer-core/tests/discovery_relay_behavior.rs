//! DISCOVERY, DRIVEN OVER A WIRE — the behavioral proof the unit tests cannot give.
//!
//! Advisor finding F3 against `aad7b2a`: the green gate did not exercise F1 at all. The
//! transport-shape tests compared CONSTRUCTORS and searched SOURCE STRINGS, the fixture started
//! from a hand-assembled unsigned draft rather than a signed announcement, and the target handoff
//! went through `OfferDraft`/`parse_offer` rather than the daemon's own post mapping. Every one of
//! those can pass while the relay leg is wrong, which is precisely what happened.
//!
//! So each test here drives the REAL [`discovery::fetch_directory_async`] against a scripted relay
//! and asserts on what came back:
//!
//! | Ending the relay scripts | What discovery must report |
//! |---|---|
//! | events + `EOSE` | rows, `read_confirmed = true` |
//! | no events + `EOSE` | empty, `read_confirmed = true` — an answered empty market |
//! | nothing at all, socket up | empty, `read_confirmed = FALSE` — an unanswered read |
//! | one event, then silence | that row KEPT, `read_confirmed = FALSE` |
//! | one event, then socket dropped | `read_confirmed = FALSE` |
//! | `CLOSED` naming our subscription | `Err(DiscoveryError::Relay)` carrying the reason |
//!
//! The announcements are SIGNED with a real key, built from a real `[seat]` config through the same
//! `Advertisement::capability` → `heartbeat_for_state` chain the seller daemon publishes through, so
//! a row here is evidence about the deployed path and not about a fixture. One test serves an event
//! whose signature has been tampered with and asserts it never becomes a row.
//!
//! No money anywhere: no wallet is opened, no mint is contacted, and the fixture records every
//! inbound frame so "discovery published nothing" is read off the wire rather than promised.

#![cfg(all(unix, feature = "wallet"))]

mod discovery_relay_fixture;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use discovery_relay_fixture::{Script, ScriptedRelay};

use maxplayer_core::discovery::{
    self, ADMISSION_UNSTATED, DEFAULT_DIRECTORY_LIMIT, DirectoryPolicy, DiscoveryError,
};
use maxplayer_core::heartbeat;
use maxplayer_core::home::{self, MaxplayerHome, SeatConfig, TargetedAdmission};
use maxplayer_core::seller_roster::Advertisement;

use nostr_sdk::prelude::{JsonUtil, Keys};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The subscription id the discovery read opens. Asserted on rather than imported: the fixture sees
/// the wire, and the wire is where the id has to be right for the EOSE match to mean anything.
const DIRECTORY_SUB_ID: &str = "maxplayer-discovery-directory";

const MINT: &str = "https://mint.example/Bitcoin";
const SPECIALTY: &str = "Rust async runtimes and tokio internals";

fn temp(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "maxplayer-discovery-{label}-{}-{id}",
        std::process::id()
    ))
}

/// A buyer home pointed at the scripted relay. Nothing else about it matters: discovery needs an
/// identity to authenticate a read as, and a relay url.
fn buyer_home(label: &str, relay_url: &str) -> MaxplayerHome {
    let root = temp(label);
    let mut home = home::bootstrap(&root).expect("bootstrap buyer home");
    home.config.relay_url = relay_url.to_owned();
    home
}

/// The buyer's wallet store. Its ABSENCE after a read is machine evidence that discovery opened no
/// wallet — `open_wallet_async` creates this file the moment it is called.
fn wallet_store(home: &MaxplayerHome) -> PathBuf {
    home.wallet_dir.join("cdk-wallet.sqlite")
}

/// A SIGNED kind-30340 announcement, built the way the seller daemon builds one.
///
/// The chain is the product chain: a `[seat]` config block → [`Advertisement::capability`] (the one
/// config-to-wire seam, where the specialty bound is applied) → [`heartbeat::heartbeat_for_state`]
/// → the event draft → signature. Starting from a hand-written tag set was exactly F3's complaint:
/// it lets the reader agree with a beat no seat would ever publish.
fn signed_announcement(keys: &Keys, specialty: Option<&str>, accepting: bool) -> String {
    let seat = SeatConfig {
        harness_variant: None,
        hardware: None,
        specialty: specialty.map(str::to_owned),
    };
    let advertisement = Advertisement {
        serving: accepting,
        names: vec!["claude".to_owned()],
        models: Vec::new(),
        capabilities: Vec::new(),
    };
    let capability = advertisement.capability(&seat);
    let draft = if accepting {
        heartbeat::heartbeat_for_state(
            0,
            true,
            12,
            false,
            vec![MINT.to_owned()],
            advertisement.names.clone(),
            capability,
            home::AdmissionPolicy {
                pool: true,
                targeted: TargetedAdmission::Open,
            },
        )
    } else {
        heartbeat::retraction_for_state(
            0,
            12,
            false,
            vec![MINT.to_owned()],
            advertisement.names.clone(),
            capability,
            home::AdmissionPolicy {
                pool: true,
                targeted: TargetedAdmission::Open,
            },
        )
    };
    let builder = maxplayer_core::gateway::nostr::event_builder(&draft.to_event_draft())
        .expect("announcement builder");
    builder
        .sign_with_keys(keys)
        .expect("sign announcement")
        .as_json()
}

/// The read under test, with a policy anchored to now and a short budget so an unanswered case is a
/// second of test time rather than eight.
async fn read(
    home: &MaxplayerHome,
    budget: Duration,
) -> Result<discovery::SellerDirectory, DiscoveryError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    discovery::fetch_directory_async(
        home,
        DirectoryPolicy::at(now),
        DEFAULT_DIRECTORY_LIMIT,
        budget,
    )
    .await
}

/// Every inbound verb, for the no-publication assertion. AUTH/REQ/CLOSE are the expected traffic of
/// a read; an EVENT would mean discovery published something.
async fn assert_read_only_traffic(relay: &ScriptedRelay) {
    let verbs = relay.verbs().await;
    assert!(
        !verbs.iter().any(|verb| verb == "EVENT"),
        "a read-only path must never put an EVENT on the wire: {verbs:?}"
    );
    for verb in &verbs {
        assert!(
            matches!(verb.as_str(), "REQ" | "CLOSE" | "AUTH" | "SOCKET_CLOSE"),
            "unexpected frame from a read-only path: {verb} in {verbs:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completed_empty_answer_is_a_confirmed_empty_market() {
    // EOSE with no events: the relay finished answering and holds nothing. This is the ONE shape in
    // which an empty directory may be reported as confirmed.
    let relay = ScriptedRelay::start(Script::ServeThenEose(Vec::new())).await;
    let home = buyer_home("empty-eose", &relay.url());

    let directory = read(&home, Duration::from_secs(5))
        .await
        .expect("an answered empty market is not an error");

    assert!(directory.sellers.is_empty());
    assert!(
        directory.read_confirmed,
        "an EOSE for our own subscription is exactly what confirms a read"
    );
    assert_eq!(directory.events_read, 0);
    assert_read_only_traffic(&relay).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_read_is_never_reported_as_an_empty_market() {
    // The relay takes the REQ and says nothing: socket healthy, answer never arrives. F1's first
    // trace. Before the fix this returned `empty_confirmed()` on the strength of a liveness probe
    // for a DIFFERENT subscription.
    let relay = ScriptedRelay::start(Script::ServeThenSilence(Vec::new())).await;
    let home = buyer_home("silent", &relay.url());

    let directory = read(&home, Duration::from_secs(1))
        .await
        .expect("an unanswered read is a directory we cannot vouch for, not an error");

    assert!(directory.sellers.is_empty());
    assert!(
        !directory.read_confirmed,
        "no EOSE ever came, so nothing may certify this emptiness"
    );
    // The REQ did reach the relay — this is a read that was ASKED and not answered, which is the
    // case that matters. A test that never got its REQ out would pass the assertion above for the
    // wrong reason.
    assert!(
        relay.frames().await.iter().any(|frame| frame.verb == "REQ"
            && frame.subscription_id.as_deref() == Some(DIRECTORY_SUB_ID)),
        "the directory REQ must have gone out under its own id"
    );
    assert_read_only_traffic(&relay).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_partial_answer_keeps_its_rows_and_still_refuses_to_certify_itself() {
    // One beat, then silence with no EOSE. F1's second trace, and the one the old code got most
    // wrong: nonempty events bypassed the empty branch entirely and were stamped confirmed.
    let keys = Keys::generate();
    let relay = ScriptedRelay::start(Script::ServeThenSilence(vec![signed_announcement(
        &keys,
        Some(SPECIALTY),
        true,
    )]))
    .await;
    let home = buyer_home("partial", &relay.url());

    let directory = read(&home, Duration::from_secs(1))
        .await
        .expect("a partial read still returns what it holds");

    assert_eq!(
        directory.sellers.len(),
        1,
        "rows already in hand are USEFUL and must be kept: {directory:?}"
    );
    assert_eq!(directory.sellers[0].pubkey, keys.public_key().to_hex());
    assert!(
        !directory.read_confirmed,
        "holding a row is not evidence the relay finished answering"
    );
    assert_read_only_traffic(&relay).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_socket_mid_answer_leaves_the_read_unconfirmed() {
    // Same partial shape, ended by a DISCONNECT rather than a timeout. The SDK's stream simply
    // ends; there is no completion discriminator in it, which is why completion is tracked on our
    // own EOSE and nowhere else.
    let keys = Keys::generate();
    let relay = ScriptedRelay::start(Script::ServeThenDrop(vec![signed_announcement(
        &keys,
        Some(SPECIALTY),
        true,
    )]))
    .await;
    let home = buyer_home("dropped", &relay.url());

    let directory = read(&home, Duration::from_secs(2))
        .await
        .expect("a dropped socket is not a hard error for a read that got rows");

    assert!(
        !directory.read_confirmed,
        "a socket that died before EOSE answered nothing, whatever it managed to send first"
    );
    assert_read_only_traffic(&relay).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_subscription_is_an_error_with_its_reason_not_an_empty_market() {
    // CLOSED naming our subscription: an auth failure, a policy rejection. Reporting this as "no
    // sellers" would be the worst available lie about it, so it must surface as an error carrying
    // the relay's own words.
    let relay = ScriptedRelay::start(Script::Close(
        "restricted: this relay does not serve seat directories".to_owned(),
    ))
    .await;
    let home = buyer_home("refused", &relay.url());

    let error = read(&home, Duration::from_secs(5))
        .await
        .expect_err("a refusal must not read as an empty market");

    match error {
        DiscoveryError::Relay(reason) => assert!(
            reason.contains("restricted"),
            "the relay's own reason must survive into the error: {reason}"
        ),
        other => panic!("a CLOSED must be a relay error, got {other:?}"),
    }
    assert_read_only_traffic(&relay).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_authentication_is_an_error_not_an_unanswered_read() {
    // RESIDUAL F1a. The relay challenges, the client signs an AUTH, the relay REFUSES it with a
    // negative OK — and then says nothing at all: no CLOSED, no EOSE. A reader watching only the
    // pool's notifications cannot see that rejection (`AuthenticationFailed` is emitted on the
    // RELAY's channel and is not forwarded to the pool's), so it waits out its deadline and reports
    // an unanswered read. That is a downgrade: "this relay refused your identity" is an explicit,
    // actionable fact, and "nobody answered" is the absence of one.
    let relay = ScriptedRelay::start(Script::ChallengeThenRejectAuth(
        "restricted: this pubkey may not read here".to_owned(),
    ))
    .await;
    let home = buyer_home("auth-rejected", &relay.url());

    let error = read(&home, Duration::from_secs(5))
        .await
        .expect_err("a refused authentication must not read as an unanswered market");

    match error {
        DiscoveryError::Relay(reason) => assert!(
            reason.contains("authentication"),
            "the error must name the authentication rejection: {reason}"
        ),
        other => panic!("a rejected AUTH must be a relay error, got {other:?}"),
    }

    // And the exchange really happened the way the case describes: the client answered the
    // challenge, and no CLOSED or EOSE was ever sent to end the subscription for it.
    let verbs = relay.verbs().await;
    assert!(
        verbs.iter().any(|verb| verb == "AUTH"),
        "the client must have answered the challenge: {verbs:?}"
    );
    assert_read_only_traffic(&relay).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_that_never_reached_a_relay_is_not_an_answered_market() {
    // RESIDUAL F1b, the failure-result half. `Client::subscribe_with_id` returns
    // `Result<Output<()>>` and the pool folds per-relay send failures into `output.failed`,
    // returning `Ok(output)` even when NOTHING succeeded — so a caller checking only the outer
    // `Err` treats a REQ that reached nobody as a REQ that was sent. The read now subscribes
    // through the SINGLE RELAY, whose result is the failure of this relay's REQ.
    //
    // Here nothing is listening on the port at all, so the REQ has no relay to reach. Whichever
    // disposition the SDK produces — a hard error, or an unanswered read — the ONE outcome that
    // must be impossible is a confirmed directory: there is no market fact to be had from a
    // subscription that never landed.
    let port = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("probe bind");
        let port = probe.local_addr().expect("probe addr").port();
        drop(probe);
        port
    };
    let home = buyer_home("unreachable", &format!("ws://127.0.0.1:{port}"));

    match read(&home, Duration::from_secs(2)).await {
        Err(DiscoveryError::Relay(reason)) => {
            assert!(!reason.is_empty(), "a relay error must carry its reason");
        }
        Err(other) => panic!("an unreachable relay is a relay error, got {other:?}"),
        Ok(directory) => {
            assert!(
                !directory.read_confirmed,
                "a subscription that reached no relay must NEVER produce a confirmed \
                 directory: {directory:?}"
            );
            assert!(directory.sellers.is_empty());
        }
    }

    assert!(
        !wallet_store(&home).exists(),
        "a failed read must not open a wallet"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_read_leaves_no_subscription_open_behind_it() {
    // Cleanup, observed on the wire: a relay must not be left streaming into a subscription nobody
    // reads. The read does both — `unsubscribe` then `disconnect` — and EITHER ends the
    // subscription, so the assertion is on the property rather than on which frame won a race.
    //
    // Why not insist on the CLOSE alone: `unsubscribe` hands the frame to the relay's writer task
    // and `disconnect` tears the socket down, so whether the CLOSE is flushed first is a timing
    // matter inside the SDK. Demanding it made this test intermittent — observed failing on a run
    // where the socket close won. Dropping the socket is COMPLETE cleanup on its own; a CLOSE
    // naming our id is the courteous version of the same fact. What would be a defect is neither,
    // or a CLOSE naming somebody else's subscription, and both are asserted below.
    let keys = Keys::generate();
    let relay = ScriptedRelay::start(Script::ServeThenEose(vec![signed_announcement(
        &keys,
        Some(SPECIALTY),
        true,
    )]))
    .await;
    let home = buyer_home("cleanup", &relay.url());

    let directory = read(&home, Duration::from_secs(5)).await.expect("read");
    assert!(directory.read_confirmed);

    assert!(
        relay
            .wait_for_frames(Duration::from_secs(3), |frames| frames.iter().any(
                |frame| {
                    (frame.verb == "CLOSE"
                        && frame.subscription_id.as_deref() == Some(DIRECTORY_SUB_ID))
                        || frame.verb == "SOCKET_CLOSE"
                }
            ))
            .await,
        "the read must end its subscription, by CLOSE or by dropping the socket: {:?}",
        relay.frames().await
    );

    // And it must never close a subscription that is not ours.
    let frames = relay.frames().await;
    for frame in &frames {
        if frame.verb == "CLOSE" {
            assert_eq!(
                frame.subscription_id.as_deref(),
                Some(DIRECTORY_SUB_ID),
                "a read may only close its OWN subscription: {frames:?}"
            );
        }
    }
    assert_eq!(
        relay.connections(),
        1,
        "one read is one socket — no reconnect storm"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_specialty_travels_config_to_signed_beat_to_discovery_row() {
    // The JOIN F3 asks for, end to end: a `[seat] specialty` config value, through the production
    // config-to-wire seam, signed by a real key, served by a relay, read back by the real discovery
    // path, and asserted on the ROW a buyer sees. No hand-written tags anywhere in it.
    let keys = Keys::generate();
    let relay = ScriptedRelay::start(Script::ServeThenEose(vec![signed_announcement(
        &keys,
        Some(SPECIALTY),
        true,
    )]))
    .await;
    let home = buyer_home("joined", &relay.url());

    let directory = read(&home, Duration::from_secs(5)).await.expect("read");

    assert!(directory.read_confirmed);
    assert_eq!(directory.sellers.len(), 1, "{directory:?}");
    let seat = &directory.sellers[0];
    assert_eq!(seat.pubkey, keys.public_key().to_hex());
    assert_eq!(seat.specialty.as_deref(), Some(SPECIALTY));
    assert_eq!(seat.rate_sats, 12);
    assert_eq!(seat.accepted_mints, vec![MINT.to_owned()]);
    assert_eq!(seat.agents, vec!["claude".to_owned()]);
    assert_ne!(
        seat.admits_targeted, ADMISSION_UNSTATED,
        "this beat STATED its admission, so the row must not read as unstated"
    );

    // And no money was touched to learn any of it.
    assert!(
        !wallet_store(&home).exists(),
        "discovery must not open a wallet"
    );
    assert_read_only_traffic(&relay).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_seat_with_no_configured_specialty_is_still_discovered_over_the_wire() {
    // The migration property, over the real transport rather than in the reducer: a seat configured
    // before the field existed publishes no specialty tag and must still be a row.
    let keys = Keys::generate();
    let relay = ScriptedRelay::start(Script::ServeThenEose(vec![signed_announcement(
        &keys, None, true,
    )]))
    .await;
    let home = buyer_home("unlabelled", &relay.url());

    let directory = read(&home, Duration::from_secs(5)).await.expect("read");

    assert_eq!(directory.sellers.len(), 1, "{directory:?}");
    assert_eq!(directory.sellers[0].specialty, None);
    assert!(directory.read_confirmed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_retraction_served_over_the_wire_removes_the_seat() {
    // A seat's own last word. Served through the production retraction emitter, so the test cannot
    // pass against a shape no seat publishes.
    let keys = Keys::generate();
    let relay = ScriptedRelay::start(Script::ServeThenEose(vec![signed_announcement(
        &keys,
        Some(SPECIALTY),
        false,
    )]))
    .await;
    let home = buyer_home("retracted", &relay.url());

    let directory = read(&home, Duration::from_secs(5)).await.expect("read");

    assert!(
        directory.sellers.is_empty(),
        "a retracted seat is not a row: {directory:?}"
    );
    assert_eq!(directory.skipped.retracted, 1);
    assert_eq!(directory.events_read, 1, "and it says what it saw");
    assert!(directory.read_confirmed);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_whose_signature_does_not_verify_never_becomes_a_row() {
    // A forged announcement: a real signed beat with one byte of its signature changed. Whatever
    // layer rejects it — the SDK verifies on receipt — the OBSERVABLE must be that it is not a seat
    // a buyer can be handed, and that a genuine beat in the same response still is.
    let honest_keys = Keys::generate();
    let forged_keys = Keys::generate();

    let honest = signed_announcement(&honest_keys, Some(SPECIALTY), true);
    let forged = tamper_signature(&signed_announcement(&forged_keys, Some("forged"), true));

    let relay = ScriptedRelay::start(Script::ServeThenEose(vec![forged, honest])).await;
    let home = buyer_home("forged", &relay.url());

    let directory = read(&home, Duration::from_secs(5)).await.expect("read");

    assert!(
        directory
            .sellers
            .iter()
            .all(|seat| seat.pubkey != forged_keys.public_key().to_hex()),
        "an event with a broken signature must never become a row: {directory:?}"
    );
    assert_eq!(
        directory.sellers.len(),
        1,
        "and the honest beat in the same response must survive: {directory:?}"
    );
    assert_eq!(
        directory.sellers[0].pubkey,
        honest_keys.public_key().to_hex()
    );
}

/// Flip one hex digit of an event's `sig`, leaving everything else — including the id — intact.
fn tamper_signature(event_json: &str) -> String {
    let mut event: serde_json::Value = serde_json::from_str(event_json).expect("event json");
    let sig = event["sig"].as_str().expect("sig").to_owned();
    let mut chars: Vec<char> = sig.chars().collect();
    let last = chars.len() - 1;
    chars[last] = if chars[last] == 'a' { 'b' } else { 'a' };
    event["sig"] = serde_json::Value::String(chars.into_iter().collect());
    event.to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discovered_pubkey_is_accepted_by_the_daemon_post_mapping_offline() {
    // F3's last item. The unit handoff test goes through `OfferDraft`/`parse_offer`, which BYPASSES
    // the daemon's own post parameters — so it could pass while the value a buyer actually hands to
    // `post_job` was rejected. Here the pubkey comes out of a real discovery read and goes into the
    // real daemon post mapping. OFFLINE: the mapping is exercised, not a post.
    let keys = Keys::generate();
    let relay = ScriptedRelay::start(Script::ServeThenEose(vec![signed_announcement(
        &keys,
        Some(SPECIALTY),
        true,
    )]))
    .await;
    let home = buyer_home("handoff", &relay.url());

    let directory = read(&home, Duration::from_secs(5)).await.expect("read");
    let discovered = directory.sellers[0].pubkey.clone();

    let mapping = maxplayer_core::buyer::map_post_job_params(serde_json::json!({
        "task": "port a crate to tokio",
        "output": "text/plain",
        "amount_sats": 1,
        "seller_pubkey": discovered,
    }))
    .expect("the daemon post mapping must accept a discovered pubkey");

    assert_eq!(
        mapping.request.seller_pubkey.as_deref(),
        Some(discovered.as_str()),
        "the discovered pubkey must arrive as the TARGETED seller, unchanged"
    );
    assert!(
        !mapping.request.untargeted,
        "a discovered seat is a targeted post, never an open-pool one"
    );

    // Still no money: the mapping is a parse, and nothing here pays anyone.
    assert!(!wallet_store(&home).exists(), "no wallet may be opened");
    assert_read_only_traffic(&relay).await;
}
