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
/// Local input files are part of the intent: the same paths with different
/// contents are a different hire. Metadata only (no reads); unreadable sources
/// still fingerprint, and posting itself reports them.
fn input_state(params: &Value) -> Value {
    let Some(inputs) = params.get("inputs").and_then(Value::as_array) else {
        return Value::Null;
    };
    Value::Array(
        inputs
            .iter()
            .map(|input| {
                let state = input
                    .get("source")
                    .and_then(Value::as_str)
                    .and_then(|source| std::fs::symlink_metadata(source).ok())
                    .map(|m| {
                        let modified = m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos().to_string());
                        json!({ "len": m.len(), "modified_ns": modified })
                    });
                state.unwrap_or(Value::Null)
            })
            .collect(),
    )
}
/// `(handle, fingerprint, explicit)`.
fn identity(params: &mut Value) -> Result<(String, String, bool), String> {
    let inputs = input_state(params);
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
    let intent = json!({ "arguments": canonical(params), "input_state": inputs });
    let fingerprint = hex::encode(Sha256::digest(intent.to_string().as_bytes()));
    let explicit = key.is_some();
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
        explicit,
    ))
}
pub(super) fn poll(context: &BuyerContext, id: Value, handle: &str) -> Response {
    poll_store(&context.store, id, handle, false)
}
/// `deduplicated` marks an answer given to a REPEATED post_job instead of posting.
fn poll_store(store: &BuyerStore, id: Value, handle: &str, deduplicated: bool) -> Response {
    match store.preparation(handle) {
        Ok(Some(Some(mut response))) => {
            response.id = id;
            if let Some(Value::Object(result)) = response.result.as_mut() {
                result.insert("status".into(), json!("posted"));
                result.insert("preparation_id".into(), json!(handle));
                if deduplicated {
                    result.insert("deduplicated".into(), json!(true));
                    result.insert(
                        "note".into(),
                        json!("an identical post_job already posted this job; nothing new was published"),
                    );
                }
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
    let (handle, fingerprint, explicit) = match identity(&mut params) {
        Ok(identity) => identity,
        Err(e) => return Response::err(id, CODE_METHOD_NOT_FOUND, e),
    };
    match store.claim_preparation(&handle, &fingerprint, explicit, now_unix()) {
        Ok(true) => {
            let completion_store = store.clone();
            let observer_store = store.clone();
            let owned_handle = handle.clone();
            let observed_handle = handle.clone();
            let observer: crate::job_lifecycle::OfferObserver = Arc::new(move |offer: &str| {
                observer_store
                    .record_preparation_offer(&observed_handle, offer)
                    .map_err(|e| e.to_string())
            });
            // Dropping a socket/MCP future doesn't cancel this owner. Catch task
            // panics as terminal responses so polling cannot hang forever.
            let worker = tokio::spawn(crate::job_lifecycle::with_offer_observer(
                observer,
                work(params),
            ));
            let completion = tokio::spawn(async move {
                let response = worker.await.unwrap_or_else(|_| {
                    Response::err(
                        Value::Null,
                        CODE_INTERNAL,
                        "preparation worker stopped; inspect buyer state before reposting",
                    )
                });
                if let Err(e) =
                    completion_store.finish_preparation(&owned_handle, &response, now_unix())
                {
                    crate::opline!(
                        "buyer preparation result persistence failed: {e}; do not repost"
                    );
                }
            });
            // Fast jobs retain the ordinary single-call result. A slow one gets a
            // durable handle well before MCP's 15-second deadline. Timeout drops
            // only this waiter, NOT either daemon task.
            let _ = tokio::time::timeout(Duration::from_secs(1), completion).await;
            poll_store(&store, id, &handle, false)
        }
        Ok(false) => poll_store(&store, id, &handle, true),
        Err(e) => Response::err(id, CODE_INTERNAL, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn settled(store: &BuyerStore, handle: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.preparation(handle).unwrap().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn posting_identity_ignores_key_order_but_not_intent() {
        let (a, f, explicit) = identity(&mut json!({"task":"hi","amount_sats":10})).unwrap();
        assert!(!explicit);
        assert_eq!(
            identity(&mut json!({"amount_sats":10,"task":"hi"})).unwrap(),
            (a.clone(), f, false)
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
        assert!(first.2);
        assert!(identity(&mut json!({"request_id":"../bad"})).is_err());
    }

    #[test]
    fn changed_input_file_contents_are_a_different_intent() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("brief.txt");
        std::fs::write(&source, "one").unwrap();
        let params = json!({"task":"t","inputs":[{"source":source,"path":"brief.txt"}]});
        let before = identity(&mut params.clone()).unwrap();
        assert_eq!(identity(&mut params.clone()).unwrap(), before);
        std::fs::write(&source, "two, longer").unwrap();
        let after = identity(&mut params.clone()).unwrap();
        assert_ne!(after.0, before.0, "automatic key follows the file");
        let keyed =
            json!({"request_id":"k","task":"t","inputs":[{"source":source,"path":"brief.txt"}]});
        let keyed_before = identity(&mut keyed.clone()).unwrap();
        std::fs::write(&source, "three, longer still").unwrap();
        let keyed_after = identity(&mut keyed.clone()).unwrap();
        assert_eq!(keyed_after.0, keyed_before.0);
        assert_ne!(
            keyed_after.1, keyed_before.1,
            "same request_id with changed files is refused"
        );
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
        let handle = pending["preparation_id"].as_str().unwrap().to_owned();
        let again = submit(store.clone(), json!(2), params.clone(), |_| async {
            panic!("duplicate job")
        })
        .await;
        assert_eq!(again.result.unwrap(), pending);
        release.send(()).unwrap();
        settled(&store, &handle).await;
        let ready = poll_store(&store, json!(3), &handle, false).result.unwrap();
        assert_eq!(ready["status"], "posted");
        assert_eq!(ready["job_id"], "only-job");
        assert!(ready.get("deduplicated").is_none());
        let repeat = submit(store.clone(), json!(4), params, |_| async {
            panic!("reposted completed job")
        })
        .await
        .result
        .unwrap();
        assert_eq!(repeat["job_id"], "only-job");
        assert_eq!(
            repeat["deduplicated"], true,
            "a repeat is told it posted nothing"
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
        settled(&store, &handle).await;
        assert!(
            poll_store(&store, Value::Null, &handle, false)
                .error
                .is_some()
        );
    }

    /// Regression (#1096 review, finding 1): a failure before any offer was queued was
    /// cached forever, so an identical retry after the relay recovered never ran.
    #[tokio::test]
    async fn transient_failure_before_publication_reruns_on_identical_retry() {
        let root = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(root.path().join("buyer.sqlite")).unwrap();
        let params = json!({"task":"same","request_id":"hire-1"});
        let first = submit(store.clone(), json!(1), params.clone(), |_| async {
            Response::err(Value::Null, CODE_INTERNAL, "base input upload unavailable")
        })
        .await;
        assert!(first.error.is_some());
        let retry = submit(store.clone(), json!(2), params, |_| async {
            Response::ok(Value::Null, json!({"job_id":"posted-now"}))
        })
        .await
        .result
        .unwrap();
        assert_eq!(retry["job_id"], "posted-now");
        assert!(retry.get("deduplicated").is_none());
    }

    /// Once the posting path says an offer is about to be queued/published, a later
    /// failure (or crash) is never re-run by an identical retry: it may be live.
    #[tokio::test]
    async fn failure_after_offer_was_queued_is_never_rerun() {
        let root = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(root.path().join("buyer.sqlite")).unwrap();
        let params = json!({"task":"same"});
        let first = submit(store.clone(), json!(1), params.clone(), |_| async {
            crate::job_lifecycle::note_offer_before_publication("offer-1").unwrap();
            Response::err(Value::Null, CODE_INTERNAL, "relay publish timed out")
        })
        .await;
        assert!(first.error.is_some());
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let retry = submit(store.clone(), json!(2), params, move |_| async move {
            counted.fetch_add(1, Ordering::SeqCst);
            Response::ok(Value::Null, json!({"job_id":"duplicate"}))
        })
        .await;
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(retry.error.is_some());
    }

    /// A restart before any offer was queued leaves a retryable handle, not a
    /// permanently stuck one.
    #[tokio::test]
    async fn restart_before_publication_lets_the_identical_retry_run() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("buyer.sqlite");
        let params = json!({"task":"same","request_id":"hire-2"});
        let (handle, fingerprint, explicit) = identity(&mut params.clone()).unwrap();
        let store = BuyerStore::open(&path).unwrap();
        assert!(
            store
                .claim_preparation(&handle, &fingerprint, explicit, now_unix())
                .unwrap()
        );
        drop(store); // daemon died mid-preparation
        let store = BuyerStore::open(&path).unwrap();
        store.interrupt_preparations(now_unix()).unwrap();
        let retry = submit(store.clone(), json!(1), params, |_| async {
            Response::ok(Value::Null, json!({"job_id":"after-restart"}))
        })
        .await
        .result
        .unwrap();
        assert_eq!(retry["job_id"], "after-restart");
    }
}
