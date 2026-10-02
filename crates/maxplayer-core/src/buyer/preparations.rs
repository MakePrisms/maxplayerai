//! MCP's bounded acknowledgement of long posting work. Keep the existing
//! post/validation/money path unchanged; own it in the daemon, not the tool call.
use super::*;
use sha2::{Digest, Sha256};

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), canonical(v)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}
fn identity(params: &mut Value) -> Result<(String, String), String> {
    let map = params
        .as_object_mut()
        .ok_or("post_job arguments must be an object")?;
    let key = map.remove("request_id");
    let key = match key {
        None => None,
        Some(Value::String(s))
            if !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Some(s)
        }
        _ => {
            return Err(
                "request_id must contain 1–128 ASCII letters/digits, dash or underscore".into(),
            );
        }
    };
    let fingerprint = hex::encode(Sha256::digest(canonical(params).to_string().as_bytes()));
    let material = key.map_or_else(
        || format!("arguments:{fingerprint}"),
        |k| format!("explicit:{k}"),
    );
    Ok((
        format!(
            "preparation:{}",
            hex::encode(Sha256::digest(material.as_bytes()))
        ),
        fingerprint,
    ))
}
pub(super) fn poll(context: &BuyerContext, id: Value, handle: &str) -> Response {
    poll_store(&context.store, id, handle)
}
fn poll_store(store: &BuyerStore, id: Value, handle: &str) -> Response {
    match store.preparation(handle) {
        Ok(Some(Some(mut response))) => {
            response.id = id;
            if let Some(Value::Object(result)) = response.result.as_mut() {
                result.insert("status".into(), json!("posted"));
                result.insert("preparation_id".into(), json!(handle));
            }
            response
        }
        Ok(Some(None)) => Response::ok(
            id,
            json!({"status":"preparing","preparation_id":handle,
            "next":"Call get_job with job_id=preparation_id; do not post another job."}),
        ),
        Ok(None) => Response::err(id, CODE_METHOD_NOT_FOUND, "unknown preparation handle"),
        Err(e) => Response::err(id, CODE_INTERNAL, e.to_string()),
    }
}
pub(super) async fn start(context: &Arc<BuyerContext>, id: Value, params: Value) -> Response {
    let worker_context = context.clone();
    submit(
        context.store.clone(),
        id,
        params,
        move |params| async move { post_job(&worker_context, Value::Null, params).await },
    )
    .await
}
async fn submit<F, Fut>(store: BuyerStore, id: Value, mut params: Value, work: F) -> Response
where
    F: FnOnce(Value) -> Fut,
    Fut: std::future::Future<Output = Response> + Send + 'static,
{
    let (handle, fingerprint) = match identity(&mut params) {
        Ok(pair) => pair,
        Err(e) => return Response::err(id, CODE_METHOD_NOT_FOUND, e),
    };
    match store.claim_preparation(&handle, &fingerprint) {
        Ok(true) => {
            let completion_store = store.clone();
            let owned_handle = handle.clone();
            // Dropping a socket/MCP future doesn't cancel this owner. Catch task
            // panics as terminal responses so polling cannot hang forever.
            let worker = tokio::spawn(work(params));
            let completion = tokio::spawn(async move {
                let response = worker.await.unwrap_or_else(|_| {
                    Response::err(
                        Value::Null,
                        CODE_INTERNAL,
                        "preparation worker stopped; inspect buyer state before reposting",
                    )
                });
                if let Err(e) = completion_store.finish_preparation(&owned_handle, &response) {
                    crate::opline!(
                        "buyer preparation result persistence failed: {e}; do not repost"
                    );
                }
            });
            // Fast jobs retain the ordinary single-call result. A slow one gets a
            // durable handle well before MCP's 15-second deadline. Timeout drops
            // only this waiter, NOT either daemon task.
            let _ = tokio::time::timeout(Duration::from_secs(1), completion).await;
        }
        Ok(false) => {}
        Err(e) => return Response::err(id, CODE_INTERNAL, e.to_string()),
    }
    poll_store(&store, id, &handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn posting_identity_ignores_key_order_but_not_intent() {
        let (a, f) = identity(&mut json!({"task":"hi","amount_sats":10})).unwrap();
        assert_eq!(
            identity(&mut json!({"amount_sats":10,"task":"hi"})).unwrap(),
            (a.clone(), f)
        );
        assert_ne!(
            identity(&mut json!({"task":"other","amount_sats":10}))
                .unwrap()
                .0,
            a
        );
        let first = identity(&mut json!({"request_id":"same","task":"a"})).unwrap();
        let changed = identity(&mut json!({"request_id":"same","task":"b"})).unwrap();
        assert_eq!(first.0, changed.0);
        assert_ne!(first.1, changed.1);
        assert!(identity(&mut json!({"request_id":"../bad"})).is_err());
    }
    #[tokio::test]
    async fn slow_post_returns_handle_and_retries_do_not_start_duplicate_work() {
        let root = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(root.path().join("buyer.sqlite")).unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let params = json!({"task":"same","request_id":"posting-1"});
        let pending = submit(store.clone(), json!(1), params.clone(), |_| async move {
            gate.await.unwrap();
            Response::ok(Value::Null, json!({"job_id":"only-job"}))
        })
        .await
        .result
        .unwrap();
        assert_eq!(pending["status"], "preparing");
        let handle = pending["preparation_id"].as_str().unwrap();
        let again = submit(store.clone(), json!(2), params.clone(), |_| async {
            panic!("duplicate job")
        })
        .await;
        assert_eq!(again.result.unwrap(), pending);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.preparation(handle).unwrap().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let ready = poll_store(&store, json!(3), handle).result.unwrap();
        assert_eq!(ready["status"], "posted");
        assert_eq!(ready["job_id"], "only-job");
        assert_eq!(
            submit(store.clone(), json!(4), params, |_| async {
                panic!("reposted completed job")
            })
            .await
            .result
            .unwrap(),
            ready
        );
    }
    #[tokio::test]
    async fn disconnected_waiter_does_not_cancel_or_duplicate_preparation() {
        let root = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(root.path().join("buyer.sqlite")).unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let params = json!({"task":"same"});
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                submit(store.clone(), Value::Null, params.clone(), |_| async move {
                    gate.await.unwrap();
                    Response::err(Value::Null, CODE_INTERNAL, "fixture failure")
                })
            )
            .await
            .is_err()
        );
        let retry = submit(store.clone(), Value::Null, params.clone(), |_| async {
            panic!("duplicate work after disconnect")
        })
        .await;
        assert_eq!(retry.result.unwrap()["status"], "preparing");
        release.send(()).unwrap();
        let handle = identity(&mut params.clone()).unwrap().0;
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.preparation(&handle).unwrap().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(poll_store(&store, Value::Null, &handle).error.is_some());
    }
}
