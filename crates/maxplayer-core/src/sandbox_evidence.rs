//! Offline validation of a **saved** live containment matrix.
//!
//! The live gates in `tests/sandbox_netns_live.rs` need a docker daemon, a gVisor runtime and a
//! built sidecar image, so they are `#[ignore]`d and an ordinary `cargo test` run reports them as
//! *ignored*. That is the right call for replay — and it leaves a hole that a review found: the
//! named offline acceptance command was `cargo test -p maxplayer-core --features acp,wallet`, which
//! validates **nothing** about the live matrix. "0 passed, 11 ignored" and "the matrix is complete"
//! produce the same green.
//!
//! This module closes that hole. A live run writes down what it measured; this validates that
//! record offline, deterministically, with no daemon:
//!
//! * every required case is present, exactly once, with the outcome the matrix requires;
//! * a case with an empty or unrecognised outcome is **unscored**, and unscored fails;
//! * an id nobody requires is refused, because that is what a renamed or truncated record looks
//!   like;
//! * the record names the source commit, the artifact that produced it, and the host it ran on, so
//!   the matrix is attributable to something rather than floating free.
//!
//! It deliberately does **not** re-run anything. Replay stays separate, and a validator that shells
//! out to docker would be the live gate again under another name.
//!
//! ## The saved format
//!
//! Line oriented, because it has to be writable by hand from a log and diffable in review. `#`
//! starts a comment; blank lines are ignored. Header lines are `key=value`. Case lines start with
//! `case` and carry `key=value` fields:
//!
//! ```text
//! source_head=0721dcec44131cbd0298e035d48b1aa935567088
//! artifact_sha256=a852e2e4d57fa0ed4873a1c36a2a567b2336901dd05fb1ce9776d69fa9919714
//! artifact_path=target-linux/debug/deps/sandbox_netns_live-82001bf1221bb710
//! host=lima:gvisor-repro linux-6.8.0-134-generic aarch64 runsc-release-20260817.0
//! case id=integrated.denied.v4 outcome=refused log=raw/live-integrated.txt
//! ```

/// What a payload did, as recorded. The vocabulary is closed on purpose: a free-text outcome is an
/// unscored outcome, and "it failed" is exactly the ambiguity the live oracle exists to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The payload ran and its connection succeeded.
    Connected,
    /// The payload ran and its connection was refused or timed out.
    Refused,
    /// The payload's process never reached its first statement. Never containment evidence.
    NeverStarted,
    /// Preparation refused to launch anything at all (fail-closed).
    LaunchRefused,
}

impl Outcome {
    /// Parse the recorded word. `None` for anything else, which the validator reports as unscored
    /// rather than guessing a direction.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "connected" => Some(Self::Connected),
            "refused" => Some(Self::Refused),
            "never-started" => Some(Self::NeverStarted),
            "launch-refused" => Some(Self::LaunchRefused),
            _ => None,
        }
    }

    /// The word a record must carry for this outcome.
    pub fn word(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Refused => "refused",
            Self::NeverStarted => "never-started",
            Self::LaunchRefused => "launch-refused",
        }
    }
}

/// One case the matrix must contain, and the outcome that case is only evidence at.
///
/// The required outcome is part of the requirement because the direction is the evidence. A leg
/// recorded as `refused` when it must be `connected` is a positive control that did not hold, and a
/// matrix that accepts either has no positive controls.
#[derive(Debug, Clone, Copy)]
pub struct RequiredCase {
    pub id: &'static str,
    pub outcome: Outcome,
    /// What this case establishes, quoted in the failure so a missing entry says why it matters.
    pub establishes: &'static str,
}

/// The complete matrix. A live run that does not produce every one of these has not produced the
/// matrix, and this list is the only place that is written down.
///
/// Entries whose live legs are not yet authored are **deliberately present**: the requirement comes
/// from the review, not from what happens to exist, and a gate that only demands what is already
/// built cannot report an incomplete matrix. Each one fails as missing until a live run supplies it.
pub const REQUIRED_CASES: &[RequiredCase] = &[
    // ── The separate OUTPUT-only baseline arm ────────────────────────────────────────────────
    RequiredCase {
        id: "baseline.output-only.runsc.leak",
        outcome: Outcome::Connected,
        establishes: "the leak reproduces: with only the OUTPUT chain installed and verified, a \
                      gVisor payload reaches a denied destination",
    },
    RequiredCase {
        id: "baseline.output-only.runc.contained",
        outcome: Outcome::Refused,
        establishes: "the discriminator: the same OUTPUT policy does contain a runc payload, so the \
                      leak above is a runtime property and not a policy that never applied",
    },
    RequiredCase {
        id: "baseline.veth.runsc.contained",
        outcome: Outcome::Refused,
        establishes: "the same rendered policy on the veth stops the gVisor payload",
    },
    RequiredCase {
        id: "baseline.veth.runsc.allowed",
        outcome: Outcome::Connected,
        establishes: "the veth filters are not a blanket deny",
    },
    // ── The integrated production launch path ────────────────────────────────────────────────
    RequiredCase {
        id: "integrated.denied.v4",
        outcome: Outcome::Refused,
        establishes: "a job prepared and launched by production prepare_launch/launch is contained \
                      over IPv4 TCP",
    },
    RequiredCase {
        id: "integrated.allowed.v4",
        outcome: Outcome::Connected,
        establishes: "the integrated path's positive control over IPv4 TCP",
    },
    RequiredCase {
        id: "integrated.denied.v6",
        outcome: Outcome::Refused,
        establishes: "IPv6 TCP containment measured by connection, not by readback — an unfiltered \
                      address family is the cheapest bypass there is",
    },
    RequiredCase {
        id: "integrated.allowed.v6",
        outcome: Outcome::Connected,
        establishes: "the IPv6 positive control",
    },
    RequiredCase {
        id: "integrated.allowed.neighbour-port",
        outcome: Outcome::Connected,
        establishes: "an allowed destination stays allowed on a second port, ruling out a filter \
                      that matched one port number",
    },
    RequiredCase {
        id: "integrated.denied.neighbour-port",
        outcome: Outcome::Refused,
        establishes: "a denied destination stays denied on a neighbouring port, so the pinhole is a \
                      pinhole and not an open host",
    },
    RequiredCase {
        id: "integrated.exception.proxy-pinhole",
        outcome: Outcome::Connected,
        establishes: "the permitted proxy exception really passes through every installed layer",
    },
    RequiredCase {
        id: "integrated.runc.denied",
        outcome: Outcome::Refused,
        establishes: "runc compatibility: the added veth filters do not break containment for the \
                      runtime that was already contained",
    },
    RequiredCase {
        id: "integrated.runc.allowed",
        outcome: Outcome::Connected,
        establishes: "runc compatibility: allowed traffic still flows under the added filters",
    },
    RequiredCase {
        id: "integrated.never-started.oracle",
        outcome: Outcome::NeverStarted,
        establishes: "the oracle's red-prove: a payload that could not start is scored NeverStarted \
                      and never counted as a denial",
    },
    RequiredCase {
        id: "integrated.fail-closed.preparation",
        outcome: Outcome::LaunchRefused,
        establishes: "containment that cannot be installed refuses the launch and starts no payload",
    },
    // ── Lifecycle isolation ──────────────────────────────────────────────────────────────────
    RequiredCase {
        id: "integrated.sibling.contained-after-cleanup",
        outcome: Outcome::Refused,
        establishes: "a sibling job keeps its containment across another job's teardown",
    },
    RequiredCase {
        id: "integrated.sibling.allowed-after-cleanup",
        outcome: Outcome::Connected,
        establishes: "a sibling job keeps working across another job's teardown",
    },
    RequiredCase {
        id: "host.unaffected.during-cleanup",
        outcome: Outcome::Connected,
        establishes: "the VM host's own egress is unaffected before, during and after cleanup — no \
                      host-global mutation",
    },
];

/// A validated record of one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedCase {
    pub id: String,
    pub outcome: Outcome,
    pub log: String,
}

/// A validated saved matrix: what produced it, and every case it scored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedMatrix {
    /// The commit the artifact was built from, 40 hex.
    pub source_head: String,
    /// SHA-256 of the executed test binary, 64 hex.
    pub artifact_sha256: String,
    pub artifact_path: String,
    pub host: String,
    pub cases: Vec<SavedCase>,
}

/// Every header a record must carry. Identity is not decoration: a matrix that does not say which
/// source, which binary and which host produced it cannot be checked against anything later.
const REQUIRED_HEADERS: &[&str] = &["source_head", "artifact_sha256", "artifact_path", "host"];

/// Validate a saved matrix. `Err` carries **every** problem found, not the first: a record with
/// four missing cases should take one round trip to fix, not four.
pub fn validate(text: &str) -> Result<SavedMatrix, Vec<String>> {
    let mut problems = Vec::new();
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut cases: Vec<SavedCase> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();

    for (number, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let at = number + 1;
        if let Some(rest) = line.strip_prefix("case ") {
            match parse_case(rest, at) {
                Ok(case) => {
                    if seen_ids.contains(&case.id) {
                        problems.push(format!(
                            "line {at}: case {:?} is recorded twice — one live measurement per \
                             case, or the second silently overwrites the first",
                            case.id
                        ));
                    } else {
                        seen_ids.push(case.id.clone());
                        cases.push(case);
                    }
                }
                Err(problem) => problems.push(problem),
            }
        } else if line.starts_with("case") {
            problems.push(format!("line {at}: {line:?} is neither a header nor a `case ` record"));
        } else {
            match line.split_once('=') {
                Some((key, value)) => {
                    headers.push((key.trim().to_owned(), value.trim().to_owned()))
                }
                None => problems.push(format!(
                    "line {at}: {line:?} is not `key=value` and not a `case ` record"
                )),
            }
        }
    }

    // Headers: present, unique, non-empty, and the two digests actually digest-shaped.
    let mut resolved: Vec<(&str, String)> = Vec::new();
    for name in REQUIRED_HEADERS {
        let found: Vec<&(String, String)> =
            headers.iter().filter(|(key, _)| key == name).collect();
        match found.as_slice() {
            [] => problems.push(format!(
                "the record has no {name} — a matrix that does not say what produced it is not \
                 attributable to anything"
            )),
            [(_, value)] if value.is_empty() => {
                problems.push(format!("{name} is empty, which is the same as absent"))
            }
            [(_, value)] => resolved.push((name, value.clone())),
            _ => problems.push(format!(
                "{name} is given {} times; one record describes one run",
                found.len()
            )),
        }
    }
    let header = |name: &str| -> String {
        resolved
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    let source_head = header("source_head");
    if !source_head.is_empty() && !is_hex(&source_head, 40) {
        problems.push(format!(
            "source_head {source_head:?} is not a 40-character hex commit id — an abbreviated head \
             cannot be compared to a published one"
        ));
    }
    let artifact_sha256 = header("artifact_sha256");
    if !artifact_sha256.is_empty() && !is_hex(&artifact_sha256, 64) {
        problems.push(format!("artifact_sha256 {artifact_sha256:?} is not a 64-character hex digest"));
    }

    // Coverage, in the order the matrix declares, so the failure reads as a checklist.
    for required in REQUIRED_CASES {
        match cases.iter().find(|case| case.id == required.id) {
            None => problems.push(format!(
                "missing case {}: {} — a matrix without it is incomplete, not passing",
                required.id, required.establishes
            )),
            Some(case) if case.outcome != required.outcome => problems.push(format!(
                "case {} is recorded as {} but is only evidence at {}: {}",
                required.id,
                case.outcome.word(),
                required.outcome.word(),
                required.establishes
            )),
            Some(_) => {}
        }
    }
    for case in &cases {
        if !REQUIRED_CASES.iter().any(|required| required.id == case.id) {
            problems.push(format!(
                "case {:?} is not one the matrix requires — a renamed or truncated id records a \
                 measurement against nothing",
                case.id
            ));
        }
    }

    if problems.is_empty() {
        Ok(SavedMatrix {
            source_head,
            artifact_sha256,
            artifact_path: header("artifact_path"),
            host: header("host"),
            cases,
        })
    } else {
        Err(problems)
    }
}

/// `id=… outcome=… log=…`, all three required and none of them empty.
fn parse_case(rest: &str, at: usize) -> Result<SavedCase, String> {
    let mut id = None;
    let mut outcome_word = None;
    let mut log = None;
    for field in rest.split_whitespace() {
        let (key, value) = field.split_once('=').ok_or_else(|| {
            format!("line {at}: {field:?} in a case record is not `key=value`")
        })?;
        match key {
            "id" => id = Some(value.to_owned()),
            "outcome" => outcome_word = Some(value.to_owned()),
            "log" => log = Some(value.to_owned()),
            other => {
                return Err(format!("line {at}: a case record has no {other:?} field"));
            }
        }
    }
    let id = id.filter(|value| !value.is_empty()).ok_or_else(|| {
        format!("line {at}: a case record with no id scores nothing")
    })?;
    let word = outcome_word.unwrap_or_default();
    if word.is_empty() {
        return Err(format!(
            "line {at}: case {id} has an empty outcome — an unscored case is a case that was not \
             measured, and it fails rather than passing quietly"
        ));
    }
    let outcome = Outcome::parse(&word).ok_or_else(|| {
        format!(
            "line {at}: case {id} records outcome {word:?}, which is not one of connected, \
             refused, never-started, launch-refused — an outcome nobody can read is unscored"
        )
    })?;
    let log = log.filter(|value| !value.is_empty()).ok_or_else(|| {
        format!(
            "line {at}: case {id} names no log — an outcome with nothing behind it is an assertion, \
             not evidence"
        )
    })?;
    Ok(SavedCase { id, outcome, log })
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// The environment variable naming the saved matrix. Set by the offline acceptance entrypoint; when
/// it is unset the validator's own behaviour is still gated by the tests below.
pub const EVIDENCE_PATH_VAR: &str = "MAXPLAYER_LIVE_EVIDENCE";

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete record, built from the requirement itself so the positive control cannot drift
    /// out of date as the matrix grows.
    fn complete() -> String {
        let mut text = String::from(
            "# saved live containment matrix\n\
             source_head=0721dcec44131cbd0298e035d48b1aa935567088\n\
             artifact_sha256=a852e2e4d57fa0ed4873a1c36a2a567b2336901dd05fb1ce9776d69fa9919714\n\
             artifact_path=target-linux/debug/deps/sandbox_netns_live-82001bf1221bb710\n\
             host=lima:gvisor-repro linux-6.8.0-134-generic aarch64 runsc-release-20260817.0\n",
        );
        for case in REQUIRED_CASES {
            text.push_str(&format!(
                "case id={} outcome={} log=raw/{}.txt\n",
                case.id,
                case.outcome.word(),
                case.id
            ));
        }
        text
    }

    /// The positive control. Without it every refusal below could come from a validator that
    /// refuses everything.
    #[test]
    fn a_complete_record_validates_and_carries_its_identities() {
        let matrix = validate(&complete()).expect("a complete record must validate");
        assert_eq!(matrix.source_head, "0721dcec44131cbd0298e035d48b1aa935567088");
        assert_eq!(matrix.cases.len(), REQUIRED_CASES.len());
        assert!(matrix.host.contains("runsc"), "{:?}", matrix.host);
    }

    /// The gate the review asked for: a missing case fails, and says which and why.
    #[test]
    fn a_missing_case_fails_and_names_what_it_would_have_shown() {
        let dropped = REQUIRED_CASES[0].id;
        let text: String = complete()
            .lines()
            .filter(|line| !line.contains(&format!("id={dropped} ")))
            .map(|line| format!("{line}\n"))
            .collect();
        let problems = validate(&text).expect_err("an incomplete matrix must not validate");
        assert!(
            problems.iter().any(|p| p.contains(dropped) && p.contains("missing case")),
            "{problems:?}"
        );
        // Every required case is reachable this way, not just the first: a checklist that only
        // checks its head is not a checklist.
        for case in REQUIRED_CASES {
            let text: String = complete()
                .lines()
                .filter(|line| !line.contains(&format!("id={} ", case.id)))
                .map(|line| format!("{line}\n"))
                .collect();
            let problems = validate(&text).expect_err("still incomplete");
            assert!(problems.iter().any(|p| p.contains(case.id)), "{}: {problems:?}", case.id);
        }
    }

    /// An empty or unreadable outcome is unscored, and unscored fails. This is the difference
    /// between "11 ignored" and "the matrix is complete".
    #[test]
    fn an_unscored_case_fails_rather_than_passing_quietly() {
        for (record, expect) in [
            ("case id=integrated.denied.v4 outcome= log=raw/x.txt", "empty outcome"),
            ("case id=integrated.denied.v4 outcome=failed log=raw/x.txt", "not one of connected"),
            ("case id=integrated.denied.v4 outcome=refused log=", "names no log"),
            ("case id= outcome=refused log=raw/x.txt", "no id scores nothing"),
        ] {
            let text = complete()
                .lines()
                .filter(|line| !line.contains("id=integrated.denied.v4 "))
                .map(|line| format!("{line}\n"))
                .collect::<String>()
                + record
                + "\n";
            let problems = validate(&text).expect_err("an unscored case must fail");
            assert!(
                problems.iter().any(|problem| problem.contains(expect)),
                "expected {expect:?} in {problems:?}"
            );
        }
    }

    /// A case recorded in the wrong direction fails. A positive control recorded as a refusal is a
    /// control that did not hold, and accepting it would let a blanket-deny ruleset pass.
    #[test]
    fn a_case_recorded_in_the_wrong_direction_fails() {
        let text = complete().replace(
            "case id=integrated.allowed.v4 outcome=connected",
            "case id=integrated.allowed.v4 outcome=refused",
        );
        let problems = validate(&text).expect_err("a failed positive control must fail the matrix");
        assert!(
            problems.iter().any(|p| p.contains("integrated.allowed.v4")
                && p.contains("only evidence at connected")),
            "{problems:?}"
        );
    }

    /// A duplicate and an unknown id both fail: the first hides a second measurement, the second is
    /// what a renamed or truncated record looks like.
    #[test]
    fn duplicate_and_unknown_case_ids_fail() {
        let duplicated =
            complete() + "case id=integrated.denied.v4 outcome=connected log=raw/again.txt\n";
        let problems = validate(&duplicated).expect_err("a duplicate must fail");
        assert!(problems.iter().any(|p| p.contains("recorded twice")), "{problems:?}");

        let unknown = complete() + "case id=integrated.denied.v5 outcome=refused log=raw/x.txt\n";
        let problems = validate(&unknown).expect_err("an unknown id must fail");
        assert!(
            problems.iter().any(|p| p.contains("not one the matrix requires")),
            "{problems:?}"
        );
    }

    /// Identity headers are required and shaped. An unattributable matrix is not evidence about any
    /// particular source or binary.
    #[test]
    fn a_record_without_source_and_artifact_identity_fails() {
        for (name, broken) in [
            ("source_head", complete().replace("source_head=0721dcec44131cbd0298e035d48b1aa935567088\n", "")),
            ("artifact_sha256", complete().replace("artifact_sha256=a852e2e4d57fa0ed4873a1c36a2a567b2336901dd05fb1ce9776d69fa9919714\n", "")),
            ("artifact_path", complete().replace("artifact_path=target-linux/debug/deps/sandbox_netns_live-82001bf1221bb710\n", "")),
            ("host", complete().replace("host=lima:gvisor-repro linux-6.8.0-134-generic aarch64 runsc-release-20260817.0\n", "")),
        ] {
            let problems = validate(&broken).expect_err("a record missing {name} must fail");
            assert!(problems.iter().any(|p| p.contains(name)), "{name}: {problems:?}");
        }
        // Shape, not just presence: an abbreviated head cannot be compared to a published one.
        let short = complete().replace("source_head=0721dcec44131cbd0298e035d48b1aa935567088", "source_head=0721dce");
        let problems = validate(&short).expect_err("an abbreviated head must fail");
        assert!(problems.iter().any(|p| p.contains("40-character hex")), "{problems:?}");
        let empty = complete().replace("host=lima:gvisor-repro linux-6.8.0-134-generic aarch64 runsc-release-20260817.0", "host=");
        let problems = validate(&empty).expect_err("an empty header must fail");
        assert!(problems.iter().any(|p| p.contains("host is empty")), "{problems:?}");
    }

    /// An empty file fails — the state this repository is in right now. No live matrix has been
    /// produced for this candidate, because the live runtime is held, and the gate says so instead
    /// of reporting a green.
    #[test]
    fn an_empty_record_fails_with_the_whole_checklist() {
        let problems = validate("").expect_err("nothing measured is not a pass");
        assert!(
            problems.len() >= REQUIRED_HEADERS.len() + REQUIRED_CASES.len(),
            "an empty record must report every missing header and case, got {}",
            problems.len()
        );
    }

    /// The acceptance entrypoint's own leg: when a saved matrix is named, validate that file. This
    /// is the test `scripts/sandbox-acceptance.sh` runs with the variable set, and it fails when the
    /// file is absent, unreadable or incomplete.
    #[test]
    fn the_named_saved_matrix_validates() {
        let Ok(path) = std::env::var(EVIDENCE_PATH_VAR) else {
            // Unset: the validator's behaviour is gated by the tests above, and there is nothing
            // here to check. The acceptance entrypoint is what sets it.
            return;
        };
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "{EVIDENCE_PATH_VAR}={path} could not be read: {error}\nA named saved matrix that \
                 is not there is a missing gate, not an absent one."
            )
        });
        if let Err(problems) = validate(&text) {
            panic!(
                "the saved live matrix at {path} is not complete:\n  {}",
                problems.join("\n  ")
            );
        }
    }
}
