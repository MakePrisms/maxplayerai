//! `nostr://` mint transport (credits stage 1): the wallet-side connector against an in-process
//! NIP-01 relay and a SCRIPTED mint responder (canned NUT JSON, no cdk mint, no protoc). The real
//! CDK mint over relays is exercised by the `maxplayer-mint` sidecar's tests (stage 2).
#![cfg(all(unix, feature = "wallet"))]

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cdk::Error;
use cdk::mint_url::MintUrl;
use cdk::nuts::{CheckStateRequest, CurrencyUnit, Id, RestoreRequest, SwapRequest};
use cdk::wallet::{MintConnector, WalletSubscription};
use maxplayer_core::mint_wire::{self, REQUEST_KIND, RESPONSE_KIND, Request};
use maxplayer_core::nostr_mint::{NostrMintConnector, build_wallet_with_relays};
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::nips::nip44::{self, Version};
use nostr_sdk::prelude::{
    Client, Event, EventBuilder, EventId, Filter, Keys, Kind, RelayMessage, RelayPoolNotification,
    Tag, Timestamp, ToBech32,
};
use serde_json::{Value, json};

/// How the scripted mint behaves.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    /// Answer every delivery.
    Answer,
    /// Ignore the FIRST delivery of each request event (a lost reply), answer later ones.
    DropFirst,
    /// Before the real answer, publish decoys: forged signer, wrong request id, wrong e-tag,
    /// malformed JSON, wrong envelope version.
    DecoysThenAnswer,
    /// Publish only decoys, never a real answer.
    DecoysOnly,
    /// Never answer.
    Silent,
    WrongVersion,
    MissingVersion,
    NonNumericVersion,
    Malformed,
    NutError,
    /// Answer every request with a transport error code.
    Err(&'static str),
}

#[derive(Default)]
struct Seen {
    /// (request event id, decrypted request) for every raw delivery.
    deliveries: Vec<(EventId, Request)>,
}

struct Mint {
    keys: Keys,
    url: String,
    seen: Arc<Mutex<Seen>>,
}

fn canned(op: &str) -> Value {
    match op {
        mint_wire::op::INFO => json!({"name": "scripted", "nuts": {}}),
        mint_wire::op::KEYS | mint_wire::op::KEYSET => json!({"keysets": []}),
        mint_wire::op::KEYSETS => json!({"keysets": []}),
        mint_wire::op::SWAP => json!({"signatures": []}),
        mint_wire::op::CHECKSTATE => json!({"states": []}),
        mint_wire::op::RESTORE => json!({"outputs": [], "signatures": []}),
        _ => Value::Null,
    }
}

async fn reply(client: &Client, signer: &Keys, to: &Event, id: &str, plain: String, e: EventId) {
    let content = nip44::encrypt(signer.secret_key(), &to.pubkey, plain, Version::V2).unwrap();
    let _ = id;
    let event = EventBuilder::new(Kind::Custom(RESPONSE_KIND), content)
        .tag(Tag::public_key(to.pubkey))
        .tag(Tag::event(e))
        .sign_with_keys(signer)
        .unwrap();
    let _ = client.send_event(&event).await;
}

async fn start_mint(relay_url: &str, mode: Mode) -> Mint {
    let keys = Keys::generate();
    let client = Client::new(keys.clone());
    client.add_relay(relay_url).await.unwrap();
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(5)).await;
    let mut notifications = client.notifications();
    client
        .subscribe(
            Filter::new()
                .kind(Kind::Custom(REQUEST_KIND))
                .pubkey(keys.public_key())
                .since(Timestamp::now() - Duration::from_secs(5)),
            None,
        )
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (k, s, c) = (keys.clone(), seen.clone(), client.clone());
    tokio::spawn(async move {
        // Raw `Message` notifications: the pool's `Event` notification dedups by event id, and a
        // re-send carries the SAME id, so only raw deliveries show it.
        while let Ok(notification) = notifications.recv().await {
            let RelayPoolNotification::Message {
                message: RelayMessage::Event { event, .. },
                ..
            } = notification
            else {
                continue;
            };
            let event: Event = event.into_owned();
            if event.kind != Kind::Custom(REQUEST_KIND) {
                continue;
            }
            let Ok(plain) = nip44::decrypt(k.secret_key(), &event.pubkey, &event.content) else {
                continue;
            };
            let Ok(request) = serde_json::from_str::<Request>(&plain) else {
                continue;
            };
            let nth = {
                let mut seen = s.lock().unwrap();
                seen.deliveries.push((event.id, request.clone()));
                seen.deliveries
                    .iter()
                    .filter(|(id, _)| *id == event.id)
                    .count()
            };
            let ok = json!({"v": 1, "id": request.id, "ok": canned(&request.op)}).to_string();
            let decoys = |c: Client, k: Keys, event: Event, request: Request| async move {
                let forger = Keys::generate();
                reply(&c, &forger, &event, &request.id, ok_for(&request), event.id).await;
                let wrong_id =
                    json!({"v":1,"id":"not-this-one","ok":canned(&request.op)}).to_string();
                reply(&c, &k, &event, &request.id, wrong_id, event.id).await;
                reply(
                    &c,
                    &k,
                    &event,
                    &request.id,
                    ok_for(&request),
                    EventId::all_zeros(),
                )
                .await;
                reply(&c, &k, &event, &request.id, "{not json".into(), event.id).await;
                let wrong_v = json!({"v":9,"id":request.id,"ok":canned(&request.op)}).to_string();
                reply(&c, &k, &event, &request.id, wrong_v, event.id).await;
            };
            match mode {
                Mode::Silent => {}
                Mode::WrongVersion => reply(&c, &k, &event, &request.id, json!({"v":9,"id":request.id,"ok":{}}).to_string(), event.id).await,
                Mode::MissingVersion => reply(&c, &k, &event, &request.id, json!({"id":request.id,"ok":{}}).to_string(), event.id).await,
                Mode::NonNumericVersion => reply(&c, &k, &event, &request.id, json!({"v":"secret","id":request.id,"ok":{}}).to_string(), event.id).await,
                Mode::Malformed => reply(&c, &k, &event, &request.id, "{not json".into(), event.id).await,
                Mode::NutError => reply(&c, &k, &event, &request.id, json!({"v":1,"id":request.id,"err":{"code":50000,"detail":"secret\u{001b}[31m"}}).to_string(), event.id).await,
                Mode::Answer => reply(&c, &k, &event, &request.id, ok, event.id).await,
                Mode::DropFirst => {
                    if nth >= 2 {
                        reply(&c, &k, &event, &request.id, ok, event.id).await
                    }
                }
                Mode::DecoysThenAnswer => {
                    decoys(c.clone(), k.clone(), event.clone(), request.clone()).await;
                    reply(&c, &k, &event, &request.id, ok, event.id).await;
                }
                Mode::DecoysOnly => {
                    decoys(c.clone(), k.clone(), event.clone(), request.clone()).await
                }
                Mode::Err(code) => {
                    let err =
                        json!({"v":1,"id":request.id,"err":{"code":code,"detail":"scripted"}})
                            .to_string();
                    reply(&c, &k, &event, &request.id, err, event.id).await;
                }
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let url = format!("nostr://{}", keys.public_key().to_bech32().unwrap());
    Mint { keys, url, seen }
}

fn ok_for(request: &Request) -> String {
    json!({"v": 1, "id": request.id, "ok": canned(&request.op)}).to_string()
}

async fn relay() -> (LocalRelay, String) {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.expect("relay run");
    let url = relay.url().await.to_string();
    (relay, url)
}

fn connector(mint: &Mint, relay_url: &str, window_ms: u64, resend_ms: u64) -> NostrMintConnector {
    NostrMintConnector::new(
        &MintUrl::from_str(&mint.url).unwrap(),
        vec![relay_url.to_owned()],
    )
    .unwrap()
    .with_timing(
        Duration::from_millis(window_ms),
        Duration::from_millis(resend_ms),
        Duration::from_secs(3),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nostr_mint_every_holder_op_round_trips_through_the_connector() {
    let (_relay, relay_url) = relay().await;
    let mint = start_mint(&relay_url, Mode::Answer).await;
    let c = connector(&mint, &relay_url, 5_000, 1_000);

    assert_eq!(
        c.get_mint_info().await.expect("info").name.as_deref(),
        Some("scripted")
    );
    assert!(c.get_mint_keys().await.expect("keys").is_empty());
    assert!(
        c.get_mint_keysets()
            .await
            .expect("keysets")
            .keysets
            .is_empty()
    );
    let id = Id::from_str("009a1f293253e41e").unwrap();
    assert!(matches!(
        c.get_mint_keyset(id).await,
        Err(Error::UnknownKeySet)
    ));
    assert!(
        c.post_swap(SwapRequest::new(vec![], vec![]))
            .await
            .expect("swap")
            .signatures
            .is_empty()
    );
    assert!(
        c.post_check_state(CheckStateRequest { ys: vec![] })
            .await
            .expect("checkstate")
            .states
            .is_empty()
    );
    assert!(
        c.post_restore(RestoreRequest { outputs: vec![] })
            .await
            .expect("restore")
            .signatures
            .is_empty()
    );

    let seen = mint.seen.lock().unwrap();
    let ops: Vec<&str> = seen.deliveries.iter().map(|(_, r)| r.op.as_str()).collect();
    assert_eq!(
        ops,
        [
            "info",
            "keys",
            "keysets",
            "keyset",
            "swap",
            "checkstate",
            "restore"
        ]
    );
    let keyset_body = &seen.deliveries[3].1.body;
    assert_eq!(keyset_body["id"], json!("009a1f293253e41e"));
    // A fresh throwaway client key per request: relays can't tie requests together.
    assert!(seen.deliveries.iter().all(|(_, r)| r.v == 1 && r.exp > 0));
    let _ = &mint.keys;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nostr_mint_forged_wrong_id_wrong_tag_malformed_and_wrong_version_replies_are_ignored() {
    let (_relay, relay_url) = relay().await;
    let mint = start_mint(&relay_url, Mode::DecoysThenAnswer).await;
    let c = connector(&mint, &relay_url, 5_000, 1_000);
    let info = c
        .get_mint_info()
        .await
        .expect("the real answer after the decoys");
    assert_eq!(info.name.as_deref(), Some("scripted"));

    // Decoys alone never produce a result: the request ends ambiguous.
    let (_relay2, relay_url2) = relay().await;
    let decoys = start_mint(&relay_url2, Mode::DecoysOnly).await;
    let c = connector(&decoys, &relay_url2, 1_500, 400);
    let error = c
        .post_swap(SwapRequest::new(vec![], vec![]))
        .await
        .expect_err("no valid reply");
    assert!(matches!(error, Error::Timeout), "{error:?}");
    assert!(!error.is_definitive_failure());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nostr_mint_a_lost_reply_is_recovered_by_an_identical_resend() {
    let (_relay, relay_url) = relay().await;
    let mint = start_mint(&relay_url, Mode::DropFirst).await;
    let c = connector(&mint, &relay_url, 5_000, 500);
    let swap = c
        .post_swap(SwapRequest::new(vec![], vec![]))
        .await
        .expect("answered on the re-send");
    assert!(swap.signatures.is_empty());

    let seen = mint.seen.lock().unwrap();
    assert!(
        seen.deliveries.len() >= 2,
        "a re-send happened: {}",
        seen.deliveries.len()
    );
    let (first_event, first_request) = &seen.deliveries[0];
    for (event, request) in &seen.deliveries {
        assert_eq!(
            event, first_event,
            "the re-send is the IDENTICAL signed event"
        );
        assert_eq!(request, first_request, "one logical request id");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nostr_mint_no_reply_is_an_ambiguous_timeout_after_the_window() {
    let (_relay, relay_url) = relay().await;
    let mint = start_mint(&relay_url, Mode::Silent).await;
    let c = connector(&mint, &relay_url, 1_500, 400);
    let started = std::time::Instant::now();
    let error = c
        .post_swap(SwapRequest::new(vec![], vec![]))
        .await
        .expect_err("silent mint");
    assert!(matches!(error, Error::Timeout), "{error:?}");
    assert!(
        !error.is_definitive_failure(),
        "a missing reply must never let cdk compensate"
    );
    assert!(started.elapsed() >= Duration::from_millis(1_400));
    assert!(started.elapsed() < Duration::from_secs(6));
    let seen = mint.seen.lock().unwrap();
    assert!(seen.deliveries.len() >= 2, "re-sent within the window");
    // PR #1034 review: `exp` is when the connector stops waiting, never later. A 1.5s window
    // Cashu review: by the time the connector returns, a mint applying `now >= exp` refuses it.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for (_, request) in &seen.deliveries {
        assert!(
            mint_wire::request_expired(request.exp, now),
            "exp {} outlives the wait (now {now})",
            request.exp
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nostr_mint_transport_errors_map_to_their_cdk_class() {
    let (_relay, relay_url) = relay().await;
    let limited = start_mint(&relay_url, Mode::Err(mint_wire::code::RATE_LIMITED)).await;
    let error = connector(&limited, &relay_url, 5_000, 1_000)
        .post_swap(SwapRequest::new(vec![], vec![]))
        .await
        .expect_err("rate limited");
    assert!(matches!(error, Error::HttpError(Some(429), _)), "{error:?}");
    assert!(error.is_definitive_failure(), "the mint did not run it");

    let unsupported = start_mint(&relay_url, Mode::Err(mint_wire::code::UNSUPPORTED)).await;
    let error = connector(&unsupported, &relay_url, 5_000, 1_000)
        .get_mint_info()
        .await
        .expect_err("unsupported");
    assert!(error.is_definitive_failure(), "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nostr_mint_all_relays_down_is_a_clean_error() {
    let npub = Keys::generate().public_key().to_bech32().unwrap();
    let c = NostrMintConnector::new(
        &MintUrl::from_str(&format!("nostr://{npub}")).unwrap(),
        vec!["ws://127.0.0.1:1".to_owned()],
    )
    .unwrap()
    .with_timing(
        Duration::from_secs(2),
        Duration::from_millis(500),
        Duration::from_secs(1),
    );
    let started = std::time::Instant::now();
    let (result, observation) = c.call_observed("info", Value::Null).await;
    let error = result.expect_err("no relay");
    assert_eq!(observation.reachability, "relay_unreachable");
    assert!(observation.reply_ms.is_none());
    assert!(
        observation
            .relays
            .values()
            .all(|r| r.acceptance == "unknown")
    );
    assert!(matches!(error, Error::HttpError(None, _)), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nostr_mint_wallet_polls_through_the_connector_instead_of_websocket() {
    // cdk's WebSocket subscription joins `/v1/ws` onto the mint URL and panics on `nostr://`; the
    // factory's wallet must use poll mode, which goes through the connector (NUT-07 checkstate).
    let (_relay, relay_url) = relay().await;
    let mint = start_mint(&relay_url, Mode::Answer).await;
    let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
    let wallet = build_wallet_with_relays(
        &mint.url,
        CurrencyUnit::Sat,
        store,
        [7; 64],
        None,
        vec![relay_url.clone()],
    )
    .expect("nostr wallet");
    assert_eq!(wallet.mint_url.to_string(), mint.url);
    let y = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798".to_owned();
    let mut subscription = wallet
        .subscribe(WalletSubscription::ProofState(vec![y]))
        .await
        .expect("subscribe");
    let _ = tokio::time::timeout(Duration::from_secs(8), subscription.recv()).await;
    let seen = mint.seen.lock().unwrap();
    assert!(
        seen.deliveries
            .iter()
            .any(|(_, r)| r.op == mint_wire::op::CHECKSTATE),
        "the subscription polled checkstate over the relay: {:?}",
        seen.deliveries
            .iter()
            .map(|(_, r)| r.op.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_distinguish_signed_errors_versions_and_accepted_silence() {
    let (_relay, url) = relay().await;
    for (mode, expected) in [
        (Mode::WrongVersion, "unsupported_protocol"),
        (Mode::MissingVersion, "invalid_response"),
        (Mode::NonNumericVersion, "invalid_response"),
        (Mode::Malformed, "invalid_response"),
        (Mode::NutError, "nut_error"),
        (Mode::Err("internal"), "internal"),
        (Mode::Err("expired"), "expired"),
        (Mode::Err("rate_limited"), "rate_limited"),
        (Mode::Err("private_named_error"), "unknown_protocol_error"),
        (Mode::Silent, "unanswered"),
    ] {
        let mint = start_mint(&url, mode).await;
        let started = std::time::Instant::now();
        let (result, evidence) = connector(&mint, &url, 1200, 300)
            .call_observed("info", Value::Null)
            .await;
        assert!(result.is_err());
        assert_eq!(evidence.outcome, expected);
        let ignored = matches!(
            mode,
            Mode::WrongVersion | Mode::MissingVersion | Mode::NonNumericVersion | Mode::Malformed
        );
        // Only a usable reply is an answer. Malformed or other-version replies are evidence:
        // the call waits out its window and ends `Timeout`, exactly as before diagnostics.
        assert_eq!(
            evidence.reply_ms.is_some(),
            mode != Mode::Silent && !ignored,
            "{mode:?}"
        );
        assert_eq!(evidence.ignored_reply_ms.is_some(), ignored, "{mode:?}");
        if ignored {
            assert!(
                matches!(result.as_ref().unwrap_err(), Error::Timeout),
                "{mode:?}: {result:?}"
            );
            assert!(started.elapsed() >= Duration::from_millis(1200), "{mode:?}");
            assert_eq!(evidence.reachability, "unusable_reply");
            assert!(
                evidence.message().contains("ignored"),
                "{}",
                evidence.message()
            );
        }
        assert_eq!(evidence.budget_ms, 1200);
        if mode == Mode::Silent {
            assert!(evidence.relays.values().any(|r| r.acceptance == "accepted"));
            assert!(evidence.message().contains("cannot be distinguished"));
        }
        assert!(!result.as_ref().unwrap_err().to_string().contains("secret"));
        assert!(
            !result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("private_named_error")
        );
        assert!(!format!("{evidence:?}").contains("secret"));
        if matches!(
            mode,
            Mode::WrongVersion | Mode::MissingVersion | Mode::NonNumericVersion | Mode::Malformed
        ) {
            assert!(!result.as_ref().unwrap_err().is_definitive_failure());
        }
        if mode == Mode::NutError {
            assert_eq!(evidence.nut_code, Some(50000));
            assert!(!result.unwrap_err().is_definitive_failure());
        }
        let public = serde_json::to_string(&evidence).unwrap();
        assert!(!public.contains("secret"));
        assert!(!evidence.message().contains("secret"));
    }
}

/// Minimal loopback relay control frames, independent of the builder's policy implementation.
/// Nothing can leave loopback, and raw text must never enter public diagnostics.
async fn control_relay(mode: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    use futures_util::{SinkExt, StreamExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        if mode == "auth_failed" {
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!(["AUTH", "loopback-challenge"]).to_string().into(),
            ))
            .await
            .unwrap();
        }
        while let Some(Ok(message)) = ws.next().await {
            let Ok(text) = message.to_text() else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            let frames = match v[0].as_str() {
                Some("REQ") => match mode {
                    "closed" => vec![json!(["CLOSED", v[1], "auth-required: secret\u{001b}[31m"])],
                    "unrelated_closed" => vec![json!(["CLOSED", "unrelated", "secret"])],
                    "notice" => vec![json!(["NOTICE", "secret\u{001b}[31m"])],
                    _ => vec![json!(["EOSE", v[1]])],
                },
                Some("AUTH") => vec![json!(["OK", v[1]["id"], false, "restricted: secret"])],
                Some("EVENT") => match mode {
                    "refused" => vec![json!([
                        "OK",
                        v[1]["id"],
                        false,
                        "blocked: secret\u{001b}[31m"
                    ])],
                    "unrelated_ok" => vec![json!(["OK", EventId::all_zeros().to_hex(), true, ""])],
                    "accepted" | "disconnect" => vec![json!(["OK", v[1]["id"], true, ""])],
                    _ => vec![],
                },
                _ => vec![],
            };
            for frame in frames {
                if ws
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        frame.to_string().into(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if mode == "disconnect" && v[0] == "EVENT" {
                let _ = ws.close(None).await;
                return;
            }
        }
    });
    (url, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_control_frames_require_event_and_subscription_correlation() {
    for mode in [
        "refused",
        "accepted",
        "no_ack",
        "unrelated_ok",
        "closed",
        "unrelated_closed",
        "notice",
        "disconnect",
        "auth_failed",
    ] {
        let (url, task) = control_relay(mode).await;
        let mint = MintUrl::from_str(&format!(
            "nostr://{}",
            Keys::generate().public_key().to_bech32().unwrap()
        ))
        .unwrap();
        let c = NostrMintConnector::new(&mint, vec![url])
            .unwrap()
            .with_timing(
                Duration::from_millis(700),
                Duration::from_secs(2),
                Duration::from_millis(300),
            );
        let (result, obs) = c.call_observed("info", Value::Null).await;
        assert!(result.is_err());
        assert!(
            obs.reply_ms.is_none(),
            "ACK/NOTICE/CLOSED never prove a mint answer: {mode}"
        );
        let r = obs.relays.values().next().unwrap();
        assert_eq!(
            r.acceptance,
            match mode {
                "refused" => "refused",
                "accepted" | "disconnect" => "accepted",
                _ => "unknown",
            },
            "{mode}"
        );
        assert_eq!(
            r.closed,
            mode == "closed",
            "unrelated CLOSED must not poison this subscription"
        );
        assert_eq!(r.notice, mode == "notice");
        if mode == "refused" {
            assert!(
                r.publish_failed,
                "retain the failed pooled publish Output, not only raw OK"
            );
        }
        if mode == "auth_failed" {
            assert_eq!(r.auth, "failed");
        }
        assert!(!serde_json::to_string(&obs).unwrap().contains("secret"));
        assert!(
            !result.unwrap_err().is_definitive_failure(),
            "relay evidence cannot clear money"
        );
        task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_summary_uses_the_best_relay_path() {
    // One relay accepted the request with its reply subscription open, another failed AUTH or
    // closed the subscription: the mint stayed silent on a path that could have shown an answer.
    for other in ["auth_failed", "closed", "refused"] {
        let (accepted, a) = control_relay("accepted").await;
        let (worse, b) = control_relay(other).await;
        let mint = MintUrl::from_str(&format!(
            "nostr://{}",
            Keys::generate().public_key().to_bech32().unwrap()
        ))
        .unwrap();
        let c = NostrMintConnector::new(&mint, vec![worse, accepted])
            .unwrap()
            .with_timing(
                Duration::from_millis(1500),
                Duration::from_secs(5),
                Duration::from_millis(500),
            );
        let (result, obs) = c.call_observed("info", Value::Null).await;
        assert!(matches!(result, Err(Error::Timeout)), "{other}: {result:?}");
        assert!(
            obs.relays.values().any(|r| r.acceptance == "accepted"),
            "{other}: {obs:?}"
        );
        assert_eq!(obs.reachability, "unanswered", "{other}: {obs:?}");
        assert!(
            obs.message().contains("Relay accepted the request"),
            "{other}: {}",
            obs.message()
        );
        a.abort();
        b.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_reply_wins_while_another_relay_withholds_ack() {
    let (_relay, url) = relay().await;
    let (silent_url, task) = control_relay("no_ack").await;
    let mint = start_mint(&url, Mode::Answer).await;
    let c = NostrMintConnector::new(&mint.url.parse().unwrap(), vec![url, silent_url])
        .unwrap()
        .with_timing(
            Duration::from_secs(3),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
    let (result, obs) = c.call_observed("info", Value::Null).await;
    assert!(
        result.is_ok(),
        "a held pooled publish cannot hide a real answer"
    );
    assert!(obs.reply_ms.unwrap() < 2000);
    assert_eq!(obs.relays.len(), 2);
    assert!(obs.relays.values().any(|r| r.acceptance == "unknown"));
    task.abort();
}
