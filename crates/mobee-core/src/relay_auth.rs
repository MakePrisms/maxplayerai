//! Shared NIP-42 relay-auth handshake, neutral to any single consumer.
//!
//! mobee-relay requires NIP-42 AUTH for the p-gated kind-1059 subscribe AND for all writes, and the
//! handshake shape is identical on the seller receive path and the buyer receipt-publish path. This
//! module owns the one `wait_for_nip42_auth` both use, so neither depends on the other's error type
//! or lifecycle. Callers map the outcome to their own gate: the seller degrades on `NoChallenge`,
//! the buyer fails closed on anything but `Authenticated`.

use std::time::Duration;

/// Outcome of [`wait_for_nip42_auth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthWait {
    /// The relay issued a NIP-42 challenge and `automatic_authentication` completed it.
    Authenticated,
    /// The relay issued NO challenge within the window. NOT a failure (see the fn doc).
    NoChallenge,
}

/// A fatal NIP-42 handshake failure (active rejection, relay shutdown, or channel closed). Distinct
/// from `NoChallenge`, which is non-fatal. Neutral so no consumer's error type leaks here; callers
/// map it into their own (`DaemonError`, the buyer gate, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAuthError(pub String);

impl std::fmt::Display for RelayAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for RelayAuthError {}

/// Drain `notifications` until the relay's NIP-42 AUTH completes, the relay actively rejects auth
/// (fatal), or the window elapses with no challenge (`NoChallenge`, non-fatal).
///
/// Caller must subscribe `relay.notifications()` **before** `connect` so the `Authenticated` event
/// cannot be missed.
///
/// mobee-relay p-gates kind-1059: unauthenticated `REQ kinds:[1059] #p:self` is `CLOSED` with
/// `restricted:` (not `auth-required:`). nostr-sdk 0.44 treats `restricted:` as `Remove` — the sub
/// is dropped, so a post-auth `resubscribe()` never restores it. Auth **before** the 1059 subscribe
/// is therefore load-bearing for seller receive, and mobee-relay challenges on connect so
/// `Authenticated` arrives in milliseconds.
///
/// A window with NO challenge is reported as `NoChallenge` rather than a fatal error: a relay that
/// challenges only lazily (on the first `REQ`/`EVENT`, e.g. the in-process test relay) will
/// challenge when the caller subscribes, and `automatic_authentication` completes auth then. The
/// caller logs the degrade loudly. An ACTIVE rejection (`AuthenticationFailed`) or a relay shutdown
/// stays fatal (fail-closed), unchanged.
pub async fn wait_for_nip42_auth(
    notifications: &mut tokio::sync::broadcast::Receiver<nostr_sdk::pool::RelayNotification>,
    timeout: Duration,
) -> Result<AuthWait, RelayAuthError> {
    use nostr_sdk::pool::RelayNotification;

    let within_window = tokio::time::timeout(timeout, async {
        loop {
            match notifications.recv().await {
                Ok(RelayNotification::Authenticated) => return Ok(AuthWait::Authenticated),
                Ok(RelayNotification::AuthenticationFailed) => {
                    return Err(RelayAuthError(
                        "NIP-42 authentication failed (required for kind-1059 p-gated receive)"
                            .into(),
                    ));
                }
                Ok(RelayNotification::Shutdown) => {
                    return Err(RelayAuthError(
                        "relay shutdown before NIP-42 authentication".into(),
                    ));
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(RelayAuthError(
                        "relay notification channel closed before NIP-42 authentication".into(),
                    ));
                }
            }
        }
    })
    .await;

    // Elapsed with no challenge → NoChallenge (non-fatal). Within the window → the loop's result
    // (Authenticated, or a fatal active failure).
    within_window.unwrap_or(Ok(AuthWait::NoChallenge))
}
