//! The durable request log (spec §3.4, obligations 2–5).
//!
//! Every request the mint admits is keyed by (client pubkey, request `id`) and bound to the request
//! event id and a digest of `v`/`op`/`body`/`exp`. The record lives in the mint's own sqlite store
//! (cdk's KV namespace), so it survives restarts together with the mint state it describes.
//!
//! - `executing` is written BEFORE the mint is called; `completed` (with the exact reply) after.
//! - A duplicate of a `completed` request gets the recorded reply, even after `exp`, and before the
//!   rate limiter sees it.
//! - A duplicate of an `executing` request (the process died, or the outcome was ambiguous) is
//!   reconciled from the mint's own committed state: a swap whose outputs are ALL signed is
//!   answered with those signatures (the same ones, DLEQ included, that the first execution
//!   returned); a swap with NONE signed never took effect (cdk compensates an unfinished swap saga
//!   at start), so it is re-executed if still valid or refused `expired`; a partial match is
//!   impossible for a committed swap and is refused `internal` with an alarm.
//!
//! This gives the same guarantee as writing the reply inside cdk's finalize transaction — every
//! replay answers exactly what was committed — without wrapping cdk's mint database. Requests are
//! handled one at a time ([`crate::server`]), so two copies of one request never race in-process.

use cdk::Mint;
use cdk::error::{ErrorCode as NutErrorCode, ErrorResponse};
use cdk::nuts::{BlindSignature, BlindedMessage, RestoreRequest};
use maxplayer_core::mint_wire::{ErrorCode, Outcome, Request, code};
use nostr_sdk::prelude::PublicKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const NAMESPACE: &str = "maxplayer_mint";
const REQUESTS: &str = "requests";

/// One admitted request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Record {
    Executing {
        event_id: String,
        digest: String,
    },
    Completed {
        event_id: String,
        digest: String,
        outcome: Outcome,
    },
}

impl Record {
    /// Whether this record belongs to the same request (same event, same content).
    pub fn matches(&self, event_id: &str, digest: &str) -> bool {
        let (e, d) = match self {
            Record::Executing { event_id, digest } => (event_id, digest),
            Record::Completed {
                event_id, digest, ..
            } => (event_id, digest),
        };
        e == event_id && d == digest
    }
}

/// KV key for (client, request id): 64 lowercase hex chars (inside cdk's key alphabet).
pub fn key(client: &PublicKey, request_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(client.to_bytes());
    hasher.update([0u8]);
    hasher.update(request_id.as_bytes());
    hex(&hasher.finalize())
}

/// Digest of everything the request asks for.
pub fn digest(request: &Request) -> Result<String, String> {
    let bytes = serde_json::to_vec(&(request.v, &request.op, &request.body, request.exp))
        .map_err(|error| format!("digest request: {error}"))?;
    Ok(hex(&Sha256::digest(bytes)))
}

pub async fn read(mint: &Mint, key: &str) -> Result<Option<Record>, String> {
    let bytes = mint
        .localstore()
        .kv_read(NAMESPACE, REQUESTS, key)
        .await
        .map_err(|error| format!("read request log: {error}"))?;
    bytes
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|error| format!("decode request log: {error}"))
        })
        .transpose()
}

pub async fn write(mint: &Mint, key: &str, record: &Record) -> Result<(), String> {
    let value = serde_json::to_vec(record).map_err(|error| format!("encode record: {error}"))?;
    let mut tx = mint
        .localstore()
        .begin_transaction()
        .await
        .map_err(|error| format!("begin: {error}"))?;
    tx.kv_write(NAMESPACE, REQUESTS, key, &value)
        .await
        .map_err(|error| format!("write request log: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit request log: {error}"))
}

/// Whether `outcome` is final: a success, or a failure that promises nothing ran. Ambiguous
/// failures (`internal`, NUT 11002 pending, unknown codes) stay `executing` and are reconciled.
pub fn settled(outcome: &Outcome) -> bool {
    match outcome {
        Outcome::Ok(_) => true,
        Outcome::Err(error) => match &error.code {
            ErrorCode::Named(name) => name != code::INTERNAL,
            ErrorCode::Nut(nut) => cdk::Error::from(ErrorResponse {
                code: NutErrorCode::from_code(*nut),
                detail: String::new(),
            })
            .is_definitive_failure(),
        },
    }
}

/// How much of a swap's output set the mint has signed.
pub enum Signed {
    All(Vec<BlindSignature>),
    None,
    Partial,
}

/// Look the outputs up in the mint's signature store (NUT-09 restore), in request order.
pub async fn signed_outputs(mint: &Mint, outputs: &[BlindedMessage]) -> Result<Signed, String> {
    let restored = mint
        .restore(RestoreRequest {
            outputs: outputs.to_vec(),
        })
        .await
        .map_err(|error| format!("restore: {error}"))?;
    // Every output is looked up, so a gap anywhere (not just a missing prefix) reads as Partial.
    let found: Vec<Option<BlindSignature>> = outputs
        .iter()
        .map(|output| {
            restored
                .outputs
                .iter()
                .position(|signed| signed.blinded_secret == output.blinded_secret)
                .and_then(|index| restored.signatures.get(index).cloned())
        })
        .collect();
    let signed = found.iter().filter(|signature| signature.is_some()).count();
    Ok(if signed == 0 {
        Signed::None
    } else if signed == outputs.len() {
        Signed::All(found.into_iter().flatten().collect())
    } else {
        Signed::Partial
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
