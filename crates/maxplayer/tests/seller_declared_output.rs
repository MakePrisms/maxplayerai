//! #686 — the buyer's DECLARED OUTPUT TYPE reaches the hired agent.
//!
//! The offer's `["output", …]` tag is MANDATORY on ingest (`gateway::parse_offer` refuses an offer
//! without it) and is a MIME / output type — `text/plain`, `application/json`. It used to stop at the
//! parsed offer: it was never journaled with the other offer facts and never stated to the agent, so
//! the one party who has to act on it was the one party never told.
//!
//! ── Why this tooth lives in an INTEGRATION TEST of the CLI crate ──────────────────────────────────
//!
//! The code it exercises is in `maxplayer-core`, but that crate's `seller_node` / `seller_exec`
//! modules are gated behind its `wallet` feature, which is OFF by default — so `cargo test -p
//! maxplayer-core --locked --offline`, one of this repo's declared checks, does not compile (let
//! alone run) their in-crate tests. The `maxplayer` crate sets `default = ["wallet"]`, and its
//! `wallet` turns on `maxplayer-core/wallet`, so `cargo test -p maxplayer --locked --offline` — also
//! a declared check — builds this path and runs this file. The seller-side unit teeth in
//! `maxplayer-core` (store round-trip, migration, prompt composition, wire→row mapping) stay where
//! they belong and run in CI's `--features …,wallet` jobs; this one guarantees the seam is covered by
//! the declared check set itself.
//!
//! `crates/maxplayer/src/sell.rs` would be the topical home, but `mod sell` is `#[cfg(feature =
//! "acp")]` and `acp` is not a default feature either — a test there is invisible to the same checks.
//!
//! ── The bites, each applied ALONE and measured (this file, at this commit) ────────────────────────
//!   - stop persisting it (`offer.output` → `None` in `store::record_offer`'s params)  [persist]
//!     ⇒ `the_declared_output_type_is_persisted_and_reaches_the_agents_prompt` FAILS, the other two
//!       pass — it is the reopened row that goes empty, not the prompt composition.
//!   - the call site drops it (`None` for the declared output in `seller_node::run::job_prompt`)
//!     ⇒ ALL THREE FAIL.
//!   - the prompt drops it (remove `{output_section}` from `compose_agent_prompt`'s format string)
//!     ⇒ ALL THREE FAIL.
//!
//! The fourth link, WIRE → ROW (`output: Some(offer.output.clone())` in `seller_node::run::offer_row`),
//! is not reachable from here — that mapping is private to `maxplayer-core`. Biting it turns
//! `seller_node::run::tests::the_declared_output_type_survives_wire_to_row_to_prompt_across_a_restart`
//! red (measured the same way), which runs in CI's wallet-featured job.

use maxplayer_core::seller_node::run::job_prompt;
use maxplayer_core::seller_node::store::{Offer, SellerStore};

const GIT_REMOTE: &str = "https://relay.example/git/abc.git";
const DEADLINE: u64 = 2_000_000_000;

fn offer(output: &str) -> Offer {
    Offer {
        offer_id: "a".repeat(64),
        buyer_pubkey: "b".repeat(64),
        amount_sats: 21,
        unit: "sat".to_owned(),
        task: "write the report".to_owned(),
        deadline_unix: DEADLINE as i64,
        targeted: true,
        requested_agent: None,
        output: Some(output.to_owned()),
    }
}

fn temp_root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "maxplayer-686-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

/// The whole seam in the order the daemon walks it: stored offer → RESTART → the exact prompt call
/// `execute_job` makes.
///
/// The store is reopened between the write and the read on purpose. Execution can be a restart away
/// from the claim, so a resumed job re-reads its facts from the store — a field that lived only in
/// memory is gone for that job permanently. That is precisely why the type is persisted rather than
/// only plumbed.
#[test]
fn the_declared_output_type_is_persisted_and_reaches_the_agents_prompt() {
    let root = temp_root("declared-output");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mk temp root");
    let db = root.join("seller.sqlite");

    // Claim time: the offer's facts are journaled, the declared output type among them.
    {
        let store = SellerStore::open(&db).expect("open store");
        store
            .record_offer(&offer("application/json"), 1)
            .expect("record offer");
    }

    // …the process dies here. A fresh handle over the same file is all the resumed node has.
    let store = SellerStore::open(&db).expect("reopen store");
    let resumed = store
        .offer_row(&"a".repeat(64))
        .expect("offer row")
        .expect("the offer survives the restart");
    assert_eq!(
        resumed.output.as_deref(),
        Some("application/json"),
        "the declared output type must be PERSISTED — a resumed job reads its facts from the store"
    );

    let prompt = job_prompt(&resumed, GIT_REMOTE, DEADLINE);
    assert!(
        prompt.contains("application/json"),
        "the buyer's declared output type must reach the hired agent's prompt: {prompt}"
    );
    assert!(
        prompt.contains("DECLARED OUTPUT TYPE:"),
        "stated as the buyer's declared output type, so the agent knows whose fact it is: {prompt}"
    );
    // The buyer's task still comes FIRST — the preamble never pushes it down.
    assert!(prompt.starts_with("write the report"), "task first: {prompt}");

    let _ = std::fs::remove_dir_all(&root);
}

/// A VALUE, not fixed prose. Every other input is held identical and only the declared type varies:
/// each prompt must carry its own type and NOT the other's, which a hardcoded line cannot satisfy.
#[test]
fn the_prompt_carries_this_offers_own_output_type_and_not_another_offers() {
    let json = job_prompt(&offer("application/json"), GIT_REMOTE, DEADLINE);
    let plain = job_prompt(&offer("text/plain"), GIT_REMOTE, DEADLINE);

    assert!(json.contains("application/json"), "{json}");
    assert!(!json.contains("text/plain"), "not the other type: {json}");
    assert!(plain.contains("text/plain"), "{plain}");
    assert!(
        !plain.contains("application/json"),
        "not the other type: {plain}"
    );
    assert_ne!(
        json, plain,
        "the declared output type must reach the prompt, not be dropped on the way"
    );
}

/// ABSENT ⇒ SILENT. A row recorded before the column existed declares no output type, and inventing
/// a default would state a fact in the prompt that no buyer ever gave.
///
/// ⛔ Also the scope line: the prompt STATES the type and says the task wins if the two disagree.
/// Nothing refuses or penalises a delivery whose format does not match — enforcement is a money-path
/// decision with its own blast radius and is deliberately NOT part of this change.
#[test]
fn an_offer_with_no_declared_output_type_states_none_and_nothing_is_enforced() {
    let mut none = offer("text/plain");
    none.output = None;
    let prompt = job_prompt(&none, GIT_REMOTE, DEADLINE);
    assert!(
        !prompt.contains("DECLARED OUTPUT TYPE"),
        "no declared type ⇒ nothing stated: {prompt}"
    );

    let stated = job_prompt(&offer("application/json"), GIT_REMOTE, DEADLINE);
    let lower = stated.to_lowercase();
    for banned in ["refuse", "decline", "reject", "must not deliver", "penalt"] {
        assert!(
            !lower.contains(banned),
            "stating the output type must not become an enforcement threat (found {banned:?}): \
             {stated}"
        );
    }
    assert!(
        stated.contains("The task above wins where the two disagree."),
        "the buyer's task, not the tag, stays authoritative: {stated}"
    );
}
