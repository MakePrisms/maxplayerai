//! Offline only. Public, deterministic TEST keys; never use these identities on a relay.
//! cargo run -p maxplayer-core --features wallet --example try_it_fixtures
use maxplayer_core::{gateway::*, heartbeat::SeatCapability, receipt::*};
use nostr_sdk::{prelude::*, secp256k1::Message};
fn signed(d: &EventDraft, keys: &Keys, time: u64) -> Event {
    maxplayer_core::gateway::nostr::event_builder(d)
        .unwrap()
        .custom_created_at(Timestamp::from(time))
        .sign_with_keys(keys)
        .unwrap()
}
fn main() {
    let buyer = Keys::parse(&"01".repeat(32)).unwrap();
    let seller = Keys::parse(&"02".repeat(32)).unwrap();
    let b = buyer.public_key().to_hex();
    let s = seller.public_key().to_hex();
    let time = 1_800_000_000;
    // Try it appends a text-only instruction to every question (web/app/src/try/wire.ts TEXT_ONLY).
    let task = "Explain slick tyres 🏎️\nKeep  internal spaces.\n\n(Reply in plain text, under 300 words. Don't create, edit or commit any files.)";
    let od = OfferDraft::new(task, "text/plain", 0, time + 300, &s)
        .with_payment_mode(PaymentMode::None)
        .accepting_delivery(["inline"])
        .to_event_draft();
    assert!(parse_offer(&od).is_ok());
    let offer = signed(&od, &buyer, time);
    let oid = offer.id.to_hex();
    let cd = claim_draft(
        &oid,
        &b,
        &s,
        ClaimPayment::None,
        &[],
        &SeatCapability::default(),
    );
    let claim = signed(&cd, &seller, time + 1);
    let cid = claim.id.to_hex();
    let ad = award_draft(&oid, &cid, &b, &s);
    let award = signed(&ad, &buyer, time + 2);
    let answer = "Slick tyres put more rubber on dry tarmac.\n\nExact bytes: café 🏎️  \n";
    let jh = maxplayer_core::job_lifecycle::job_hash_for_offer(&oid, task, 0);
    let pre = ReceiptPreimage {
        protocol: ReceiptProtocol::V1,
        job_hash: jh.clone(),
        offer_id: oid.clone(),
        amount: 0,
        unit: "sat".into(),
        buyer_pubkey: b,
        seller_pubkey: s.clone(),
        delivery_integrity_hash: result_content_hash_hex(answer),
        delivery_kind: "inline".into(),
        exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.into(),
        creq_hash: None,
    };
    let cosig = seller
        .sign_schnorr(&Message::from_digest(pre.digest_bytes()))
        .to_string();
    let rd = inline_result_draft(
        &oid,
        &buyer.public_key().to_hex(),
        "text/plain",
        0,
        &jh,
        &cosig,
        answer,
        &[],
    );
    assert_eq!(parse_inline_result_delivery(&rd).unwrap(), answer);
    let result = signed(&rd, &seller, time + 3);
    let accept = signed(
        &accept_draft(&oid, &cid, &buyer.public_key().to_hex(), &s),
        &buyer,
        time + 4,
    );
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({"source":"Rust gateway builders/parsers + job_lifecycle::job_hash_for_offer + receipt::ReceiptPreimage", "offer":offer,"claim":claim,"award":award,"result":result,"accept":accept,"jobHash":jh,"contentHash":pre.delivery_integrity_hash,"preimage":pre.canonical_json(),"digest":pre.digest_hex()})).unwrap());
}
