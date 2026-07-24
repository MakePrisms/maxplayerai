//! The serialized wallet actor — the node's single owner of the RECEIVING cdk wallet.
//!
//! One task owns the [`Wallet`] and pulls commands from an mpsc queue, servicing exactly one at a
//! time. Because the receiver loop `await`s each command to completion before the next `recv`,
//! wallet operations are serialized by construction: no two proof-changing operations can ever
//! overlap, even under many concurrent collectors. This is the money-safe boundary for collecting
//! K receipts — unsynchronized wallet mutation, not concurrent receipts, is the hazard, and this
//! queue removes it.
//!
//! This exposes `balance` (a read). Redeeming a receipt's proofs is a proof-changing op that lands
//! behind this same single slot; the exactly-once credit it must honor is already enforced upstream
//! by the store's unique `receipt_id` dedup ([`super::store::SellerStore::collect_receipt`]).
//!
//! Concurrency here is deliberately 1. The queue — not SQLite locking — is the concurrency boundary
//! inside the process, mirroring the home lock across processes.

use cdk::wallet::Wallet;
use tokio::sync::{mpsc, oneshot};

enum Command {
    /// Read the total spendable balance (sats) at the wallet's bound mint.
    Balance {
        reply: oneshot::Sender<Result<u64, String>>,
    },
    /// Test-only serialization probe: reports the max concurrent in-flight count observed. A
    /// single-task actor always observes 1.
    #[cfg(test)]
    Probe {
        reply: oneshot::Sender<usize>,
    },
}

/// A cheap, cloneable handle to the wallet actor.
#[derive(Clone)]
pub struct WalletHandle {
    tx: mpsc::Sender<Command>,
}

/// The actor stopped accepting work (the owning task exited).
#[derive(Debug)]
pub struct WalletActorGone;

impl std::fmt::Display for WalletActorGone {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "wallet actor is not running")
    }
}

impl std::error::Error for WalletActorGone {}

impl WalletHandle {
    /// Total spendable balance (sats). Serialized behind the actor queue.
    pub async fn balance(&self) -> Result<Result<u64, String>, WalletActorGone> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Balance { reply })
            .await
            .map_err(|_| WalletActorGone)?;
        rx.await.map_err(|_| WalletActorGone)
    }

    #[cfg(test)]
    async fn probe(&self) -> Result<usize, WalletActorGone> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Probe { reply })
            .await
            .map_err(|_| WalletActorGone)?;
        rx.await.map_err(|_| WalletActorGone)
    }
}

/// Spawn the actor, moving `wallet` into the owning task. The returned handle is the only way to
/// reach the wallet.
pub fn spawn(wallet: Wallet) -> WalletHandle {
    // A small bounded queue: callers await a slot, which applies backpressure rather than growing
    // an unbounded backlog.
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    tokio::spawn(async move {
        #[cfg(test)]
        let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        while let Some(command) = rx.recv().await {
            match command {
                Command::Balance { reply } => {
                    let result = wallet
                        .total_balance()
                        .await
                        .map(|amount| amount.to_u64())
                        .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
                #[cfg(test)]
                Command::Probe { reply } => {
                    use std::sync::atomic::Ordering;
                    let observed = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    tokio::task::yield_now().await;
                    let peak = in_flight.load(Ordering::SeqCst).max(observed);
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    let _ = reply.send(peak);
                }
            }
        }
    });
    WalletHandle { tx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buyer_fund;
    use crate::home::bootstrap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-wactor-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_are_serialized_through_one_slot() {
        let root = temp_home("serial");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        // Opening the wallet touches only the local sqlite store (no network); a fresh home has an
        // empty wallet.
        let wallet = buyer_fund::open_wallet_async(&home).await.expect("open wallet");
        let handle = spawn(wallet);

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let handle = handle.clone();
            set.spawn(async move { handle.probe().await.expect("probe") });
        }
        let mut peak = 0usize;
        while let Some(result) = set.join_next().await {
            peak = peak.max(result.expect("join"));
        }
        assert_eq!(peak, 1, "wallet actor must process exactly one op at a time");

        let balance = handle.balance().await.expect("actor alive").expect("balance");
        assert_eq!(balance, 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
