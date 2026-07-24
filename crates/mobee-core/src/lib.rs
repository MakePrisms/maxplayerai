pub mod announce;
#[cfg(all(feature = "wallet", feature = "gateway"))]
pub mod authorize_pay;
pub mod budget;
#[cfg(feature = "wallet")]
pub mod collect;
#[cfg(all(feature = "wallet", feature = "gateway"))]
pub mod job_lifecycle;
#[cfg(all(feature = "wallet", feature = "gateway"))]
pub mod profile;
pub mod contribution;
pub mod delivery;
#[cfg(feature = "git-delivery")]
pub mod delivery_git;
#[cfg(feature = "git-delivery")]
pub mod git_transport;
#[cfg(feature = "wallet")]
pub mod doctor;
pub mod delivery_transport;
pub mod driver;
pub mod durable;
pub mod engine;
pub mod episode;
pub mod event;
pub mod format;
pub mod gateway;
pub mod heartbeat;
pub mod home;
pub mod kinds;
pub mod log;
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
pub mod runtime_guard;
pub mod seller;
#[cfg(feature = "wallet")]
pub mod seller_daemon;
/// Persistent per-home seller node (exclusive lock, receiving wallet + identity behind serialized
/// actors, durable lifecycle store with a nostr event outbox, single relay ingester, agent roster).
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
