//! Network-probe primitives for the `mobee doctor` seller self-check.
//!
//! The CLI orchestration + human output lives in the `mobee` binary crate (`mobee doctor`); the
//! network probes live here because their clients — the relay client (nostr-sdk) and the mint
//! client (cdk) — are `mobee-core` dependencies the binary crate does not carry.
//!
//! Diagnostic only: nothing here touches the pay gate, the journal, or the receipt bind.

use std::time::Duration;

/// Result of [`probe_relay`]: how the relay handshake resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayProbe {
    /// Connected and the relay completed the NIP-42 AUTH handshake.
    Authenticated,
    /// Connected, but the relay issued no NIP-42 challenge within the window. Non-fatal — mirrors
    /// the daemon's `NoChallenge`: `automatic_authentication` stays on, so a relay that challenges
    /// on the first REQ still authenticates the live daemon.
    ConnectedNoChallenge,
}

/// Connect to `relay_url` with the seller key and wait out the NIP-42 auth handshake — the same
/// connect+auth sequence the seller daemon runs at boot, so a PASS here means the daemon would
/// authenticate too. Returns `Err` on a bad key, an unreachable relay, or an active auth rejection.
pub async fn probe_relay(
    relay_url: &str,
    secret_key_hex: &str,
    timeout: Duration,
) -> Result<RelayProbe, String> {
    use nostr_sdk::pool::RelayNotification;
    use nostr_sdk::prelude::{Client, Keys, RelayUrl};

    let keys = Keys::parse(secret_key_hex).map_err(|error| format!("key parse: {error}"))?;
    let client = Client::new(keys);
    // Seller receive depends on this; match the daemon so the probe exercises the same path.
    client.automatic_authentication(true);
    client
        .add_relay(relay_url)
        .await
        .map_err(|error| format!("add relay: {error}"))?;

    let parsed = RelayUrl::parse(relay_url).map_err(|error| format!("parse relay url: {error}"))?;
    let relay = client
        .relays()
        .await
        .get(&parsed)
        .cloned()
        .ok_or_else(|| "relay missing after add_relay".to_string())?;

    // Subscribe BEFORE connect — `Authenticated` is not re-emitted (daemon invariant).
    let mut notifications = relay.notifications();
    relay
        .try_connect(timeout)
        .await
        .map_err(|error| format!("could not connect: {error}"))?;

    let auth = tokio::time::timeout(timeout, async {
        loop {
            match notifications.recv().await {
                Ok(RelayNotification::Authenticated) => return Ok(RelayProbe::Authenticated),
                Ok(RelayNotification::AuthenticationFailed) => {
                    return Err(
                        "NIP-42 authentication failed (required for kind-1059 p-gated receive)"
                            .to_string(),
                    );
                }
                Ok(RelayNotification::Shutdown) => {
                    return Err("relay shut down before NIP-42 authentication".to_string());
                }
                Ok(_) => {}
                Err(_) => {
                    return Err("relay notification channel closed before auth".to_string());
                }
            }
        }
    })
    .await;

    let outcome = auth.unwrap_or(Ok(RelayProbe::ConnectedNoChallenge));
    client.disconnect().await;
    outcome
}

/// GET `{mint_url}/v1/info` (via the cdk mint client) within `timeout`. `Ok` iff the mint answers a
/// well-formed info document — the same reachability the wallet relies on before any mint op.
pub async fn probe_mint(mint_url: &str, timeout: Duration) -> Result<(), String> {
    use cdk::wallet::{HttpClient, MintConnector};
    use std::str::FromStr;

    let url = cashu::MintUrl::from_str(mint_url.trim())
        .map_err(|error| format!("invalid mint url: {error}"))?;
    let client = HttpClient::new(url, None);
    tokio::time::timeout(timeout, client.get_mint_info())
        .await
        .map_err(|_| format!("mint did not respond within {timeout:?}"))?
        .map_err(|error| format!("mint info request failed: {error}"))?;
    Ok(())
}
