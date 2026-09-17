//! Feasibility gates F1 (production binary) and F2 (IPC) for the killable delivery-push executor.
//!
//! These are not unit tests of a protocol type. They run the **shipped binary** — the same artifact
//! `.github/release-platforms.json` builds for linux-x64, linux-arm64 and darwin-arm64 — as a real
//! child over real pipes, because the feasibility question is precisely whether that artifact can
//! host the child half at all. A protocol proven only against an in-process stub proves nothing
//! about the thing production re-execs.
//!
//! This file lives in the BINARY crate deliberately: `CARGO_BIN_EXE_maxplayer` exists only here, and
//! it is the only way a test can name the real executable rather than a path it guessed.

#![cfg(feature = "wallet")]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use maxplayer_core::delivery_executor::{
    CHILD_ENV_ALLOWLIST, CHILD_SUBCOMMAND, PROTOCOL_VERSION, PushRequest, ToChild, ToParent,
};

/// Planted in THIS process's environment before the child is spawned. If the child can see it, the
/// environment is being inherited and a real credential would travel the same way.
const SENTINEL: &str = "delivery-push-feasibility-sentinel-must-not-be-inherited";

fn child() -> Command {
    // SAFETY: the tests in this file are the only writers, and they set the same value.
    unsafe { std::env::set_var("MAXPLAYER_FEASIBILITY_SENTINEL", SENTINEL) };
    let mut command = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
    command
        .arg(CHILD_SUBCOMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(
            CHILD_ENV_ALLOWLIST
                .iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (name.to_string(), value))),
        );
    command
}

fn line<T: for<'de> serde::Deserialize<'de>>(reader: &mut impl BufRead) -> T {
    let mut buffer = String::new();
    let read = reader.read_line(&mut buffer).expect("read a frame");
    assert!(read > 0, "the child closed its pipe without sending a frame");
    serde_json::from_str(buffer.trim_end()).expect("a well-formed frame")
}

/// F1: the artifact this product ships dispatches the child entrypoint, and says hello in the
/// protocol this parent speaks.
///
/// If this fails, process isolation is not feasible with the binary we ship, and the design must
/// change rather than the claim.
#[test]
fn the_shipped_binary_hosts_the_delivery_push_child_entrypoint() {
    let mut spawned = child().spawn().expect("the shipped binary runs");
    let mut out = BufReader::new(spawned.stdout.take().expect("stdout"));
    let hello: ToParent = line(&mut out);

    let ToParent::Hello { version, argv, env } = hello else {
        panic!("the first frame from the child must be its hello");
    };
    assert_eq!(
        version, PROTOCOL_VERSION,
        "the shipped binary speaks a different protocol version than this parent"
    );
    assert_eq!(
        argv.len(),
        2,
        "the child received an argv this parent did not send: {argv:?}"
    );
    assert_eq!(argv[1], CHILD_SUBCOMMAND);

    // F2, first half: nothing INHERITED rides in the environment. Proven from INSIDE the real
    // child, which reports the environment it actually received, rather than asserted about the
    // spawn spec — argv and the environment are world-readable through `ps` and `/proc`, so a
    // credential that reached either would be readable by every process on the box.
    //
    // `env_clear()` does not produce an EMPTY environment on every platform, and this gate is where
    // that was discovered rather than assumed: darwin's libSystem injects
    // `__CF_USER_TEXT_ENCODING` (uid + locale, no secret) into a spawned process itself. The
    // property worth asserting is therefore not "the environment is exactly the allowlist" — that is
    // a statement about the OS — but "nothing this process was carrying reached the child unless we
    // chose it", which is the security claim. Platform injections are listed by name so that a NEW
    // one shows up here as a failure to be understood rather than passing unnoticed.
    const PLATFORM_INJECTED: [&str; 1] = ["__CF_USER_TEXT_ENCODING"];
    for name in env.keys() {
        assert!(
            CHILD_ENV_ALLOWLIST.contains(&name.as_str()) || PLATFORM_INJECTED.contains(&name.as_str()),
            "the child was given {name}, which is neither on the allowlist nor a known platform \
             injection; argv and the environment are world-readable, so nothing may travel there \
             unexamined"
        );
    }
    for (name, value) in &env {
        assert!(
            !name.contains(SENTINEL) && !value.contains(SENTINEL),
            "a variable this test process was carrying reached the child as {name}; inherited \
             environment is exactly the leak the allowlist exists to prevent"
        );
    }

    drop(spawned.stdin.take());
    let status = spawned.wait().expect("the child exits");
    assert!(
        !status.success(),
        "a child whose parent never sent a push request must fail, not report success"
    );
}

/// F2: the whole pipe protocol over real pipes to the real binary — request in, terminal outcome
/// out, exit status matching the outcome.
///
/// The push is pointed at a workdir that does not exist, so this exercises the framing, the request
/// decode and the error return without needing a git fixture or a network peer. What it proves is
/// the leg the feasibility question was about: a request crosses, an answer comes back, and the
/// child ends by itself.
#[test]
fn a_push_request_crosses_the_pipe_and_its_outcome_comes_back() {
    let mut spawned = child().spawn().expect("the shipped binary runs");
    let mut input = spawned.stdin.take().expect("stdin");
    let mut out = BufReader::new(spawned.stdout.take().expect("stdout"));
    let _hello: ToParent = line(&mut out);

    let absent = PathBuf::from("/nonexistent-delivery-workdir-for-the-feasibility-gate");
    let request = ToChild::Push(PushRequest {
        workdir: absent,
        remote_url: "https://relay.invalid/repo.git".to_owned(),
        branch: "job-feasibility".to_owned(),
        gated_oid: "0".repeat(40),
        // No mint may be asked for: an unauthenticated remote that asks to sign is a protocol
        // violation the parent kills for, and this test pins that the child does not ask.
        authenticated: false,
        // The parent stamps a remaining duration AND the same deadline as an absolute wall-clock
        // instant, from one moment, so the child charges the pipe transit to itself rather than
        // restarting its clock at the read. Stamped the same way here.
        budget_ms: 5_000,
        deadline_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
            .saturating_add(5_000),
    });
    let mut frame = serde_json::to_string(&request).expect("encode");
    frame.push('\n');
    input.write_all(frame.as_bytes()).expect("write the request");
    input.flush().expect("flush");

    let outcome: ToParent = line(&mut out);
    match outcome {
        ToParent::Done { oid, error } => {
            assert!(oid.is_none(), "a push into a missing workdir cannot succeed");
            let error = error.expect("a failed push names its reason");
            assert!(
                !error.is_empty(),
                "the child must return a reason, not an empty error"
            );
        }
        ToParent::Mint { destination } => panic!(
            "the child asked to authorize {destination} for an UNAUTHENTICATED remote; the parent \
             kills for this, and it must never happen"
        ),
        ToParent::Hello { .. } => panic!("the child said hello twice"),
        // The child's own pre-transmit gate. It never fires here: the push fails at the missing
        // workdir, before the transport reaches a wire request.
        ToParent::Check { phase } => panic!(
            "the child asked about its authority at {phase} for a push that never reached the wire"
        ),
    }

    drop(input);
    let status = spawned.wait().expect("the child exits");
    assert_eq!(
        status.code(),
        Some(1),
        "a failed push must exit 1, so a parent that lost the pipe can still tell what happened"
    );
}
