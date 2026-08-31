//! `maxplayer __deliver <phase1|phase2> <inputs.json>` — the container-side delivery orchestrator
//! (Track B). INTERNAL, not a user surface: the seller daemon runs this as the sandbox image
//! entrypoint so clone/gate/commit/push happen INSIDE the container, and the host never opens a git
//! repository the job agent could write.
//!
//! - `phase1`: provision the workdir, run the agent as a child (inheriting the container's stdin/
//!   stdout so the host keeps driving ACP), gate + sentinel + commit, and write the delivery oid. No
//!   push credential is present. The inputs file is deleted before the agent runs, so `job_hash` lives
//!   only in memory (B-2).
//! - `phase2`: harden the workdir's git config (whole-file replacement, so a planted `insteadOf`
//!   cannot redirect the push) and push it with the host-minted, branch-scoped token, retrying
//!   transient/conflict failures. No agent runs here, so the token never coexists with job code.
//!
//! The heavy lifting lives in [`maxplayer_core::delivery_orchestrator`]; this is only the argv shim.

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
