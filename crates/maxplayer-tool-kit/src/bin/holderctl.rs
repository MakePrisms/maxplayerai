//! `holderctl` — the seller operator's CLI. Speaks to the control socket only.
//!
//! `attach`/`detach` are seller-side wiring for job isolation: they create and remove a per-job
//! socket. They do not grant, meter or expire anything, and `detach` deliberately reports that
//! the tool stayed enrolled.

use maxplayer_tool_kit::{client, proto};
use serde_json::json;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let socket = PathBuf::from(
        flag(&args, "--socket")
            .or_else(|| std::env::var("HOLDER_CONTROL_SOCKET").ok())
            .unwrap_or_else(|| {
                eprintln!("holderctl: --socket <path> or HOLDER_CONTROL_SOCKET is required");
                std::process::exit(2);
            }),
    );
    let cmd = args.get(1).map(String::as_str).unwrap_or("");

    let (method, params) = match cmd {
        "status" => (proto::METHOD_STATUS, json!({})),
        "health" => (proto::METHOD_HEALTH, json!({})),
        "tools" => (proto::METHOD_TOOLS_LIST, json!({})),
        "reenroll" => ("holder/reenroll", json!({})),
        "shutdown" => (proto::METHOD_SHUTDOWN, json!({})),
        "attach" => {
            let job_id = require(&args, "--job-id");
            let job_root = require(&args, "--job-root");
            ("holder/attach_job", json!({"job_id": job_id, "job_root": job_root}))
        }
        "detach" => {
            let job_id = require(&args, "--job-id");
            ("holder/detach_job", json!({"job_id": job_id}))
        }
        _ => {
            eprintln!("usage: holderctl <status|health|tools|attach|detach|reenroll|shutdown> --socket <path>");
            std::process::exit(2);
        }
    };

    match client::call(&socket, method, params) {
        Ok(result) => {
            println!("{}", serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()));
            // An unhealthy holder is a non-zero exit, so a shell harness can gate on it.
            if result.get("healthy") == Some(&json!(false)) {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("holderctl: {e}");
            std::process::exit(1);
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn require(args: &[String], name: &str) -> String {
    flag(args, name).unwrap_or_else(|| {
        eprintln!("holderctl: {name} <value> is required");
        std::process::exit(2);
    })
}
