//! Email signup spec §6: a key that is not a relay member is refused at NIP-42 sign-in, and the
//! clients say "run `maxplayer account link`". Driven against the shared members-only test relay
//! (`support/auth_gated_relay.rs`, which models buzz with `BUZZ_REQUIRE_RELAY_MEMBERSHIP`).
#![cfg(all(unix, feature = "wallet"))]

#[path = "support/auth_gated_relay.rs"]
mod auth_gated_relay;

use std::time::Duration;

use auth_gated_relay::{AuthRelay, AuthRelayOptions};
use maxplayer_core::relay_refusal::{self, LINK_COMMAND};
use nostr_sdk::prelude::{Client, EventBuilder, Keys};

async fn members_only(member: &Keys) -> AuthRelay {
    AuthRelay::start(AuthRelayOptions {
        members: Some([member.public_key(), member.public_key()]),
        ..Default::default()
    })
    .await
}

/// The short-lived signed-write path (`profile set`, NIP-34 announce) connects through
/// `relay_auth::connect_authenticated`: a non-member gets the hint, a member signs in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_member_sign_in_names_the_link_command() {
    let member = Keys::generate();
    let relay = members_only(&member).await;

    let error = match maxplayer_core::relay_auth::connect_authenticated(
        &Keys::generate(),
        &relay.url,
    )
    .await
    {
        Ok(_) => panic!("a non-member must be refused"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains(&format!("run `{LINK_COMMAND}`")), "{error}");

    let client = maxplayer_core::relay_auth::connect_authenticated(&member, &relay.url)
        .await
        .expect("a member signs in");
    client.disconnect().await;
}

/// `maxplayer doctor`'s relay probe (seller key) carries the same hint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_relay_probe_names_the_link_command() {
    let relay = members_only(&Keys::generate()).await;
    let secret = Keys::generate().secret_key().to_secret_hex();
    let error = maxplayer_core::doctor::probe_relay(&relay.url, &secret, Duration::from_secs(5))
        .await
        .expect_err("non-member");
    assert!(error.contains(&format!("run `{LINK_COMMAND}`")), "{error}");
}

/// A write from a non-member: the SDK's per-relay reason is recognised as a registration refusal,
/// so every `no relay accepted …` site that wraps it with `with_hint` gets the hint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_write_reason_is_recognised() {
    let relay = members_only(&Keys::generate()).await;
    let outsider = Keys::generate();
    let client = Client::new(outsider.clone());
    client.automatic_authentication(true);
    client.add_relay(&relay.url).await.unwrap();
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(5)).await;
    let event = EventBuilder::text_note("hello")
        .sign_with_keys(&outsider)
        .unwrap();
    let output = client
        .send_event_to([relay.url.as_str()], &event)
        .await
        .expect("send attempted");
    client.disconnect().await;
    assert!(output.success.is_empty(), "{output:?}");
    let reason = output
        .failed
        .values()
        .next()
        .expect("a refusal reason")
        .clone();
    assert!(
        relay_refusal::is_registration_refusal(&reason),
        "unrecognised refusal text: {reason}"
    );
    assert!(relay_refusal::with_hint(reason).contains(LINK_COMMAND));
}


// ---- Issue #1228: buzz refuses an unregistered key's sign-in AND closes the socket. nostr-sdk then
// never reports `AuthenticationFailed`, and every write used to fail with a bare `timeout` (no hint).

/// A relay that refuses every key's sign-in the way relay.maxplayer.ai does (`OK false restricted:
/// not a relay member`, then close).
async fn buzz_gate() -> AuthRelay {
    let member = Keys::generate();
    AuthRelay::start(AuthRelayOptions {
        members: Some([member.public_key(), member.public_key()]),
        buzz_refusal: true,
        ..Default::default()
    })
    .await
}

fn assert_hinted(error: &str) {
    assert!(error.contains("not a relay member"), "the relay's refusal is named: {error}");
    assert!(error.contains(&format!("run `{LINK_COMMAND}`")), "{error}");
}

fn home_on(relay_url: &str) -> (tempfile::TempDir, maxplayer_core::home::MaxplayerHome) {
    let dir = tempfile::tempdir().unwrap();
    let mut home = maxplayer_core::home::bootstrap(dir.path()).expect("home");
    maxplayer_core::home::save_config(&mut home, |config| {
        config.relay_url = relay_url.to_owned();
    })
    .expect("point the home at the fixture relay");
    (dir, home)
}

/// `connect_authenticated` (NIP-34 announce) and every `wait_for_nip42_auth` caller (seller boot,
/// buyer receipt, private content): a refusal-and-close is a refusal, not "no challenge".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_1228_a_refused_and_closed_sign_in_is_reported_with_the_hint() {
    let relay = buzz_gate().await;
    let error = match maxplayer_core::relay_auth::connect_authenticated(&Keys::generate(), &relay.url)
        .await
    {
        Ok(_) => panic!("a non-member must be refused"),
        Err(error) => error.to_string(),
    };
    assert_hinted(&error);
}

/// `maxplayer doctor`'s relay probe used to PASS (no challenge) against a refusing relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_1228_doctor_fails_a_refused_and_closed_sign_in() {
    let relay = buzz_gate().await;
    let secret = Keys::generate().secret_key().to_secret_hex();
    let error = maxplayer_core::doctor::probe_relay(&relay.url, &secret, Duration::from_secs(5))
        .await
        .expect_err("a refused sign-in is not a pass");
    assert_hinted(&error);
}

/// `maxplayer profile set` (kind-0): the field report's exact command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_1228_profile_set_names_the_refused_sign_in() {
    let relay = buzz_gate().await;
    let (_dir, mut home) = home_on(&relay.url);
    let error = maxplayer_core::profile::set_profile_async(
        &mut home,
        maxplayer_core::profile::SetProfileRequest {
            name: Some("unlinked".into()),
            about: None,
        },
    )
    .await
    .expect_err("a non-member's kind-0 is refused")
    .to_string();
    assert!(error.contains("after the relay refused NIP-42 sign-in"), "{error}");
    assert_hinted(&error);
}

/// Buyer award (kind-3405): stays `Unresolved` (a session verdict, never a money decision), but
/// its detail now names the refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_1228_a_refused_award_send_names_the_refused_sign_in() {
    use nostr_sdk::prelude::JsonUtil;
    let relay = buzz_gate().await;
    let buyer = Keys::generate();
    let award = EventBuilder::text_note("award stand-in").sign_with_keys(&buyer).unwrap();
    let outcome = maxplayer_core::job_lifecycle::send_signed_award_async(
        &buyer,
        &relay.url,
        &award.id.to_hex(),
        &award.as_json(),
    )
    .await;
    match outcome {
        maxplayer_core::job_lifecycle::SendOutcome::Unresolved { detail } => assert_hinted(&detail),
        other => panic!("a refused sign-in is a session verdict, never Acked/Refused: {other:?}"),
    }
}

/// Buyer daemon writes (session follow-ups and closes) go through the relay actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_1228_the_buyer_daemon_names_the_refused_sign_in() {
    let relay = buzz_gate().await;
    let buyer = Keys::generate();
    let handle = maxplayer_core::buyer::relay::spawn(buyer.clone(), &relay.url)
        .await
        .expect("spawn");
    let note = EventBuilder::text_note("follow-up stand-in").sign_with_keys(&buyer).unwrap();
    let error = handle
        .publish(note)
        .await
        .expect("actor answered")
        .expect_err("a non-member's write is refused")
        .to_string();
    assert_hinted(&error);
}
