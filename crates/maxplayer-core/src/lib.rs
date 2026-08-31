pub mod agent_presets;
pub mod announce;
#[cfg(all(feature = "wallet", feature = "gateway"))]
pub mod authorize_pay;
pub mod budget;
// Unconditional on purpose: a build that emits or reads a filterable field must be able to name the
// token set that field is bound to, so the vocabulary cannot sit behind a feature. The one item that
// needs an executor — `probe_capabilities`, which takes a `wallet`-gated `SandboxPolicy` — carries
// the gate itself rather than dragging the whole module behind it.
pub mod capability;
pub mod checks;
#[cfg(feature = "wallet")]
pub mod collect;
#[cfg(all(feature = "wallet", feature = "gateway"))]
pub mod job_lifecycle;
#[cfg(all(feature = "wallet", feature = "gateway"))]
pub mod profile;
pub mod contribution;
#[cfg(all(feature = "wallet", feature = "gateway"))]
pub mod crossmint;
#[cfg(all(feature = "wallet", feature = "gateway"))]
pub mod crossmint_hop;
pub mod delivery;
pub mod delivery_sentinel;
#[cfg(feature = "git-delivery")]
pub mod delivery_git;
#[cfg(feature = "git-delivery")]
pub mod git_transport;
#[cfg(feature = "git-delivery")]
pub mod delivery_orchestrator;
#[cfg(feature = "git-delivery")]
mod store_maint;
#[cfg(feature = "wallet")]
pub mod doctor;
pub mod delivery_transport;
pub mod driver;
pub mod durable;
pub mod engine;
pub mod env_provision;
pub mod episode;
pub mod event;
pub mod format;
pub mod gateway;
pub mod heartbeat;
pub mod home;
pub mod kinds;
pub mod log;
// Ungated on purpose: the CLI's MCP tool table reads the long-poll cap from here on a build with
// no `wallet` feature, where `job_lifecycle` is compiled out.
pub mod long_poll;
pub mod oplog;
#[cfg(feature = "wallet")]
pub mod buyer_fund;
/// Persistent per-home buyer daemon (exclusive lock, unix-socket RPC, wallet/identity
/// behind serialized actors, durable state DB). See [`buyer`].
// NOTE: the wallet/buyer feature-flag structure is under review in issue #133 —
// do not restructure the flags here (that is #133's job).
#[cfg(feature = "wallet")]
pub mod buyer;
#[cfg(feature = "wallet")]
pub mod wallet_ops;
#[cfg(feature = "wallet")]
pub mod payment;
pub mod payment_send;
#[cfg(feature = "wallet")]
pub mod payment_wallet;
pub mod receipt;
/// Shared NIP-42 relay-auth handshake, neutral to any single consumer (seller receive + buyer
/// receipt-publish both use it).
#[cfg(feature = "gateway")]
pub mod relay_auth;
pub mod runtime_guard;
/// Host-side network containment for a docker job (#797): which destinations a job may reach, and
/// the `iptables` rules that enforce it on the two chains container traffic actually splits across.
///
/// Deliberately UNGATED, unlike [`seller_exec`] and [`credential_proxy`] which it serves. Those are
/// `wallet`-only, so a default-features test run cannot execute a line of them — the policy is the
/// part that decides what a stranger's job can reach, and it is compiled and tested on every build
/// rather than only on the money-path one.
pub mod sandbox_net;
/// Putting `sandbox_net`'s policy in force: the holder container that owns the job's network
/// namespace, and the sidecar that installs the rules into it before the job exists. Unconditional
/// for the same reason as the renderer above — the argv that grants `NET_ADMIN` and the one that
/// contains the job are decided here, so they are compiled and tested on every build.
pub mod sandbox_netns;
pub mod seller;
/// The seller's harness registry: the agent harnesses one node enables, what it advertises for
/// them, and which one a given job dispatches to.
pub mod seller_agents;
/// Agent-run + delivery-shaping helpers for the seller node: run the awarded agent, compose its
/// delivery prompt, derive the delivery kind, and shape the public exec-metadata block. Kept in its
/// own module (with a neutral error) so the run loop stays focused on the relay surface.
#[cfg(feature = "wallet")]
pub mod seller_exec;
/// Host-side credential-containment proxy (#647): the real model credential never enters a
/// docker-mode job's container. A per-job placeholder is forwarded in its place and substituted for
/// the real value at egress, only for an allowlisted upstream. Gated to `wallet` like its sole caller
/// [`seller_exec`].
///
/// ⚠ **Two feature sets skip code here, in opposite directions, and both print `Finished`.**
/// `--features acp` alone compiles neither this module nor [`seller_exec`], because the gate is
/// `wallet`, so a syntax error in either stays invisible behind a green build. `--features wallet`
/// alone compiles both files but skips every `acp`-gated item inside them — the containment launch
/// path and the tests that exercise it — and it is a real CI row, not a hypothetical: the money-path
/// job runs `wallet` without `acp`. A test that calls an `acp`-gated helper therefore needs
/// `#[cfg(feature = "acp")]` of its own; without it that row fails to compile while every set you
/// are likely to build by hand stays green.
///
/// An exit status has no access to "did my code compile" — the **test count** does: this crate
/// reports ~327 tests under `acp` and ~1062 under `wallet`. Containment lives at the intersection, so
/// build `--features acp,gateway,git-delivery,wallet` (CI's feature-union row) and check the
/// denominator, not the status. A test filter that matches 0 tests is a question, never "no tests for
/// that yet".
#[cfg(feature = "wallet")]
pub mod credential_proxy;
/// Host-only ChatGPT session parsing for the Docker Codex credential proxy.
#[cfg(feature = "wallet")]
pub mod codex_subscription;
/// Which of a node's resolved harnesses are serving right now: the live availability layer over the
/// boot registry, so a harness that cannot deliver stops being advertised and stops attracting awards.
pub mod seller_roster;
/// Persistent per-home seller node (exclusive lock, receiving wallet + identity behind serialized
/// actors, durable lifecycle store with a nostr event outbox, single relay ingester).
/// The durable substrate the live seller loop moves onto; mirrors the buyer daemon's shape. The
/// wallet/node feature-flag structure is under review in issue #133 — do not restructure the flags
/// here (that is #133's job).
#[cfg(feature = "wallet")]
pub mod seller_node;
pub mod seller_memory;
pub mod telemetry;
#[cfg(feature = "git-delivery")]
pub mod seller_git;
#[cfg(feature = "wallet")]
pub mod wallet;

pub use event::{Envelope, Event};
pub use log::{EventLog, LogError, ReadError, Replay};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
