//! Shared test-only helpers.
//!
//! Currently just one: a `LocalRelay` starter that retries the upstream `find_available_port`
//! race. See [`start_relay`] for the mechanism. Gated on `feature = "wallet"` because every
//! current caller lives behind that feature (`collect`, `job_lifecycle`, `buyer`,
//! `seller_node`) — widen the `cfg` on the `mod test_support;` declaration in `lib.rs` if a
//! caller outside that feature set shows up.
#![cfg(test)]

// NOTE: imported from the crate root, not `prelude` — `prelude` glob-re-exports `Error` from
// multiple sources (`nostr::prelude::*` also defines one) and resolves ambiguously; the crate
// root's `Error` (`pub use self::error::Error;` in `nostr-relay-builder`'s `lib.rs`) is the
// single unambiguous binding for the type actually returned by `LocalRelay::run()`.
use nostr_relay_builder::{Error as RelayBuilderError, LocalRelay, RelayBuilder};

/// Number of attempts before giving up on starting a `LocalRelay`.
const START_RELAY_MAX_ATTEMPTS: u32 = 5;

/// Start a `LocalRelay`, retrying with a freshly built relay when the bind loses the port race
/// in `nostr-relay-builder`'s `find_available_port` (0.44.1, `local/util.rs`): it probes a port
/// by binding a `TcpListener` purely to observe success, then drops the listener immediately —
/// the port is unheld from that instant. `LocalRelay::run()` binds the real listener later, and
/// anything (including another `LocalRelay` racing the same probe) can steal the port in
/// between. See maxplayerai#363.
///
/// `builder_fn` is called once per attempt so a retry gets a fresh `RelayBuilder` — and
/// therefore a fresh `find_available_port` call / a newly-picked random port — rather than
/// re-binding the exact address the previous attempt already lost. Retrying `run()` on the SAME
/// `LocalRelay` would not help: its address is resolved once into a `OnceCell` on first `run()`,
/// so a second call on the same instance would keep re-trying the same already-lost port.
///
/// Panics (matching the `.expect("relay run")` call sites this replaces) if every attempt fails,
/// or if `run()` fails for a reason other than `AddrInUse`.
pub(crate) async fn start_relay(builder_fn: impl Fn() -> RelayBuilder) -> LocalRelay {
    for attempt in 1..=START_RELAY_MAX_ATTEMPTS {
        let relay = LocalRelay::new(builder_fn());
        match relay.run().await {
            Ok(()) => return relay,
            Err(RelayBuilderError::IO(e))
                if e.kind() == std::io::ErrorKind::AddrInUse
                    && attempt < START_RELAY_MAX_ATTEMPTS =>
            {
                continue;
            }
            Err(e) => panic!("relay run: {e}"),
        }
    }
    unreachable!("loop above always returns Ok or panics on its final attempt")
}
