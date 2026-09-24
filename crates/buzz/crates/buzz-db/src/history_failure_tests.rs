//! SQL row-decoding failure must not become a successful short history page.
use super::*;

#[tokio::test]
#[ignore = "requires isolated Postgres via PRIVATE_HISTORY_TEST_DATABASE_URL"]
async fn historical_decode_failure_never_returns_a_short_or_partial_success() {
    let url =
        std::env::var("PRIVATE_HISTORY_TEST_DATABASE_URL").expect("explicit isolated DB required");
    // One connection keeps the TEMP table local to this test; no migrations or
    // persistent schema/data changes even if the test uses a shared test server.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("CREATE TEMP TABLE events (community_id uuid, id bytea, pubkey bytea, created_at timestamptz, kind int, tags jsonb, content text, sig bytea, received_at timestamptz, channel_id uuid, deleted_at timestamptz)")
        .execute(&pool).await.unwrap();
    let community = CommunityId::from_uuid(Uuid::new_v4());
    let keys = nostr::Keys::generate();
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(3403), "valid delivery")
        .custom_created_at(nostr::Timestamp::from(10))
        .sign_with_keys(&keys)
        .unwrap();
    let encoded = serde_json::to_value(&event).unwrap();
    sqlx::query("INSERT INTO events (community_id,id,pubkey,created_at,kind,tags,content,sig,received_at) VALUES ($1,$2,$3,to_timestamp(10),3403,$4,$5,$6,now())")
        .bind(community.as_uuid()).bind(event.id.to_bytes().to_vec()).bind(keys.public_key().to_bytes().to_vec())
        .bind(encoded["tags"].clone()).bind(&event.content).bind(hex::decode(encoded["sig"].as_str().unwrap()).unwrap())
        .execute(&pool).await.unwrap();
    let mut query = EventQuery::for_community(community);
    query.kinds = Some(vec![3403]);
    query.limit = Some(1);
    assert_eq!(
        query_events(&pool, &query).await.unwrap()[0].event.id,
        event.id
    );
    // Newer matching stored row fills LIMIT but cannot deserialize as Nostr tags.
    sqlx::query("INSERT INTO events SELECT community_id,$1,pubkey,to_timestamp(20),kind,'{}'::jsonb,content,sig,received_at,channel_id,deleted_at FROM events")
        .bind(vec![42_u8; 32]).execute(&pool).await.unwrap();
    assert!(matches!(
        query_events(&pool, &query).await,
        Err(DbError::InvalidData(_))
    ));
    // A failed row also condemns the full result rather than leaking partial success.
    query.limit = Some(2);
    assert!(matches!(
        query_events(&pool, &query).await,
        Err(DbError::InvalidData(_))
    ));
    sqlx::query("DELETE FROM events WHERE id=$1")
        .bind(vec![42_u8; 32])
        .execute(&pool)
        .await
        .unwrap();
    let recovered = query_events(&pool, &query).await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].event.id, event.id);
    pool.close().await;
}
