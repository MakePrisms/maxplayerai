//! The signer actor — the node's single owner of the seller Nostr identity.
//!
//! The seller key is read from `$MAXPLAYER_HOME/key` once at startup and lives only inside this task.
//! Every published event is signed here, through the queue, so there is exactly one signing
//! principal per home and the secret never leaves the actor — no agent process, no client, ever
//! sees it. The outbox publisher hands drafts to [`SignerHandle::sign`]; the fixed authored-at
//! second it passes makes the resulting event id deterministic, so a re-publish after a crash is
//! idempotent at the relay.

use nostr_sdk::{JsonUtil, Keys, Timestamp};
use tokio::sync::{mpsc, oneshot};

use crate::gateway::{self, EventDraft};
use crate::home::{self, HomeError, MaxplayerHome};

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
    /// Schnorr-sign a 32-byte receipt-preimage digest (hex) with the seller key — the seller's half
    /// of the delivery co-signature. The digest is computed by the caller from the STORED creq
    /// (audit N-4); the actor only signs, so the key never leaves this task.
    SignReceiptHash {
        digest_hex: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Build the NIP-98 (`kind:27235`) `Authorization` header for a relay-git push to `remote_url`,
    /// signed with the seller key. Routing the push authorization through the actor keeps the secret
    /// confined to this task + the authenticated relay client — the push path never re-reads the key.
    HttpAuthHeader {
        remote_url: String,
        /// `Some(refname)` scopes the token to one fully-qualified ref (`refs/heads/…`); the relay
        /// then refuses a push to any other ref. `None` mints the unscoped header.
        ref_scope: Option<String>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// NIP-44/NIP-17 unwrap of a kind-1059 gift-wrap addressed to the seller, decoded to its NUT-18
    /// payment (or `None` when it is not a decodable own-payment wrap). The decrypt needs the seller
    /// key, so it runs INSIDE the actor; only the buyer's decrypted payment (never the seller key)
    /// leaves.
    UnwrapPaymentWrap {
        event: Box<nostr_sdk::Event>,
        reply: oneshot::Sender<Result<Option<crate::payment_send::ReceivedPayment>, String>>,
    },
    /// Derive the cashu P2PK signing key from the seller key. cdk's `receive` witnesses P2PK-locked
    /// proofs with the RAW key (no signature-only path), so the derived key is materialized here from
    /// the seller key — which never leaves the actor — and handed out solely for that redeem. This is
    /// a derived payment key, not the seller identity key.
    CashuP2pkSecret {
        reply: oneshot::Sender<Result<cashu::SecretKey, String>>,
    },
}

/// A cheap, cloneable handle to the signer actor.
#[derive(Clone, Debug)]
pub struct SignerHandle {
    tx: mpsc::Sender<Command>,
    /// Cached once at spawn so the common read need not round-trip.
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
    /// The seller public key (hex), served from the cache set at spawn.
    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    /// Send one command and await its reply, with BOTH legs bounded.
    ///
    /// Both legs must be bounded because both are timer-less, and a timer-less await is the one thing
    /// that can park this node permanently and silently (#173). `send` parks forever if the queue is
    /// full and the actor is not draining it; `rx.await` parks forever if the actor is alive but never
    /// answers. Neither arms a timer, so the runtime has nothing to wake for — the run loop's
    /// `select!` is never polled again, the heartbeat tick never fires, the watchdog dies with it, and
    /// the process sits at 0% CPU looking healthy with no error logged anywhere.
    ///
    /// A bound cannot make a stuck actor answer. What it does is convert an invisible permanent park
    /// into a named, logged, recoverable failure at this exact call site — which is why this ships
    /// without yet knowing which call stuck in the field: it makes parking silently impossible.
    async fn round_trip<T>(
        &self,
        call: &'static str,
        command: Command,
        rx: oneshot::Receiver<T>,
    ) -> Result<T, SignerActorGone> {
        match tokio::time::timeout(SIGNER_CALL_TIMEOUT, self.tx.send(command)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(SignerActorGone { call, cause: "actor exited" }),
            Err(_) => {
                return Err(SignerActorGone {
                    call,
                    cause: "queue stayed full (actor not draining)",
                })
            }
        }
        match tokio::time::timeout(SIGNER_CALL_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(SignerActorGone { call, cause: "actor dropped the reply" }),
            Err(_) => Err(SignerActorGone { call, cause: "actor never answered" }),
        }
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
        self.round_trip(
            "sign",
            Command::Sign {
                draft,
                created_at,
                reply,
            },
            rx,
        )
        .await
    }

    /// Schnorr-sign a receipt-preimage digest (32-byte hex) through the actor — the delivery
    /// co-signature. Returns the signature hex, or the inner `Err` when the digest is malformed. The
    /// seller key never leaves the actor.
    pub async fn sign_receipt_hash(
        &self,
        digest_hex: String,
    ) -> Result<Result<String, String>, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.round_trip(
            "sign_receipt_hash",
            Command::SignReceiptHash { digest_hex, reply },
            rx,
        )
        .await
    }

    /// Build the NIP-98 push `Authorization` header for `remote_url` through the actor — the seller
    /// key never leaves the actor, so the push path is not a third custody site. Returns the header
    /// string, or the inner `Err` when the remote url / signing fails.
    pub async fn http_auth_header(
        &self,
        remote_url: String,
        ref_scope: Option<String>,
    ) -> Result<Result<String, String>, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.round_trip(
            "http_auth_header",
            Command::HttpAuthHeader {
                remote_url,
                ref_scope,
                reply,
            },
            rx,
        )
        .await
    }

    /// Decode a gift-wrap to its NUT-18 payment through the actor (the NIP-44 decrypt needs the
    /// seller key, which never leaves the actor). `Ok(None)` = not a decodable own-payment wrap.
    pub async fn unwrap_payment_wrap(
        &self,
        event: nostr_sdk::Event,
    ) -> Result<Result<Option<crate::payment_send::ReceivedPayment>, String>, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.round_trip(
            "unwrap_payment_wrap",
            Command::UnwrapPaymentWrap {
                event: Box::new(event),
                reply,
            },
            rx,
        )
        .await
    }

    /// The cashu P2PK signing key derived from the seller key, for a cdk `receive` that must witness
    /// P2PK-locked proofs with the raw key. The seller key never leaves the actor.
    pub async fn cashu_p2pk_secret(
        &self,
    ) -> Result<Result<cashu::SecretKey, String>, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.round_trip("cashu_p2pk_secret", Command::CashuP2pkSecret { reply }, rx)
            .await
    }

    /// The seller public key (hex), routed through the actor queue (proves the serialized path).
    pub async fn public_key_via_actor(&self) -> Result<String, SignerActorGone> {
        let (reply, rx) = oneshot::channel();
        self.round_trip("public_key", Command::PublicKey { reply }, rx)
            .await
    }
}

/// Load the seller key from `home` and spawn the signer actor. The secret is consumed into the task
/// and never held elsewhere.
pub fn spawn(home: &MaxplayerHome) -> Result<SignerHandle, HomeError> {
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
                Command::SignReceiptHash { digest_hex, reply } => {
                    let result = crate::seller::sign_receipt_hash(&keys, &digest_hex)
                        .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
                Command::HttpAuthHeader {
                    remote_url,
                    ref_scope,
                    reply,
                } => {
                    let result = crate::git_transport::nip98_authorization_header_with_keys(
                        &remote_url,
                        &keys,
                        ref_scope.as_deref(),
                    )
                    .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
                Command::UnwrapPaymentWrap { event, reply } => {
                    let result = crate::seller::unwrap_own_payment_gift_wrap(&keys, &event)
                        .await
                        .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
                Command::CashuP2pkSecret { reply } => {
                    let result =
                        crate::seller::cashu_secret_from_nostr_hex(&keys.secret_key().to_secret_hex())
                            .map_err(|error| error.to_string());
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
            "maxplayer-seller-signer-{label}-{}-{id}",
            std::process::id()
        ))
    }

    // TOOTH (#173) — a signer round-trip is BOUNDED, so an actor that never answers cannot park the
    // caller forever. This is the defect that parks the whole node: the run loop awaits signer
    // round-trips inside `select!` branch bodies, and a timer-less await there means the `select!` is
    // never polled again — the heartbeat tick stops, the watchdog stops with it, and the process sits
    // at 0% CPU with nothing logged. Verified against the field frame: the park is at the runtime's
    // own park point with no timer pending, which excludes every BOUNDED await (nostr-sdk's publish
    // path is capped at WAIT_FOR_OK_TIMEOUT = 10s and would have armed one) and leaves exactly these
    // timer-less channel awaits.
    //
    // The stalled actor here holds each command — and therefore each reply sender — forever, which is
    // the one shape that hangs: dropping the sender would surface as a recv error, not a park.
    //
    // Time is paused, so the production bound elapses instantly in wall-clock. The OUTER timeout is
    // what makes a revert fail cleanly instead of hanging the suite: remove the bound in `round_trip`
    // and there is no timer at 30s, auto-advance jumps to the outer 600s, and the assert goes red.
    //
    // Two calls, not one: the second proves the caller was left usable rather than merely returning
    // once — which is the property the run loop actually needs to keep ticking.
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
            let call = tokio::time::timeout(outer, handle.sign(EventDraft::new(1, Vec::new(), ""), 0));
            let outcome = call.await.unwrap_or_else(|_| {
                panic!(
                    "attempt {attempt}: the signer round-trip never returned — an unbounded \
                     timer-less await here parks the run loop's select, and with it the heartbeat \
                     tick and the whole watchdog, silently"
                )
            });
            let error = outcome.expect_err("a stalled actor cannot produce a signature");
            assert!(
                error.to_string().contains("sign") && error.to_string().contains("never answered"),
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

    fn claim_draft() -> EventDraft {
        gateway::claim_draft(&"e".repeat(64), &"b".repeat(64), &"s".repeat(64), gateway::ClaimPayment::Sat("creqA-test"), &[], &Default::default())
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

    // The delivery co-signature: the actor schnorr-signs a receipt digest with the seller key and
    // returns a 64-byte (128-hex) signature, never the secret; a malformed digest fails cleanly.
    #[tokio::test(flavor = "current_thread")]
    async fn signs_receipt_hash_through_the_actor_never_leaking_the_secret() {
        let root = temp_home("receipt");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let secret = home::read_secret_key_hex(&home).expect("secret");
        let signer = spawn(&home).expect("spawn");

        let digest = "ab".repeat(32); // 32 bytes hex
        let sig = signer
            .sign_receipt_hash(digest.clone())
            .await
            .expect("actor")
            .expect("sign");
        assert_eq!(sig.len(), 128, "schnorr signature is 64 bytes / 128 hex");
        assert_ne!(sig, secret, "signature is not the secret");
        let _ = digest;
        // A malformed digest (not 32 bytes) fails closed rather than signing garbage.
        assert!(signer.sign_receipt_hash("zz".to_owned()).await.expect("actor").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    // The push authorization is signed THROUGH the actor: the header is a valid "Nostr <base64>"
    // NIP-98 token and never contains the secret. This is what keeps the push off a third custody
    // site — the seller key is used only inside the actor to sign it.
    #[tokio::test(flavor = "current_thread")]
    async fn builds_nip98_push_header_through_the_actor_never_leaking_the_secret() {
        let root = temp_home("httpauth");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let secret = home::read_secret_key_hex(&home).expect("secret");
        let signer = spawn(&home).expect("spawn");

        let header = signer
            .http_auth_header("https://relay.example/git/o/r.git".to_owned(), None)
            .await
            .expect("actor")
            .expect("header");
        assert!(header.starts_with("Nostr "), "NIP-98 auth scheme: {header}");
        assert!(!header.contains(&secret), "push header must not leak the secret");

        // A `Some(ref)` scope reaches the mint through the actor: the decoded event carries the ref
        // string, and the scoped header differs from the unscoped one. This is the actor-level guard
        // that the scope is threaded end to end; git_transport's unit tests check the tag SHAPE.
        let scoped = signer
            .http_auth_header(
                "https://relay.example/git/o/r.git".to_owned(),
                Some("refs/heads/maxplayer/abc12345".to_owned()),
            )
            .await
            .expect("actor")
            .expect("header");
        assert_ne!(scoped, header, "the scope must change the minted token");
        let json = {
            use base64::Engine as _;
            let b64 = scoped.strip_prefix("Nostr ").expect("Nostr scheme");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("base64");
            String::from_utf8(bytes).expect("utf8")
        };
        assert!(
            json.contains("refs/heads/maxplayer/abc12345"),
            "scoped token must carry the ref: {json}"
        );
        assert!(!scoped.contains(&secret), "scoped header must not leak the secret");

        // A malformed remote url fails cleanly rather than signing garbage.
        assert!(signer
            .http_auth_header("not a url".to_owned(), None)
            .await
            .expect("actor")
            .is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    // A signed event carries the protocol tags a live buyer requires (`parse_offer` rejects an event
    // without `["v","1"]` / `["t","maxplayer"]`). Proves the outbox→signer path emits wire-valid events.
    #[tokio::test(flavor = "current_thread")]
    async fn signed_event_carries_the_protocol_tags() {
        use crate::gateway::{MAXPLAYER_TAG, PROTOCOL_VERSION};
        let root = temp_home("wire-valid");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let signer = spawn(&home).expect("spawn");

        let signed = signer.sign(claim_draft(), 1000).await.expect("actor").expect("sign");
        // The claim_draft carries `["v","1"]` + `["t","maxplayer"]`; they must survive into the signed
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
        assert!(has("v", PROTOCOL_VERSION), "signed event must carry [\"v\",\"1\"]: {}", signed.json);
        assert!(has("t", MAXPLAYER_TAG), "signed event must carry [\"t\",\"maxplayer\"]: {}", signed.json);
        let _ = std::fs::remove_dir_all(&root);
    }
}
