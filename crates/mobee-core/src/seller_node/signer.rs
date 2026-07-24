//! The signer actor — the node's single owner of the seller Nostr identity.
//!
//! The seller key is read from `$MOBEE_HOME/key` once at startup and lives only inside this task.
//! Every published event is signed here, through the queue, so there is exactly one signing
//! principal per home and the secret never leaves the actor — no agent process, no client, ever
//! sees it. The outbox publisher hands drafts to [`SignerHandle::sign`]; the fixed authored-at
//! second it passes makes the resulting event id deterministic, so a re-publish after a crash is
//! idempotent at the relay.

use nostr_sdk::{JsonUtil, Keys, Timestamp};
use tokio::sync::{mpsc, oneshot};

use crate::gateway::{self, EventDraft};
use crate::home::{self, HomeError, MobeeHome};

/// A signed event, ready to publish.
#[derive(Debug, Clone)]
pub struct SignedEvent {
    pub id: String,
    pub json: String,
}

enum Command {
    PublicKey {
        reply: oneshot::Sender<String>,
    },
    /// Sign a full event `draft` (kind + content + protocol/routing tags) authored at the fixed
    /// `created_at` second (deterministic id for crash-idempotent re-publish).
    Sign {
        draft: EventDraft,
        created_at: i64,
        reply: oneshot::Sender<Result<SignedEvent, String>>,
    },
}

/// A cheap, cloneable handle to the signer actor.
#[derive(Clone)]
pub struct SignerHandle {
    tx: mpsc::Sender<Command>,
    /// Cached once at spawn so the common read need not round-trip.
    public_key_hex: String,
}

/// The signer task exited.
#[derive(Debug)]
pub struct SignerActorGone;

impl std::fmt::Display for SignerActorGone {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "signer actor is not running")
    }
}

impl std::error::Error for SignerActorGone {}

impl SignerHandle {
    /// The seller public key (hex), served from the cache set at spawn.
    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    /// Sign a full event draft through the serialized signer. `created_at` is the fixed authored-at
    /// second so the event id is deterministic across retries. The draft's tags (version, namespace,
    /// routing) are applied verbatim, so the signed event is wire-valid.
    pub async fn sign(
        &self,
        draft: EventDraft,
        created_at: i64,
    ) -> Result<Result<SignedEvent, String>, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Sign {
                draft,
                created_at,
                reply,
            })
            .await
            .map_err(|_| SignerActorGone)?;
        rx.await.map_err(|_| SignerActorGone)
    }

    /// The seller public key (hex), routed through the actor queue (proves the serialized path).
    pub async fn public_key_via_actor(&self) -> Result<String, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::PublicKey { reply })
            .await
            .map_err(|_| SignerActorGone)?;
        rx.await.map_err(|_| SignerActorGone)
    }
}

/// Load the seller key from `home` and spawn the signer actor. The secret is consumed into the task
/// and never held elsewhere.
pub fn spawn(home: &MobeeHome) -> Result<SignerHandle, HomeError> {
    let secret = home::read_secret_key_hex(home)?;
    let keys =
        Keys::parse(&secret).map_err(|error| HomeError::Key(format!("signer key parse: {error}")))?;
    let public_key_hex = keys.public_key().to_hex();

    let (tx, mut rx) = mpsc::channel::<Command>(64);
    tokio::spawn(async move {
        // `keys` (holding the secret) lives only inside this task.
        while let Some(command) = rx.recv().await {
            match command {
                Command::PublicKey { reply } => {
                    let _ = reply.send(keys.public_key().to_hex());
                }
                Command::Sign {
                    draft,
                    created_at,
                    reply,
                } => {
                    let result = sign_event(&keys, &draft, created_at);
                    let _ = reply.send(result);
                }
            }
        }
    });

    Ok(SignerHandle {
        tx,
        public_key_hex,
    })
}

fn sign_event(keys: &Keys, draft: &EventDraft, created_at: i64) -> Result<SignedEvent, String> {
    // Reuse the canonical draft→builder conversion so the tags (version, namespace, routing) are
    // applied exactly as the rest of the protocol builds them — no hand-rolled tag handling.
    let event = gateway::nostr::event_builder(draft)
        .map_err(|error| error.to_string())?
        .custom_created_at(Timestamp::from(created_at.max(0) as u64))
        .sign_with_keys(keys)
        .map_err(|error| error.to_string())?;
    let json = event.try_as_json().map_err(|error| error.to_string())?;
    Ok(SignedEvent {
        id: event.id.to_hex(),
        json,
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
        std::env::temp_dir().join(format!(
            "mobee-seller-signer-{label}-{}-{id}",
            std::process::id()
        ))
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

    fn claim_draft() -> EventDraft {
        gateway::claim_draft(&"e".repeat(64), &"b".repeat(64), &"s".repeat(64), "creqA-test")
    }

    // The fixed created_at makes the signed event id deterministic — the property the outbox relies
    // on for crash-idempotent re-publish.
    #[tokio::test(flavor = "current_thread")]
    async fn signing_is_deterministic_for_a_fixed_created_at() {
        let root = temp_home("determinism");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let secret = home::read_secret_key_hex(&home).expect("secret");
        let signer = spawn(&home).expect("spawn");

        let first = signer.sign(claim_draft(), 1000).await.expect("a").expect("sign");
        let again = signer.sign(claim_draft(), 1000).await.expect("b").expect("sign");
        assert_eq!(first.id, again.id, "same draft + created_at ⇒ same event id");
        assert!(!first.json.contains(&secret), "signed json must not leak the secret");
        let _ = std::fs::remove_dir_all(&root);
    }

    // A signed event carries the protocol tags a live buyer requires (`parse_offer` rejects an event
    // without `["v","0"]` / `["t","mobee"]`). Proves the outbox→signer path emits wire-valid events.
    #[tokio::test(flavor = "current_thread")]
    async fn signed_event_carries_the_protocol_tags() {
        use crate::gateway::{MOBEE_TAG, PROTOCOL_VERSION};
        let root = temp_home("wire-valid");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let signer = spawn(&home).expect("spawn");

        let signed = signer.sign(claim_draft(), 1000).await.expect("actor").expect("sign");
        // The claim_draft carries `["v","0"]` + `["t","mobee"]`; they must survive into the signed
        // event's tags array (rendered in its JSON).
        let value: serde_json::Value = serde_json::from_str(&signed.json).expect("event json");
        let tags = value["tags"].as_array().expect("tags array");
        let has = |name: &str, val: &str| {
            tags.iter().any(|tag| {
                tag.as_array()
                    .and_then(|parts| Some((parts.first()?.as_str()?, parts.get(1)?.as_str()?)))
                    == Some((name, val))
            })
        };
        assert!(has("v", PROTOCOL_VERSION), "signed event must carry [\"v\",\"0\"]: {}", signed.json);
        assert!(has("t", MOBEE_TAG), "signed event must carry [\"t\",\"mobee\"]: {}", signed.json);
        let _ = std::fs::remove_dir_all(&root);
    }
}
