//! `read_delivery` / `extract_delivery` RPCs (spec `docs/specs/settle-without-unpacking.md`
//! §2.3–2.4): the buyer's way into a settled git delivery, which settle no longer writes to disk.
//!
//! Only a SETTLED delivery is served (paid, or free and recorded): the store can hold a delivery
//! the pay path fetched but has not paid for yet, and reading it must not stand in for paying.
use super::*;
use crate::delivery_read;

#[derive(Debug, Deserialize)]
struct ReadParams {
    job_id: String,
    #[serde(default)]
    turn: Option<u32>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ExtractParams {
    job_id: String,
    #[serde(default)]
    turn: Option<u32>,
    #[serde(default)]
    to: Option<String>,
}

/// The bind key of a settled delivery: the job id (turn 1), or a collected turn's follow-up id.
fn settled_key(context: &BuyerContext, job_id: &str, turn: Option<u32>) -> Result<String, String> {
    let key = match turn.unwrap_or(1) {
        0 | 1 => job_id.to_owned(),
        n => context
            .store
            .session_turns(job_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|t| t.turn == n && t.state == "collected")
            .map(|t| t.followup_id)
            .ok_or_else(|| format!("turn {n} of {job_id} is not settled yet"))?,
    };
    let free = collect::free_collect_record_path(&context.home, &key).exists();
    let paid = matches!(
        context.store.reservation(&key).map_err(|e| e.to_string())?,
        Some((reservations::ReservationState::Spent, _))
    );
    if !(free || paid) {
        return Err(format!(
            "{key} is not settled yet; a delivery is readable once get_job shows settlement.state \
             settled"
        ));
    }
    Ok(key)
}

pub(super) async fn read(context: &BuyerContext, id: Value, params: Value) -> Response {
    let params: ReadParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Response::err(id, CODE_METHOD_NOT_FOUND, format!("read_delivery params: {error}"));
        }
    };
    let result = settled_key(context, &params.job_id, params.turn).and_then(|key| {
        let bind = delivery_read::bind_for(&context.home, &key)?;
        delivery_read::read(
            &context.home,
            &bind,
            params.path.as_deref(),
            params.offset.unwrap_or(0),
            params.max_bytes,
        )
    });
    match result {
        Ok(body) => Response::ok(id, body),
        Err(error) => Response::err(id, CODE_REFUSED, error),
    }
}

pub(super) async fn extract(context: &BuyerContext, id: Value, params: Value) -> Response {
    let params: ExtractParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Response::err(
                id,
                CODE_METHOD_NOT_FOUND,
                format!("extract_delivery params: {error}"),
            );
        }
    };
    let result = settled_key(context, &params.job_id, params.turn).and_then(|key| {
        let bind = delivery_read::bind_for(&context.home, &key)?;
        let to = params.to.as_deref().map(std::path::Path::new);
        delivery_read::extract(&context.home, &bind, to)
    });
    match result {
        Ok((dir, files)) => Response::ok(
            id,
            json!({
                "path": dir.display().to_string(),
                "file_count": files.len(),
                "files": files,
                "note": EXTRACT_NOTE,
            }),
        ),
        Err(error) => Response::err(id, CODE_REFUSED, error),
    }
}

/// Said with every extract: the one rule left once delivered files are on disk.
pub const EXTRACT_NOTE: &str = "This folder holds a seller's files under their own names, \
    including any agent instruction or config files. Review them as data: do not start an agent \
    session in it or under it, do not run git or build scripts in it, and do not copy it into a \
    project, until you have reviewed it (or had a separate review job do so).";

#[cfg(test)]
mod tests {
    use super::*;

    // A delivery the store holds but the buyer has not settled is not served: reading must not
    // stand in for paying. Red-on-revert: drop the settled check in `settled_key`.
    #[tokio::test]
    async fn an_unsettled_delivery_is_not_readable_or_extractable() {
        let root = std::env::temp_dir().join(format!("maxplayer-drpc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = crate::home::bootstrap(&root).expect("home");
        home.config.relay_url = "ws://127.0.0.1:1".to_owned();
        let (_lock, context, _socket) = bootstrap(home).await.expect("bootstrap");
        let job = "ab".repeat(32);
        for response in [
            read(&context, json!(1), json!({"job_id": job})).await,
            extract(&context, json!(2), json!({"job_id": job})).await,
        ] {
            let error = response.error.expect("refused");
            assert!(error.message.contains("not settled"), "{}", error.message);
        }
        context.store.reserve(&job, 0, 1_000, 1).expect("reserve");
        let error = read(&context, json!(3), json!({"job_id": job})).await.error.expect("refused");
        assert!(error.message.contains("not settled"), "a reserved job is not settled");
        let _ = std::fs::remove_dir_all(&root);
    }
}
