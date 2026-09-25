//! Validate nested NUT-18 data before serde can discard duplicate/unknown fields.
//! The signed invoice bytes are never rewritten, and payment-token recipients do not change.
use super::{Error, Result, wire::HostPolicy};
use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use cashu::nuts::nut18::PaymentRequest;
use ciborium::Value;
use nostr_sdk::prelude::{Nip19Profile, PublicKey, ToBech32};
use std::{collections::BTreeSet, io::Cursor, str::FromStr};

fn map<'a>(v: &'a Value, keys: &[&str]) -> Result<Vec<(&'a str, &'a Value)>> {
    let Value::Map(entries) = v else {
        return Err(Error("invalid invoice object"));
    };
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for (k, v) in entries {
        let Value::Text(k) = k else {
            return Err(Error("invalid invoice key"));
        };
        if !keys.contains(&k.as_str()) || !seen.insert(k) {
            return Err(Error("unknown or duplicate invoice key"));
        }
        out.push((k.as_str(), v));
    }
    Ok(out)
}
fn value<'a>(entries: &[(&str, &'a Value)], key: &str) -> Result<&'a Value> {
    entries
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .ok_or(Error("missing invoice field"))
}
fn text(v: &Value) -> Result<&str> {
    if let Value::Text(s) = v {
        Ok(s)
    } else {
        Err(Error("invalid invoice string"))
    }
}
fn uint(v: &Value) -> Result<u64> {
    if let Value::Integer(i) = v {
        (*i).try_into()
            .map_err(|_| Error("invalid invoice integer"))
    } else {
        Err(Error("invalid invoice integer"))
    }
}

pub fn validate(
    raw: &str,
    offer: &str,
    amount: u64,
    seller: &str,
    host: &HostPolicy,
) -> Result<PaymentRequest> {
    if raw.len() > 16 * 1024 {
        return Err(Error("invoice too large"));
    }
    super::require_hex(offer, 32)?;
    super::require_hex(seller, 32)?;
    let request = PaymentRequest::from_str(raw).map_err(|_| Error("invalid payment request"))?;
    let decoded: Value = if let Some(s) = raw.strip_prefix("creqA") {
        let codec = GeneralPurpose::new(
            &alphabet::URL_SAFE,
            GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
        );
        let bytes = codec
            .decode(s)
            .map_err(|_| Error("invalid invoice base64"))?;
        let mut cursor = Cursor::new(&bytes);
        let v: Value =
            ciborium::from_reader(&mut cursor).map_err(|_| Error("invalid invoice CBOR"))?;
        if cursor.position() != bytes.len() as u64 {
            return Err(Error("trailing invoice data"));
        }
        v
    } else {
        // NUT-26's permissive TLV decoder ignores unknowns/duplicates/trailing fragments.
        // Admit only the exact canonical supported CREQ-B representation to exclude those.
        let canonical = request
            .to_bech32_string()
            .map_err(|_| Error("invalid invoice TLV"))?;
        if !raw.eq_ignore_ascii_case(&canonical) {
            return Err(Error("noncanonical or extended invoice TLV"));
        }
        let mut bytes = vec![];
        ciborium::into_writer(&request, &mut bytes)
            .map_err(|_| Error("invalid decoded invoice"))?;
        ciborium::from_reader(bytes.as_slice()).map_err(|_| Error("invalid decoded invoice"))?
    };
    let entries = map(&decoded, &["i", "a", "u", "s", "m", "d", "t", "nut10"])?;
    if text(value(&entries, "i")?)? != offer
        || uint(value(&entries, "a")?)? != amount
        || text(value(&entries, "u")?)? != "sat"
        || value(&entries, "s")? != &Value::Bool(true)
    {
        return Err(Error("invoice does not match offer"));
    }
    for key in ["d", "nut10"] {
        if entries.iter().any(|(k, v)| *k == key && **v != Value::Null) {
            return Err(Error("private invoice contains optional content"));
        }
    }
    let Value::Array(mints) = value(&entries, "m")? else {
        return Err(Error("invalid invoice mints"));
    };
    if mints.is_empty() || mints.len() > 32 {
        return Err(Error("invalid invoice mint count"));
    }
    let mut seen = BTreeSet::new();
    for mint in mints {
        let mint = text(mint)?;
        host.mint(mint)?;
        if !seen.insert(mint) {
            return Err(Error("duplicate invoice mint"));
        }
    }
    let Value::Array(transports) = value(&entries, "t")? else {
        return Err(Error("invalid invoice transport"));
    };
    if transports.len() != 1 {
        return Err(Error("invalid invoice transport count"));
    }
    let transport = map(&transports[0], &["t", "a", "g"])?;
    let profile = Nip19Profile::new(
        PublicKey::from_hex(seller).map_err(|_| Error("invalid seller key"))?,
        [],
    )
    .to_bech32()
    .map_err(|_| Error("invalid recipient profile"))?;
    if text(value(&transport, "t")?)? != "nostr" || text(value(&transport, "a")?)? != profile {
        return Err(Error("invoice recipient mismatch"));
    }
    let expected = Value::Array(vec![Value::Array(vec![
        Value::Text("n".into()),
        Value::Text("17".into()),
    ])]);
    if value(&transport, "g")? != &expected {
        return Err(Error("invalid invoice transport tags"));
    }
    Ok(request)
}
