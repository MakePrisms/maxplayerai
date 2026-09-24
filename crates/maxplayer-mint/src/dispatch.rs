//! One envelope `op` → one cdk mint call (spec §3.2). A local-issue mint serves the ops a holder
//! needs to verify, split and spend credits; minting and melting are `unsupported`.

use cdk::Mint;
use cdk::error::ErrorResponse;
use cdk::nuts::{CheckStateRequest, Id, KeysResponse, RestoreRequest, SwapRequest};
use cdk::util::unix_time;
use maxplayer_core::mint_wire::{ErrorBody, ErrorCode, Outcome, code, op};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Most inputs and outputs per request (spec §6: 128 proofs ≈ 51 KB, inside the NIP-44 limit).
pub const MAX_IO: usize = 128;

/// Run `operation` against `mint`. Errors raised before the mint is called (`bad_request`,
/// `unsupported`) promise nothing was executed.
pub async fn execute(mint: &Mint, operation: &str, body: Value) -> Outcome {
    match run(mint, operation, body).await {
        Ok(value) => Outcome::Ok(value),
        Err(error) => Outcome::Err(error),
    }
}

async fn run(mint: &Mint, operation: &str, body: Value) -> Result<Value, ErrorBody> {
    match operation {
        op::INFO => encode(&mint.mint_info().await.map_err(nut)?.time(unix_time())),
        op::KEYS => encode(&mint.pubkeys()),
        op::KEYSET => {
            #[derive(serde::Deserialize)]
            struct Body {
                id: Id,
            }
            let Body { id } = decode(operation, body)?;
            let keyset = mint
                .keyset(&id)
                .ok_or_else(|| nut(cdk::Error::UnknownKeySet))?;
            encode(&KeysResponse {
                keysets: vec![keyset],
            })
        }
        op::KEYSETS => encode(&mint.keysets()),
        op::SWAP => {
            let request: SwapRequest = decode(operation, body)?;
            encode(&mint.process_swap_request(request).await.map_err(nut)?)
        }
        op::CHECKSTATE => {
            let request: CheckStateRequest = decode(operation, body)?;
            encode(&mint.check_state(&request).await.map_err(nut)?)
        }
        op::RESTORE => {
            let request: RestoreRequest = decode(operation, body)?;
            encode(&mint.restore(request).await.map_err(nut)?)
        }
        other => Err(named(
            code::UNSUPPORTED,
            format!("`{other}` is not served by a local-issue mint"),
        )),
    }
}

/// A cdk error as its NUT error code, exactly what an HTTPS mint would return.
pub fn nut(error: cdk::Error) -> ErrorBody {
    let response = ErrorResponse::from(error);
    ErrorBody {
        code: ErrorCode::Nut(response.code.to_code()),
        detail: response.detail,
    }
}

pub fn named(name: &str, detail: impl Into<String>) -> ErrorBody {
    ErrorBody {
        code: ErrorCode::Named(name.to_owned()),
        detail: detail.into(),
    }
}

fn decode<T: DeserializeOwned>(operation: &str, body: Value) -> Result<T, ErrorBody> {
    serde_json::from_value(body)
        .map_err(|error| named(code::BAD_REQUEST, format!("{operation} body: {error}")))
}

fn encode<T: Serialize>(value: &T) -> Result<Value, ErrorBody> {
    serde_json::to_value(value).map_err(|error| named(code::INTERNAL, format!("encode: {error}")))
}
