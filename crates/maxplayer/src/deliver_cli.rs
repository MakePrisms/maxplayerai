//! `maxplayer __deliver <phase1|phase2> <inputs.json>` — the container-side delivery orchestrator
//! (Track B). INTERNAL, not a user surface: the seller daemon runs this as the sandbox image
//! entrypoint so clone/gate/commit/push happen INSIDE the container, and the host never opens a git
//! repository the job agent could write.
//!
//! - `phase1`: the ONE-container delivery. Reads and DELETES the inputs file (C3), provisions the
//!   workdir, DRIVES the ACP agent itself (Task B9: no nested docker, an explicit environment
//!   allowlist), gates + sentinels + commits, reaps every other process, obtains the branch-scoped
//!   push token per the inputs' token source, pushes with the gated oid as the expected oid (C6), and
//!   writes the oid and an outcome file for the host. Exit 0 only when the push succeeded.
//! - `phase2`: the older push-only leg (harden the config, push with a host-minted token). Kept for
//!   the split-container shape; the seller daemon does not launch it.
//!
//! The heavy lifting lives in [`maxplayer_core::delivery_orchestrator`]; this is only the argv shim.
//! `phase1` builds its own current-thread Tokio runtime inside the core entry, because this CLI is
//! synchronous.

use std::io::Write;

/// Entry point for `maxplayer __deliver …`. `args` is everything after `__deliver`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    #[cfg(not(feature = "wallet"))]
    {
        let _ = (args, out);
        let _ = writeln!(err, "__deliver requires a wallet build (git-delivery is absent)");
        return 2;
    }
    #[cfg(feature = "wallet")]
    {
        use maxplayer_core::delivery_orchestrator as orch;
        const SUCCESS: i32 = 0;
        const USAGE_ERROR: i32 = 1;
        const RUNTIME_ERROR: i32 = 2;

        let phase = args.first().map(String::as_str);
        let inputs = args.get(1).map(std::path::Path::new);
        match (phase, inputs) {
            (Some("phase1"), Some(path)) => match orch::run_phase1_entry(path) {
                Ok(output) => {
                    let _ = writeln!(out, "{}", output.delivery_oid);
                    SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "__deliver phase1 failed: {error}");
                    RUNTIME_ERROR
                }
            },
            (Some("phase2"), Some(path)) => match orch::run_phase2_entry(path) {
                Ok(oid) => {
                    let _ = writeln!(out, "{oid}");
                    SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "__deliver phase2 failed: {error}");
                    RUNTIME_ERROR
                }
            },
            _ => {
                let _ = writeln!(err, "usage: maxplayer __deliver <phase1|phase2> <inputs.json>");
                USAGE_ERROR
            }
        }
    }
}
