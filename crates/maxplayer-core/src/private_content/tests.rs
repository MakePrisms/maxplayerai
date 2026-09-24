use super::*;
use base64::Engine;
use nostr_sdk::prelude::{JsonUtil, Keys, Timestamp};
use sha2::Digest;
pub(super) fn keys(n: u8) -> Keys {
    Keys::parse(&hex::encode([n; 32])).unwrap()
}
pub(super) fn body() -> ContentBody {
    let mut recipients = vec![
        keys(1).public_key().to_hex(),
        keys(2).public_key().to_hex(),
        keys(3).public_key().to_hex(),
    ];
    recipients.sort();
    ContentBody {
        schema: BODY_SCHEMA.into(),
        job_id: "10".repeat(32),
        offer_id: None,
        award_id: None,
        message_id: "20".repeat(32),
        kind: ContentType::Task,
        revision: 0,
        supersedes: None,
        author: keys(1).public_key().to_hex(),
        recipients,
        text: "private-canary-750493".into(),
        requested_output: Some("text/plain".into()),
        dispatch: Some(Dispatch::default()),
        attachments: vec![],
        contribution: None,
    }
}
fn binding(p: &PreparedContent) -> Binding<'_> {
    let b = p.body();
    Binding {
        buyer: &b.author,
        seller: b
            .recipients
            .iter()
            .find(|r| **r == keys(2).public_key().to_hex())
            .unwrap(),
        service: b
            .recipients
            .iter()
            .find(|r| **r == keys(3).public_key().to_hex())
            .unwrap(),
        author: &b.author,
        job_id: &b.job_id,
        offer_id: b.offer_id.as_deref(),
        award_id: b.award_id.as_deref(),
        message_id: &b.message_id,
        commitment: p.commitment(),
        kind: b.kind,
    }
}
#[test]
fn roundtrip_preserves_exact_bytes_and_nonce_hides_guessable_text() {
    let p = PreparedContent::new(body()).unwrap();
    let decoded = PreparedContent::decode(p.envelope()).unwrap();
    assert_eq!(decoded.body_bytes(), p.body_bytes());
    assert_eq!(decoded.commitment(), p.commitment());
    let other = PreparedContent::new(body()).unwrap();
    assert_ne!(other.commitment(), p.commitment());
    p.validate_binding(&binding(&p)).unwrap();
}
#[test]
fn duplicate_nested_json_keys_and_trailing_data_are_rejected() {
    for s in [
        r#"{"a":1,"a":2}"#,
        r#"{"a":[{"b":1,"b":2}]}"#,
        r#"{"a":1}{}"#,
    ] {
        assert!(strict_json::validate(s.as_bytes()).is_err());
    }
    assert!(strict_json::validate(br#"{"a":[true,null,1,"v"]}"#).is_ok());
    let p = PreparedContent::new(body()).unwrap();
    let duplicate = p
        .envelope()
        .replacen("{", r#"{"schema":"maxplayer.content-envelope.v2","#, 1);
    assert!(PreparedContent::decode(&duplicate).is_err());
}
#[test]
fn changed_envelope_or_context_is_never_accepted() {
    let p = PreparedContent::new(body()).unwrap();
    let mut expected = binding(&p);
    expected.commitment = "00";
    assert!(p.validate_binding(&expected).is_err());
    let mut expected = binding(&p);
    expected.award_id = Some("a");
    assert!(p.validate_binding(&expected).is_err());
    let mut expected = binding(&p);
    expected.kind = ContentType::Answer;
    assert!(p.validate_binding(&expected).is_err());
    let mut expected = binding(&p);
    expected.service = &p.body().author;
    assert!(p.validate_binding(&expected).is_err());
    let mut expected = binding(&p);
    expected.job_id = "30";
    assert!(p.validate_binding(&expected).is_err());
}
#[test]
fn refuses_oversize_and_wrong_domain_and_task_revisions() {
    let mut b = body();
    b.text = "x".repeat(MAX_BODY_BYTES);
    assert!(PreparedContent::new(b).is_err());
    let mut b = body();
    b.schema = "cashu.payment".into();
    assert!(PreparedContent::new(b).is_err());
    let mut b = body();
    b.revision = 1;
    b.supersedes = Some("30".repeat(32));
    assert!(PreparedContent::new(b).is_err());
    let mut b = body();
    b.offer_id = Some("30".repeat(32));
    assert!(PreparedContent::new(b).is_err());
}
#[test]
fn attachment_paths_and_recipients_are_closed() {
    for path in [
        "/etc/passwd",
        "../x",
        "a/../b",
        "a//b",
        "a\\b",
        "C:/x",
        ".git/config",
        "a/.GIT/config",
        "a\0b",
    ] {
        assert!(validate_path(path).is_err(), "{path:?}");
    }
    assert!(validate_path("src/a.rs").is_ok());
    let mut b = body();
    b.recipients.push(b.recipients[0].clone());
    assert!(PreparedContent::new(b).is_err());
    let mut b = body();
    b.dispatch.as_mut().unwrap().harness_family = Some("secret task".into());
    assert!(PreparedContent::new(b).is_err());
}
#[tokio::test]
async fn every_recipient_gets_identical_content_outsiders_and_forged_authors_fail() {
    let p = PreparedContent::new(body()).unwrap();
    for n in 1..=3 {
        let receiver = keys(n);
        let event = transport::wrap(&keys(1), receiver.public_key(), p.envelope().into())
            .await
            .unwrap();
        assert!(!event.as_json().contains(&p.body().text));
        let decoded = transport::unwrap_content(&receiver, &event).await.unwrap();
        assert_eq!(decoded.body_bytes(), p.body_bytes());
        decoded.validate_binding(&binding(&p)).unwrap();
        assert!(transport::unwrap_content(&keys(4), &event).await.is_err());
        assert!(event.created_at.as_secs() <= Timestamp::now().as_secs());
        assert!(Timestamp::now().as_secs() - event.created_at.as_secs() < 181);
    }
    let forged = transport::wrap(&keys(4), keys(2).public_key(), p.envelope().into())
        .await
        .unwrap();
    assert!(transport::unwrap_content(&keys(2), &forged).await.is_err());
}
#[tokio::test]
async fn maximum_body_fits_both_encryption_layers_and_relay_limit() {
    let mut b = body();
    let overhead = serde_json::to_vec(&b).unwrap().len() - b.text.len();
    b.text = "x".repeat(MAX_BODY_BYTES - overhead);
    let p = PreparedContent::new(b).unwrap();
    assert_eq!(p.body_bytes().len(), MAX_BODY_BYTES);
    let event = transport::wrap(&keys(1), keys(2).public_key(), p.envelope().into())
        .await
        .unwrap();
    assert!(event.content.len() < transport::MAX_WRAPPER_CONTENT);
    assert_eq!(
        transport::unwrap_content(&keys(2), &event)
            .await
            .unwrap()
            .commitment(),
        p.commitment()
    );
}
#[cfg(feature = "wallet")]
#[test]
fn outbox_atomic_recipient_set_retry_and_durable_conflict_detection() {
    let mut db = store::ContentStore::in_memory().unwrap();
    let p = PreparedContent::new(body()).unwrap();
    db.enqueue(&p, &binding(&p)).unwrap();
    assert_eq!(db.pending(&p.body().author, 100).unwrap().len(), 3);
    db.enqueue(&p, &binding(&p)).unwrap();
    assert_eq!(db.pending(&p.body().author, 100).unwrap().len(), 3);
    db.relay_accepted(&p.body().job_id, &p.body().message_id, &p.body().author)
        .unwrap();
    assert_eq!(db.pending(&p.body().author, 100).unwrap().len(), 2);
    let conflicting = PreparedContent::new(body()).unwrap();
    assert!(db.enqueue(&conflicting, &binding(&conflicting)).is_err());
    assert!(
        db.receive(
            &conflicting,
            &binding(&conflicting),
            &conflicting.body().author,
            1000
        )
        .is_err()
    );
    assert!(
        db.receive(&p, &binding(&p), &p.body().author, 1000)
            .unwrap()
    );
    assert!(
        !db.receive(&p, &binding(&p), &p.body().author, 1001)
            .unwrap()
    );
    assert_eq!(db.receive_since(&p.body().author).unwrap(), 0);
    db.complete_backfill(&p.body().author, 1001).unwrap();
    assert_eq!(db.receive_since(&p.body().author).unwrap(), 761);
    assert_eq!(
        db.get(&p.body().job_id, &p.body().message_id)
            .unwrap()
            .unwrap()
            .commitment(),
        p.commitment()
    );
}
#[cfg(feature = "wallet")]
#[test]
fn restart_preserves_outbox_and_job_identity_reservation() {
    let dir = std::env::temp_dir().join(format!("maxplayer-private-{}", random_id().unwrap()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("content.sqlite");
    let p = PreparedContent::new(body()).unwrap();
    {
        let mut db = store::ContentStore::open(&path).unwrap();
        db.enqueue(&p, &binding(&p)).unwrap();
        db.reserve_offer(&p.body().author, &p.body().job_id, &"aa".repeat(32))
            .unwrap();
    }
    {
        let mut db = store::ContentStore::open(&path).unwrap();
        assert_eq!(db.pending(&p.body().author, 10).unwrap().len(), 3);
        db.reserve_offer(&p.body().author, &p.body().job_id, &"aa".repeat(32))
            .unwrap();
        assert!(
            db.reserve_offer(&p.body().author, &p.body().job_id, &"bb".repeat(32))
                .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}
fn host() -> wire::HostPolicy {
    wire::HostPolicy {
        git_prefix: "https://git.example/jobs/".into(),
        accepted_mints: vec!["https://mint.example".into()],
    }
}
fn sign(kind: u16, tags: Vec<Vec<String>>, author: u8) -> nostr_sdk::prelude::Event {
    use nostr_sdk::prelude::{EventBuilder, Kind, Tag};
    EventBuilder::new(Kind::from(kind), "")
        .allow_self_tagging()
        .tags(tags.into_iter().map(|t| Tag::parse(t).unwrap()))
        .sign_with_keys(&keys(author))
        .unwrap()
}
fn offer_tags(p: &PreparedContent) -> Vec<Vec<String>> {
    vec![
        vec!["t".into(), "maxplayer".into()],
        vec!["v".into(), "2".into()],
        vec!["job".into(), p.body().job_id.clone()],
        vec!["visibility".into(), "private".into()],
        vec!["discovery".into(), "targeted".into()],
        vec!["output".into(), "text".into()],
        vec!["amount".into(), "100".into(), "sat".into()],
        vec!["param".into(), "deadline".into(), "2000000000".into()],
        vec!["p".into(), keys(2).public_key().to_hex()],
        vec!["content-id".into(), p.body().message_id.clone()],
        vec!["content-commitment".into(), p.commitment().into()],
    ]
}
#[test]
fn private_offer_schema_and_every_singleton_reject_mutations() {
    let p = PreparedContent::new(body()).unwrap();
    let tags = offer_tags(&p);
    let event = sign(3401, tags.clone(), 1);
    wire::validate_private(&event, &host()).unwrap();
    wire::bind_content(
        &p,
        &event,
        &event,
        None,
        None,
        None,
        &keys(3).public_key().to_hex(),
        &host(),
    )
    .unwrap();
    for index in 0..tags.len() {
        let mut duplicate = tags.clone();
        duplicate.push(tags[index].clone());
        assert!(
            wire::validate_private(&sign(3401, duplicate, 1), &host()).is_err(),
            "duplicate row {index}"
        );
        let mut extra = tags.clone();
        extra[index].push("leaked-secret".into());
        assert!(
            wire::validate_private(&sign(3401, extra, 1), &host()).is_err(),
            "extra column {index}"
        );
        let mut missing = tags.clone();
        missing.remove(index);
        assert!(
            wire::validate_private(&sign(3401, missing, 1), &host()).is_err(),
            "missing row {index}"
        );
    }
    for tag in [
        vec!["i", "secret"],
        vec!["title", "secret"],
        vec!["param", "harness_model", "secret"],
        vec!["unknown", "secret"],
    ] {
        let mut injected = tags.clone();
        injected.push(tag.into_iter().map(str::to_owned).collect());
        assert!(wire::validate_private(&sign(3401, injected, 1), &host()).is_err());
    }
    let mut bad = tags.clone();
    bad.iter_mut().find(|t| t[0] == "amount").unwrap()[1] = "0100".into();
    assert!(wire::validate_private(&sign(3401, bad, 1), &host()).is_err());
}
#[test]
fn signed_parser_refuses_uncommitted_extra_event_fields() {
    let p = PreparedContent::new(body()).unwrap();
    let e = sign(3401, offer_tags(&p), 1);
    wire::parse_signed(&e.as_json()).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&e.as_json()).unwrap();
    json["extra"] = serde_json::json!("canary");
    assert!(wire::parse_signed(&json.to_string()).is_err());
}
#[test]
fn open_pool_is_explicit_public_task_not_post_award_inputs() {
    let p = PreparedContent::new(body()).unwrap();
    let mut tags = offer_tags(&p);
    tags.retain(|t| !matches!(t[0].as_str(), "p" | "content-id" | "content-commitment"));
    tags.iter_mut().find(|t| t[0] == "discovery").unwrap()[1] = "open".into();
    let task = wire::PublicTask {
        schema: "maxplayer.public-task.v2".into(),
        text: "public task".into(),
        requested_output: "text/plain".into(),
        dispatch: Dispatch::default(),
        contribution: None,
    };
    tags.push(vec!["i".into(), serde_json::to_string(&task).unwrap()]);
    wire::validate_private(&sign(3401, tags.clone(), 1), &host()).unwrap();
    tags.last_mut().unwrap()[1]=r#"{"schema":"maxplayer.public-task.v2","text":"task","requested_output":"text/plain","dispatch":{},"attachments":[]}"#.into();
    assert!(wire::validate_private(&sign(3401, tags, 1), &host()).is_err());
}
#[cfg(feature = "wallet")]
#[test]
fn invoice_rejects_unknown_duplicate_nested_fields_and_descriptive_payloads() {
    use cashu::nuts::nut18::PaymentRequest;
    use nostr_sdk::prelude::{Nip19Profile, ToBech32};
    let offer = "ab".repeat(32);
    let seller = keys(2).public_key();
    let profile = Nip19Profile::new(seller, []).to_bech32().unwrap();
    let json = serde_json::json!({"i":offer,"a":100,"u":"sat","s":true,"m":["https://mint.example"],"d":null,"t":[{"t":"nostr","a":profile,"g":[["n","17"]]}]});
    let request: PaymentRequest = serde_json::from_value(json.clone()).unwrap();
    let raw = request.to_string();
    invoice::validate(&raw, &offer, 100, &seller.to_hex(), &host()).unwrap();
    let b = request.to_bech32_string().unwrap();
    invoice::validate(&b, &offer, 100, &seller.to_hex(), &host()).unwrap();
    for pointer in ["/d", "/extra", "/t/0/extra"] {
        let mut v = json.clone();
        match pointer {
            "/d" => v["d"] = serde_json::json!("secret"),
            "/extra" => v["extra"] = serde_json::json!("secret"),
            _ => v["t"][0]["extra"] = serde_json::json!("secret"),
        };
        let mut bytes = vec![];
        ciborium::into_writer(&v, &mut bytes).unwrap();
        let raw = format!(
            "creqA{}",
            base64::engine::general_purpose::URL_SAFE.encode(bytes)
        );
        assert!(invoice::validate(&raw, &offer, 100, &seller.to_hex(), &host()).is_err());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(raw.strip_prefix("creqA").unwrap())
        .unwrap();
    let mut value: ciborium::Value = ciborium::from_reader(bytes.as_slice()).unwrap();
    if let ciborium::Value::Map(ref mut entries) = value {
        entries.push(entries[0].clone());
    }
    let mut bytes = vec![];
    ciborium::into_writer(&value, &mut bytes).unwrap();
    let raw = format!(
        "creqA{}",
        base64::engine::general_purpose::URL_SAFE.encode(bytes)
    );
    assert!(invoice::validate(&raw, &offer, 100, &seller.to_hex(), &host()).is_err());
}
#[test]
fn independent_python_vectors_pin_body_commitment_job_and_paid_free_receipt_bytes() {
    let v: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/private-content-v2/vectors.json"
    ))
    .unwrap();
    let p = PreparedContent::decode(v["envelope"].as_str().unwrap()).unwrap();
    assert_eq!(p.body_bytes(), v["body"].as_str().unwrap().as_bytes());
    assert_eq!(p.commitment(), v["commitment"].as_str().unwrap());
    assert_eq!(
        job_hash(v["offer_id"].as_str().unwrap()).unwrap(),
        v["job_hash"]
    );
    for vector in v["receipts"].as_array().unwrap() {
        let receipt = crate::receipt::ReceiptPreimage {
            job_hash: v["job_hash"].as_str().unwrap().into(),
            offer_id: v["offer_id"].as_str().unwrap().into(),
            amount: 100,
            unit: "sat".into(),
            buyer_pubkey: "11".repeat(32),
            seller_pubkey: "22".repeat(32),
            delivery_integrity_hash: vector["integrity"].as_str().unwrap().into(),
            delivery_kind: vector["kind"].as_str().unwrap().into(),
            exec_metadata_commitment: "none".into(),
            creq_hash: vector["paid"].as_bool().unwrap().then(|| "77".repeat(32)),
        };
        assert_eq!(
            settlement::canonical_json(&receipt).unwrap(),
            vector["preimage"]
        );
        assert_eq!(
            hex::encode(settlement::digest(&receipt).unwrap()),
            vector["digest"]
        );
        use nostr_sdk::secp256k1::{Message, Secp256k1, XOnlyPublicKey, schnorr::Signature};
        let signature = vector["signature"]
            .as_str()
            .unwrap()
            .parse::<Signature>()
            .unwrap();
        let signer = vector["signer"]
            .as_str()
            .unwrap()
            .parse::<XOnlyPublicKey>()
            .unwrap();
        let digest = settlement::digest(&receipt).unwrap();
        let secp = Secp256k1::verification_only();
        secp.verify_schnorr(&signature, &Message::from_digest(digest), &signer)
            .unwrap();
        let mut altered = digest;
        altered[0] ^= 1;
        assert!(
            secp.verify_schnorr(&signature, &Message::from_digest(altered), &signer)
                .is_err()
        );
    }
}
#[test]
fn offer_builder_keeps_custom_dispatch_private_and_preserves_public_discovery() {
    use builders::{OfferOptions, prepare_offer};
    use wire::{Output, Visibility};
    let mut request = crate::gateway::OfferDraft::new(
        "TASK CANARY",
        "custom/output",
        100,
        2000000000,
        keys(2).public_key().to_hex(),
    );
    request.requested_agent = Some("custom-preset-canary".into());
    request.requested_model = Some("model-canary".into());
    let service = keys(3).public_key().to_hex();
    let job = "99".repeat(32);
    let options = || OfferOptions {
        visibility: Visibility::Private,
        category: Output::Other,
        service: &service,
        job_id: &job,
        attachments: vec![],
        contribution: None,
    };
    let prepared = prepare_offer(&keys(1), &request, options(), &host()).unwrap();
    let wire = prepared.event.as_json();
    for secret in [
        "TASK CANARY",
        "custom/output",
        "custom-preset-canary",
        "model-canary",
    ] {
        assert!(!wire.contains(secret));
    }
    let task = prepared.task.unwrap();
    assert_eq!(
        task.body().dispatch.as_ref().unwrap().agent,
        request.requested_agent
    );
    request.seller_pubkey = None;
    let open = prepare_offer(&keys(1), &request, options(), &host()).unwrap();
    assert!(open.task.is_none());
    assert!(open.event.as_json().contains("TASK CANARY"));
}

#[cfg(feature = "wallet")]
#[test]
fn unknown_job_inbox_is_bounded_by_inner_author_and_never_exposed_as_verified() {
    let mut db = store::ContentStore::in_memory().unwrap();
    let p = PreparedContent::new(body()).unwrap();
    let recipient = keys(2).public_key().to_hex();
    assert!(db.stage(&p, &recipient, 100).unwrap());
    assert!(!db.stage(&p, &recipient, 101).unwrap());
    assert!(
        db.get(&p.body().job_id, &p.body().message_id)
            .unwrap()
            .is_none()
    );
    assert!(
        db.stage(&PreparedContent::new(body()).unwrap(), &recipient, 102)
            .is_err()
    );
    for _ in 1..128 {
        let mut b = body();
        b.message_id = random_id().unwrap();
        db.stage(&PreparedContent::new(b).unwrap(), &recipient, 102)
            .unwrap();
    }
    let mut b = body();
    b.message_id = random_id().unwrap();
    let next = PreparedContent::new(b).unwrap();
    assert!(db.stage(&next, &recipient, 102).is_err());
    assert_eq!(db.receive_since(&recipient).unwrap(), 0);
    assert!(
        db.staged(
            &p.body().author,
            &p.body().job_id,
            &p.body().message_id,
            103
        )
        .unwrap()
        .is_some()
    );
    db.receive(&p, &binding(&p), &recipient, 103).unwrap();
    assert!(
        db.staged(
            &p.body().author,
            &p.body().job_id,
            &p.body().message_id,
            103
        )
        .unwrap()
        .is_none()
    );
    assert!(db.stage(&next, &recipient, 104).unwrap());
    // Expiration frees bounded staging only; previously verified records are retained.
    let mut b = body();
    b.message_id = random_id().unwrap();
    db.stage(&PreparedContent::new(b).unwrap(), &recipient, 100_000)
        .unwrap();
    assert!(
        db.get(&p.body().job_id, &p.body().message_id)
            .unwrap()
            .is_some()
    );
}

#[cfg(feature = "wallet")]
#[tokio::test]
async fn service_publication_failure_retries_same_content_without_blocking_participants() {
    use session::ContentSender;
    struct Sender {
        refuse: Option<String>,
        events: Vec<nostr_sdk::Event>,
    }
    impl ContentSender for Sender {
        async fn send(&mut self, event: nostr_sdk::Event) -> Result<()> {
            let refused = self
                .refuse
                .as_ref()
                .is_some_and(|r| event.tags.iter().any(|t| t.as_slice() == ["p", r.as_str()]));
            self.events.push(event);
            if refused {
                Err(Error("offline"))
            } else {
                Ok(())
            }
        }
    }
    let p = PreparedContent::new(body()).unwrap();
    let mut db = store::ContentStore::in_memory().unwrap();
    db.enqueue(&p, &binding(&p)).unwrap();
    let mut sender = Sender {
        refuse: Some(keys(3).public_key().to_hex()),
        events: vec![],
    };
    let report = session::flush(&mut db, &keys(1), &mut sender, 10)
        .await
        .unwrap();
    assert_eq!(
        report,
        session::FlushReport {
            accepted: 2,
            pending: 1
        }
    );
    let old_service = sender
        .events
        .iter()
        .find(|e| {
            e.tags
                .iter()
                .any(|t| t.as_slice() == ["p", keys(3).public_key().to_hex().as_str()])
        })
        .unwrap()
        .id;
    sender.refuse = None;
    let report = session::flush(&mut db, &keys(1), &mut sender, 10)
        .await
        .unwrap();
    assert_eq!(
        report,
        session::FlushReport {
            accepted: 1,
            pending: 0
        }
    );
    let retry = sender.events.last().unwrap();
    assert_ne!(retry.id, old_service);
    assert_eq!(
        transport::unwrap_content(&keys(3), retry)
            .await
            .unwrap()
            .envelope(),
        p.envelope()
    );
    assert!(db.pending(&p.body().author, 10).unwrap().is_empty());
}

#[cfg(feature = "wallet")]
#[test]
fn input_snapshot_pins_bytes_and_materializes_only_manifest_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init_bare(dir.path().join("repo")).unwrap();
    let source = dir.path().join("source");
    std::fs::write(&source, b"original").unwrap();
    let job = "ab".repeat(32);
    let snapshot = inputs::prepare(
        &repo,
        &job,
        &[inputs::InputFile {
            source: source.clone(),
            path: "context/task.txt".into(),
        }],
    )
    .unwrap();
    std::fs::write(source, b"changed after preparing").unwrap();
    let destination = dir.path().join("snapshot");
    inputs::materialize(&repo, &job, &snapshot.manifest, &destination).unwrap();
    assert_eq!(
        std::fs::read(destination.join("context/task.txt")).unwrap(),
        b"original"
    );
    assert!(inputs::materialize(&repo, &job, &snapshot.manifest, &destination).is_err());
    let mut corrupt = snapshot.manifest.clone();
    corrupt[0].sha256 = "00".repeat(32);
    let absent = dir.path().join("absent");
    assert!(inputs::materialize(&repo, &job, &corrupt, &absent).is_err());
    assert!(!absent.exists());
    let mut corrupt = snapshot.manifest.clone();
    corrupt[0].path = "../escape".into();
    assert!(inputs::materialize(&repo, &job, &corrupt, &absent).is_err());
    assert!(inputs::materialize(&repo, &"bb".repeat(32), &snapshot.manifest, &absent).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        assert_eq!(
            std::fs::metadata(destination.join("context/task.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let link = dir.path().join("link");
        symlink("source", &link).unwrap();
        assert!(
            inputs::prepare(
                &repo,
                &job,
                &[inputs::InputFile {
                    source: link,
                    path: "link".into()
                }]
            )
            .is_err()
        );
    }
}

#[cfg(feature = "wallet")]
#[test]
fn carrier_and_all_recipient_copies_are_atomic_and_reject_job_id_reuse() {
    let p = PreparedContent::new(body()).unwrap();
    let offer = sign(3401, offer_tags(&p), 1);
    let service = keys(3).public_key().to_hex();
    let host = host();
    let context = store::SignedContext {
        carrier: &offer,
        offer: &offer,
        award: None,
        claim: None,
        result: None,
        service: &service,
        host: &host,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("content.sqlite");
    {
        let mut db = store::ContentStore::open(&path).unwrap();
        db.enqueue_signed(&p, &context).unwrap();
    }
    let mut db = store::ContentStore::open(&path).unwrap();
    assert_eq!(db.pending_carriers(&p.body().author, 10).unwrap()[0], offer);
    assert_eq!(db.pending(&p.body().author, 10).unwrap().len(), 3);
    let mut changed = body();
    changed.message_id = random_id().unwrap();
    changed.text = "different task".into();
    let changed = PreparedContent::new(changed).unwrap();
    let other_offer = sign(3401, offer_tags(&changed), 1);
    let other_context = store::SignedContext {
        carrier: &other_offer,
        offer: &other_offer,
        award: None,
        claim: None,
        result: None,
        service: &service,
        host: &host,
    };
    assert!(db.enqueue_signed(&changed, &other_context).is_err());
    assert_eq!(db.pending(&p.body().author, 10).unwrap().len(), 3);
    assert_eq!(db.pending_carriers(&p.body().author, 10).unwrap().len(), 1);
    db.carrier_accepted(&offer).unwrap();
    assert!(
        db.pending_carriers(&p.body().author, 10)
            .unwrap()
            .is_empty()
    );
}

#[cfg(feature = "wallet")]
#[tokio::test]
async fn selected_seller_inline_answer_binds_exact_award_and_independent_service_copy() {
    let p = PreparedContent::new(body()).unwrap();
    let mut tags = offer_tags(&p);
    tags.iter_mut().find(|t| t[0] == "amount").unwrap()[1] = "0".into();
    tags.push(vec!["param".into(), "payment".into(), "none".into()]);
    tags.push(vec![
        "param".into(),
        "accepts-delivery".into(),
        "inline".into(),
    ]);
    let offer = sign(3401, tags, 1);
    let common = || {
        vec![
            vec!["t".into(), "maxplayer".into()],
            vec!["v".into(), "2".into()],
            vec!["job".into(), p.body().job_id.clone()],
            vec!["e".into(), offer.id.to_hex(), "".into(), "root".into()],
            vec!["p".into(), keys(1).public_key().to_hex()],
        ]
    };
    let mut tags = common();
    tags.extend([
        vec!["status".into(), "processing".into()],
        vec!["payment".into(), "none".into()],
    ]);
    let claim = sign(3402, tags, 2);
    let mut tags = common();
    tags.extend([
        vec!["e".into(), claim.id.to_hex()],
        vec!["p".into(), keys(2).public_key().to_hex()],
        vec!["status".into(), "accepted".into()],
    ]);
    let award = sign(3405, tags, 1);
    let mut answer = body();
    answer.message_id = random_id().unwrap();
    answer.author = keys(2).public_key().to_hex();
    answer.offer_id = Some(offer.id.to_hex());
    answer.award_id = Some(award.id.to_hex());
    answer.kind = ContentType::Answer;
    answer.dispatch = None;
    answer.requested_output = None;
    answer.text = "Private final answer".into();
    let answer = PreparedContent::new(answer).unwrap();
    let mut tags = common();
    tags.extend([
        vec!["award".into(), award.id.to_hex()],
        vec!["amount".into(), "0".into(), "sat".into()],
        vec!["output".into(), "text".into()],
        vec!["job-hash".into(), job_hash(&offer.id.to_hex()).unwrap()],
        vec!["sig".into(), "seller".into(), "11".repeat(64)],
        vec!["delivery".into(), "inline".into()],
        vec!["content-id".into(), answer.body().message_id.clone()],
        vec!["content-commitment".into(), answer.commitment().into()],
    ]);
    let result = sign(3403, tags.clone(), 2);
    let service = keys(3).public_key().to_hex();
    let host = host();
    let context = store::SignedContext {
        carrier: &result,
        offer: &offer,
        claim: Some(&claim),
        award: Some(&award),
        result: None,
        service: &service,
        host: &host,
    };
    context.validate(&answer).unwrap();
    let wrap = transport::wrap(&keys(2), keys(3).public_key(), answer.envelope().into())
        .await
        .unwrap();
    let copy = transport::unwrap_content(&keys(3), &wrap).await.unwrap();
    let mut service_db = store::ContentStore::in_memory().unwrap();
    assert!(
        service_db
            .receive_signed(&copy, &context, &service, 100)
            .unwrap()
    );
    assert!(
        !service_db
            .receive_signed(&copy, &context, &service, 101)
            .unwrap()
    );
    for field in [
        "award",
        "amount",
        "output",
        "job-hash",
        "content-id",
        "content-commitment",
        "job",
    ] {
        let mut mutated = tags.clone();
        let row = mutated.iter_mut().find(|t| t[0] == field).unwrap();
        row[1] = match field {
            "amount" => "1".into(),
            "output" => "code".into(),
            _ => "dd".repeat(32),
        };
        let carrier = sign(3403, mutated, 2);
        let wrong = store::SignedContext {
            carrier: &carrier,
            ..context
        };
        assert!(wrong.validate(&answer).is_err(), "changed {field}");
    }
    let loser = sign(3403, tags, 4);
    let wrong = store::SignedContext {
        carrier: &loser,
        ..context
    };
    assert!(wrong.validate(&answer).is_err());
    let preaward = store::SignedContext {
        award: None,
        ..context
    };
    assert!(preaward.validate(&answer).is_err());
}

#[cfg(feature = "wallet")]
#[test]
fn provisioning_auth_binds_method_route_payload_and_has_fresh_replay_identity() {
    use base64::engine::general_purpose::STANDARD;
    use nostr_sdk::prelude::Event;
    let url = format!("https://relay.example/api/jobs/private/{}", "ab".repeat(32));
    let body = br#"{"signed_offer":"fixture"}"#;
    let a = hosting::auth_header(&keys(1), &url, body).unwrap();
    let b = hosting::auth_header(&keys(1), &url, body).unwrap();
    let decode = |h: &str| {
        Event::from_json(STANDARD.decode(h.strip_prefix("Nostr ").unwrap()).unwrap()).unwrap()
    };
    let a = decode(&a);
    let b = decode(&b);
    a.verify().unwrap();
    b.verify().unwrap();
    assert_ne!(a.id, b.id);
    assert!(a.tags.iter().any(|t| t.as_slice() == ["method", "PUT"]));
    assert!(a.tags.iter().any(|t| t.as_slice() == ["u", url.as_str()]));
    assert!(
        a.tags
            .iter()
            .any(|t| t.as_slice() == ["payload", hex::encode(sha2::Sha256::digest(body)).as_str()])
    );
    assert!(hosting::auth_header(&keys(1), &url.replace("https:", "http:"), body).is_err());
    assert!(hosting::auth_header(&keys(1), &format!("{url}?redirect=elsewhere"), body).is_err());
}

#[test]
fn free_offer_cannot_carry_a_price_and_claim_details_are_not_a_candidate_pitch() {
    let p = PreparedContent::new(body()).unwrap();
    let mut tags = offer_tags(&p);
    tags.push(vec!["param".into(), "payment".into(), "none".into()]);
    assert!(wire::validate_private(&sign(3401, tags.clone(), 1), &host()).is_err());
    tags.iter_mut().find(|t| t[0] == "amount").unwrap()[1] = "0".into();
    wire::validate_private(&sign(3401, tags, 1), &host()).unwrap();
    let mut claim = body();
    claim.kind = ContentType::ClaimDetails;
    claim.offer_id = Some("ab".repeat(32));
    claim.requested_output = None;
    assert!(PreparedContent::new(claim.clone()).is_err());
    claim.text.clear();
    PreparedContent::new(claim).unwrap();
}

#[cfg(feature = "wallet")]
#[tokio::test]
async fn retry_backlog_rotates_across_restart_and_does_not_starve_participants() {
    struct Sender { service: String, delivered: Vec<nostr_sdk::Event> }
    impl session::ContentSender for Sender {
        async fn send(&mut self, event: nostr_sdk::Event) -> Result<()> {
            if event.tags.iter().any(|t| t.as_slice() == ["p", self.service.as_str()]) {
                return Err(Error("service refused"));
            }
            self.delivered.push(event);
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("content.sqlite");
    let mut db = store::ContentStore::open(&path).unwrap();
    for n in 0..65 {
        let mut b = body();
        b.message_id = format!("{n:064x}");
        let p = PreparedContent::new(b).unwrap();
        db.enqueue(&p, &binding(&p)).unwrap();
        for recipient in [keys(1), keys(2)] {
            db.relay_accepted(&p.body().job_id, &p.body().message_id, &recipient.public_key().to_hex()).unwrap();
        }
    }
    let fresh = PreparedContent::new(body()).unwrap();
    db.enqueue(&fresh, &binding(&fresh)).unwrap();
    let author = keys(1).public_key().to_hex();
    let first = db.pending(&author, 1).unwrap().remove(0);
    db.copy_attempted(&first).unwrap();
    drop(db);
    let mut db = store::ContentStore::open(&path).unwrap();
    assert_ne!(db.pending(&author, 1).unwrap()[0].content.body().message_id, first.content.body().message_id);
    let mut sender = Sender { service: keys(3).public_key().to_hex(), delivered: vec![] };
    let report = session::flush(&mut db, &keys(1), &mut sender, 64).await.unwrap();
    assert_eq!(report.accepted, 2);
    assert_eq!(report.pending, 62);
    for recipient in [keys(1), keys(2)] {
        let wrapper = sender.delivered.iter().find(|e| e.tags.iter().any(|t|
            t.as_slice() == ["p", recipient.public_key().to_hex().as_str()])).unwrap();
        assert_eq!(transport::unwrap_content(&recipient, wrapper).await.unwrap().envelope(), fresh.envelope());
    }
    assert_eq!(db.pending(&author, 256).unwrap().len(), 66);
}

#[cfg(unix)]
#[test]
fn content_database_rejects_symlink_without_touching_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, b"untouched").unwrap();
    let link = dir.path().join("content.sqlite");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(store::ContentStore::open(&link).is_err());
    assert_eq!(std::fs::read(target).unwrap(), b"untouched");
}


#[cfg(feature = "wallet")]
#[test]
fn unbound_inbox_conflicts_and_sender_quota_do_not_abort_other_authors() {
    let mut db = store::ContentStore::in_memory().unwrap();
    let recipient = keys(2).public_key().to_hex();
    let p = PreparedContent::new(body()).unwrap();
    assert!(db.stage_untrusted(&p, &recipient, 100).unwrap());
    assert!(!db.stage_untrusted(&PreparedContent::new(body()).unwrap(), &recipient, 100).unwrap());
    for n in 0..128 {
        let mut b = body();
        b.message_id = format!("{n:064x}");
        let _ = db.stage_untrusted(&PreparedContent::new(b).unwrap(), &recipient, 100).unwrap();
    }
    let mut b = body();
    b.author = keys(3).public_key().to_hex();
    let other = PreparedContent::new(b).unwrap();
    assert!(db.stage_untrusted(&other, &recipient, 100).unwrap());
    assert_eq!(db.staged(&p.body().author, &p.body().job_id, &p.body().message_id, 100).unwrap().unwrap().envelope(), p.envelope());
}
