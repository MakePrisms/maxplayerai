//! Golden fixtures for the read-only web trade market (web/app/src/trade).
//!
//! Every case is a real signed Nostr event plus THIS crate's verdict on it:
//! `parse_lot` for kind-3410 listings, `lifecycle` for kind-3411 chains. The
//! web validator must agree case by case. Test keys only; nothing is published.
//!
//! Copy this file to crates/maxplayer-trade/examples/web_market_fixtures.rs on
//! the trade branch (PR #1107), then from that crate:
//!   cargo run --example web_market_fixtures > <repo>/web/app/test/fixtures/trade-rust.json
use anyhow::Result;
use maxplayer_trade::{
    Asset, DeadlinePolicy, LOT, Leg, Lot, STATUS, Status, lifecycle, lot_event, parse_lot,
    status_event,
};
use nostr_sdk::prelude::*;
use serde_json::{Value, json};

const T0: u64 = 1_791_500_000;
const MINT_A: &str = "https://mint.minibits.cash/Bitcoin";
const MINT_B: &str = "https://testnut.cashu.space";
const NPUB_MINT: &str = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";

fn keys(byte: u8) -> Keys {
    Keys::parse(&hex::encode([byte; 32])).unwrap()
}

fn base_lot(k: &Keys, created: u64) -> Lot {
    Lot {
        trade_v: 1,
        give: Leg { asset: Asset { mint_url: MINT_A.into(), unit: "sat".into() }, net: 64 },
        want: Leg { asset: Asset { mint_url: MINT_B.into(), unit: "sat".into() }, net: 48 },
        maker_trade_pubkey: k.public_key().to_hex(),
        expires_at: created + 86400,
        deadline_policy: DeadlinePolicy::default(),
        fee_policy: "sender-funds-net-v1".into(),
    }
}

fn tags_for(l: &Value) -> Vec<Vec<String>> {
    let s = |p: &str| l.pointer(p).map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }).unwrap_or_default();
    vec![
        vec!["t".into(), "maxplayer".into()],
        vec!["v".into(), "1".into()],
        vec!["g".into(), s("/give/mint_url")],
        vec!["w".into(), s("/want/mint_url")],
        vec!["u".into(), s("/give/unit")],
        vec!["x".into(), s("/want/unit")],
        vec!["expiration".into(), s("/expires_at")],
    ]
}

fn sign(k: &Keys, kind: u16, created: u64, tags: Vec<Vec<String>>, content: String) -> Event {
    let tags = tags.into_iter().map(|t| Tag::parse(t).unwrap()).collect::<Vec<_>>();
    EventBuilder::new(Kind::Custom(kind), content)
        .custom_created_at(Timestamp::from(created))
        .tags(tags)
        .sign_with_keys(k)
        .unwrap()
}

/// A listing built field by field like `lot_event`, then edited by `f`.
fn lot_case(
    k: &Keys,
    created: u64,
    f: impl FnOnce(&mut Value, &mut Vec<Vec<String>>) -> Option<String>,
) -> Event {
    let mut v = serde_json::to_value(base_lot(k, created)).unwrap();
    let mut tags = tags_for(&v);
    let raw = f(&mut v, &mut tags);
    let content = raw.unwrap_or_else(|| serde_json::to_string(&v).unwrap());
    sign(k, LOT, created, tags, content)
}

fn status_raw(k: &Keys, created: u64, tags: Vec<Vec<String>>, content: Value) -> Event {
    sign(k, STATUS, created, tags, content.to_string())
}

fn status_tags(lot: &Event) -> Vec<Vec<String>> {
    vec![
        vec!["t".into(), "maxplayer".into()],
        vec!["v".into(), "1".into()],
        vec!["e".into(), lot.id.to_hex()],
    ]
}

fn rev(lot: &Event, seq: u64, prev: &str, status: &str) -> Value {
    json!({"trade_v": 1, "lot_id": lot.id.to_hex(), "seq": seq, "prev": prev, "status": status})
}

fn lot_verdict(name: &str, e: &Event, now: u64) -> Value {
    let r = parse_lot(e, now);
    json!({"name": name, "now": now, "event": e, "rust": match r {
        Ok(_) => json!({"ok": true}),
        Err(err) => json!({"ok": false, "error": err.to_string()}),
    }})
}

fn chain_verdict(name: &str, lot: &Event, statuses: &[Event]) -> Value {
    let r = lifecycle(lot, statuses);
    json!({"name": name, "lot": lot, "statuses": statuses, "rust": match r {
        Ok(s) => json!({"ok": true, "status": s}),
        Err(err) => json!({"ok": false, "error": err.to_string()}),
    }})
}

fn main() -> Result<()> {
    let maker = keys(0x11);
    let other = keys(0x22);
    let now = T0 + 600;
    let mut lots = vec![];

    // The crate's own builder, at real wall-clock time.
    let real = lot_event(
        &maker,
        Leg { asset: Asset::new(MINT_A)?, net: 32 },
        Leg { asset: Asset::new(MINT_B)?, net: 24 },
    )?;
    let real_now = real.created_at.as_secs() + 10;
    lots.push(lot_verdict("lot_event builder", &real, real_now));
    lots.push(lot_verdict("lot_event builder, at expiry", &real, real.created_at.as_secs() + 86400));
    lots.push(lot_verdict("lot_event builder, one second before expiry", &real, real.created_at.as_secs() + 86399));

    let ok = lot_case(&maker, T0, |_, _| None);
    lots.push(lot_verdict("valid", &ok, now));
    let mut tampered = ok.clone();
    tampered.content.push(' ');
    lots.push(lot_verdict("tampered content", &tampered, now));
    let mut bad_sig = ok.clone();
    bad_sig.sig = other.sign_schnorr(&nostr_sdk::prelude::secp256k1::Message::from_digest(*ok.id.as_bytes()));
    lots.push(lot_verdict("signature by another key", &bad_sig, now));
    lots.push(lot_verdict("future created_at beyond 60s", &lot_case(&maker, now + 61, |_, _| None), now));
    lots.push(lot_verdict("future created_at within 60s", &lot_case(&maker, now + 60, |_, _| None), now));

    type Edit = Box<dyn FnOnce(&mut Value, &mut Vec<Vec<String>>) -> Option<String>>;
    let edits: Vec<(&str, Edit)> = vec![
        ("same asset both legs", Box::new(|v, t| { v["want"]["mint_url"] = json!(MINT_A); *t = tags_for(v); None })),
        ("zero give", Box::new(|v, _| { v["give"]["net"] = json!(0); None })),
        ("want 1000000", Box::new(|v, _| { v["want"]["net"] = json!(1_000_000); None })),
        ("want 1000001", Box::new(|v, _| { v["want"]["net"] = json!(1_000_001); None })),
        ("float amount", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replace("\"net\":64", "\"net\":64.0")))),
        ("exponent amount", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replace("\"net\":64", "\"net\":6.4e1")))),
        ("negative amount", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replace("\"net\":64", "\"net\":-64")))),
        ("string amount", Box::new(|v, _| { v["give"]["net"] = json!("64"); None })),
        ("trade_v 2", Box::new(|v, _| { v["trade_v"] = json!(2); None })),
        ("trade_v 257", Box::new(|v, _| { v["trade_v"] = json!(257); None })),
        ("unit usd", Box::new(|v, t| { v["give"]["unit"] = json!("usd"); *t = tags_for(v); None })),
        ("trailing slash mint", Box::new(|v, t| { v["give"]["mint_url"] = json!(format!("{MINT_B}/")); *t = tags_for(v); None })),
        ("uppercase host mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://TESTNUT.cashu.space"); *t = tags_for(v); None })),
        ("default port mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://mint.example:443"); *t = tags_for(v); None })),
        ("custom port mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://mint.example:3338"); *t = tags_for(v); None })),
        ("query mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://mint.example?x=1"); *t = tags_for(v); None })),
        ("empty query mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://mint.example/?"); *t = tags_for(v); None })),
        ("fragment mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://mint.example#f"); *t = tags_for(v); None })),
        ("userinfo mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://u@mint.example"); *t = tags_for(v); None })),
        ("http mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("http://mint.example"); *t = tags_for(v); None })),
        ("loopback http mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("http://127.0.0.1:3338"); *t = tags_for(v); None })),
        ("ftp mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("ftp://mint.example"); *t = tags_for(v); None })),
        ("nostr npub mint", Box::new(|v, t| { v["give"]["mint_url"] = json!(NPUB_MINT); *t = tags_for(v); None })),
        ("percent path mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://mint.example/a%20b"); *t = tags_for(v); None })),
        ("space path mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://mint.example/a b"); *t = tags_for(v); None })),
        ("dot segment mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://mint.example/a/../b"); *t = tags_for(v); None })),
        ("idn mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://mïnt.example"); *t = tags_for(v); None })),
        ("punycode mint", Box::new(|v, t| { v["give"]["mint_url"] = json!("https://xn--mnt-ula.example"); *t = tags_for(v); None })),
        ("expires_at not created+86400", Box::new(|v, t| { v["expires_at"] = json!(T0 + 86399); *t = tags_for(v); None })),
        ("other deadline policy", Box::new(|v, _| { v["deadline_policy"]["short_seconds"] = json!(600); None })),
        ("other fee policy", Box::new(|v, _| { v["fee_policy"] = json!("taker-pays"); None })),
        ("maker pubkey mismatch", Box::new(|v, _| { v["maker_trade_pubkey"] = json!(keys(0x22).public_key().to_hex()); None })),
        ("maker pubkey uppercase", Box::new(|v, _| { v["maker_trade_pubkey"] = json!(keys(0x11).public_key().to_hex().to_uppercase()); None })),
        ("unknown top-level field", Box::new(|v, _| { v["note"] = json!("hi"); None })),
        ("unknown leg field", Box::new(|v, _| { v["give"]["memo"] = json!("hi"); None })),
        ("unknown deadline field", Box::new(|v, _| { v["deadline_policy"]["x"] = json!(1); None })),
        ("missing fee policy", Box::new(|v, _| { v.as_object_mut().unwrap().remove("fee_policy"); None })),
        ("null fee policy", Box::new(|v, _| { v["fee_policy"] = json!(null); None })),
        ("duplicate key", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replacen("{", "{\"fee_policy\":\"x\",", 1)))),
        ("duplicate leg key", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replacen("\"net\":64", "\"net\":64,\"net\":64", 1)))),
        ("trailing garbage", Box::new(|v, _| Some(serde_json::to_string(v).unwrap() + "x"))),
        ("trailing whitespace", Box::new(|v, _| Some(serde_json::to_string(v).unwrap() + " \n"))),
        ("leading whitespace", Box::new(|v, _| Some(format!("\t{}", serde_json::to_string(v).unwrap())))),
        ("array content", Box::new(|_, _| Some("[]".into()))),
        ("empty content", Box::new(|_, _| Some(String::new()))),
        ("escaped key", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replacen("\"fee_policy\"", "\"fee\\u005fpolicy\"", 1)))),
        ("lone surrogate in string", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replacen("sender-funds-net-v1", "sender-funds-net-v1\\ud800", 1)))),
        ("oversized content", Box::new(|v, _| Some(format!("{}{}", serde_json::to_string(v).unwrap(), " ".repeat(8193))))),
        ("duplicate mint_url in flattened leg", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replacen("\"unit\":\"sat\"", &format!("\"unit\":\"sat\",\"mint_url\":\"{MINT_A}\""), 1)))),
        ("duplicate unit in flattened leg", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replacen("\"unit\":\"sat\"", "\"unit\":\"sat\",\"unit\":\"sat\"", 1)))),
        ("mint_url number", Box::new(|v, _| { v["give"]["mint_url"] = json!(5); None })),
        ("expires_at beyond u64", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replacen(&format!("\"expires_at\":{}", T0 + 86400), "\"expires_at\":18446744073709551617", 1)))),
        ("expires_at with leading zero", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replacen(&format!("\"expires_at\":{}", T0 + 86400), &format!("\"expires_at\":0{}", T0 + 86400), 1)))),
        ("negative zero trade_v", Box::new(|v, _| Some(serde_json::to_string(v).unwrap().replacen("\"trade_v\":1", "\"trade_v\":-0", 1)))),
        ("struct as array", Box::new(|v, _| {
            let o = v.as_object().unwrap();
            Some(json!([o["trade_v"], o["give"], o["want"], o["maker_trade_pubkey"], o["expires_at"], o["deadline_policy"], o["fee_policy"]]).to_string())
        })),
        ("deadline policy as array", Box::new(|v, _| { v["deadline_policy"] = json!([3600, 900, 2700]); None })),
        ("missing t tag", Box::new(|_, t| { t.retain(|x| x[0] != "t"); None })),
        ("wrong t tag", Box::new(|_, t| { t[0][1] = "other".into(); None })),
        ("duplicate t tag", Box::new(|_, t| { t.push(vec!["t".into(), "maxplayer".into()]); None })),
        ("v tag 2", Box::new(|_, t| { t[1][1] = "2".into(); None })),
        ("g tag disagrees", Box::new(|_, t| { t[2][1] = MINT_B.into(); None })),
        ("expiration tag disagrees", Box::new(|_, t| { t[6][1] = format!("{}", T0 + 1); None })),
        ("g tag with extra element", Box::new(|_, t| { t[2].push("x".into()); None })),
        ("missing x tag", Box::new(|_, t| { t.retain(|x| x[0] != "x"); None })),
        ("extra unrelated tags", Box::new(|_, t| { t.push(vec!["client".into(), "x".into()]); t.push(vec!["p".into()]); None })),
        ("17 tags", Box::new(|_, t| { for i in 0..10 { t.push(vec!["z".into(), i.to_string()]); } None })),
        ("16 tags", Box::new(|_, t| { for i in 0..9 { t.push(vec!["z".into(), i.to_string()]); } None })),
        ("tag value 513 bytes", Box::new(|_, t| { t.push(vec!["z".into(), "a".repeat(513)]); None })),
        ("tag value 512 bytes", Box::new(|_, t| { t.push(vec!["z".into(), "a".repeat(512)]); None })),
        ("multibyte tag value 513 bytes", Box::new(|_, t| { t.push(vec!["z".into(), format!("{}é", "a".repeat(511))]); None })),
    ];
    for (name, f) in edits {
        lots.push(lot_verdict(name, &lot_case(&maker, T0, f), now));
    }
    // Wrong kind: a 3411 shaped like a listing.
    let mut v = serde_json::to_value(base_lot(&maker, T0))?;
    let wrong_kind = sign(&maker, STATUS, T0, tags_for(&v), serde_json::to_string(&mut v)?);
    lots.push(lot_verdict("wrong kind", &wrong_kind, now));

    // ---- status chains ----
    let lot = lot_case(&maker, T0, |_, _| None);
    let lid = lot.id.to_hex();
    let s1 = status_event(&maker, lot.id, 1, lot.id, Status::Available)?;
    let sold = status_event(&maker, lot.id, 2, s1.id, Status::Sold)?;
    let cancelled = status_event(&maker, lot.id, 2, s1.id, Status::Cancelled)?;
    let mut chains = vec![
        chain_verdict("no statuses", &lot, &[]),
        chain_verdict("available", &lot, std::slice::from_ref(&s1)),
        chain_verdict("sold", &lot, &[sold.clone(), s1.clone()]),
        chain_verdict("cancelled", &lot, &[s1.clone(), cancelled.clone()]),
        chain_verdict("duplicate event idempotent", &lot, &[s1.clone(), s1.clone()]),
        chain_verdict("fork at seq 2", &lot, &[s1.clone(), sold.clone(), cancelled.clone()]),
    ];
    let reopen = status_event(&maker, lot.id, 3, sold.id, Status::Available)?;
    chains.push(chain_verdict("terminal cannot reopen", &lot, &[s1.clone(), sold.clone(), reopen]));
    let after_cancel = status_event(&maker, lot.id, 3, cancelled.id, Status::Sold)?;
    chains.push(chain_verdict("cancelled then sold", &lot, &[s1.clone(), cancelled.clone(), after_cancel]));
    let s2a = status_event(&maker, lot.id, 2, s1.id, Status::Available)?;
    let s3 = status_event(&maker, lot.id, 3, s2a.id, Status::Sold)?;
    chains.push(chain_verdict("available, available, sold", &lot, &[s3.clone(), s1.clone(), s2a.clone()]));
    chains.push(chain_verdict("gap at seq 2", &lot, &[s1.clone(), s3.clone()]));
    let gap = status_event(&maker, lot.id, 3, s1.id, Status::Sold)?;
    chains.push(chain_verdict("seq 3 linked to seq 1", &lot, &[s1.clone(), gap]));
    let bad_prev = status_event(&maker, lot.id, 2, lot.id, Status::Sold)?;
    chains.push(chain_verdict("prev skips seq 1", &lot, &[s1.clone(), bad_prev]));
    let first_sold = status_event(&maker, lot.id, 1, lot.id, Status::Sold)?;
    chains.push(chain_verdict("initial sold", &lot, &[first_sold]));
    let seq0 = status_event(&maker, lot.id, 0, lot.id, Status::Available)?;
    chains.push(chain_verdict("seq 0", &lot, &[seq0]));
    let wrong_prev1 = status_event(&maker, lot.id, 1, s1.id, Status::Available)?;
    chains.push(chain_verdict("seq 1 prev is not the lot", &lot, &[wrong_prev1]));
    let foreign = status_event(&other, lot.id, 2, s1.id, Status::Sold)?;
    chains.push(chain_verdict("foreign signer", &lot, &[s1.clone(), foreign]));
    let mut forged = sold.clone();
    forged.content = forged.content.replace("sold", "cancelled");
    chains.push(chain_verdict("tampered status", &lot, &[s1.clone(), forged]));
    chains.push(chain_verdict("wrong lot_id", &lot, &[status_raw(&maker, T0 + 1, status_tags(&lot), json!({"trade_v":1,"lot_id": s1.id.to_hex(),"seq":1,"prev":lid,"status":"available"}))]));
    let mut e_mismatch = status_tags(&lot);
    e_mismatch[2][1] = s1.id.to_hex();
    chains.push(chain_verdict("e tag mismatch", &lot, &[status_raw(&maker, T0 + 1, e_mismatch, rev(&lot, 1, &lid, "available"))]));
    let mut two_e = status_tags(&lot);
    two_e.push(vec!["e".into(), lid.clone()]);
    chains.push(chain_verdict("two e tags", &lot, &[status_raw(&maker, T0 + 1, two_e, rev(&lot, 1, &lid, "available"))]));
    let mut no_t = status_tags(&lot);
    no_t.remove(0);
    chains.push(chain_verdict("status missing t tag", &lot, &[status_raw(&maker, T0 + 1, no_t, rev(&lot, 1, &lid, "available"))]));
    let mut with_extra = rev(&lot, 1, &lid, "available");
    with_extra["note"] = json!("x");
    chains.push(chain_verdict("status unknown field", &lot, &[status_raw(&maker, T0 + 1, status_tags(&lot), with_extra)]));
    chains.push(chain_verdict("status Available capitalised", &lot, &[status_raw(&maker, T0 + 1, status_tags(&lot), rev(&lot, 1, &lid, "Available"))]));
    chains.push(chain_verdict("status unknown value", &lot, &[status_raw(&maker, T0 + 1, status_tags(&lot), rev(&lot, 1, &lid, "expired"))]));
    let mut obj_status = rev(&lot, 1, &lid, "available");
    obj_status["status"] = json!({"available": null});
    chains.push(chain_verdict("status as externally tagged object", &lot, &[status_raw(&maker, T0 + 1, status_tags(&lot), obj_status)]));
    let mut obj_status2 = rev(&lot, 1, &lid, "available");
    obj_status2["status"] = json!({"available": 1});
    chains.push(chain_verdict("status as tagged object with value", &lot, &[status_raw(&maker, T0 + 1, status_tags(&lot), obj_status2)]));
    let seq_float = rev(&lot, 1, &lid, "available").to_string().replace("\"seq\":1", "\"seq\":1.0");
    chains.push(chain_verdict("seq as float", &lot, &[sign(&maker, STATUS, T0 + 1, status_tags(&lot), seq_float)]));
    let seq_huge = rev(&lot, 1, &lid, "available").to_string().replace("\"seq\":1", "\"seq\":18446744073709551617");
    chains.push(chain_verdict("seq beyond u64", &lot, &[sign(&maker, STATUS, T0 + 1, status_tags(&lot), seq_huge)]));
    let dup_seq = rev(&lot, 1, &lid, "available").to_string().replace("\"seq\":1", "\"seq\":2,\"seq\":1");
    chains.push(chain_verdict("duplicate seq key", &lot, &[sign(&maker, STATUS, T0 + 1, status_tags(&lot), dup_seq)]));
    let arr = json!([1, lid, 1, lid, "available"]).to_string();
    chains.push(chain_verdict("revision as array", &lot, &[sign(&maker, STATUS, T0 + 1, status_tags(&lot), arr)]));
    let mut e_extra = status_tags(&lot);
    e_extra[2].push("wss://relay.example".into());
    chains.push(chain_verdict("e tag with relay hint", &lot, &[status_raw(&maker, T0 + 1, e_extra, rev(&lot, 1, &lid, "available"))]));
    chains.push(chain_verdict("prev uppercase", &lot, &[status_raw(&maker, T0 + 1, status_tags(&lot), rev(&lot, 1, &lid.to_uppercase(), "available"))]));
    let kind_mix = sign(&maker, LOT, T0 + 1, status_tags(&lot), rev(&lot, 1, &lid, "available").to_string());
    chains.push(chain_verdict("status with listing kind", &lot, &[kind_mix.clone()]));
    // The 256-event bound is exercised in the web test with this same test key
    // (0x11 repeated); 257 signed events would make this file half a megabyte.

    // A chain for the crate-built listing too, so the builder is covered end to end.
    let rs1 = status_event(&maker, real.id, 1, real.id, Status::Available)?;
    let rsold = status_event(&maker, real.id, 2, rs1.id, Status::Sold)?;
    chains.push(chain_verdict("lot_event builder, sold", &real, &[rs1, rsold]));

    let out = json!({
        "generator": "crates/maxplayer-trade/examples/web_market_fixtures.rs",
        "crate": "maxplayer-trade (PR #1107)",
        "lots": lots,
        "chains": chains,
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
