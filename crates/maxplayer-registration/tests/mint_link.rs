//! Cosigned mint link (`/account/link/mint/*`) against a real Postgres, the fake relay and the
//! fake Resend. Each test targets one check, so removing that check fails exactly here.

mod common;

use common::{Resp, TestApp};
use maxplayer_registration::{api, db, mint_link};
use nostr::{JsonUtil, Keys, PublicKey, Timestamp, ToBech32};
use serde_json::{Value, json};

const EMAIL: &str = "you@example.com";

async fn challenge(app: &TestApp, account: &Keys, mint: &PublicKey) -> Resp {
    app.post(
        account,
        api::PATH_LINK_MINT_CHALLENGE,
        json!({ "mint_pubkey": mint.to_hex() }),
    )
    .await
}

/// The mint key's proof for `challenge`, made out to `account`, for the real confirm URL.
fn proof(app: &TestApp, mint: &Keys, challenge: &str, account: &PublicKey) -> Value {
    proof_for(app, mint, challenge, account, app.now_ts())
}

fn proof_for(
    app: &TestApp,
    mint: &Keys,
    challenge: &str,
    account: &PublicKey,
    ts: Timestamp,
) -> Value {
    let url = app.state.cfg.public_url(api::PATH_LINK_MINT_CONFIRM);
    let ev = mint_link::build_proof(mint, &url, challenge, account, ts);
    serde_json::from_str(&ev.as_json()).unwrap()
}

async fn confirm(app: &TestApp, account: &Keys, body: Value) -> Resp {
    app.post(account, api::PATH_LINK_MINT_CONFIRM, body).await
}

fn body(challenge: &str, mint: &PublicKey, label: &str, proof: Value) -> Value {
    json!({ "challenge": challenge, "mint_pubkey": mint.to_hex(), "label": label, "mint_proof": proof })
}

/// Challenge + proof + confirm, all correct.
async fn cosign(app: &TestApp, account: &Keys, mint: &Keys, label: &str) -> Resp {
    let r = challenge(app, account, &mint.public_key()).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let c = r.body["challenge"].as_str().unwrap().to_string();
    let p = proof(app, mint, &c, &account.public_key());
    confirm(app, account, body(&c, &mint.public_key(), label, p)).await
}

fn err(r: &Resp) -> (u16, Option<&str>) {
    (r.status, r.body["error"].as_str())
}

/// An email-linked account key (the operator's agent/home key).
async fn account(app: &TestApp) -> Keys {
    let k = Keys::generate();
    let r = app.link(&k, EMAIL, "laptop").await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    k
}

#[tokio::test]
async fn cosigned_link_copies_the_account_and_adds_the_mint_without_an_email() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    let emails_before = app.emails().len();

    let r = challenge(&app, &acct, &mint.public_key()).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["expires_in_secs"], 300);
    assert_eq!(r.body["mint_pubkey"], mint.public_key().to_hex());
    assert_eq!(r.body["proof_kind"], mint_link::PROOF_KIND);
    assert_eq!(
        r.body["proof_url"],
        "https://relay.maxplayer.ai/account/link/mint/confirm"
    );
    let c = r.body["challenge"].as_str().unwrap().to_string();
    assert!(mint_link::valid_challenge(&c));

    let p = proof(&app, &mint, &c, &acct.public_key());
    let r = confirm(&app, &acct, body(&c, &mint.public_key(), "mint-1", p)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["status"], "linked");
    assert_eq!(r.body["first_link"], true);
    assert_eq!(r.body["email"], EMAIL);
    assert_eq!(r.body["label"], "mint-1");
    assert_eq!(r.body["linked_by"], acct.public_key().to_hex());
    assert_eq!(r.body["terms_version"], "20261029");
    assert_eq!(r.body["relay_member"], "added");

    let mint_hex = mint.public_key().to_hex();
    assert!(app.relay.is_member(&mint_hex));
    let reg = db::get_registration(&app.db, &mint_hex)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reg.code_entered_by, "cosigned");
    assert_eq!(reg.linked_by.as_deref(), Some(&*acct.public_key().to_hex()));
    assert_eq!(
        app.emails().len(),
        emails_before,
        "no code or welcome email"
    );

    // The mint key's own status shows the link and who vouched for it.
    let s = app.get(&mint, api::PATH_STATUS).await;
    assert_eq!(s.body["registered"], true);
    assert_eq!(s.body["code_entered_by"], "cosigned");
    assert_eq!(s.body["linked_by"], acct.public_key().to_hex());
    // The account's own status is unchanged and names no voucher.
    let s = app.get(&acct, api::PATH_STATUS).await;
    assert_eq!(s.body["code_entered_by"], "human");
    assert_eq!(s.body["linked_by"], Value::Null);
}

#[tokio::test]
async fn npub_mint_key_is_accepted() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    let r = app
        .post(
            &acct,
            api::PATH_LINK_MINT_CHALLENGE,
            json!({ "mint_pubkey": mint.public_key().to_bech32().unwrap() }),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["mint_pubkey"], mint.public_key().to_hex());
}

#[tokio::test]
async fn a_challenge_is_used_once() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    let r = challenge(&app, &acct, &mint.public_key()).await;
    let c = r.body["challenge"].as_str().unwrap().to_string();
    let b = body(
        &c,
        &mint.public_key(),
        "m",
        proof(&app, &mint, &c, &acct.public_key()),
    );
    assert_eq!(confirm(&app, &acct, b.clone()).await.status, 200);
    // Same body, fresh NIP-98 (so not `replayed_auth`): the challenge itself is spent.
    let r = confirm(&app, &acct, b).await;
    assert_eq!(err(&r), (410, Some("challenge_used")));
}

#[tokio::test]
async fn a_challenge_expires() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    let r = challenge(&app, &acct, &mint.public_key()).await;
    let c = r.body["challenge"].as_str().unwrap().to_string();
    app.state.clock.advance(301);
    // Proof and NIP-98 are fresh; only the challenge is old.
    let p = proof(&app, &mint, &c, &acct.public_key());
    let r = confirm(&app, &acct, body(&c, &mint.public_key(), "m", p)).await;
    assert_eq!(err(&r), (410, Some("challenge_expired")));
    assert!(!app.relay.is_member(&mint.public_key().to_hex()));
}

#[tokio::test]
async fn the_challenge_is_bound_to_the_mint_key() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    let other = Keys::generate();
    let r = challenge(&app, &acct, &mint.public_key()).await;
    let c = r.body["challenge"].as_str().unwrap().to_string();

    // Swap the mint key for another one that signs a valid proof for the same challenge.
    let p = proof(&app, &other, &c, &acct.public_key());
    let r = confirm(&app, &acct, body(&c, &other.public_key(), "m", p)).await;
    assert_eq!(err(&r), (404, Some("unknown_challenge")));

    // Name the right mint but let another key sign the proof.
    let p = proof(&app, &other, &c, &acct.public_key());
    let r = confirm(&app, &acct, body(&c, &mint.public_key(), "m", p)).await;
    assert_eq!(err(&r), (401, Some("mint_proof_invalid")));
    assert_eq!(r.body["reason"], "wrong_key");

    assert!(!app.relay.is_member(&other.public_key().to_hex()));
    assert!(!app.relay.is_member(&mint.public_key().to_hex()));
    // Nothing was spent: the real pair still links.
    let p = proof(&app, &mint, &c, &acct.public_key());
    let r = confirm(&app, &acct, body(&c, &mint.public_key(), "m", p)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
}

#[tokio::test]
async fn the_challenge_is_bound_to_the_account_key() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    // Another registered account (another email) tries to take over the challenge.
    let thief = Keys::generate();
    assert_eq!(app.link(&thief, "thief@example.com", "t").await.status, 200);
    let mint = Keys::generate();
    let r = challenge(&app, &acct, &mint.public_key()).await;
    let c = r.body["challenge"].as_str().unwrap().to_string();

    // Even with a proof made out to the thief, the challenge belongs to `acct`.
    let p = proof(&app, &mint, &c, &thief.public_key());
    let r = confirm(&app, &thief, body(&c, &mint.public_key(), "m", p)).await;
    assert_eq!(err(&r), (404, Some("unknown_challenge")));

    // A mint proof made out to the thief cannot be used by the real account either.
    let p = proof(&app, &mint, &c, &thief.public_key());
    let r = confirm(&app, &acct, body(&c, &mint.public_key(), "m", p)).await;
    assert_eq!(err(&r), (401, Some("mint_proof_invalid")));
    assert_eq!(r.body["reason"], "wrong_account");
    assert!(
        db::get_registration(&app.db, &mint.public_key().to_hex())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_mint_proof_must_cover_this_endpoint_challenge_and_time() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    let r = challenge(&app, &acct, &mint.public_key()).await;
    let c = r.body["challenge"].as_str().unwrap().to_string();
    let a = acct.public_key();
    let m = mint.public_key();

    // Missing proof altogether: refused as a bad body.
    let r = confirm(
        &app,
        &acct,
        json!({ "challenge": c, "mint_pubkey": m.to_hex(), "label": "m", "mint_proof": "x" }),
    )
    .await;
    assert_eq!(err(&r), (400, Some("invalid_body")));

    // Signed for another endpoint.
    let other_url = app.state.cfg.public_url(api::PATH_LINK_CONFIRM);
    let ev = mint_link::build_proof(&mint, &other_url, &c, &a, app.now_ts());
    let p: Value = serde_json::from_str(&ev.as_json()).unwrap();
    let r = confirm(&app, &acct, body(&c, &m, "m", p)).await;
    assert_eq!(r.body["reason"], "wrong_url", "{:?}", r.body);

    // Signed over another (e.g. an older) challenge.
    let p = proof(&app, &mint, &mint_link::new_challenge(), &a);
    let r = confirm(&app, &acct, body(&c, &m, "m", p)).await;
    assert_eq!(r.body["reason"], "wrong_challenge", "{:?}", r.body);

    // Too old.
    let old = Timestamp::from(app.now_ts().as_secs() - 61);
    let p = proof_for(&app, &mint, &c, &a, old);
    let r = confirm(&app, &acct, body(&c, &m, "m", p)).await;
    assert_eq!(r.body["reason"], "stale", "{:?}", r.body);

    // Edited after signing.
    let mut p = proof(&app, &mint, &c, &a);
    p["content"] = json!("x");
    let r = confirm(&app, &acct, body(&c, &m, "m", p)).await;
    assert_eq!(r.body["reason"], "bad_signature", "{:?}", r.body);

    assert!(!app.relay.is_member(&m.to_hex()));
    // None of these spent the challenge.
    let p = proof(&app, &mint, &c, &a);
    assert_eq!(confirm(&app, &acct, body(&c, &m, "m", p)).await.status, 200);
}

#[tokio::test]
async fn only_an_email_linked_key_can_vouch() {
    let app = TestApp::start().await;
    let stranger = Keys::generate();
    let mint = Keys::generate();
    let r = challenge(&app, &stranger, &mint.public_key()).await;
    assert_eq!(err(&r), (403, Some("account_not_registered")));

    // A cosigned mint key cannot vouch for a further key.
    let acct = account(&app).await;
    assert_eq!(cosign(&app, &acct, &mint, "m").await.status, 200);
    let next = Keys::generate();
    let r = challenge(&app, &mint, &next.public_key()).await;
    assert_eq!(err(&r), (403, Some("account_is_cosigned")));
}

/// The confirm re-checks the voucher under lock: an account that stopped being an email link
/// (here: rewritten to a cosigned row by SQL) after the challenge cannot confirm.
#[tokio::test]
async fn an_account_that_became_cosigned_cannot_confirm() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    let r = challenge(&app, &acct, &mint.public_key()).await;
    let c = r.body["challenge"].as_str().unwrap().to_string();
    sqlx::query(
        "UPDATE registrations SET code_entered_by = 'cosigned', linked_by = $2 WHERE pubkey = $1",
    )
    .bind(acct.public_key().to_hex())
    .bind(Keys::generate().public_key().to_hex())
    .execute(&app.db)
    .await
    .unwrap();
    let p = proof(&app, &mint, &c, &acct.public_key());
    let r = confirm(&app, &acct, body(&c, &mint.public_key(), "m", p)).await;
    assert_eq!(err(&r), (403, Some("account_is_cosigned")));
}

#[tokio::test]
async fn an_account_removed_after_the_challenge_cannot_confirm() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    let r = challenge(&app, &acct, &mint.public_key()).await;
    let c = r.body["challenge"].as_str().unwrap().to_string();
    sqlx::query("DELETE FROM registrations WHERE pubkey = $1")
        .bind(acct.public_key().to_hex())
        .execute(&app.db)
        .await
        .unwrap();
    let p = proof(&app, &mint, &c, &acct.public_key());
    let r = confirm(&app, &acct, body(&c, &mint.public_key(), "m", p)).await;
    assert_eq!(err(&r), (403, Some("account_not_registered")));
    assert!(
        db::get_registration(&app.db, &mint.public_key().to_hex())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_mint_key_linked_elsewhere_is_refused() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    // The mint key already belongs to another email (by its own code).
    let mint = Keys::generate();
    assert_eq!(app.link(&mint, "other@example.com", "x").await.status, 200);
    let r = challenge(&app, &acct, &mint.public_key()).await;
    assert_eq!(err(&r), (409, Some("mint_linked_to_other_account")));
    assert!(!r.body.to_string().contains("example.com"), "{:?}", r.body);

    // Linked elsewhere between the challenge and the confirm.
    let mint2 = Keys::generate();
    let r = challenge(&app, &acct, &mint2.public_key()).await;
    let c = r.body["challenge"].as_str().unwrap().to_string();
    app.state.clock.advance(61);
    assert_eq!(app.link(&mint2, "other@example.com", "x").await.status, 200);
    let p = proof(&app, &mint2, &c, &acct.public_key());
    let r = confirm(&app, &acct, body(&c, &mint2.public_key(), "m", p)).await;
    assert_eq!(err(&r), (409, Some("mint_linked_to_other_account")));
    let reg = db::get_registration(&app.db, &mint2.public_key().to_hex())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reg.email, "other@example.com",
        "the other link is untouched"
    );
    assert_eq!(reg.linked_by, None);

    // Cosigned to another account first: also refused.
    let other = Keys::generate();
    assert_eq!(app.link(&other, "third@example.com", "o").await.status, 200);
    let mint3 = Keys::generate();
    assert_eq!(cosign(&app, &other, &mint3, "m").await.status, 200);
    let r = challenge(&app, &acct, &mint3.public_key()).await;
    assert_eq!(err(&r), (409, Some("mint_linked_to_other_account")));
}

#[tokio::test]
async fn a_mint_key_email_linked_to_the_same_email_is_left_alone() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    app.state.clock.advance(61);
    assert_eq!(app.link(&mint, EMAIL, "own").await.status, 200);
    let r = challenge(&app, &acct, &mint.public_key()).await;
    assert_eq!(err(&r), (409, Some("mint_already_linked")));
}

#[tokio::test]
async fn relinking_a_cosigned_mint_refreshes_it() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    assert_eq!(cosign(&app, &acct, &mint, "m1").await.status, 200);
    let r = cosign(&app, &acct, &mint, "m2").await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["first_link"], false);
    assert_eq!(r.body["label"], "m2");
    assert_eq!(
        app.count("SELECT count(*) FROM registrations").await,
        2,
        "account + mint"
    );
}

#[tokio::test]
async fn a_cosigned_key_can_still_link_by_email_code() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();
    assert_eq!(cosign(&app, &acct, &mint, "m").await.status, 200);
    // Same email, by code: the row becomes an ordinary email link (no voucher).
    let r = app.link(&mint, EMAIL, "m").await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let reg = db::get_registration(&app.db, &mint.public_key().to_hex())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reg.code_entered_by, "human");
    assert_eq!(reg.linked_by, None);
}

#[tokio::test]
async fn bad_requests_are_refused_before_anything_is_stored() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    let mint = Keys::generate();

    for (mint_pubkey, code) in [
        (json!("nope"), "invalid_mint_pubkey"),
        (json!(acct.public_key().to_hex()), "same_key"),
    ] {
        let r = app
            .post(
                &acct,
                api::PATH_LINK_MINT_CHALLENGE,
                json!({ "mint_pubkey": mint_pubkey }),
            )
            .await;
        assert_eq!(err(&r), (400, Some(code)));
    }
    let r = app
        .post(&acct, api::PATH_LINK_MINT_CHALLENGE, json!({}))
        .await;
    assert_eq!(err(&r), (400, Some("invalid_body")));
    assert_eq!(
        app.count("SELECT count(*) FROM mint_link_challenges").await,
        0
    );

    let r = challenge(&app, &acct, &mint.public_key()).await;
    let c = r.body["challenge"].as_str().unwrap().to_string();
    let a = acct.public_key();
    let m = mint.public_key();
    let p = proof(&app, &mint, &c, &a);
    let r = confirm(&app, &acct, body("ab", &m, "m", p.clone())).await;
    assert_eq!(err(&r), (400, Some("invalid_challenge")));
    let r = confirm(&app, &acct, body(&c, &m, "<b>", p.clone())).await;
    assert_eq!(err(&r), (400, Some("invalid_label")));
    let r = confirm(&app, &acct, body(&c, &a, "m", p)).await;
    assert_eq!(err(&r), (400, Some("same_key")));

    // The confirm is NIP-98 authenticated like every route.
    let b = body(&c, &m, "m", proof(&app, &mint, &c, &a))
        .to_string()
        .into_bytes();
    let r = app
        .send("POST", api::PATH_LINK_MINT_CONFIRM, None, Some(b), &[])
        .await;
    assert_eq!(err(&r), (401, Some("missing_auth")));
    assert!(!app.relay.is_member(&m.to_hex()));
}

#[tokio::test]
async fn challenges_are_rate_limited_per_account() {
    let app = TestApp::start_with(|cfg| cfg.limits.mint_challenges_per_pubkey_per_hour = 2).await;
    let acct = account(&app).await;
    for _ in 0..2 {
        let r = challenge(&app, &acct, &Keys::generate().public_key()).await;
        assert_eq!(r.status, 200, "{:?}", r.body);
    }
    let r = challenge(&app, &acct, &Keys::generate().public_key()).await;
    assert_eq!(err(&r), (429, Some("rate_limited")));
}

#[tokio::test]
async fn cleanup_drops_old_challenges() {
    let app = TestApp::start().await;
    let acct = account(&app).await;
    challenge(&app, &acct, &Keys::generate().public_key()).await;
    assert_eq!(
        app.count("SELECT count(*) FROM mint_link_challenges").await,
        1
    );
    app.state.clock.advance(2 * 86_400);
    db::cleanup(&app.db, app.state.now()).await.unwrap();
    assert_eq!(
        app.count("SELECT count(*) FROM mint_link_challenges").await,
        0
    );
}
