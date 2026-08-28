//! #828 — the seller's OPERATOR-AUTHORED context reaches the hired agent (read-on-start seam).
//!
//! `seller_memory` shipped whole and orphaned: the module, its `[seller_memory]` config and the
//! injection site inside `compose_agent_prompt` all existed, but `seller_node::run::job_prompt` —
//! "the ONE place a stored offer row becomes the hired agent's prompt" — passed a hardcoded `None`
//! for `memory_section`. So every job started blank no matter what an operator had written. That
//! single literal was the whole of the gap; this file is the tooth that keeps it closed.
//!
//! ── Why this tooth lives in an INTEGRATION TEST of the CLI crate ──────────────────────────────────
//!
//! Same reason as `seller_declared_output.rs`, and it is the reason `job_prompt` is `pub` at all
//! (`seller_node/run.rs`): the code under test is in `maxplayer-core`, but its `seller_node` module
//! is gated behind that crate's `wallet` feature, which is OFF by default. `cargo test -p
//! maxplayer-core --locked --offline` — a declared check — does not compile these tests, let alone
//! run them. The `maxplayer` crate has `default = ["wallet"]`, so `cargo test -p maxplayer --locked
//! --offline` builds this path and runs this file. A tooth for this seam that lived only in
//! `maxplayer-core` would be invisible to the declared check set.
//!
//! ── The bites, each applied ALONE and measured (this file, at this commit) ────────────────────────
//!   - restore the hardcoded `None` at `job_prompt`'s `compose_agent_prompt` call
//!     ⇒ `the_operator_authored_memory_index_reaches_the_agents_prompt` AND
//!       `the_golden_invariant_holds_no_memory_is_byte_identical` FAIL; the other three pass.
//!   - make `job_memory_section` ignore `memory_enabled`
//!     ⇒ `a_disabled_config_injects_nothing` FAILS alone.
//!   - make `job_memory_section` propagate the `InvalidData` error instead of degrading
//!     ⇒ `an_over_budget_index_degrades_instead_of_blocking_the_job` FAILS alone (it panics).
//!   - `None => format!("{base}\n\n")` in `compose_agent_prompt`'s `match memory_section`
//!     ⇒ `the_golden_invariant_holds_no_memory_is_byte_identical` FAILS alone.
//!
//! That last bite is why this file carries two assertions that do not route through `job_prompt(…,
//! None)`. The first version of the golden test derived its baseline that way and stayed GREEN under
//! the bite — a self-referential baseline moves with the arm it is meant to police. Measured, then
//! fixed, then re-measured red.

use maxplayer_core::home::SellerMemoryConfig;
use maxplayer_core::seller_memory::{MAX_MEMORY_INDEX_BYTES, MEMORY_INDEX_FILE, memory_dir};
use maxplayer_core::seller_node::run::{job_memory_section, job_prompt};
use maxplayer_core::seller_node::store::Offer;

const GIT_REMOTE: &str = "https://relay.example/git/abc.git";
const DEADLINE: u64 = 2_000_000_000;

fn temp_root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "maxplayer-828-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

/// A home root with `memory/MEMORY.md` written to `index`. Returns the root.
fn home_with_index(label: &str, index: &str) -> std::path::PathBuf {
    let root = temp_root(label);
    let _ = std::fs::remove_dir_all(&root);
    let dir = memory_dir(&root);
    std::fs::create_dir_all(&dir).expect("mk memory dir");
    std::fs::write(dir.join(MEMORY_INDEX_FILE), index).expect("write index");
    root
}

fn offer() -> Offer {
    Offer {
        offer_id: "a".repeat(64),
        buyer_pubkey: "b".repeat(64),
        amount_sats: 21,
        unit: "sat".to_owned(),
        task: "apply the brand guidelines to the landing page".to_owned(),
        deadline_unix: DEADLINE as i64,
        targeted: true,
        requested_agent: None,
        output: Some("text/plain".to_owned()),
    }
}

/// THE TOOTH (#828). An operator writes brand guidelines into `MEMORY.md`; the hired agent is told.
///
/// This is the whole feature bob asked for, stated as one assertion: context an operator authored
/// loads with the job. The index CONTENT is inlined into the prompt text, so it arrives even for a
/// containerized seller, whose mount namespace cannot reach the host memory directory at all.
#[test]
fn the_operator_authored_memory_index_reaches_the_agents_prompt() {
    let brand = "# Memory\n\nAcme brand: always set headings in Söhne, never centre body copy.\n";
    let root = home_with_index("reaches-prompt", brand);

    let section = job_memory_section(&root, &SellerMemoryConfig::default());
    let section = section.expect("a non-empty index renders a read-on-start section");
    let prompt = job_prompt(&offer(), GIT_REMOTE, DEADLINE, Some(section.as_str()));

    assert!(
        prompt.contains("Söhne, never centre body copy"),
        "the operator's own words must reach the agent verbatim: {prompt}"
    );
    assert!(
        prompt.contains("SELLER MEMORY"),
        "the index arrives inside the framed read-on-start section: {prompt}"
    );
    // The task is still the prompt's subject — memory is added context, never a replacement.
    assert!(prompt.contains("apply the brand guidelines to the landing page"));

    let _ = std::fs::remove_dir_all(&root);
}

/// The golden invariant (`seller_exec.rs`): `None` ⇒ byte-identical to the memory-disabled prompt.
///
/// Wiring the read path must not change ONE BYTE for a seller that has no memory. `memory_enabled`
/// defaults to TRUE, so this is what makes the default safe: absent index and blank index both
/// render nothing, and nothing is what shipped before.
#[test]
fn the_golden_invariant_holds_no_memory_is_byte_identical() {
    let baseline = job_prompt(&offer(), GIT_REMOTE, DEADLINE, None);
    assert!(
        !baseline.contains("SELLER MEMORY"),
        "control: the memory-off prompt must not already carry the section this test looks for"
    );

    // (a) No memory directory at all — the state every seller is in today.
    let bare = temp_root("no-dir");
    let _ = std::fs::remove_dir_all(&bare);
    let none_section = job_memory_section(&bare, &SellerMemoryConfig::default());
    assert_eq!(none_section, None);
    assert_eq!(
        job_prompt(&offer(), GIT_REMOTE, DEADLINE, none_section.as_deref()),
        baseline,
        "absent index ⇒ byte-identical to the memory-disabled prompt"
    );

    // (b) The directory exists but the index is blank/whitespace — seeded, never written.
    let blank = home_with_index("blank-index", "   \n\n\t\n");
    let blank_section = job_memory_section(&blank, &SellerMemoryConfig::default());
    assert_eq!(blank_section, None);
    assert_eq!(
        job_prompt(&offer(), GIT_REMOTE, DEADLINE, blank_section.as_deref()),
        baseline,
        "blank index ⇒ byte-identical to the memory-disabled prompt"
    );

    // The load-bearing half, and the reason the two assertions above are not vacuous: a prompt WITH
    // memory differs from the baseline, and it differs only by APPENDING. The memory-off output
    // survives byte-for-byte as its prefix, which is exactly what the golden invariant claims.
    let real = home_with_index("golden-real", "# Memory\n\nhouse style: sentence case headings\n");
    let section = job_memory_section(&real, &SellerMemoryConfig::default())
        .expect("control: a non-empty index must render a section, or the test above proves nothing");
    let with_memory = job_prompt(&offer(), GIT_REMOTE, DEADLINE, Some(section.as_str()));
    assert_ne!(with_memory, baseline, "memory must actually change the prompt");
    assert!(
        with_memory.starts_with(&baseline),
        "memory is APPENDED — the memory-disabled prompt is preserved byte-for-byte as the prefix"
    );

    // ── The two assertions that are NOT self-referential ──────────────────────────────────────
    //
    // Everything above derives its baseline by calling `job_prompt` with `None`, so all of it moves
    // together if the `None` arm itself changes: MEASURED — appending a separator there
    // (`None => format!("{base}\n\n")`) left all five tests GREEN. A test that cannot see the
    // regression it is named for is worse than no test, so the invariant needs an anchor that does
    // not pass through the arm under test.
    //
    // (i) `None` appends NOTHING, stated as an observable property of the output rather than as a
    //     comparison against itself: the composed prompt ends where its own last sentence ends.
    assert_eq!(
        baseline,
        baseline.trim_end(),
        "None must add nothing at all — not even a trailing separator"
    );
    // (ii) The join is exactly one blank line, pinned independently of both match arms.
    assert_eq!(
        with_memory,
        format!("{baseline}\n\n{section}"),
        "Some(section) is base + one blank line + section, and base is the memory-off prompt"
    );

    let _ = std::fs::remove_dir_all(&blank);
    let _ = std::fs::remove_dir_all(&real);
}

/// `memory_enabled = false` reads nothing, even with a perfectly good index on disk.
#[test]
fn a_disabled_config_injects_nothing() {
    let root = home_with_index("disabled", "# Memory\n\nreal content that must not be read\n");
    let config = SellerMemoryConfig { memory_enabled: false, ..SellerMemoryConfig::default() };

    assert_eq!(job_memory_section(&root, &config), None);

    let _ = std::fs::remove_dir_all(&root);
}

/// THE SEAM MUST NEVER BLOCK A JOB. An index over `MAX_MEMORY_INDEX_BYTES` is REFUSED with
/// `InvalidData` by `read_on_start_section` — deliberately, so a runaway file cannot bloat every
/// prompt. That refusal is an `io::Error`, and an error on this path must NOT propagate: the job
/// would otherwise fail over diagnostic context that never feeds the pay gate, the journal or the
/// receipt bind. It degrades to a normal, memory-free job instead.
#[test]
fn an_over_budget_index_degrades_instead_of_blocking_the_job() {
    let runaway = "x".repeat(MAX_MEMORY_INDEX_BYTES + 1);
    let root = home_with_index("over-budget", &runaway);

    // Control: one byte under the bound DOES inject, so the refusal below is the size bound doing
    // its job and not the read silently failing for some unrelated reason.
    let ok_root = home_with_index("at-budget", &"y".repeat(MAX_MEMORY_INDEX_BYTES - 1));
    assert!(
        job_memory_section(&ok_root, &SellerMemoryConfig::default()).is_some(),
        "control: an index just under the bound must still be injected"
    );

    // No panic, no error type — just no memory.
    let section = job_memory_section(&root, &SellerMemoryConfig::default());
    assert_eq!(
        section, None,
        "an over-budget index degrades to no memory rather than failing the job"
    );
    // And the job it would have run is exactly the job that runs today.
    assert_eq!(
        job_prompt(&offer(), GIT_REMOTE, DEADLINE, section.as_deref()),
        job_prompt(&offer(), GIT_REMOTE, DEADLINE, None),
        "the degraded job is byte-identical to a normal memory-free job"
    );

    let _ = std::fs::remove_dir_all(&ok_root);

    let _ = std::fs::remove_dir_all(&root);
}

/// THE INERT TEETH (#828). Wiring the read path must not turn the feature ON for anyone.
///
/// `memory_enabled` defaults to TRUE, so this wire runs for every seller at once. It is inert only
/// because a seller with no `memory/MEMORY.md` renders nothing — and that safety is one accidental
/// `ensure_memory_dir` call away from gone: `ensure_memory_dir` SEEDS A NON-EMPTY index (it links
/// `operator-notes.md`), so calling it from this path would flip every existing seller from inert to
/// injecting on its next job, with no config change and no operator ever having written a word.
///
/// So the assertion is not only "the prompt is unchanged" — it is that the read path CREATES
/// NOTHING. Creation stays an operator act. A prose bound would rot; this one fails.
#[test]
fn the_read_wire_never_creates_the_memory_directory() {
    let root = temp_root("never-creates");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mk home root");
    let dir = memory_dir(&root);
    assert!(!dir.exists(), "precondition: the seller has no memory directory");

    let section = job_memory_section(&root, &SellerMemoryConfig::default());
    assert_eq!(section, None, "no index ⇒ nothing to inject");

    // The whole of the safety: the read path did not seed anything on its way past.
    assert!(
        !dir.exists(),
        "the read wire must never create memory/ — ensure_memory_dir seeds a NON-EMPTY index, and \
         creating one here would silently enable injection for every existing seller"
    );
    assert!(
        !dir.join(MEMORY_INDEX_FILE).exists(),
        "and it must never write an index"
    );

    // And the prompt an existing seller gets is the one it got before this feature was wired.
    assert_eq!(
        job_prompt(&offer(), GIT_REMOTE, DEADLINE, section.as_deref()),
        job_prompt(&offer(), GIT_REMOTE, DEADLINE, None),
        "an untouched seller's prompt is byte-identical to the memory-off output"
    );

    let _ = std::fs::remove_dir_all(&root);
}
