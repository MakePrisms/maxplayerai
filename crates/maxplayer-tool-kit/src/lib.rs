//! Seller-level tool holder.
//!
//! The shape this crate implements, and the one correction that defines it:
//!
//! > "the seller is defined by it's offering, there is no offering per job, it is per seller,
//! > tool should at all times be active together with the seller daemon" — Petar, 2026-09-09
//!
//! So: the offering and the allowed operation list are **seller configuration**. The tool is
//! enrolled once and stays logged in for as long as the daemon runs. No award, payment, job
//! start or job completion opens, closes, or renews anything. Jobs of that seller call an
//! interface that is already up.
//!
//! What this crate does still enforce, because none of it is an entitlement question:
//! credentials stay outside buyer-controlled job containers; there is no shell and no argv
//! passthrough; operation parameters are validated against the seller's declared list; file
//! access is confined to the calling job's own directory; and the endpoint answers only the
//! seller's authorized clients.
//!
//! Availability is intended while the daemon runs. That is not a guarantee against vendor
//! failure, and confining *parameters* is not protection against a seller authorizing an
//! operation whose legitimate use a buyer then directs.

pub mod client;
pub mod config;
pub mod http;
pub mod mcp_bridge;
pub mod proto;
pub mod safeio;
pub mod validate;

use std::fmt;

/// A synthetic credential. No `Debug`, no `Display`, no `Serialize` — the only way out is
/// [`Secret::expose`], which is greppable in review.
///
/// Modelled on `codex_subscription.rs`'s no-`Debug` token type, for the same reason: the
/// commonest way a secret reaches a log is a struct that derives `Debug` two refactors later.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Deliberately verbose. Every call site should be visible in a diff.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Health of the seller's tool. Reported by the daemon; not a promise about the vendor.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum Health {
    /// Enrolled and the last vendor check succeeded.
    Healthy,
    /// Daemon is up but the tool cannot serve — auth rejected, vendor unreachable, not enrolled.
    /// Carries a reason safe to show a seller operator; never a credential.
    Unhealthy(String),
}

impl Health {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Health::Healthy)
    }
}
