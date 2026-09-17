//! The positive demonstrations named in the scope correction.
//!
//! Every claim about calls is checked against the **vendor's** counters, never the holder's
//! self-report. `login_count` is the load-bearing one: it is how "enrolled once, not once per
//! job" stops being a claim and becomes an observation.

mod common;

use common::Fixture;
use serde_json::json;

/// One enrolment, two sequential jobs. The tool is not logged in per job and is not logged out
/// when a job ends.
#[test]
fn one_enrollment_serves_two_sequential_jobs() {
    let fx = Fixture::start();
    assert_eq!(fx.vendor_stats()["login_count"], json!(1), "startup should log in exactly once");

    // Job A.
    let a = fx.make_job("job-a");
    std::fs::write(a.join("input.txt"), "first job payload").unwrap();
    let res = fx
        .job_call(
            "job-a",
            "tools/call",
            json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
        )
        .expect("job A call");
    assert_eq!(res["isError"], json!(false));
    assert_eq!(std::fs::read_to_string(a.join("out.txt")).unwrap(), "FIRST JOB PAYLOAD");

    // Job A ends. This must not disturb the tool.
    let detached = fx.detach_job("job-a");
    assert_eq!(detached["tool_still_enrolled"], json!(true));
    assert_eq!(detached["health"], json!({"state": "healthy"}), "a job ending is not a health event");

    // Job B, a different job of the same seller.
    let b = fx.make_job("job-b");
    std::fs::write(b.join("input.txt"), "second job payload").unwrap();
    let res = fx
        .job_call(
            "job-b",
            "tools/call",
            json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "reverse"}}),
        )
        .expect("job B call");
    assert_eq!(res["isError"], json!(false));
    assert_eq!(std::fs::read_to_string(b.join("out.txt")).unwrap(), "daolyap boj dnoces");

    // The vendor is the witness: two transforms, still one login.
    let stats = fx.vendor_stats();
    assert_eq!(stats["transform_count"], json!(2), "both jobs should have reached the vendor");
    assert_eq!(stats["login_count"], json!(1), "the second job must not have triggered a login");
    assert_eq!(stats["auth_failures"], json!(0));

    // And the holder agrees it never re-enrolled.
    let status = fx.ctl("holder/status", json!({})).unwrap();
    assert_eq!(status["enrollments_this_process"], json!(1));
    assert_eq!(status["calls_served"], json!(2));
}

/// The operation list is seller configuration: same list, every job, and the same list the
/// operator sees.
#[test]
fn operation_list_is_seller_level() {
    let fx = Fixture::start();
    fx.make_job("job-a");
    fx.make_job("job-b");

    let control = fx.ctl("tools/list", json!({})).unwrap();
    let from_a = fx.job_call("job-a", "tools/list", json!({})).unwrap();
    let from_b = fx.job_call("job-b", "tools/list", json!({})).unwrap();

    assert_eq!(from_a, from_b, "two jobs of one seller must see one offering");
    assert_eq!(from_a, control, "jobs must see exactly what the seller configured");
    assert_eq!(from_a["tools"][0]["name"], json!("transform-file"));
    // The schema is closed: an undeclared argument has nowhere to hide.
    assert_eq!(from_a["tools"][0]["inputSchema"]["additionalProperties"], json!(false));
}

/// A real MCP JSON-RPC session over stdio, spawned the way an agent spawns an MCP server, with
/// only this job's socket reachable.
#[test]
fn mcp_stdio_session_drives_the_tool() {
    let fx = Fixture::start();
    let root = fx.make_job("job-mcp");
    std::fs::write(root.join("input.txt"), "mcp payload").unwrap();

    let mut mcp = fx.mcp_session("job-mcp");
    let init = mcp.initialize();
    assert_eq!(init["result"]["protocolVersion"], json!("2024-11-05"));
    assert_eq!(init["result"]["serverInfo"]["name"], json!("maxplayer-tool-kit-holder"));

    // A notification draws no reply; if it did, this next id would not match.
    mcp.notify("notifications/initialized", json!({}));

    let listed = mcp.request("tools/list", json!({}));
    assert_eq!(listed["result"]["tools"][0]["name"], json!("transform-file"));

    let called = mcp.request(
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
    );
    assert_eq!(called["result"]["isError"], json!(false));
    assert_eq!(called["result"]["outputs"][0]["path"], json!("out.txt"), "paths are reported job-relative");
    assert_eq!(std::fs::read_to_string(root.join("out.txt")).unwrap(), "MCP PAYLOAD");
    assert_eq!(fx.vendor_stats()["transform_count"], json!(1));
}

/// Restarting the daemon reuses the persisted session instead of logging in again.
#[test]
fn restart_reuses_the_persisted_session() {
    let mut fx = Fixture::start();
    let status = fx.ctl("holder/status", json!({})).unwrap();
    assert_eq!(status["resumed_existing_session"], json!(false), "first start enrols");

    fx.stop_holder();
    fx.start_holder();

    let status = fx.ctl("holder/status", json!({})).unwrap();
    assert_eq!(status["resumed_existing_session"], json!(true));
    assert_eq!(status["enrollments_this_process"], json!(0), "restart must not log in again");
    assert_eq!(status["healthy"], json!(true));
    assert_eq!(fx.vendor_stats()["login_count"], json!(1), "still exactly one login, ever");

    // And it still works after the restart.
    let root = fx.make_job("job-after-restart");
    std::fs::write(root.join("input.txt"), "post restart").unwrap();
    fx.job_call(
        "job-after-restart",
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
    )
    .expect("call after restart");
    assert_eq!(std::fs::read_to_string(root.join("out.txt")).unwrap(), "POST RESTART");
}

/// Availability follows the daemon. Stopping it takes the tool away; nothing else does.
#[test]
fn tool_availability_follows_the_daemon() {
    let mut fx = Fixture::start();
    let root = fx.make_job("job-lifecycle");
    std::fs::write(root.join("input.txt"), "before stop").unwrap();
    fx.job_call(
        "job-lifecycle",
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
    )
    .expect("call while the daemon runs");

    fx.stop_holder();

    // Both endpoints are gone, and the sockets are cleaned up rather than left as litter.
    assert!(!fx.control_socket().exists(), "control socket should be removed on stop");
    assert!(!fx.job_socket("job-lifecycle").exists(), "job socket should be removed on stop");
    let err = fx
        .job_call(
            "job-lifecycle",
            "tools/call",
            json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out2.txt", "mode": "upper"}}),
        )
        .expect_err("the tool must be unavailable once the daemon stops");
    assert!(err.contains("unreachable"), "expected an unreachable endpoint, got: {err}");

    // Starting it again restores availability without a new login.
    fx.start_holder();
    assert_eq!(fx.ctl("holder/status", json!({})).unwrap()["healthy"], json!(true));
    assert_eq!(fx.vendor_stats()["login_count"], json!(1));
}

/// The credential is not reachable from a job's directory, and the session file is private and
/// lives in holder state.
#[test]
fn credential_stays_out_of_job_directories() {
    let fx = Fixture::start();
    let a = fx.make_job("job-a");
    std::fs::write(a.join("input.txt"), "payload").unwrap();
    fx.job_call(
        "job-a",
        "tools/call",
        json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": "upper"}}),
    )
    .unwrap();

    // The session file is where it should be, and only the owner can read it.
    let auth = fx.auth_file();
    assert!(auth.exists(), "the tool should have persisted a session");
    assert_eq!(common::mode_of(&auth), 0o600, "session file must not be group/world readable");
    assert!(!auth.starts_with(&a), "session file must not live inside a job directory");
    assert_eq!(common::mode_of(&fx.state), 0o700, "holder state directory must be private");

    // Nothing under the job directory contains the secret or the session token.
    let secret = common::synthetic_secret();
    let mut checked = 0usize;
    for entry in walk(&a) {
        let bytes = std::fs::read(&entry).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(secret), "{} leaked the client secret", entry.display());
        assert!(!text.contains("sess-"), "{} leaked a session token", entry.display());
        checked += 1;
    }
    assert!(checked >= 2, "expected to have inspected the job's input and output");

    // Job output is confined to the job that produced it.
    let b = fx.make_job("job-b");
    assert!(!b.join("out.txt").exists(), "one job's output must not appear in another's directory");
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}
