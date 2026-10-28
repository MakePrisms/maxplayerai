//! End-to-end tests of the registration service against a real Postgres (a fresh
//! database + NOSUPERUSER role per test), a fake buzz relay and a fake Resend.

mod common;

use std::sync::atomic::Ordering;

use common::{TestApp, auth_header, tamper};
use maxplayer_registration::{api, db, sync, webhook};
use nostr::{Keys, Timestamp, ToBech32};
use serde_json::json;

const EMAIL: &str = "you@example.com";

// ------------------------------------------------------------------ happy path + 9030

#[tokio::test]
async fn link_flow_sends_code_links_key_sends_9030_and_welcome() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    let pk = agent.public_key().to_hex();

    let r = app
        .request_link(&agent, "  You@Example.COM ", "my-laptop")
        .await;
    assert_eq!(r.status, 202, "{:?}", r.body);
    assert_eq!(r.body["status"], "code_sent");
    assert_eq!(r.body["expires_in_secs"], 600);
    assert_eq!(r.body["max_attempts"], 5);

    let emails = app.emails();
    assert_eq!(emails.len(), 1);
    let code_mail = &emails[0];
    assert_eq!(code_mail.to(), EMAIL, "email is trimmed + lowercased");
    assert_eq!(code_mail.template(), "maxplayer-code");
    assert_eq!(
        code_mail.body["from"],
        "Maxplayer <noreply@mail.maxplayer.ai>"
    );
    let vars: Vec<&String> = code_mail.body["template"]["variables"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(vars.len(), 5);
    assert_eq!(code_mail.var("AGENT_LABEL"), "my-laptop");
    assert_eq!(
        code_mail.var("TERMS_URL"),
        "https://www.maxplayer.ai/terms/v1"
    );
    assert_eq!(
        code_mail.var("PRIVACY_URL"),
        "https://www.maxplayer.ai/privacy/v1"
    );
    let npub = agent.public_key().to_bech32().unwrap();
    assert_eq!(
        code_mail.var("NPUB_SHORT"),
        format!("{}…{}", &npub[..8], &npub[npub.len() - 4..])
    );
    let code = code_mail.var("CODE").to_string();
    assert!(code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()));
    assert!(code_mail.idempotency_key.starts_with("maxplayer-code-"));
    assert!(
        !app.relay.is_member(&pk),
        "nothing reaches the relay before the code is entered"
    );

    let before = app.state.clock.unix();
    let r = app.confirm(&agent, &code).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["status"], "linked");
    assert_eq!(r.body["first_link"], true);
    assert_eq!(r.body["email"], EMAIL);
    assert_eq!(r.body["relay_member"], "added");
    assert_eq!(r.body["marketing"], "subscribed");
    assert!(app.relay.is_member(&pk));

    // The 9030 exactly as sent.
    let cmds = app.relay.commands();
    assert_eq!(cmds.len(), 1);
    let ev = &cmds[0];
    ev.verify().expect("valid signature");
    assert_eq!(ev.kind.as_u16(), 9030);
    assert_eq!(
        ev.pubkey,
        app.admin.public_key(),
        "signed by the relay admin key"
    );
    assert_eq!(ev.content, "");
    let tags: Vec<Vec<String>> = ev.tags.iter().map(|t| t.as_slice().to_vec()).collect();
    assert_eq!(
        tags,
        vec![vec!["-".to_string()], vec!["p".to_string(), pk.clone()]]
    );
    assert!(
        ev.created_at.as_secs().abs_diff(before) <= 5,
        "fresh created_at"
    );
    let raw = serde_json::to_string(ev).unwrap();
    assert!(!raw.contains("example.com"), "no email in relay events");

    // Welcome email.
    let emails = app.emails();
    assert_eq!(emails.len(), 2);
    let w = &emails[1];
    assert_eq!(w.template(), "maxplayer-welcome");
    assert_eq!(w.to(), EMAIL);
    let vars: Vec<&String> = w.body["template"]["variables"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(vars.len(), 3);
    assert_eq!(w.var("AGENT_LABEL"), "my-laptop");
    assert_eq!(w.var("TERMS_URL"), "https://www.maxplayer.ai/terms/v1");
    assert_eq!(w.var("PRIVACY_URL"), "https://www.maxplayer.ai/privacy/v1");

    // Rows per spec §5.
    let reg = db::get_registration(&app.db, &pk).await.unwrap().unwrap();
    assert_eq!(reg.email, EMAIL);
    assert_eq!(reg.label, "my-laptop");
    assert_eq!(reg.terms_version, "v1");
    assert_eq!(reg.privacy_version, "v1");
    assert_eq!(reg.code_entered_by, "human");
    let consent = db::get_consent(&app.db, EMAIL).await.unwrap().unwrap();
    assert_eq!(consent.method, "signup_default");
    assert_eq!(consent.notice_version, "v1");
    assert!(consent.unsubscribed_at.is_none());
    assert_eq!(app.count("SELECT count(*) FROM managed_members").await, 1);

    // Status.
    let s = app.get(&agent, api::PATH_STATUS).await;
    assert_eq!(s.status, 200);
    assert_eq!(s.body["registered"], true);
    assert_eq!(s.body["email"], EMAIL);
    assert_eq!(s.body["terms_version"], "v1");
    assert_eq!(s.body["marketing"], "subscribed");
    assert_eq!(s.body["relay_member"], "added");
    let other = app.get(&Keys::generate(), api::PATH_STATUS).await;
    assert_eq!(other.body, json!({ "registered": false }));
}

#[tokio::test]
async fn codes_are_stored_only_as_keyed_hashes() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    app.request_link(&agent, EMAIL, "a").await;
    let code = app.last_code(EMAIL);
    let hashes: Vec<Vec<u8>> = sqlx::query_scalar("SELECT code_hash FROM email_codes")
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes[0].len(), 32);
    assert!(!hashes[0].windows(6).any(|w| w == code.as_bytes()));
    use sha2::Digest;
    assert_ne!(
        hashes[0],
        sha2::Sha256::digest(code.as_bytes()).to_vec(),
        "not a plain hash"
    );
    let hits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM email_codes WHERE email LIKE '%' || $1 || '%' OR label LIKE '%' || $1 || '%'",
    )
    .bind(&code)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(hits, 0);
}

#[tokio::test]
async fn service_runs_as_a_non_superuser_role() {
    let app = TestApp::start().await;
    let su: bool = sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = current_user")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert!(!su);
    let migrations: i64 = app
        .count("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .await;
    assert_eq!(migrations, 1);
}

// ------------------------------------------------------------------ wrong code, expiry

#[tokio::test]
async fn wrong_codes_count_down_and_lock_the_code() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    app.request_link(&agent, EMAIL, "a").await;
    let code = app.last_code(EMAIL);
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for left in [4, 3, 2, 1] {
        let r = app.confirm(&agent, wrong).await;
        assert_eq!(r.status, 400, "{:?}", r.body);
        assert_eq!(r.body["error"], "wrong_code");
        assert_eq!(r.body["attempts_left"], left);
    }
    let r = app.confirm(&agent, "not a code").await;
    assert_eq!(
        r.status, 429,
        "malformed input is a wrong try: {:?}",
        r.body
    );
    assert_eq!(r.body["error"], "too_many_attempts");
    let r = app.confirm(&agent, &code).await;
    assert_eq!(
        r.status, 429,
        "the right code no longer works after 5 tries"
    );
    assert!(!app.relay.is_member(&agent.public_key().to_hex()));
    assert_eq!(app.count("SELECT count(*) FROM registrations").await, 0);

    // Another key cannot use this key's code.
    let thief = Keys::generate();
    let r = app.confirm(&thief, &code).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.body["error"], "no_pending_code");
}

#[tokio::test]
async fn codes_expire_after_ten_minutes() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    app.request_link(&agent, EMAIL, "a").await;
    let code = app.last_code(EMAIL);
    app.state.clock.advance(599);
    // Still valid at 599 s: prove it with a wrong try that is counted, not expired.
    let r = app
        .confirm(&agent, if code == "000000" { "111111" } else { "000000" })
        .await;
    assert_eq!(r.body["error"], "wrong_code");
    app.state.clock.advance(2);
    let r = app.confirm(&agent, &code).await;
    assert_eq!(r.status, 410, "{:?}", r.body);
    assert_eq!(r.body["error"], "code_expired");
    assert_eq!(app.count("SELECT count(*) FROM registrations").await, 0);
    assert!(app.relay.commands().is_empty());
}

#[tokio::test]
async fn a_new_code_replaces_the_old_one() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    app.request_link(&agent, EMAIL, "a").await;
    let first = app.last_code(EMAIL);
    app.state.clock.advance(61);
    app.request_link(&agent, EMAIL, "a").await;
    let second = app.last_code(EMAIL);
    if first != second {
        let r = app.confirm(&agent, &first).await;
        assert_eq!(r.body["error"], "wrong_code", "the replaced code is dead");
    }
    let r = app.confirm(&agent, &second).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
}

#[tokio::test]
async fn a_failed_code_email_leaves_nothing_to_confirm() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    app.resend.fail_next.store(1, Ordering::SeqCst);
    let r = app.request_link(&agent, EMAIL, "a").await;
    assert_eq!(r.status, 502);
    assert_eq!(r.body["error"], "email_send_failed");
    assert!(
        !r.body.to_string().contains("example.com"),
        "no email in error bodies"
    );
    let r = app.confirm(&agent, "123456").await;
    assert_eq!(r.body["error"], "no_pending_code");
}

// ------------------------------------------------------------------ rate limits

#[tokio::test]
async fn resend_cooldown_and_per_key_send_limit() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    assert_eq!(app.request_link(&agent, EMAIL, "a").await.status, 202);
    let r = app.request_link(&agent, EMAIL, "a").await;
    assert_eq!(r.status, 429);
    assert_eq!(r.body["error"], "resend_cooldown");
    let retry: i64 = r.headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry));
    for _ in 0..4 {
        app.state.clock.advance(61);
        assert_eq!(app.request_link(&agent, EMAIL, "a").await.status, 202);
    }
    app.state.clock.advance(61);
    let r = app.request_link(&agent, EMAIL, "a").await;
    assert_eq!(r.status, 429);
    assert_eq!(r.body["error"], "rate_limited");
    assert_eq!(app.emails().len(), 5);
    // The window slides: an hour later it works again.
    app.state.clock.advance(3600);
    assert_eq!(app.request_link(&agent, EMAIL, "a").await.status, 202);
}

#[tokio::test]
async fn per_email_and_per_ip_send_limits() {
    let app = TestApp::start_with(|c| c.limits.sends_per_ip_per_hour = 7).await;
    // Same email, five different keys: the sixth is refused (spec: per-email limit).
    for _ in 0..5 {
        assert_eq!(
            app.request_link(&Keys::generate(), EMAIL, "a").await.status,
            202
        );
    }
    let r = app.request_link(&Keys::generate(), EMAIL, "a").await;
    assert_eq!(r.status, 429);
    assert_eq!(r.body["error"], "rate_limited");
    // Different emails from the same IP. A refused send is not counted, so 7 sends
    // went through; the next one is over the per-IP limit of 7.
    for i in 0..2 {
        assert_eq!(
            app.request_link(&Keys::generate(), &format!("x{i}@example.org"), "a")
                .await
                .status,
            202
        );
    }
    let r = app
        .request_link(&Keys::generate(), "y@example.org", "a")
        .await;
    assert_eq!(r.status, 429, "{:?}", r.body);
    assert_eq!(app.emails().len(), 7);
}

#[tokio::test]
async fn confirm_attempts_are_rate_limited_per_key() {
    let app = TestApp::start_with(|c| c.limits.confirms_per_pubkey_per_hour = 3).await;
    let agent = Keys::generate();
    for _ in 0..3 {
        let r = app.confirm(&agent, "123456").await;
        assert_eq!(r.status, 404);
    }
    let r = app.confirm(&agent, "123456").await;
    assert_eq!(r.status, 429);
    assert_eq!(r.body["error"], "rate_limited");
}

// ------------------------------------------------------------------ NIP-98

#[tokio::test]
async fn nip98_rejects_bad_signature_url_method_age_payload_and_replay() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    let path = api::PATH_LINK_REQUEST;
    let body =
        json!({ "email": EMAIL, "label": "a", "terms_version": "v1", "privacy_version": "v1" })
            .to_string()
            .into_bytes();
    let send = |auth: Option<String>, b: Vec<u8>| app.send("POST", path, auth, Some(b), &[]);

    let r = send(None, body.clone()).await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (401, Some("missing_auth"))
    );

    // Bad signature: claim another key's pubkey with this key's signature.
    let good = app.auth(&agent, path, "POST", Some(&body));
    let forged = tamper(&good, |v| {
        v["pubkey"] = json!(Keys::generate().public_key().to_hex())
    });
    let r = send(Some(forged), body.clone()).await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (401, Some("bad_signature"))
    );
    // Bad signature: edit the signed url after signing.
    let forged = tamper(&good, |v| {
        v["created_at"] = json!(v["created_at"].as_u64().unwrap() + 1)
    });
    let r = send(Some(forged), body.clone()).await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (401, Some("bad_signature"))
    );

    // URL: another route, another host, the relay's origin, the loopback origin.
    let now = app.now_ts();
    for url in [
        app.state.cfg.public_url(api::PATH_LINK_CONFIRM),
        "https://evil.example/account/link/request".to_string(),
        "http://relay.maxplayer.ai/account/link/request".to_string(),
        format!("{}{path}", app.base),
    ] {
        let r = send(
            Some(auth_header(&agent, &url, "POST", Some(&body), now)),
            body.clone(),
        )
        .await;
        assert_eq!(
            (r.status, r.body["error"].as_str()),
            (401, Some("url_mismatch")),
            "{url}"
        );
    }

    // Method.
    let r = send(
        Some(app.auth(&agent, path, "GET", Some(&body))),
        body.clone(),
    )
    .await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (401, Some("method_mismatch"))
    );
    let s = app
        .send(
            "GET",
            api::PATH_STATUS,
            Some(app.auth(&agent, api::PATH_STATUS, "POST", None)),
            None,
            &[],
        )
        .await;
    assert_eq!(
        (s.status, s.body["error"].as_str()),
        (401, Some("method_mismatch"))
    );

    // Age.
    let url = app.state.cfg.public_url(path);
    let stale = auth_header(
        &agent,
        &url,
        "POST",
        Some(&body),
        Timestamp::from(now.as_secs() - 61),
    );
    let r = send(Some(stale), body.clone()).await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (401, Some("stale_auth"))
    );

    // Payload: signed for another body, and no payload at all.
    let other = json!({ "email": "attacker@evil.example", "label": "a", "terms_version": "v1", "privacy_version": "v1" }).to_string().into_bytes();
    let r = send(Some(app.auth(&agent, path, "POST", Some(&body))), other).await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (401, Some("payload_mismatch"))
    );
    let r = send(Some(app.auth(&agent, path, "POST", None)), body.clone()).await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (401, Some("payload_missing"))
    );

    assert!(app.emails().is_empty(), "no refused request sent an email");

    // Replay: the same valid header twice.
    let once = app.auth(&agent, path, "POST", Some(&body));
    assert_eq!(send(Some(once.clone()), body.clone()).await.status, 202);
    let r = send(Some(once), body.clone()).await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (401, Some("replayed_auth"))
    );
    assert_eq!(app.emails().len(), 1);
}

// ------------------------------------------------------------------ idempotent re-link

#[tokio::test]
async fn relinking_the_same_key_and_email_is_idempotent() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    let pk = agent.public_key().to_hex();
    assert_eq!(
        app.link(&agent, EMAIL, "my-laptop").await.body["first_link"],
        true
    );
    let first = db::get_registration(&app.db, &pk).await.unwrap().unwrap();

    app.state.clock.advance(120);
    let r = app.link(&agent, "YOU@example.com", "renamed").await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["first_link"], false);
    assert_eq!(r.body["relay_member"], "added");

    assert_eq!(app.count("SELECT count(*) FROM registrations").await, 1);
    assert_eq!(
        app.count("SELECT count(*) FROM marketing_consents").await,
        1
    );
    let again = db::get_registration(&app.db, &pk).await.unwrap().unwrap();
    assert_eq!(again.created_at, first.created_at, "linked_at is kept");
    assert_eq!(again.label, "renamed");
    // A second 9030 was sent (relay treats it as a no-op) and membership is unchanged.
    let cmds = app.relay.commands();
    assert_eq!(cmds.len(), 2);
    assert!(cmds.iter().all(|e| e.kind.as_u16() == 9030));
    assert!(cmds[1].created_at > cmds[0].created_at);
    assert_eq!(app.relay.members.lock().unwrap().len(), 2, "admin + agent");
    // Only one welcome email.
    let welcomes = app
        .emails()
        .iter()
        .filter(|e| e.template() == "maxplayer-welcome")
        .count();
    assert_eq!(welcomes, 1);
    // An earlier opt-out survives a re-link.
    let r = app
        .post(&agent, api::PATH_MARKETING, json!({ "enabled": false }))
        .await;
    assert_eq!(r.body["marketing"], "unsubscribed");
    app.state.clock.advance(61);
    assert_eq!(
        app.link(&agent, EMAIL, "renamed").await.body["marketing"],
        "unsubscribed"
    );
}

#[tokio::test]
async fn one_email_many_agents_and_no_email_change() {
    let app = TestApp::start().await;
    let a = Keys::generate();
    let b = Keys::generate();
    assert_eq!(app.link(&a, EMAIL, "laptop").await.status, 200);
    // A second key on the same email gets the same answer as a new email (no
    // "already registered" leak) and becomes a second row.
    let r = app.request_link(&b, EMAIL, "vps").await;
    assert_eq!(r.status, 202);
    let code = app.last_code(EMAIL);
    let r = app.confirm(&b, &code).await;
    assert_eq!(r.body["first_link"], true);
    assert_eq!(app.count("SELECT count(*) FROM registrations").await, 2);
    assert_eq!(
        app.count("SELECT count(*) FROM marketing_consents").await,
        1
    );

    // A linked key cannot move to another email in v1.
    app.state.clock.advance(61);
    let r = app.request_link(&a, "other@example.com", "laptop").await;
    assert_eq!(r.status, 409);
    assert_eq!(r.body["error"], "key_linked_to_other_email");
    assert!(!r.body.to_string().contains("example.com"));
}

#[tokio::test]
async fn bad_inputs_are_refused_before_any_email() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    let cases = [
        (
            json!({ "email": "nope", "label": "a", "terms_version": "v1", "privacy_version": "v1" }),
            "invalid_email",
        ),
        (
            json!({ "email": EMAIL, "label": "<b>x</b>", "terms_version": "v1", "privacy_version": "v1" }),
            "invalid_label",
        ),
        (
            json!({ "email": EMAIL, "label": "a", "terms_version": "v9", "privacy_version": "v1" }),
            "unknown_terms_version",
        ),
        (
            json!({ "email": EMAIL, "label": "a", "terms_version": "v1", "privacy_version": "v9" }),
            "unknown_privacy_version",
        ),
        (
            json!({ "email": EMAIL, "label": "a", "terms_version": "v1", "privacy_version": "v1", "notice_version": "v9" }),
            "unknown_notice_version",
        ),
        (json!({ "email": EMAIL }), "invalid_body"),
    ];
    for (body, code) in cases {
        let r = app.post(&agent, api::PATH_LINK_REQUEST, body).await;
        assert_eq!((r.status, r.body["error"].as_str()), (400, Some(code)));
    }
    let r = app
        .post(
            &agent,
            api::PATH_LINK_CONFIRM,
            json!({ "code": "123456", "entered_by": "robot" }),
        )
        .await;
    assert_eq!(r.body["error"], "invalid_entered_by");
    assert!(app.emails().is_empty());
}

// ------------------------------------------------------------------ relay outbox, removal, reconcile

#[tokio::test]
async fn a_failed_add_is_retried_with_a_fresh_created_at() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    let pk = agent.public_key().to_hex();
    app.relay.fail_next.store(1, Ordering::SeqCst);
    let r = app.link(&agent, EMAIL, "a").await;
    assert_eq!(r.status, 200, "the link stands even if the relay is down");
    assert_eq!(r.body["relay_member"], "pending");
    assert!(!app.relay.is_member(&pk));
    assert_eq!(
        app.get(&agent, api::PATH_STATUS).await.body["relay_member"],
        "pending"
    );

    // Not due yet (30 s backoff).
    assert_eq!(sync::run_outbox_once(&app.state).await.unwrap(), 0);
    app.state.clock.advance(300);
    assert_eq!(sync::run_outbox_once(&app.state).await.unwrap(), 1);
    assert!(app.relay.is_member(&pk));
    let cmds = app.relay.commands();
    assert_eq!(cmds.len(), 2);
    assert!(
        cmds[1].created_at.as_secs() >= cmds[0].created_at.as_secs() + 300,
        "re-signed, not replayed"
    );
    assert_ne!(cmds[0].id, cmds[1].id);
    assert_eq!(
        app.get(&agent, api::PATH_STATUS).await.body["relay_member"],
        "added"
    );
    assert_eq!(
        sync::run_outbox_once(&app.state).await.unwrap(),
        0,
        "nothing left"
    );
}

#[tokio::test]
async fn operator_remove_sends_9031_and_deletes_the_row() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    let pk = agent.public_key();
    app.link(&agent, EMAIL, "a").await;
    assert!(app.relay.is_member(&pk.to_hex()));

    let (existed, confirmed) = sync::remove_key(&app.state, &pk).await.unwrap();
    assert!(existed && confirmed);
    assert!(!app.relay.is_member(&pk.to_hex()));
    assert!(
        db::get_registration(&app.db, &pk.to_hex())
            .await
            .unwrap()
            .is_none()
    );
    let ev = app.relay.commands().pop().unwrap();
    ev.verify().unwrap();
    assert_eq!(ev.kind.as_u16(), 9031);
    assert_eq!(ev.pubkey, app.admin.public_key());
    let tags: Vec<Vec<String>> = ev.tags.iter().map(|t| t.as_slice().to_vec()).collect();
    assert_eq!(
        tags,
        vec![vec!["-".to_string()], vec!["p".to_string(), pk.to_hex()]]
    );

    // Removing again: the relay says "member not found", treated as done.
    let (existed, confirmed) = sync::remove_key(&app.state, &pk).await.unwrap();
    assert!(!existed && confirmed);
    assert_eq!(app.count("SELECT count(*) FROM managed_members").await, 0);
}

#[tokio::test]
async fn reconcile_applies_sql_removal_and_restores_missing_members() {
    let app = TestApp::start().await;
    let kept = Keys::generate();
    let banned = Keys::generate();
    app.link(&kept, EMAIL, "kept").await;
    app.state.clock.advance(61);
    app.link(&banned, "b@example.com", "banned").await;
    // A key added by hand before registration existed (spec §10) must not be removed.
    let hand = Keys::generate().public_key().to_hex();
    app.relay
        .members
        .lock()
        .unwrap()
        .insert(hand.clone(), "member".into());

    // Operator ban = delete the row (spec §4); someone also dropped `kept` from the relay.
    sqlx::query("DELETE FROM registrations WHERE pubkey = $1")
        .bind(banned.public_key().to_hex())
        .execute(&app.db)
        .await
        .unwrap();
    app.relay
        .members
        .lock()
        .unwrap()
        .remove(&kept.public_key().to_hex());

    let report = sync::reconcile_once(&app.state).await.unwrap().unwrap();
    assert_eq!((report.queued_adds, report.queued_removes), (1, 1));
    assert_eq!(sync::run_outbox_once(&app.state).await.unwrap(), 2);
    assert!(app.relay.is_member(&kept.public_key().to_hex()));
    assert!(!app.relay.is_member(&banned.public_key().to_hex()));
    assert!(
        app.relay.is_member(&hand),
        "hand-added member kept by default"
    );
    assert!(
        app.relay.is_member(&app.admin.public_key().to_hex()),
        "admin never removed"
    );

    // Converged: a second reconcile queues nothing.
    let report = sync::reconcile_once(&app.state).await.unwrap().unwrap();
    assert_eq!((report.queued_adds, report.queued_removes), (0, 0));
}

#[tokio::test]
async fn reconcile_removals_flag_removes_unregistered_members_except_kept_ones() {
    let probe = Keys::generate().public_key();
    let app = TestApp::start_with(|c| {
        c.sync.reconcile_removals = true;
        c.sync.keep_members = vec![probe.to_bech32().unwrap()];
    })
    .await;
    let stray = Keys::generate().public_key().to_hex();
    let other_admin = Keys::generate().public_key().to_hex();
    {
        let mut m = app.relay.members.lock().unwrap();
        m.insert(stray.clone(), "member".into());
        m.insert(probe.to_hex(), "member".into());
        m.insert(other_admin.clone(), "admin".into());
    }
    let report = sync::reconcile_once(&app.state).await.unwrap().unwrap();
    assert_eq!((report.queued_adds, report.queued_removes), (0, 1));
    sync::run_outbox_once(&app.state).await.unwrap();
    assert!(!app.relay.is_member(&stray));
    assert!(app.relay.is_member(&probe.to_hex()));
    assert!(app.relay.is_member(&other_admin));
}

#[tokio::test]
async fn loopback_relay_is_signed_for_the_public_origin_with_its_host_header() {
    // Production shape: POST to http://127.0.0.1:3000, but buzz binds the tenant from
    // Host and checks NIP-98 against https://<that host>/events.
    let app =
        TestApp::start_with(|c| c.relay.auth_base_url = Some("https://relay.example.test".into()))
            .await;
    *app.relay.expect_origin.lock().unwrap() = "https://relay.example.test".into();
    *app.relay.expect_host.lock().unwrap() = Some("relay.example.test".into());
    let agent = Keys::generate();
    let r = app.link(&agent, EMAIL, "a").await;
    assert_eq!(r.body["relay_member"], "added", "{:?}", r.body);
    assert!(app.relay.is_member(&agent.public_key().to_hex()));
    let report = sync::reconcile_once(&app.state).await.unwrap().unwrap();
    assert_eq!((report.queued_adds, report.queued_removes), (0, 0));
    assert_eq!(app.relay.queries.load(Ordering::SeqCst), 1);

    // Signed for the loopback origin instead: the relay refuses it and it stays queued.
    let app = TestApp::start().await;
    *app.relay.expect_origin.lock().unwrap() = "https://relay.example.test".into();
    let r = app.link(&Keys::generate(), EMAIL, "a").await;
    assert_eq!(r.body["relay_member"], "pending");
}

// ------------------------------------------------------------------ marketing

#[tokio::test]
async fn marketing_on_off_for_a_linked_key() {
    let app = TestApp::start().await;
    let agent = Keys::generate();
    let r = app
        .post(&agent, api::PATH_MARKETING, json!({ "enabled": false }))
        .await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (404, Some("not_registered"))
    );
    app.link(&agent, EMAIL, "a").await;

    let r = app
        .post(&agent, api::PATH_MARKETING, json!({ "enabled": false }))
        .await;
    assert_eq!(r.body["marketing"], "unsubscribed");
    let c = db::get_consent(&app.db, EMAIL).await.unwrap().unwrap();
    assert_eq!(c.unsubscribe_method.as_deref(), Some("cli"));
    assert!(c.unsubscribed_at.is_some());
    assert_eq!(
        app.get(&agent, api::PATH_STATUS).await.body["marketing"],
        "unsubscribed"
    );

    let r = app
        .post(&agent, api::PATH_MARKETING, json!({ "enabled": true }))
        .await;
    assert_eq!(r.body["marketing"], "subscribed");
    let c = db::get_consent(&app.db, EMAIL).await.unwrap().unwrap();
    assert_eq!(c.method, "cli");
    assert!(c.unsubscribed_at.is_none() && c.unsubscribe_method.is_none());
}

// ------------------------------------------------------------------ Resend webhook

fn signed(id: &str, ts: i64, body: &[u8]) -> Vec<(&'static str, String)> {
    let key = webhook::parse_secret(common::WEBHOOK_SECRET).unwrap();
    vec![
        ("svix-id", id.to_string()),
        ("svix-timestamp", ts.to_string()),
        (
            "svix-signature",
            format!("v1,{}", webhook::sign(&key, id, ts, body)),
        ),
    ]
}

#[tokio::test]
async fn webhook_records_bounces_complaints_and_unsubscribes() {
    let app = TestApp::start().await;
    let now = app.state.now().timestamp();
    let body = json!({ "type": "email.bounced", "created_at": "2026-10-28T08:00:00Z", "data": { "to": ["You@Example.com"] } }).to_string().into_bytes();

    let r = app
        .send("POST", api::PATH_WEBHOOK, None, Some(body.clone()), &[])
        .await;
    assert_eq!(r.status, 401);
    let mut bad = signed("msg_1", now, &body);
    bad[2].1 = "v1,AAAA".into();
    assert_eq!(
        app.send("POST", api::PATH_WEBHOOK, None, Some(body.clone()), &bad)
            .await
            .status,
        401
    );
    assert_eq!(
        app.send(
            "POST",
            api::PATH_WEBHOOK,
            None,
            Some(body.clone()),
            &signed("msg_1", now - 400, &body)
        )
        .await
        .status,
        401
    );

    let r = app
        .send(
            "POST",
            api::PATH_WEBHOOK,
            None,
            Some(body.clone()),
            &signed("msg_1", now, &body),
        )
        .await;
    assert_eq!((r.status, r.body["status"].as_str()), (200, Some("ok")));
    let r = app
        .send(
            "POST",
            api::PATH_WEBHOOK,
            None,
            Some(body.clone()),
            &signed("msg_1", now, &body),
        )
        .await;
    assert_eq!(r.body["status"], "duplicate");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM email_bounces WHERE email = $1")
        .bind(EMAIL)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(n, 1);

    let body =
        json!({ "type": "contact.updated", "data": { "email": EMAIL, "unsubscribed": true } })
            .to_string()
            .into_bytes();
    app.send(
        "POST",
        api::PATH_WEBHOOK,
        None,
        Some(body.clone()),
        &signed("msg_2", now, &body),
    )
    .await;
    let c = db::get_consent(&app.db, EMAIL).await.unwrap().unwrap();
    assert_eq!(c.unsubscribe_method.as_deref(), Some("link"));

    let body = json!({ "type": "email.complained", "data": { "to": ["c@example.com"] } })
        .to_string()
        .into_bytes();
    app.send(
        "POST",
        api::PATH_WEBHOOK,
        None,
        Some(body.clone()),
        &signed("msg_3", now, &body),
    )
    .await;
    let c = db::get_consent(&app.db, "c@example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.unsubscribe_method.as_deref(), Some("complaint"));
}

#[tokio::test]
async fn webhook_without_a_secret_is_unavailable_and_health_works() {
    let app = TestApp::start().await;
    let mut state = maxplayer_registration::AppState::new(
        app.state.cfg.clone(),
        app.db.clone(),
        Keys::generate(),
        None,
        None,
    )
    .unwrap();
    state.clock = app.state.clock.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(api::serve(
        l,
        std::sync::Arc::new(state),
        std::future::pending(),
    ));
    let c = reqwest::Client::new();
    let r = c
        .post(format!("{base}{}", api::PATH_WEBHOOK))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 503);
    let r = c
        .get(format!("{base}{}", api::PATH_HEALTH))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

// ------------------------------------------------------------------ cleanup

#[tokio::test]
async fn cleanup_drops_old_codes_counters_and_replay_records() {
    let app = TestApp::start().await;
    app.request_link(&Keys::generate(), EMAIL, "a").await;
    assert!(app.count("SELECT count(*) FROM email_codes").await > 0);
    assert!(app.count("SELECT count(*) FROM rate_events").await > 0);
    app.state.clock.advance(2 * 86_400);
    db::cleanup(&app.db, app.state.now()).await.unwrap();
    assert_eq!(app.count("SELECT count(*) FROM email_codes").await, 0);
    assert_eq!(app.count("SELECT count(*) FROM rate_events").await, 0);
    assert_eq!(app.count("SELECT count(*) FROM nip98_seen").await, 0);
}
