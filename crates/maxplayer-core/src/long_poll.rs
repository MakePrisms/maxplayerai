//! Long-poll bounds for the `get_job(wait_for=…)` surface.
//!
//! This module exists to be reachable WITHOUT the `wallet` feature. The cap below is consumed by
//! two places that do not share a feature set: [`crate::job_lifecycle`] and [`crate::buyer`], which
//! are gated on `wallet` (+ `gateway`), and the CLI crate's MCP tool table, which is not — a
//! no-wallet build still advertises the tool list and refuses the calls at `route_tool`. Defining
//! the cap inside a `wallet`-gated module therefore made an ungated caller reference a gated item,
//! which is exactly the break #658 shipped (`use maxplayer_core::job_lifecycle` from `mcp.rs`).
//!
//! Keeping it here removes that coupling rather than guarding it: there is no longer a gated path
//! for an ungated caller to reach through.

/// Cap for `get_job(wait_for=…)` long-poll. Must stay < MCP tool deadline (~15s) so
/// cap-hit returns PENDING for re-poll instead of starving the client read-timeout (~60s).
pub const WAIT_FOR_CAP_SECS: u64 = 10;
