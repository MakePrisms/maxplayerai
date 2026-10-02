//! MCP's bounded acknowledgement of long posting work. Keep the existing
//! post/validation/money path unchanged; own it in the daemon, not the tool call.
use super::*;
use sha2::{Digest, Sha256};
use std::time::Instant;

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
/// Pre-claim fingerprinting must answer well inside MCP's 15-second tool deadline
/// (the daemon then waits at most 1s more): a call that times out at MCP while the
/// daemon goes on to claim and publish invites a second hire under a new key.
const FINGERPRINT_BUDGET: Duration = Duration::from_secs(8);

/// The input files could not be hashed before the deadline. Nothing was claimed.
#[derive(Debug, PartialEq, Eq)]
struct FingerprintExpired;

/// Local input files are part of the intent: the same paths with different
/// CONTENTS are a different hire, and rewriting identical bytes is not. Hashes the
/// bytes posting would read (regular file, no symlink follow, bounded like
/// `private_content::inputs`); an unreadable or oversized source fingerprints as
/// such, and posting itself reports it. Files are hashed in parallel and the whole
/// read stops at `deadline`.
fn input_state(params: &Value, deadline: Instant) -> Result<Value, FingerprintExpired> {
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(inputs) = params.get("inputs").and_then(Value::as_array) else {
        return Ok(Value::Null);
    };
    let digest = |source: &str| -> Result<Option<String>, FingerprintExpired> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let Ok(file) = options.open(source) else {
            return Ok(None);
        };
        let Ok(metadata) = file.metadata() else {
            return Ok(None);
        };
        if !metadata.is_file() || metadata.len() > crate::private_content::MAX_FILE_BYTES {
            return Ok(None);
        }
        let mut hasher = Sha256::new();
        let mut reader = file.take(crate::private_content::MAX_FILE_BYTES + 1);
        let mut buffer = vec![0; 256 * 1024];
        loop {
            if Instant::now() >= deadline {
                return Err(FingerprintExpired);
            }
            let Ok(n) = reader.read(&mut buffer) else {
                return Ok(None);
            };
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        Ok(Some(hex::encode(hasher.finalize())))
    };
    let sources: Vec<Option<&str>> = inputs
        .iter()
        .map(|input| input.get("source").and_then(Value::as_str))
        .collect();
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .clamp(1, 8)
        .min(sources.len().max(1));
    let mut digests: Vec<Result<Option<String>, FingerprintExpired>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(source) = sources.get(i) else {
                                break done;
                            };
                            done.push((i, source.map_or(Ok(None), digest)));
                        }
                    })
                })
                .collect();
            let mut all: Vec<_> = handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect();
            all.sort_by_key(|(i, _)| *i);
            all.into_iter().map(|(_, d)| d).collect()
        });
    if digests.len() != sources.len() {
        // A hashing thread panicked; treat it like running out of time: no claim.
        return Err(FingerprintExpired);
    }
    let digests = digests
        .drain(..)
        .map(|d| d.map(|d| d.map_or(Value::Null, Value::String)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Array(digests))
}
/// `(handle, fingerprint, explicit)`.
#[cfg(test)]
fn identity(params: &mut Value) -> Result<(String, String, bool), String> {
    identity_before(params, Instant::now() + FINGERPRINT_BUDGET)
}
fn identity_before(
    params: &mut Value,
    deadline: Instant,
) -> Result<(String, String, bool), String> {
    let inputs = input_state(params, deadline).map_err(|FingerprintExpired| {
        format!(
            "post_job input files could not be fingerprinted within {}s, so this call claimed \
             and posted nothing. Retry the identical call (a second read is usually faster), \
             or attach fewer or smaller input files",
            FINGERPRINT_BUDGET.as_secs()
        )
    })?;
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
/// A failure after an offer was recorded always names it: it may be live.
fn poll_store(store: &BuyerStore, id: Value, handle: &str, deduplicated: bool) -> Response {
    match store.preparation_state(handle) {
        Ok(Some((Some(mut response), offer))) => {
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
            if let Some(error) = response.error.as_mut() {
                let offer = offer
                    .as_deref()
                    .filter(|o| *o != store::preparations::UNKNOWN_OFFER);
                if let Some(offer) = offer.filter(|o| !error.message.contains(*o)) {
                    error.message.push_str(&format!(
                        " Offer {offer} was queued for publication before this failure and may \
                         be live; auto-award was NOT armed. Inspect it with get_job \
                         job_id={offer}."
                    ));
                }
                if deduplicated {
                    error.message.push_str(
                        " (deduplicated: this repeated post_job did not run or publish anything)",
                    );
                }
            }
            response
        }
        Ok(Some((None, _))) => Response::ok(
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
async fn submit<F, Fut>(store: BuyerStore, id: Value, params: Value, work: F) -> Response
where
    F: FnOnce(Value) -> Fut,
    Fut: std::future::Future<Output = Response> + Send + 'static,
{
    submit_within(store, id, params, FINGERPRINT_BUDGET, work).await
}
async fn submit_within<F, Fut>(
    store: BuyerStore,
    id: Value,
    params: Value,
    budget: Duration,
    work: F,
) -> Response
where
    F: FnOnce(Value) -> Fut,
    Fut: std::future::Future<Output = Response> + Send + 'static,
{
    // Hashing input files is blocking I/O, bounded so the ack beats MCP's deadline.
    let deadline = Instant::now() + budget;
    let identified = tokio::task::spawn_blocking(move || {
        let mut params = params;
        let identity = identity_before(&mut params, deadline);
        (params, identity)
    })
    .await;
    let (params, (handle, fingerprint, explicit)) = match identified {
        Ok((params, Ok(identity))) => (params, identity),
        Ok((_, Err(e))) => return Response::err(id, CODE_METHOD_NOT_FOUND, e),
        Err(_) => return Response::err(id, CODE_INTERNAL, "post_job fingerprint worker stopped"),
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
    fn input_file_contents_not_timestamps_are_the_intent() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("brief.txt");
        std::fs::write(&source, "one").unwrap();
        let params = json!({"task":"t","inputs":[{"source":source,"path":"brief.txt"}]});
        let before = identity(&mut params.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&source, "one").unwrap(); // same bytes, new mtime
        assert_eq!(
            identity(&mut params.clone()).unwrap(),
            before,
            "rewriting identical bytes is the same request (#1096 advisor F5)"
        );
        std::fs::write(&source, "two").unwrap(); // same length, new bytes
        let after = identity(&mut params.clone()).unwrap();
        assert_ne!(after.0, before.0, "automatic key follows the contents");
        let keyed =
            json!({"request_id":"k","task":"t","inputs":[{"source":source,"path":"brief.txt"}]});
        let keyed_before = identity(&mut keyed.clone()).unwrap();
        std::fs::write(&source, "six").unwrap();
        let keyed_after = identity(&mut keyed.clone()).unwrap();
        assert_eq!(keyed_after.0, keyed_before.0);
        assert_ne!(
            keyed_after.1, keyed_before.1,
            "same request_id with changed contents is refused"
        );
    }

    /// Regression (#1096 advisor round 2, nit 2): hashing large inputs ran unbounded
    /// before the claim, so MCP could time out while the daemon went on to publish.
    #[tokio::test]
    async fn input_fingerprint_past_its_budget_claims_and_posts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("big.bin");
        std::fs::write(&source, vec![7u8; 1024 * 1024]).unwrap();
        let store = BuyerStore::open(dir.path().join("buyer.sqlite")).unwrap();
        let params = json!({"task":"t","inputs":[{"source":source,"path":"big.bin"}]});
        let refused = submit_within(
            store.clone(),
            json!(1),
            params.clone(),
            Duration::ZERO,
            |_| async { panic!("posted after the fingerprint budget expired") },
        )
        .await
        .error
        .expect("expired fingerprint is refused")
        .message;
        assert!(refused.contains("claimed and posted nothing"), "{refused}");
        let handle = identity(&mut params.clone()).unwrap().0;
        assert!(
            store.preparation(&handle).unwrap().is_none(),
            "no preparation row, so an identical retry starts cleanly"
        );
        let posted = submit(store.clone(), json!(2), params, |_| async {
            Response::ok(Value::Null, json!({"job_id":"in-budget"}))
        })
        .await
        .result
        .unwrap();
        assert_eq!(posted["job_id"], "in-budget");
    }

    #[test]
    fn many_inputs_fingerprint_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<_> = (0..20)
            .map(|i| {
                let source = dir.path().join(format!("f{i}"));
                std::fs::write(&source, format!("body {i}")).unwrap();
                json!({"source":source,"path":format!("f{i}")})
            })
            .collect();
        let state = input_state(
            &json!({"inputs":files}),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        let expected: Vec<Value> = (0..20)
            .map(|i| json!(hex::encode(Sha256::digest(format!("body {i}").as_bytes()))))
            .collect();
        assert_eq!(state, Value::Array(expected));
        let missing = input_state(
            &json!({"inputs":[{"source":dir.path().join("absent"),"path":"a"}]}),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(missing, Ok(json!([null])));
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
        let message = retry.error.unwrap().message;
        assert!(
            message.contains("offer-1") && message.contains("deduplicated"),
            "the repeat names the possibly-live offer (#1096 advisor F2): {message}"
        );
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
