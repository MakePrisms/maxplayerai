//! Team invite links (docs/specs/maxplayer-team.md §12) against a real Postgres and a fake relay
//! that applies buzz's private-channel rules (9000/9001 by members only, 39002 readable by
//! members only).

mod common;

use common::TestApp;
use maxplayer_registration::{api, sync};
use nostr::Keys;
use serde_json::{Value, json};

const TTL: i64 = 72 * 3600;

async fn linked(app: &TestApp, label: &str) -> Keys {
    let k = Keys::generate();
    let r = app.link(&k, &format!("{label}@example.com"), label).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    k
}

/// A team with a linked inviter and the invite key in it (what `team invite --link` does first).
async fn team_with_invite_key(app: &TestApp) -> (Keys, String) {
    let inviter = linked(app, "inviter").await;
    let channel = app.relay.team(&[&inviter]);
    app.relay.add_to_team(&channel, &app.invite);
    (inviter, channel)
}

async fn create(app: &TestApp, who: &Keys, body: Value) -> common::Resp {
    app.post(who, api::PATH_TEAM_INVITE, body).await
}

fn token_of(link: &Value) -> String {
    let link = link.as_str().unwrap();
    link.strip_prefix("https://www.maxplayer.ai/join/")
        .unwrap_or_else(|| panic!("{link}"))
        .to_string()
}

async fn join(app: &TestApp, who: &Keys, token: &str) -> common::Resp {
    app.post(who, api::PATH_TEAM_JOIN, json!({ "token": token }))
        .await
}

#[tokio::test]
async fn a_member_creates_a_link_and_a_linked_agent_joins_with_it() {
    let app = TestApp::start().await;
    let (inviter, channel) = team_with_invite_key(&app).await;
    let r = create(&app, &inviter, json!({ "channel": channel })).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["channel"], channel.as_str());
    assert_eq!(r.body["expires_in_secs"], TTL);
    let token = token_of(&r.body["link"]);
    assert_eq!(token.len(), 43, "32 random bytes, base64url");
    assert!(!token.contains(&inviter.public_key().to_hex()[..16]));

    // Only the hash is stored.
    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_hash FROM team_invite_links")
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].windows(8).any(|w| w == &token.as_bytes()[..8]));
    let hits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM team_invite_links WHERE encode(token_hash, 'escape') LIKE '%' || $1 || '%'",
    )
    .bind(&token)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(hits, 0);

    let friend = linked(&app, "friend").await;
    assert!(!app.relay.in_team(&channel, &friend));
    let r = join(&app, &friend, &token).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body, json!({ "status": "joined", "channel": channel }));
    assert!(app.relay.in_team(&channel, &friend));
    // The add is a kind 9000 signed by the invite key, naming the joiner.
    let add = app
        .relay
        .commands()
        .into_iter()
        .rfind(|e| e.kind.as_u16() == 9000)
        .unwrap();
    assert_eq!(add.pubkey, app.invite.public_key());
    let tags: Vec<Vec<String>> = add.tags.iter().map(|t| t.as_slice().to_vec()).collect();
    assert_eq!(
        tags,
        vec![
            vec!["h".to_string(), channel.clone()],
            vec!["p".to_string(), friend.public_key().to_hex()]
        ]
    );
    // No use cap: a second agent joins with the same link.
    let second = linked(&app, "second").await;
    assert_eq!(join(&app, &second, &token).await.status, 200);
    assert!(app.relay.in_team(&channel, &second));
}

#[tokio::test]
async fn an_unlinked_joiner_is_refused_and_not_added() {
    let app = TestApp::start().await;
    let (inviter, channel) = team_with_invite_key(&app).await;
    let r = create(&app, &inviter, json!({ "channel": channel })).await;
    let token = token_of(&r.body["link"]);
    let stranger = Keys::generate();
    let r = join(&app, &stranger, &token).await;
    assert_eq!(r.status, 403, "{:?}", r.body);
    assert_eq!(r.body["error"], "not_registered");
    assert!(
        r.body["message"]
            .as_str()
            .unwrap()
            .contains("maxplayer account link")
    );
    assert!(!app.relay.in_team(&channel, &stranger));
    // Unlinked: refused before the token is even looked up.
    let r = join(&app, &stranger, "not-a-token").await;
    assert_eq!(r.body["error"], "not_registered");
}

#[tokio::test]
async fn expired_revoked_and_unknown_links_are_refused() {
    let app = TestApp::start().await;
    let (inviter, channel) = team_with_invite_key(&app).await;
    let friend = linked(&app, "friend").await;

    let r = join(&app, &friend, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (404, json!("invalid_invite"))
    );

    // --expires: a short link.
    let r = create(
        &app,
        &inviter,
        json!({ "channel": channel, "expires_in_secs": 3600 }),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let short = token_of(&r.body["link"]);
    let r = create(&app, &inviter, json!({ "channel": channel })).await;
    let long = token_of(&r.body["link"]);
    app.state.clock.advance(3601);
    let r = join(&app, &friend, &short).await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (410, json!("invite_expired"))
    );
    assert!(!app.relay.in_team(&channel, &friend));

    // Revoke: the long one stops working too.
    let r = app
        .post(
            &inviter,
            api::PATH_TEAM_REVOKE,
            json!({ "channel": channel }),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["revoked"], 1, "the expired one is not counted");
    let r = join(&app, &friend, &long).await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (410, json!("invite_revoked"))
    );
    assert!(!app.relay.in_team(&channel, &friend));
    // With no live link left, the invite key left the team at once (a 9001 naming itself).
    assert!(!app.relay.in_team(&channel, &app.invite));
    let leave = app
        .relay
        .commands()
        .into_iter()
        .rfind(|e| e.kind.as_u16() == 9001)
        .unwrap();
    assert_eq!(leave.pubkey, app.invite.public_key());
    assert!(
        leave
            .tags
            .iter()
            .any(|t| t.as_slice() == ["p", &app.invite.public_key().to_hex()])
    );

    // A default link expires after 72 h.
    app.relay.add_to_team(&channel, &app.invite);
    let r = create(&app, &inviter, json!({ "channel": channel })).await;
    let token = token_of(&r.body["link"]);
    app.state.clock.advance(TTL - 10);
    let other = linked(&app, "other").await;
    assert_eq!(join(&app, &other, &token).await.status, 200);
    app.state.clock.advance(20);
    let late = linked(&app, "late").await;
    assert_eq!(
        join(&app, &late, &token).await.body["error"],
        "invite_expired"
    );
}

#[tokio::test]
async fn only_linked_members_create_or_revoke_and_the_invite_key_must_be_in_the_team() {
    let app = TestApp::start().await;
    let (inviter, channel) = team_with_invite_key(&app).await;
    let outsider = linked(&app, "outsider").await;
    let r = create(&app, &outsider, json!({ "channel": channel })).await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (403, json!("not_team_member"))
    );
    let r = app
        .post(
            &outsider,
            api::PATH_TEAM_REVOKE,
            json!({ "channel": channel }),
        )
        .await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (403, json!("not_team_member"))
    );

    // A member whose key is not linked.
    let unlinked = Keys::generate();
    app.relay.add_to_team(&channel, &unlinked);
    let r = create(&app, &unlinked, json!({ "channel": channel })).await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (403, json!("not_registered"))
    );

    // A team the invite key was not added to.
    let bare = app.relay.team(&[&inviter]);
    let r = create(&app, &inviter, json!({ "channel": bare })).await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (409, json!("invite_key_not_in_team"))
    );

    for (body, code) in [
        (json!({ "channel": "Not-A-Uuid" }), "invalid_channel"),
        (
            json!({ "channel": channel, "expires_in_secs": 10 }),
            "invalid_expiry",
        ),
        (
            json!({ "channel": channel, "expires_in_secs": 31 * 86400 }),
            "invalid_expiry",
        ),
    ] {
        let r = create(&app, &inviter, body).await;
        assert_eq!((r.status, r.body["error"].clone()), (400, json!(code)));
    }
    assert_eq!(app.count("SELECT count(*) FROM team_invite_links").await, 0);
}

#[tokio::test]
async fn reading_the_join_paths_with_get_redeems_nothing() {
    let app = TestApp::start().await;
    let (inviter, channel) = team_with_invite_key(&app).await;
    let r = create(&app, &inviter, json!({ "channel": channel })).await;
    let token = token_of(&r.body["link"]);
    let friend = linked(&app, "friend").await;
    // A link preview or a browser: a GET, signed or not, of the join endpoint or the link path.
    for path in [api::PATH_TEAM_JOIN.to_string(), format!("/join/{token}")] {
        let r = app.get(&friend, &path).await;
        assert!(r.status == 405 || r.status == 404, "{path}: {}", r.status);
        let r = app.send("GET", &path, None, None, &[]).await;
        assert!(r.status == 405 || r.status == 404, "{path}: {}", r.status);
    }
    assert!(!app.relay.in_team(&channel, &friend));
    assert_eq!(join(&app, &friend, &token).await.status, 200, "still live");
}

#[tokio::test]
async fn the_sweep_makes_the_invite_key_leave_teams_without_a_live_link() {
    let app = TestApp::start().await;
    let (inviter, expiring) = team_with_invite_key(&app).await;
    let live = app.relay.team(&[&inviter]);
    app.relay.add_to_team(&live, &app.invite);
    let r = create(
        &app,
        &inviter,
        json!({ "channel": expiring, "expires_in_secs": 600 }),
    )
    .await;
    assert_eq!(r.status, 200);
    let r = create(&app, &inviter, json!({ "channel": live })).await;
    assert_eq!(r.status, 200);

    assert_eq!(
        sync::sweep_invites(&app.state).await.unwrap(),
        0,
        "both live"
    );
    assert!(app.relay.in_team(&expiring, &app.invite));
    app.state.clock.advance(601);
    assert_eq!(sync::sweep_invites(&app.state).await.unwrap(), 1);
    assert!(
        !app.relay.in_team(&expiring, &app.invite),
        "left the expired team"
    );
    assert!(
        app.relay.in_team(&live, &app.invite),
        "stayed in the live one"
    );
    assert!(app.relay.in_team(&expiring, &inviter), "members untouched");
    assert_eq!(
        app.count("SELECT count(*) FROM team_invite_channels").await,
        1
    );
    // Idempotent; and an owner having already removed the key counts as left.
    assert_eq!(sync::sweep_invites(&app.state).await.unwrap(), 0);
    app.relay
        .teams
        .lock()
        .unwrap()
        .get_mut(&live)
        .unwrap()
        .remove(&app.invite.public_key().to_hex());
    app.state.clock.advance(TTL);
    assert_eq!(sync::sweep_invites(&app.state).await.unwrap(), 1);
    assert_eq!(
        app.count("SELECT count(*) FROM team_invite_channels").await,
        0
    );

    // A new link brings the team back under the sweep.
    app.relay.add_to_team(&expiring, &app.invite);
    assert_eq!(
        create(&app, &inviter, json!({ "channel": expiring }))
            .await
            .status,
        200
    );
    assert_eq!(
        app.count("SELECT count(*) FROM team_invite_channels").await,
        1
    );
}

#[tokio::test]
async fn without_an_invite_key_the_sweep_does_nothing() {
    let app = TestApp::start().await;
    let state = maxplayer_registration::AppState::new(
        app.state.cfg.clone(),
        app.db.clone(),
        app.admin.clone(),
        None,
        None,
    )
    .unwrap();
    assert!(state.invite.is_none(), "AppState::new has no invite key");
    assert_eq!(sync::sweep_invites(&state).await.unwrap(), 0);
}
