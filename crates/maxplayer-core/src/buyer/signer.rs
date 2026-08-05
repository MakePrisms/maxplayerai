//! The signer actor — the buyer's single owner of the Nostr identity.
//!
//! The buyer key is read from `$MOBEE_HOME/key` once at startup and lives only
//! inside this task. Marketplace-event signing (awards, receipts) routes through
//! the queue in later phases so there is one signing principal per home and the
//! secret never leaves the actor. Step 1 exposes the public key; the secret is
//! never sent over the socket or returned to a client.

use nostr_sdk::Keys;
use tokio::sync::{mpsc, oneshot};

use crate::home::{self, HomeError, MaxplayerHome};

enum Command {
    /// Return the buyer public key (hex). Safe to expose; not secret material.
    PublicKey {
        reply: oneshot::Sender<String>,
    },
}

/// A cheap, cloneable handle to the signer actor.
#[derive(Clone)]
pub struct SignerHandle {
    tx: mpsc::Sender<Command>,
    /// Cached once at spawn so `status` need not round-trip for the common read.
    public_key_hex: String,
}

/// How long a single signer round-trip may take before it is abandoned.
///
/// Deliberately generous: everything the actor does is local cryptography measured in milliseconds,
/// so this is not a latency budget — it is a liveness bound. Its job is to guarantee the call
/// *returns*, not to police how fast.
const SIGNER_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A signer round-trip that did not complete: the actor exited, or it failed to answer within
/// [`SIGNER_CALL_TIMEOUT`]. Carries which call and which leg, so the operator log names the exact
/// site instead of a bare "actor gone".
#[derive(Debug)]
pub struct SignerActorGone {
    call: &'static str,
    cause: &'static str,
}

impl std::fmt::Display for SignerActorGone {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "signer round-trip `{}` did not complete: {}",
            self.call, self.cause
        )
    }
}

impl std::error::Error for SignerActorGone {}

impl SignerHandle {
    /// The buyer public key (hex), served from the cache set at spawn.
    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    /// Send one command and await its reply, with BOTH legs bounded.
    ///
    /// Both legs must be bounded because both are timer-less, and a timer-less await is the one
    /// thing that can park this daemon permanently and silently (#173). `send` parks forever if the
    /// queue is full and the actor is not draining it; `rx.await` parks forever if the actor is
    /// alive but never answers. Neither arms a timer, so the runtime has nothing to wake for — a
    /// caller reached from a `select!` branch body takes the whole loop down with it, at 0% CPU,
    /// with no error logged anywhere.
    ///
    /// A bound cannot make a stuck actor answer. What it does is convert an invisible permanent
    /// park into a named, logged, recoverable failure at this exact call site.
    async fn round_trip<T>(
        &self,
        call: &'static str,
        command: Command,
        rx: oneshot::Receiver<T>,
    ) -> Result<T, SignerActorGone> {
        match tokio::time::timeout(SIGNER_CALL_TIMEOUT, self.tx.send(command)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(SignerActorGone {
                    call,
                    cause: "actor exited",
                })
            }
            Err(_) => {
                return Err(SignerActorGone {
                    call,
                    cause: "queue stayed full (actor not draining)",
                })
            }
        }
        match tokio::time::timeout(SIGNER_CALL_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(SignerActorGone {
                call,
                cause: "actor dropped the reply",
            }),
            Err(_) => Err(SignerActorGone {
                call,
                cause: "actor never answered",
            }),
        }
    }

    /// The buyer public key (hex), routed through the actor queue. Proves the
    /// serialized signer path end to end (later phases sign over this same slot).
    pub async fn public_key_via_actor(&self) -> Result<String, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.round_trip("public_key", Command::PublicKey { reply }, rx)
            .await
    }
}

/// Load the buyer key from `home` and spawn the signer actor. The secret is
/// consumed into the task and never held elsewhere.
pub fn spawn(home: &MaxplayerHome) -> Result<SignerHandle, HomeError> {
    let secret = home::read_secret_key_hex(home)?;
    let keys = Keys::parse(&secret)
        .map_err(|error| HomeError::Key(format!("signer key parse: {error}")))?;
    let public_key_hex = keys.public_key().to_hex();

    let (tx, mut rx) = mpsc::channel::<Command>(64);
    tokio::spawn(async move {
        // `keys` (holding the secret) lives only inside this task.
        while let Some(command) = rx.recv().await {
            match command {
                Command::PublicKey { reply } => {
                    let _ = reply.send(keys.public_key().to_hex());
                }
            }
        }
    });

    Ok(SignerHandle {
        tx,
        public_key_hex,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::bootstrap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-buyer-signer-{label}-{}-{id}", std::process::id()))
    }

    // TOOTH (#173) — a signer round-trip is BOUNDED, so an actor that never answers cannot park the
    // caller forever. The daemon reaches this handle from tasks that also own the trade loop, so a
    // timer-less await here is not a slow call: it is a task that is never polled again, sitting at
    // 0% CPU with nothing logged. The diagnostic that identifies the class: a permanent park proves
    // no timer was pending, which excludes every BOUNDED await and leaves exactly these timer-less
    // channel awaits.
    //
    // The stalled actor here holds each command — and therefore each reply sender — forever, which
    // is the one shape that hangs: dropping the sender would surface as a recv error, not a park.
    //
    // Time is paused, so the production bound elapses instantly in wall-clock. The OUTER timeout is
    // what makes a revert fail cleanly instead of hanging the suite: remove the bound in
    // `round_trip` and there is no timer at 30s, auto-advance jumps to the outer 600s, and the
    // assert goes red.
    //
    // Two calls, not one: the second proves the caller was left usable rather than merely returning
    // once — which is the property a long-lived daemon task actually needs to keep serving.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_signer_round_trip_is_bounded_and_leaves_the_caller_usable() {
        let (tx, mut rx) = mpsc::channel::<Command>(8);
        // The stalled actor: receive, then hold. Never answers, never drops a reply sender.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(command) = rx.recv().await {
                held.push(command);
            }
        });
        let handle = SignerHandle {
            tx,
            public_key_hex: "00".repeat(32),
        };

        let outer = std::time::Duration::from_secs(600);
        for attempt in 1..=2 {
            let call = tokio::time::timeout(outer, handle.public_key_via_actor());
            let outcome = call.await.unwrap_or_else(|_| {
                panic!(
                    "attempt {attempt}: the signer round-trip never returned — an unbounded \
                     timer-less await here parks the calling task permanently and silently"
                )
            });
            let error = outcome.expect_err("a stalled actor cannot produce a public key");
            assert!(
                error.to_string().contains("public_key")
                    && error.to_string().contains("never answered"),
                "attempt {attempt}: the failure must NAME the call and the leg so an operator can \
                 see which round-trip stalled, got {error}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actor_serves_pubkey_and_never_the_secret() {
        let root = temp_home("pubkey");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let secret = home::read_secret_key_hex(&home).expect("secret");

        let signer = spawn(&home).expect("spawn signer");
        let cached = signer.public_key_hex().to_owned();
        let via_actor = signer.public_key_via_actor().await.expect("pubkey");
        assert_eq!(cached, via_actor);
        assert_eq!(cached.len(), 64);
        assert_ne!(cached, secret, "public key must never equal the secret");

        let _ = std::fs::remove_dir_all(&root);
    }
}
