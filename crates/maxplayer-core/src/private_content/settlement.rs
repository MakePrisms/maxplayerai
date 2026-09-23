//! V2 receipt bytes. Result-event identity remains in the durable local pay-bind and the
//! buyer-signed receipt's reply tag (it cannot be in the seller's self-referential signature).
use super::{Error, Result, require_hex};
use crate::receipt::ReceiptPreimage;
use sha2::{Digest, Sha256};
pub const DOMAIN: &str = "maxplayer/v2/receipt-preimage";
pub fn canonical_json(p: &ReceiptPreimage) -> Result<String> {
    for id in [&p.job_hash, &p.offer_id, &p.buyer_pubkey, &p.seller_pubkey] {
        require_hex(id, 32)?;
    }
    if p.unit != "sat"
        || p.exec_metadata_commitment != "none"
        || p.job_hash != super::job_hash(&p.offer_id)?
    {
        return Err(Error("invalid v2 receipt trade binding"));
    }
    require_hex(
        &p.delivery_integrity_hash,
        match p.delivery_kind.as_str() {
            "fork" => 20,
            "inline" => 32,
            _ => return Err(Error("invalid receipt delivery kind")),
        },
    )?;
    let mut values = vec![
        serde_json::json!(DOMAIN),
        serde_json::json!(p.job_hash),
        serde_json::json!(p.offer_id),
        serde_json::json!(p.amount),
        serde_json::json!("sat"),
        serde_json::json!(p.buyer_pubkey),
        serde_json::json!(p.seller_pubkey),
        serde_json::json!(p.delivery_integrity_hash),
        serde_json::json!(p.delivery_kind),
        serde_json::json!("none"),
    ];
    if let Some(invoice) = &p.creq_hash {
        require_hex(invoice, 32)?;
        values.push(serde_json::json!(invoice));
    }
    serde_json::to_string(&values).map_err(|_| Error("receipt serialization failed"))
}
pub fn digest(p: &ReceiptPreimage) -> Result<[u8; 32]> {
    Ok(Sha256::digest(canonical_json(p)?.as_bytes()).into())
}
