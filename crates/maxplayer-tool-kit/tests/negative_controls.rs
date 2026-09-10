//! Negative controls.
//!
//! Each case asserts the *reason* for the refusal, not merely that something failed — a
//! validator that rejected everything would otherwise pass this file. Where a refusal is
//! supposed to happen before the tool is invoked, the vendor's own counters are checked to
//! confirm nothing reached it.

mod common;

use common::Fixture;
use maxplayer_tool_kit::config::SellerToolConfig;
use maxplayer_tool_kit::safeio;
use maxplayer_tool_kit::validate::{validate_call, CallArg, Reject};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Part A — parameter grammar and job-directory confinement, tested directly.
// ---------------------------------------------------------------------------

/// A config exercising every parameter kind, including the `text` kind the shipped demo profile
/// does not use.
fn grammar_config() -> SellerToolConfig {
    serde_json::from_value(json!({
        "seller_id": "seller-test",
        "offering": "grammar fixture",
        "vendor_base_url": "http://127.0.0.1:1",
        "operations": [{
            "name": "transform-file",
            "description": "fixture",
            "subcommand": "transform",
            "max_output_bytes": 1024,
            "params": [
                {"name": "input",  "flag": "--in",    "kind": {"type": "job_input_file"}},
                {"name": "output", "flag": "--out",   "kind": {"type": "job_output_file"}},
                {"name": "mode",   "flag": "--mode",  "kind": {"type": "choice", "choices": ["upper", "lower"]}},
                {"name": "label",  "flag": "--label", "kind": {"type": "text", "max_len": 16}}
            ]
        }]
    }))
    .expect("grammar config")
}

struct JobDir {
    root: PathBuf,
    /// A second job's directory, to aim cross-job attempts at something that really exists.
    other: PathBuf,
}

impl JobDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("mtk-grammar-{}-{tag}-{nanos}", std::process::id()));
        let root = base.join("job-self");
        let other = base.join("job-other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(root.join("input.txt"), "payload").unwrap();
        std::fs::write(other.join("secret.txt"), "another job's file").unwrap();
        JobDir { root, other }
    }
}

impl Drop for JobDir {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

fn params(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn valid_params() -> BTreeMap<String, Value> {
    params(&[
        ("input", json!("input.txt")),
        ("output", json!("out.txt")),
        ("mode", json!("upper")),
        ("label", json!("ok-label")),
    ])
}

fn reject(job: &JobDir, op: &str, p: BTreeMap<String, Value>) -> Reject {
    validate_call(&grammar_config(), op, &p, &job.root)
        .expect_err("this call must be refused")
}

#[test]
fn the_valid_call_is_accepted() {
    // Without this, every other case in Part A could pass by rejecting everything.
    let job = JobDir::new("positive");
    let call = validate_call(&grammar_config(), "transform-file", &valid_params(), &job.root)
        .expect("the valid call must be accepted");
    assert_eq!(call.subcommand, "transform");
    // Arguments are built from the spec order: one per declared parameter, each carrying its
    // flag, and a file argument carries the job-RELATIVE path — never a canonicalized absolute
    // string, because resolving it here and opening it later is the F2 race.
    assert_eq!(
        call.args,
        vec![
            CallArg::Input { flag: "--in".into(), rel: PathBuf::from("input.txt") },
            CallArg::Output { flag: "--out".into(), rel: PathBuf::from("out.txt") },
            CallArg::Literal { flag: "--mode".into(), value: "upper".into() },
            CallArg::Literal { flag: "--label".into(), value: "ok-label".into() },
        ]
    );
    // No component of a file argument escaped the job by grammar; the descriptor-level guarantee
    // is Part A-2's job.
    for arg in &call.args {
        if let CallArg::Input { rel, .. } | CallArg::Output { rel, .. } = arg {
            assert!(rel.is_relative(), "a file argument must stay job-relative");
        }
    }
}

#[test]
fn unknown_operation_is_refused() {
    let job = JobDir::new("unknown-op");
    assert!(matches!(
        reject(&job, "delete-everything", valid_params()),
        Reject::UnknownOperation { .. }
    ));
}

#[test]
fn undeclared_parameter_is_refused_not_ignored() {
    let job = JobDir::new("unknown-param");
    let mut p = valid_params();
    p.insert("extra".into(), json!("x"));
    assert!(matches!(reject(&job, "transform-file", p), Reject::UnknownParam { .. }));
}

#[test]
fn missing_parameter_is_refused() {
    let job = JobDir::new("missing-param");
    let mut p = valid_params();
    p.remove("mode");
    assert!(matches!(reject(&job, "transform-file", p), Reject::MissingParam { .. }));
}

#[test]
fn non_string_parameter_is_refused() {
    let job = JobDir::new("non-string");
    let mut p = valid_params();
    p.insert("label".into(), json!(42));
    assert!(matches!(reject(&job, "transform-file", p), Reject::NotAString { .. }));
}

#[test]
fn overlong_text_is_refused() {
    let job = JobDir::new("too-long");
    let mut p = valid_params();
    p.insert("label".into(), json!("x".repeat(17)));
    assert!(matches!(
        reject(&job, "transform-file", p),
        Reject::TextTooLong { max: 16, len: 17, .. }
    ));
}

#[test]
fn control_characters_are_refused() {
    let job = JobDir::new("control");
    let mut p = valid_params();
    p.insert("label".into(), json!("a\u{7}b"));
    assert!(matches!(reject(&job, "transform-file", p), Reject::ControlCharacter { .. }));
}

#[test]
fn a_value_that_would_read_as_a_flag_is_refused() {
    let job = JobDir::new("flaglike");
    // Kept under `max_len` on purpose: a longer probe like "--output=/etc/passwd" trips the
    // length rule first and would pass this test without ever exercising the flag rule.
    for probe in ["--force", "-rf", "--out=x"] {
        let mut p = valid_params();
        p.insert("label".into(), json!(probe));
        assert!(
            matches!(reject(&job, "transform-file", p), Reject::LooksLikeFlag { .. }),
            "{probe:?} should have been refused as flag-like"
        );
    }
}

#[test]
fn shell_metacharacters_are_refused() {
    let job = JobDir::new("metachar");
    for probe in ["a;id", "a|id", "a&id", "a$(id)", "a`id`", "a>out", "a'q'"] {
        let mut p = valid_params();
        p.insert("label".into(), json!(probe));
        assert!(
            matches!(reject(&job, "transform-file", p), Reject::ShellMetacharacter { .. }),
            "{probe:?} should have been refused"
        );
    }
}

#[test]
fn a_value_outside_the_declared_choices_is_refused() {
    let job = JobDir::new("choice");
    let mut p = valid_params();
    p.insert("mode".into(), json!("exfiltrate"));
    assert!(matches!(reject(&job, "transform-file", p), Reject::NotAChoice { .. }));
}

#[test]
fn an_absolute_path_into_another_job_is_refused() {
    let job = JobDir::new("abs-cross-job");
    let mut p = valid_params();
    p.insert("input".into(), json!(job.other.join("secret.txt").to_string_lossy().to_string()));
    assert!(matches!(reject(&job, "transform-file", p), Reject::AbsolutePath { .. }));
}

#[test]
fn a_dotdot_escape_into_another_job_is_refused() {
    let job = JobDir::new("dotdot");
    let mut p = valid_params();
    p.insert("input".into(), json!("../job-other/secret.txt"));
    assert!(matches!(reject(&job, "transform-file", p), Reject::NonNormalComponent { .. }));
}

// ---------------------------------------------------------------------------
// Part A-2 — race-safe consumption (advisor F2).
//
// These used to assert that `validate_call` refused a symlink, a missing file and so on. It no
// longer does, on purpose: a path resolved at validation and re-opened later is the check/use
// race F2 is about. Refusal now happens where the file is actually consumed — the holder's
// no-follow open — so that is where these test it. `validate_call` accepts the ordinary NAME;
// `safeio` refuses what the name resolves to.
// ---------------------------------------------------------------------------

fn open_input_err(job: &JobDir, rel: &str) -> Reject {
    safeio::open_input(&job.root, Path::new(rel), "input")
        .expect_err("this input must be refused")
}

fn create_output_err(job: &JobDir, rel: &str) -> Reject {
    safeio::create_output(&job.root, Path::new(rel), "output")
        .expect_err("this output must be refused")
}

#[test]
fn a_symlinked_input_is_refused() {
    let job = JobDir::new("symlink");
    std::os::unix::fs::symlink(job.other.join("secret.txt"), job.root.join("link.txt")).unwrap();
    assert!(matches!(open_input_err(&job, "link.txt"), Reject::SymlinkedPath { .. }));
}

/// The case a string-prefix check would pass: the final component is an ordinary file, but a
/// *parent* component is a symlink out of the job directory. The no-follow walk catches it.
#[test]
fn a_symlinked_parent_directory_is_refused() {
    let job = JobDir::new("symlink-parent");
    std::os::unix::fs::symlink(&job.other, job.root.join("sub")).unwrap();
    let err = open_input_err(&job, "sub/secret.txt");
    assert!(
        matches!(err, Reject::EscapesJobDir { .. }),
        "a symlinked parent must be caught by the no-follow walk, got {err:?}"
    );
}

#[test]
fn a_symlinked_parent_is_refused_for_outputs_too() {
    let job = JobDir::new("symlink-parent-out");
    std::os::unix::fs::symlink(&job.other, job.root.join("sub")).unwrap();
    assert!(matches!(create_output_err(&job, "sub/planted.txt"), Reject::EscapesJobDir { .. }));
}

#[test]
fn a_missing_input_is_refused() {
    let job = JobDir::new("missing-input");
    assert!(matches!(open_input_err(&job, "nope.txt"), Reject::MissingInput { .. }));
}

#[test]
fn a_directory_is_not_an_input_file() {
    let job = JobDir::new("dir-input");
    std::fs::create_dir_all(job.root.join("adir")).unwrap();
    assert!(matches!(open_input_err(&job, "adir"), Reject::NotARegularFile { .. }));
}

#[test]
fn an_output_in_a_missing_directory_is_refused() {
    let job = JobDir::new("out-parent");
    assert!(matches!(create_output_err(&job, "nodir/out.txt"), Reject::OutputParentMissing { .. }));
}

/// The check/use boundary, made explicit and shown not to be a boundary at all: `validate_call`
/// accepts the ordinary names (the "check"), a symlink is then planted where each file will be
/// consumed (the "swap"), and the holder's no-follow open/create refuses both (the "use").
/// Crucially, the outside file the symlinks point at is neither read through nor truncated
/// through — the property F2 requires.
#[test]
fn a_swap_between_validation_and_use_reads_and_writes_nothing_outside() {
    let job = JobDir::new("f2-boundary");
    let outside = job.other.join("outside.txt");
    std::fs::write(&outside, "OUTSIDE SENTINEL").unwrap();

    // The check: grammar accepts input.txt and out.txt as ordinary relative names.
    let call = validate_call(&grammar_config(), "transform-file", &valid_params(), &job.root)
        .expect("grammar accepts ordinary names");

    // The swap: point both names out of the job directory.
    std::fs::remove_file(job.root.join("input.txt")).ok();
    std::os::unix::fs::symlink(&outside, job.root.join("input.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, job.root.join("out.txt")).unwrap();

    // The use: every file argument is resolved through safeio, and every one is refused.
    let mut checked_input = false;
    let mut checked_output = false;
    for arg in &call.args {
        match arg {
            CallArg::Input { rel, .. } => {
                checked_input = true;
                assert!(matches!(
                    safeio::open_input(&job.root, rel, "input"),
                    Err(Reject::SymlinkedPath { .. })
                ));
            }
            CallArg::Output { rel, .. } => {
                checked_output = true;
                assert!(matches!(
                    safeio::create_output(&job.root, rel, "output"),
                    Err(Reject::SymlinkedPath { .. })
                ));
            }
            CallArg::Literal { .. } => {}
        }
    }
    assert!(checked_input && checked_output, "both a file input and output must have been exercised");
    // Not read through, and not truncated through.
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "OUTSIDE SENTINEL");
}

#[test]
fn an_empty_path_is_refused() {
    let job = JobDir::new("empty");
    let mut p = valid_params();
    p.insert("input".into(), json!(""));
    assert!(matches!(reject(&job, "transform-file", p), Reject::EmptyPath { .. }));
}

// ---------------------------------------------------------------------------
// Part B — refusals at the live endpoint, with the vendor as witness.
// ---------------------------------------------------------------------------

/// A job may not nominate its own directory, and the attempt is refused rather than dropped.
#[test]
fn a_job_cannot_nominate_its_own_directory() {
    let fx = Fixture::start();
    let root = fx.make_job("job-a");
    std::fs::write(root.join("input.txt"), "payload").unwrap();

    for reserved in ["job_root", "job_id", "cwd", "home"] {
        let err = fx
            .job_call(
                "job-a",
                "tools/call",
                json!({"name": "transform-file", "arguments": {
                    "input": "input.txt", "output": "out.txt", "mode": "upper", reserved: "/etc"
                }}),
            )
            .expect_err("a reserved argument must be refused");
        assert!(err.contains("[1003]"), "expected a rejection code for {reserved}, got: {err}");
        assert!(err.contains(reserved), "the refusal should name {reserved}: {err}");
    }
    assert_eq!(fx.vendor_stats()["transform_count"], json!(0), "nothing should have reached the vendor");
}

/// The control endpoint is the seller's, and it does not execute work.
#[test]
fn the_control_endpoint_refuses_tool_calls() {
    let fx = Fixture::start();
    let err = fx
        .ctl("tools/call", json!({"name": "transform-file", "arguments": {}}))
        .expect_err("tools/call on the control socket must be refused");
    assert!(err.contains("job endpoint"), "unexpected error: {err}");
    assert_eq!(fx.vendor_stats()["transform_count"], json!(0));
}

/// End to end through the MCP bridge: one job reaching for another job's file.
#[test]
fn cross_job_access_is_refused_through_mcp() {
    let fx = Fixture::start();
    let a = fx.make_job("job-a");
    std::fs::write(a.join("secret.txt"), "job A's private file").unwrap();
    let b = fx.make_job("job-b");
    std::fs::write(b.join("input.txt"), "job B's own file").unwrap();

    let mut mcp = fx.mcp_session("job-b");
    mcp.initialize();

    // Absolute path into job A.
    let abs = mcp.request(
        "tools/call",
        json!({"name": "transform-file", "arguments": {
            "input": a.join("secret.txt").to_string_lossy(), "output": "out.txt", "mode": "upper"
        }}),
    );
    assert_eq!(abs["error"]["code"], json!(1003), "expected a rejection: {abs}");

    // Traversal into job A.
    let rel = mcp.request(
        "tools/call",
        json!({"name": "transform-file", "arguments": {
            "input": "../job-a/secret.txt", "output": "out.txt", "mode": "upper"
        }}),
    );
    assert_eq!(rel["error"]["code"], json!(1003), "expected a rejection: {rel}");

    // Writing into job A.
    let write_out = mcp.request(
        "tools/call",
        json!({"name": "transform-file", "arguments": {
            "input": "input.txt", "output": a.join("planted.txt").to_string_lossy(), "mode": "upper"
        }}),
    );
    assert_eq!(write_out["error"]["code"], json!(1003), "expected a rejection: {write_out}");

    assert!(!a.join("planted.txt").exists(), "job B must not have written into job A");
    assert_eq!(fx.vendor_stats()["transform_count"], json!(0), "no refused call may reach the vendor");

    // The job's own file still works, so the refusals above were specific.
    let ok = mcp.request(
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
    );
    assert_eq!(ok["result"]["isError"], json!(false), "the legitimate call must still succeed: {ok}");
    assert_eq!(fx.vendor_stats()["transform_count"], json!(1));
}

/// The configured output ceiling is enforced by the holder, and an oversized result does not
/// survive in the job directory.
#[test]
fn the_output_ceiling_is_enforced() {
    let fx = Fixture::start_configured(|cfg| {
        cfg["operations"][0]["max_output_bytes"] = json!(8);
    });
    let root = fx.make_job("job-a");
    std::fs::write(root.join("input.txt"), "far longer than eight bytes").unwrap();

    let err = fx
        .job_call(
            "job-a",
            "tools/call",
            json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
        )
        .expect_err("an oversized output must be refused");
    assert!(err.contains("[1004]"), "expected a tool-failure code, got: {err}");
    assert!(err.contains("ceiling"), "the refusal should name the ceiling: {err}");
    assert!(!root.join("out.txt").exists(), "the oversized output must not be left behind");

    // A result inside the ceiling still works, so the limit is a limit and not a break.
    std::fs::write(root.join("small.txt"), "tiny").unwrap();
    fx.job_call(
        "job-a",
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "small.txt", "output": "small-out.txt", "mode": "upper"}}),
    )
    .expect("a call inside the ceiling must succeed");
    assert_eq!(std::fs::read_to_string(root.join("small-out.txt")).unwrap(), "TINY");
}

/// A vendor-side revocation must become *visible* seller state and must fail closed, and
/// re-enrolment must recover it.
#[test]
fn a_revoked_session_is_visibly_unhealthy_then_recoverable() {
    let fx = Fixture::start();
    let root = fx.make_job("job-a");
    std::fs::write(root.join("input.txt"), "payload").unwrap();

    // Healthy to begin with, and the operator can see it.
    let (ok, stdout, _) = fx.holderctl(&["health"]);
    assert!(ok, "holderctl health should succeed while enrolled");
    assert!(stdout.contains("healthy"), "unexpected health output: {stdout}");

    let revoked = fx.revoke_vendor_sessions();
    assert_eq!(revoked, 1, "there should have been exactly one live session to revoke");

    // Visible: non-zero exit and an unhealthy state with a reason.
    let (ok, stdout, _) = fx.holderctl(&["health"]);
    assert!(!ok, "holderctl must exit non-zero once the tool cannot authenticate");
    assert!(stdout.contains("unhealthy"), "expected an unhealthy state: {stdout}");
    assert!(stdout.contains("rejected the stored session"), "expected a reason: {stdout}");

    // And status keeps reporting it, so it is not a transient observation.
    let status = fx.ctl("holder/status", json!({})).unwrap();
    assert_eq!(status["healthy"], json!(false));

    // Fails closed rather than erroring vaguely.
    let err = fx
        .job_call(
            "job-a",
            "tools/call",
            json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
        )
        .expect_err("a call must fail while unauthenticated");
    assert!(err.contains("[1001]"), "expected the unhealthy code, got: {err}");
    assert!(!root.join("out.txt").exists(), "a failed call must not leave output");

    // Re-enrolment recovers, and the vendor confirms a second login happened.
    let res = fx.ctl("holder/reenroll", json!({})).expect("re-enrolment");
    assert_eq!(res["health"], json!({"state": "healthy"}));
    assert_eq!(fx.vendor_stats()["login_count"], json!(2), "re-enrolment is a second login");

    fx.job_call(
        "job-a",
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
    )
    .expect("calls work again after re-enrolment");
    assert_eq!(std::fs::read_to_string(root.join("out.txt")).unwrap(), "PAYLOAD");
    assert!(fx.vendor_stats()["auth_failures"].as_u64().unwrap_or(0) >= 1);
}

/// A detached job's endpoint is gone, while the tool itself is untouched.
#[test]
fn detaching_a_job_removes_only_that_endpoint() {
    let fx = Fixture::start();
    let a = fx.make_job("job-a");
    std::fs::write(a.join("input.txt"), "payload").unwrap();
    let b = fx.make_job("job-b");
    std::fs::write(b.join("input.txt"), "payload").unwrap();

    let detached = fx.detach_job("job-a");
    assert_eq!(detached["tool_still_enrolled"], json!(true));

    let err = fx.job_call("job-a", "tools/list", json!({})).expect_err("job A's endpoint is gone");
    assert!(err.contains("unreachable"), "unexpected error: {err}");

    // Job B is unaffected, and so is the tool.
    fx.job_call(
        "job-b",
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
    )
    .expect("job B continues to work");
    assert_eq!(fx.vendor_stats()["login_count"], json!(1), "no re-login happened anywhere");
    assert_eq!(fx.ctl("holder/status", json!({})).unwrap()["healthy"], json!(true));
}

#[test]
fn an_unknown_method_is_refused() {
    let fx = Fixture::start();
    fx.make_job("job-a");
    let err = fx.job_call("job-a", "tools/exfiltrate", json!({})).expect_err("unknown method");
    assert!(err.contains("[-32601]"), "unexpected error: {err}");
}

/// F2 at the live endpoint, input side. Job B swaps its own input name for a symlink into job A.
/// The holder refuses it at consumption, job A's file is not read, nothing reaches the vendor,
/// and a legitimate call still succeeds afterward.
#[test]
fn a_planted_input_symlink_is_refused_at_the_live_endpoint() {
    let fx = Fixture::start();
    let a = fx.make_job("job-a");
    std::fs::write(a.join("secret.txt"), "job A private").unwrap();
    let b = fx.make_job("job-b");
    std::os::unix::fs::symlink(a.join("secret.txt"), b.join("input.txt")).unwrap();

    let mut mcp = fx.mcp_session("job-b");
    mcp.initialize();
    let res = mcp.request(
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
    );
    assert_eq!(res["error"]["code"], json!(1003), "a symlinked input must be refused: {res}");
    assert!(!b.join("out.txt").exists(), "no output should have been produced");
    assert_eq!(fx.vendor_stats()["transform_count"], json!(0), "nothing should have reached the vendor");

    // A real file in job B still works, so the refusal was specific to the swap.
    std::fs::write(b.join("real.txt"), "job b own").unwrap();
    let ok = mcp.request(
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "real.txt", "output": "out.txt", "mode": "upper"}}),
    );
    assert_eq!(ok["result"]["isError"], json!(false), "the legitimate call must still work: {ok}");
    assert_eq!(std::fs::read_to_string(b.join("out.txt")).unwrap(), "JOB B OWN");
}

/// F2 at the live endpoint, output side. Job B aims its output at job A's file through a symlink
/// named `out.txt`. The holder produces the transform in private staging, then refuses to publish
/// through the symlink, so job A's file is left exactly as it was.
#[test]
fn a_planted_output_symlink_does_not_write_outside_the_job() {
    let fx = Fixture::start();
    let a = fx.make_job("job-a");
    std::fs::write(a.join("target.txt"), "A ORIGINAL").unwrap();
    let b = fx.make_job("job-b");
    std::fs::write(b.join("input.txt"), "payload").unwrap();
    std::os::unix::fs::symlink(a.join("target.txt"), b.join("out.txt")).unwrap();

    let mut mcp = fx.mcp_session("job-b");
    mcp.initialize();
    let res = mcp.request(
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
    );
    assert_eq!(res["error"]["code"], json!(1003), "a symlinked output must be refused: {res}");
    // Job A's file is untouched: not truncated, not overwritten.
    assert_eq!(std::fs::read_to_string(a.join("target.txt")).unwrap(), "A ORIGINAL");
}
