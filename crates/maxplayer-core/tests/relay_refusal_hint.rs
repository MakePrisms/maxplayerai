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
