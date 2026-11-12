//! What a SETTLED `get_job.settlement` carries so an agent never needs `collect` to read it back:
//! `commit_oid`, `files`, `file_count`, `pay {state, amount_sats, attempt_id}` and the seller-claimed
//! `agent_used`/`model_used`, the same facts a `collect` response returns.
//!
//! Read-only and local: the accept-bind (`jobs/<id>.json`), the free-collect record
//! (`collects/<id>.json`), the buyer delivery store (the bare repo the pay path retained the
//! delivery in) and the payment journal (`payment-journal/*.jsonl`). It never accepts, pays,
//! materializes or re-settles anything, and nothing reads it back to decide anything. Session turns
//! ≥ 2 are read under their follow-up id, which keys their bind, record and journal.
use super::*;

/// Merge the settled read-back into `hint` when `hint.state` is `settled`. Every field is
/// best-effort: an unreadable source omits its field, never fails the `get_job`.
pub(super) fn enrich(home: &MaxplayerHome, store: &BuyerStore, job_id: &str, hint: &mut Value) {
    if hint["state"] != json!("settled") {
        return;
    }
    let turn = hint["turn"].as_u64().unwrap_or(1);
    let key = if turn >= 2 {
        match store.session_turns(job_id) {
            Ok(turns) => match turns
                .into_iter()
                .find(|t| u64::from(t.turn) == turn && t.state == "collected")
            {
                Some(row) => row.followup_id,
                None => return,
            },
            Err(_) => return,
        }
    } else {
        job_id.to_owned()
    };
    let Some(details) = details(home, &key) else {
        return;
    };
    if let (Value::Object(hint), Value::Object(details)) = (hint, details) {
        hint.extend(details);
    }
}

/// The settled facts for one bind key (the job id, or a turn's follow-up id). `None` when no
/// accept-bind exists under `key` (a settle that never accepted has nothing to report).
pub(super) fn details(home: &MaxplayerHome, key: &str) -> Option<Value> {
    let bind = job_lifecycle::load_accepted_bind(home, key)
        .ok()
        .flatten()?;
    let free_record = read_free_record(home, key);
    let files = if let Some(files) = free_record
        .as_ref()
        .and_then(|record| record["files"].as_array().cloned())
    {
        // A record an older build wrote names agent-control files as delivered; report them under
        // the inert names materialize now writes (the mapping is idempotent for newer records).
        let mut files = files
            .iter()
            .filter_map(|f| f.as_str().map(collect::materialized_path))
            .collect::<Vec<_>>();
        files.sort();
        files.dedup();
        Some(files)
    } else if collect::bind_is_inline(&bind) {
        Some(vec![collect::INLINE_ANSWER_FILENAME.to_owned()])
    } else {
        delivery_files(home, &bind.commit_oid).ok()
    };
    let pay = if bind.payment_mode.is_free() {
        json!({
            "state": crate::gateway::PaymentMode::None.as_wire(),
            "amount_sats": 0,
            "attempt_id": Value::Null,
        })
    } else {
        let state = journal_state(home, key);
        json!({
            // The folded payment-journal state (`closed` once the receipt is published), or null
            // when no journal for this key could be read.
            "state": state.as_ref().and_then(|s| serde_json::to_value(s).ok()).map(|v| v["state"].clone()).unwrap_or(Value::Null),
            "amount_sats": bind.amount_sats,
            "attempt_id": state.as_ref().map(|s| s.attempt_id().as_str().to_owned()),
        })
    };
    let mut value = json!({
        "commit_oid": bind.commit_oid,
        "pay": pay,
        "agent_used": bind.agent_used,
        "model_used": bind.model_used,
    });
    if let Some(files) = files {
        value["file_count"] = json!(files.len());
        value["files"] = json!(files);
    }
    Some(value)
}

fn read_free_record(home: &MaxplayerHome, key: &str) -> Option<Value> {
    let raw = std::fs::read(collect::free_collect_record_path(home, key)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// The sorted paths `collect::materialize_delivery` writes for the delivered commit's tree in the
/// buyer store (agent-control names already carry `collect::INERT_SUFFIX`), read without writing
/// anything.
fn delivery_files(home: &MaxplayerHome, commit_oid: &str) -> Result<Vec<String>, String> {
    let repo = git2::Repository::open_bare(collect::delivery_store_path(home))
        .map_err(|e| e.to_string())?;
    let store_ref = crate::delivery_git::PayPathDeliveryVerifier::store_ref_for(commit_oid);
    let tree = repo
        .revparse_single(&store_ref)
        .or_else(|_| repo.revparse_single(commit_oid))
        .and_then(|object| object.peel_to_commit())
        .and_then(|commit| commit.tree())
        .map_err(|e| e.to_string())?;
    // Bounded: a tree is a DAG, and a few shared subtrees can name 2^N paths (#1290 fable M2).
    // Same cap as collect's pre-pay check, which refuses such a delivery outright.
    const MAX_LISTED_ENTRIES: usize = 100_000;
    let mut files = Vec::new();
    let mut visited = 0usize;
    let walked = tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        visited += 1;
        if visited > MAX_LISTED_ENTRIES {
            return git2::TreeWalkResult::Abort;
        }
        if entry.kind() == Some(git2::ObjectType::Blob)
            && let Ok(name) = entry.name()
        {
            files.push(collect::materialized_path(&format!("{root}{name}")));
        }
        git2::TreeWalkResult::Ok
    });
    if visited > MAX_LISTED_ENTRIES {
        return Err(format!("delivery has more than {MAX_LISTED_ENTRIES} entries; not listed"));
    }
    walked.map_err(|e| e.to_string())?;
    files.sort();
    // A delivery settled by an older build may hold both `AGENTS.md` and `AGENTS.md.delivered`;
    // both map to one written name.
    files.dedup();
    Ok(files)
}

/// The most advanced folded payment state among the journals whose payload id is `key` (several
/// attempts can exist for one key; a `Closed` one is never masked by an earlier `Intent`).
fn journal_state(home: &MaxplayerHome, key: &str) -> Option<PaymentState> {
    fn rank(state: &PaymentState) -> u8 {
        match state {
            PaymentState::Intent { .. } => 0,
            PaymentState::Locked { .. } => 1,
            PaymentState::Sent { .. } => 2,
            PaymentState::ReceiptPublished { .. } => 3,
            PaymentState::Closed { .. } => 4,
        }
    }
    let entries = std::fs::read_dir(home.root.join("payment-journal")).ok()?;
    let mut best: Option<PaymentState> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(records) = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<PaymentRecord>)
            .collect::<Result<Vec<_>, _>>()
        else {
            continue;
        };
        let Some(first) = records.first() else {
            continue;
        };
        if first.key.payload_id() != key {
            continue;
        }
        if let Ok(Some(state)) = PaymentMachine::fold(&first.key, &records)
            && best.as_ref().is_none_or(|b| rank(&state) > rank(b))
        {
            best = Some(state);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery_git::PayPathDeliveryVerifier;
    use crate::payment::{
        DeliveryIntegrityHash, JobHash, JobId, PaymentKey, PaymentTerms, ReceiptRecord, ResultId,
    };
    use std::str::FromStr;

    fn home() -> (tempfile::TempDir, MaxplayerHome) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = crate::home::bootstrap(dir.path()).expect("home");
        (dir, home)
    }

    fn bind(
        job_id: &str,
        followup: Option<&str>,
        free: bool,
        inline: bool,
    ) -> job_lifecycle::AcceptedBind {
        job_lifecycle::AcceptedBind {
            followup_id: followup.map(str::to_owned),
            private_evidence: None,
            delivery_kind: inline.then(|| "inline".to_owned()),
            inline_answer: inline.then(|| "the answer".to_owned()),
            payment_mode: if free {
                crate::gateway::PaymentMode::None
            } else {
                crate::gateway::PaymentMode::Sat
            },
            job_id: job_id.to_owned(),
            claim_id: "c".repeat(64),
            result_id: "r".repeat(64),
            seller_pubkey: "5".repeat(64),
            commit_oid: "d".repeat(40),
            repo: "https://example.invalid/repo.git".to_owned(),
            branch: "main".to_owned(),
            job_hash: "h".repeat(64),
            amount_sats: if free { 0 } else { 100 },
            accept_event_id: "b".repeat(64),
            accepted_at: 3,
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: vec![],
            funding_mint: None,
            delivery_mint: None,
            agent_used: Some("claude-agent-acp".to_owned()),
            model_used: None,
            contribution: None,
        }
    }

    fn write_bind(home: &MaxplayerHome, bind: &job_lifecycle::AcceptedBind) {
        let key = bind.followup_id.as_deref().unwrap_or(&bind.job_id);
        let dir = home.root.join("jobs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{key}.json")),
            serde_json::to_string(bind).unwrap(),
        )
        .unwrap();
    }

    /// A bare buyer store holding a three-file commit (one an agent-control name, P1) under its
    /// retention ref; returns its oid.
    fn seed_store(home: &MaxplayerHome) -> String {
        let repo = git2::Repository::init_bare(collect::delivery_store_path(home)).unwrap();
        let readme = repo.blob(b"# delivered\n").unwrap();
        let lib = repo.blob(b"pub fn delivered() {}\n").unwrap();
        let mut sub = repo.treebuilder(None).unwrap();
        sub.insert("lib.rs", lib, 0o100644).unwrap();
        let sub = sub.write().unwrap();
        let mut top = repo.treebuilder(None).unwrap();
        top.insert("README.md", readme, 0o100644).unwrap();
        top.insert("AGENTS.md", readme, 0o100644).unwrap();
        top.insert("src", sub, 0o040000).unwrap();
        let tree = repo.find_tree(top.write().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@e").unwrap();
        let oid = repo
            .commit(None, &sig, &sig, "delivery", &tree, &[])
            .unwrap();
        let hex = oid.to_string();
        repo.reference(
            &PayPathDeliveryVerifier::store_ref_for(&hex),
            oid,
            true,
            "retain",
        )
        .unwrap();
        hex
    }

    /// A payment journal for payload `key` folded through every state to `Closed`.
    fn write_closed_journal(home: &MaxplayerHome, job_id: &str, followup: Option<&str>) -> String {
        let cashu_pk = cashu::SecretKey::from_slice(&[7u8; 32])
            .unwrap()
            .public_key();
        let nostr_pk = nostr_sdk::PublicKey::from_hex(&cashu_pk.to_string()[2..]).unwrap();
        let terms = PaymentTerms::new(
            cashu::MintUrl::from_str("https://testnut.cashu.space").unwrap(),
            cashu::Amount::from(100),
            cashu::CurrencyUnit::Sat,
            nostr_pk,
            cashu_pk,
        );
        let key = PaymentKey::new(
            JobId::new(job_id).unwrap(),
            ResultId::new("result").unwrap(),
            DeliveryIntegrityHash::from_hex("11".repeat(32)).unwrap(),
            JobHash::from_hex("22".repeat(32)).unwrap(),
            &terms,
            None,
        )
        .with_payment_id(followup.map(str::to_owned));
        let attempt_id = key.attempt_id();
        let receipt = ReceiptRecord {
            receipt_id: "e".repeat(64),
            receipt_kind: 3400,
        };
        let states = [
            PaymentState::Intent {
                attempt_id: attempt_id.clone(),
            },
            PaymentState::Locked {
                attempt_id: attempt_id.clone(),
            },
            PaymentState::Sent {
                attempt_id: attempt_id.clone(),
                payment: crate::payment_send::PaymentSent {
                    payment_id: "p".into(),
                    relay_success: vec![],
                    relay_failed: vec![],
                },
            },
            PaymentState::ReceiptPublished {
                attempt_id: attempt_id.clone(),
                receipt: receipt.clone(),
            },
            PaymentState::Closed {
                attempt_id: attempt_id.clone(),
                receipt,
            },
        ];
        let dir = home.root.join("payment-journal");
        std::fs::create_dir_all(&dir).unwrap();
        let body: String = states
            .into_iter()
            .map(|value| {
                serde_json::to_string(&PaymentRecord {
                    key: key.clone(),
                    value,
                })
                .unwrap()
                    + "\n"
            })
            .collect();
        std::fs::write(dir.join(format!("{}.jsonl", attempt_id.as_str())), body).unwrap();
        attempt_id.as_str().to_owned()
    }

    fn settled(turn: u32) -> Value {
        json!({"mode": "auto", "state": "settled", "turn": turn, "note": "n"})
    }

    // A paid git delivery: the read-back carries the bind's commit, the store tree's file list and
    // the journal's Closed pay state, without creating results/ or touching the ledger.
    #[test]
    fn a_settled_paid_delivery_reads_back_commit_files_and_pay() {
        let (_dir, home) = home();
        let store = BuyerStore::open(home.root.join("buyer.sqlite")).unwrap();
        let job = "a".repeat(64);
        let mut paid = bind(&job, None, false, false);
        paid.commit_oid = seed_store(&home);
        write_bind(&home, &paid);
        let attempt = write_closed_journal(&home, &job, None);

        let mut hint = settled(1);
        enrich(&home, &store, &job, &mut hint);
        assert_eq!(hint["commit_oid"], json!(paid.commit_oid));
        // P1: the list names what materialize writes, so an agent-control file reads back inert.
        assert_eq!(
            hint["files"],
            json!(["AGENTS.md.delivered", "README.md", "src/lib.rs"])
        );
        assert_eq!(hint["file_count"], json!(3));
        assert_eq!(
            hint["pay"],
            json!({"state": "closed", "amount_sats": 100, "attempt_id": attempt})
        );
        assert_eq!(hint["agent_used"], json!("claude-agent-acp"));
        assert_eq!(hint["note"], json!("n"), "the hint's own fields are kept");
        assert!(
            !home.root.join("results").exists(),
            "a read never materializes"
        );
        assert!(
            store.reservation(&job).unwrap().is_none(),
            "a read never touches the ledger"
        );
    }

    // A free inline delivery: pay state none, amount 0, files from the free-collect record.
    #[test]
    fn a_settled_free_delivery_reads_back_the_free_record() {
        let (_dir, home) = home();
        let store = BuyerStore::open(home.root.join("buyer.sqlite")).unwrap();
        let job = "b".repeat(64);
        write_bind(&home, &bind(&job, None, true, true));
        let record = collect::free_collect_record_path(&home, &job);
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        std::fs::write(
            &record,
            json!({"files": ["answer.txt"], "payment": "none"}).to_string(),
        )
        .unwrap();

        let mut hint = settled(1);
        enrich(&home, &store, &job, &mut hint);
        assert_eq!(hint["files"], json!(["answer.txt"]));
        assert_eq!(hint["file_count"], json!(1));
        assert_eq!(
            hint["pay"],
            json!({"state": "none", "amount_sats": 0, "attempt_id": null})
        );
    }

    // P1: a free record written by an older build lists agent-control files under their original
    // names; the read-back reports the inert names materialize writes now, once each.
    #[test]
    fn a_free_record_from_an_older_build_reads_back_inert_names() {
        let (_dir, home) = home();
        let store = BuyerStore::open(home.root.join("buyer.sqlite")).unwrap();
        let job = "d".repeat(64);
        write_bind(&home, &bind(&job, None, false, true));
        let record = collect::free_collect_record_path(&home, &job);
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        std::fs::write(
            &record,
            json!({
                "files": ["README.md", "AGENTS.md", "AGENTS.md.delivered", ".claude/settings.json"],
                "payment": "none",
            })
            .to_string(),
        )
        .unwrap();

        let mut hint = settled(1);
        enrich(&home, &store, &job, &mut hint);
        assert_eq!(
            hint["files"],
            json!([".claude.delivered/settings.json", "AGENTS.md.delivered", "README.md"])
        );
        assert_eq!(hint["file_count"], json!(3));
    }

    // A session turn ≥ 2 is read under its follow-up id (bind, journal), never turn 1's.
    #[test]
    fn a_settled_session_turn_reads_its_own_bind_and_journal() {
        let (_dir, home) = home();
        let store = BuyerStore::open(home.root.join("buyer.sqlite")).unwrap();
        let job = "c".repeat(64);
        let followup = "f".repeat(64);
        write_bind(&home, &bind(&job, None, false, true));
        let mut turn2 = bind(&job, Some(&followup), false, true);
        turn2.commit_oid = "9".repeat(40);
        write_bind(&home, &turn2);
        let attempt = write_closed_journal(&home, &job, Some(&followup));
        store
            .put_turn(&followup, &job, 2, "{}", 100, 99, 1)
            .unwrap();
        store
            .set_turn_state(&followup, "collected", Some("r2"), 2)
            .unwrap();

        let mut hint = settled(2);
        enrich(&home, &store, &job, &mut hint);
        assert_eq!(hint["commit_oid"], json!("9".repeat(40)));
        assert_eq!(hint["files"], json!(["answer.txt"]));
        assert_eq!(hint["pay"]["attempt_id"], json!(attempt));
        assert_eq!(hint["pay"]["state"], json!("closed"));

        // Turn 1's read is unaffected by turn 2's journal (keyed by the follow-up id).
        let mut one = settled(1);
        enrich(&home, &store, &job, &mut one);
        assert_eq!(one["commit_oid"], json!("d".repeat(40)));
        assert_eq!(one["pay"]["state"], Value::Null);
    }

    // Anything but settled is left exactly as the hint said; a settled hint with no bind gains nothing.
    #[test]
    fn only_a_settled_hint_with_a_bind_is_enriched() {
        let (_dir, home) = home();
        let store = BuyerStore::open(home.root.join("buyer.sqlite")).unwrap();
        let job = "d".repeat(64);
        write_bind(&home, &bind(&job, None, false, true));
        let mut settling = json!({"mode": "auto", "state": "settling", "turn": 1, "note": "n"});
        let before = settling.clone();
        enrich(&home, &store, &job, &mut settling);
        assert_eq!(settling, before);

        let mut orphan = settled(1);
        enrich(&home, &store, &"e".repeat(64), &mut orphan);
        assert_eq!(orphan, settled(1));
    }
}
