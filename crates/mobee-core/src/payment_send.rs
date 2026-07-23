use std::fmt;

use serde::{Deserialize, Serialize};

/// Buyer's NUT-18 payment reply plus the mobee routing it rides with.
///
/// The wire form is the cashu [`PaymentRequestPayload`] (NUT-18: `id`, `memo`, `mint`, `unit`,
/// `proofs`) — the buyer emits exactly the object that satisfies the seller-authored `creq`.
/// `id` echoes the request's `i` (set to the job id), `mint` is the *realized* mint the token
/// came from, and `proofs` carry the P2PK-locked ecash.
///
/// `seller_pubkey` is the NIP-17 gift-wrap recipient — routing only, NEVER serialized into the
/// payload JSON. The buyer is authenticated by the NIP-17 seal, so the payload carries no
/// self-declared buyer pubkey to cross-check.
#[cfg(feature = "wallet")]
#[derive(Clone, PartialEq, Eq)]
pub struct PaymentPayload {
    /// NIP-17 gift-wrap recipient (the seller, hex). Routing only; empty on the receive side
    /// (the recipient does not re-address a payment it already received).
    pub seller_pubkey: String,
    /// The NUT-18 payment reply carried inside the encrypted NIP-17 rumor.
    pub payload: cashu::nuts::nut18::PaymentRequestPayload,
}

#[cfg(feature = "wallet")]
impl fmt::Debug for PaymentPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The payload carries proof secrets (bearer ecash) — never Debug-print them.
        formatter.write_str("PaymentPayload(<redacted>)")
    }
}

#[cfg(feature = "wallet")]
impl PaymentPayload {
    /// The job id this payment settles (NUT-18 `id`, == the seller's request `i`).
    pub fn job_id(&self) -> &str {
        self.payload.id.as_deref().unwrap_or_default()
    }

    /// Reconstruct the bearer [`cashu::Token`] from the NUT-18 proofs. Proofs are self-contained
    /// (keyset id embedded), so no mint keyset lookup is needed to rebuild the token here — the
    /// seller redeems it against its own mint.
    pub fn to_token(&self) -> cashu::Token {
        cashu::Token::new(
            self.payload.mint.clone(),
            self.payload.proofs.clone(),
            self.payload.memo.clone(),
            self.payload.unit.clone(),
        )
    }

    /// The NUT-18 payload JSON that rides inside the encrypted NIP-17 rumor.
    fn wire_json(&self) -> Result<String, PaymentSendError> {
        serde_json::to_string(&self.payload).map_err(|error| {
            PaymentSendError::Transport(format!("failed to encode NUT-18 payment payload: {error}"))
        })
    }
}

#[cfg(all(feature = "gateway", feature = "wallet"))]
/// A parsed payment bound to its authenticated NIP-17 sender.
pub struct ReceivedPayment {
    /// The typed NUT-18 payment payload.
    pub payload: PaymentPayload,
    /// The buyer authenticated by the NIP-17 seal.
    pub buyer_pubkey: nostr_sdk::prelude::PublicKey,
}

#[cfg(all(feature = "gateway", feature = "wallet"))]
/// Parses a NUT-18 payment payload and binds it to its authenticated NIP-17 seal sender.
///
/// The buyer is authenticated solely by the seal: NUT-18 carries no self-declared buyer pubkey,
/// so there is no redundant field to spoof or cross-check — the seal sender IS the buyer.
pub fn parse_nip17_payment_payload(
    json: &str,
    seal_sender: nostr_sdk::prelude::PublicKey,
) -> Result<ReceivedPayment, PaymentSendError> {
    let request_payload: cashu::nuts::nut18::PaymentRequestPayload = serde_json::from_str(json)
        .map_err(|error| {
            PaymentSendError::Transport(format!(
                "invalid NIP-17 NUT-18 payment payload JSON: {error}"
            ))
        })?;
    if request_payload.id.as_deref().unwrap_or_default().is_empty() {
        return Err(PaymentSendError::Transport(
            "NUT-18 payment payload is missing its `id` (job correlation)".into(),
        ));
    }
    Ok(ReceivedPayment {
        payload: PaymentPayload {
            // Routing target is irrelevant on the receive side (this seller already holds it).
            seller_pubkey: String::new(),
            payload: request_payload,
        },
        buyer_pubkey: seal_sender,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentSent {
    pub payment_id: String,
    pub relay_success: Vec<String>,
    pub relay_failed: Vec<PaymentRelayFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRelayFailure {
    pub relay: String,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentSendError {
    Transport(String),
}

impl fmt::Display for PaymentSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "payment send failed: {message}"),
        }
    }
}

impl std::error::Error for PaymentSendError {}

#[cfg(feature = "wallet")]
#[allow(async_fn_in_trait)]
pub trait PaymentSend {
    async fn send_payment(
        &mut self,
        payload: PaymentPayload,
    ) -> Result<PaymentSent, PaymentSendError>;
}

#[cfg(all(any(test, feature = "test-support"), feature = "wallet"))]
#[derive(Default)]
pub struct MemoryPaymentSend {
    next_id: u64,
    payments: Vec<PaymentSent>,
}

#[cfg(all(any(test, feature = "test-support"), feature = "wallet"))]
impl MemoryPaymentSend {
    pub fn payments(&self) -> &[PaymentSent] {
        &self.payments
    }

    pub fn record_payment(
        &mut self,
        _payload: PaymentPayload,
    ) -> Result<PaymentSent, PaymentSendError> {
        self.next_id += 1;
        let sent = PaymentSent {
            payment_id: format!("mem-payment-{}", self.next_id),
            relay_success: vec!["memory://payment-send".into()],
            relay_failed: Vec::new(),
        };
        self.payments.push(sent.clone());
        Ok(sent)
    }
}

#[cfg(all(any(test, feature = "test-support"), feature = "wallet"))]
impl PaymentSend for MemoryPaymentSend {
    async fn send_payment(
        &mut self,
        payload: PaymentPayload,
    ) -> Result<PaymentSent, PaymentSendError> {
        self.record_payment(payload)
    }
}

#[cfg(all(feature = "gateway", feature = "wallet"))]
pub struct NostrPaymentSend {
    relay: String,
    keys: nostr_sdk::prelude::Keys,
}

#[cfg(feature = "gateway")]
const GIFT_WRAP_TIMESTAMP_TWEAK_MAX_SECS: u64 = 180;

#[cfg(all(feature = "gateway", feature = "wallet"))]
impl NostrPaymentSend {
    pub fn new(relay: impl Into<String>, keys: nostr_sdk::prelude::Keys) -> Self {
        Self {
            relay: relay.into(),
            keys,
        }
    }
}

#[cfg(all(feature = "gateway", feature = "wallet"))]
impl PaymentSend for NostrPaymentSend {
    async fn send_payment(
        &mut self,
        payload: PaymentPayload,
    ) -> Result<PaymentSent, PaymentSendError> {
        use nostr_sdk::prelude::{Client, PublicKey};

        let receiver = PublicKey::parse(&payload.seller_pubkey).map_err(|error| {
            PaymentSendError::Transport(format!(
                "invalid seller pubkey for NIP-17 payment send: {error}"
            ))
        })?;
        let gift_wrap = payment_send_gift_wrap(&self.keys, receiver, payload.wire_json()?).await?;
        let client = Client::new(self.keys.clone());
        client.add_relay(&self.relay).await.map_err(|error| {
            PaymentSendError::Transport(format!("failed to add relay: {error}"))
        })?;
        client.connect().await;

        let output = client
            .send_event_to([self.relay.as_str()], &gift_wrap)
            .await;
        client.disconnect().await;
        let output = output.map_err(|error| {
            PaymentSendError::Transport(format!("failed to send NIP-17 payment: {error}"))
        })?;

        payment_sent(
            output.val.to_string(),
            output
                .success
                .into_iter()
                .map(|url| url.to_string())
                .collect(),
            output
                .failed
                .into_iter()
                .map(|(url, error)| PaymentRelayFailure {
                    relay: url.to_string(),
                    error,
                })
                .collect(),
        )
    }
}

#[cfg(any(test, feature = "gateway"))]
fn payment_sent(
    payment_id: String,
    relay_success: Vec<String>,
    relay_failed: Vec<PaymentRelayFailure>,
) -> Result<PaymentSent, PaymentSendError> {
    if relay_success.is_empty() {
        return Err(PaymentSendError::Transport(
            "no relay accepted the NIP-17 payment".into(),
        ));
    }

    Ok(PaymentSent {
        payment_id,
        relay_success,
        relay_failed,
    })
}

#[cfg(feature = "gateway")]
async fn payment_send_gift_wrap(
    keys: &nostr_sdk::prelude::Keys,
    receiver: nostr_sdk::prelude::PublicKey,
    message: String,
) -> Result<nostr_sdk::prelude::Event, PaymentSendError> {
    use nostr_sdk::nostr::nips::nip44;
    use nostr_sdk::prelude::{EventBuilder, JsonUtil, Kind, Tag};

    let rumor = EventBuilder::private_msg_rumor(receiver, message).build(keys.public_key());
    let seal = EventBuilder::seal(keys, &receiver, rumor)
        .await
        .map_err(|error| {
            PaymentSendError::Transport(format!("failed to build NIP-17 seal: {error}"))
        })?
        .sign(keys)
        .await
        .map_err(|error| {
            PaymentSendError::Transport(format!("failed to sign NIP-17 seal: {error}"))
        })?;
    let wrapping_keys = nostr_sdk::prelude::Keys::generate();
    let content = nip44::encrypt(
        wrapping_keys.secret_key(),
        &receiver,
        seal.as_json(),
        nip44::Version::default(),
    )
    .map_err(|error| {
        PaymentSendError::Transport(format!("failed to encrypt NIP-17 gift wrap: {error}"))
    })?;

    EventBuilder::new(Kind::GiftWrap, content)
        .tags([Tag::public_key(receiver)])
        .custom_created_at(fresh_gift_wrap_created_at())
        .sign_with_keys(&wrapping_keys)
        .map_err(|error| {
            PaymentSendError::Transport(format!("failed to sign NIP-17 gift wrap: {error}"))
        })
}

#[cfg(feature = "gateway")]
fn fresh_gift_wrap_created_at() -> nostr_sdk::prelude::Timestamp {
    nostr_sdk::prelude::Timestamp::tweaked(0..GIFT_WRAP_TIMESTAMP_TWEAK_MAX_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "wallet")]
    const MINT: &str = "https://testnut.cashu.space";
    #[cfg(feature = "wallet")]
    const KEYSET_ID: &str = "009a1f293253e41e";
    // A recognizable, unique proof secret. The whole point of NIP-17 gift-wrapping is that this
    // never appears in plaintext on the wire — the leak test below asserts exactly that.
    #[cfg(feature = "wallet")]
    const KNOWN_SECRET: &str = "proof-secret-corr-9a6dbb84-do-not-leak";

    #[cfg(feature = "wallet")]
    fn nut18_payload(job_id: &str) -> cashu::nuts::nut18::PaymentRequestPayload {
        use std::str::FromStr;

        use cashu::{Amount, CurrencyUnit, Id, MintUrl, Proof, SecretKey};
        use cashu::secret::Secret;

        let secret = Secret::new(KNOWN_SECRET);
        // Any valid secp256k1 point works for `C`; the payload codec does not verify it (the
        // seller's mint does, at redeem). A fresh keypair keeps the proof structurally valid.
        let c = SecretKey::generate().public_key();
        let proof = Proof::new(
            Amount::from(7),
            Id::from_str(KEYSET_ID).expect("valid keyset id"),
            secret,
            c,
        );
        cashu::nuts::nut18::PaymentRequestPayload {
            id: Some(job_id.to_owned()),
            memo: None,
            mint: MintUrl::from_str(MINT).expect("valid mint url"),
            unit: CurrencyUnit::Sat,
            proofs: vec![proof],
        }
    }

    #[cfg(feature = "wallet")]
    fn payload() -> PaymentPayload {
        PaymentPayload {
            seller_pubkey: "seller".into(),
            payload: nut18_payload("job"),
        }
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn nut18_wire_json_is_the_payment_request_payload() {
        let payload = payload();
        let wire = payload.wire_json().unwrap();

        // The wire form is exactly the cashu NUT-18 payload — the buyer-declared `mint_url` of
        // the old hand-rolled envelope is gone; `mint`/`unit`/`id`/`proofs` are the NUT-18 keys.
        let parsed: cashu::nuts::nut18::PaymentRequestPayload =
            serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed, payload.payload);
        assert_eq!(parsed.id.as_deref(), Some("job"));
        assert_eq!(parsed.mint.to_string(), MINT);
        assert!(!wire.contains("mint_url"));
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn memory_payment_records_metadata_only() {
        let mut delivery = MemoryPaymentSend::default();

        let delivered = delivery.record_payment(payload()).unwrap();

        assert_eq!(delivered.payment_id, "mem-payment-1");
        assert_eq!(delivered.relay_success, ["memory://payment-send"]);
        assert!(delivered.relay_failed.is_empty());
        assert_eq!(delivery.payments(), &[delivered]);
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn payment_payload_debug_redacts_proof_secret() {
        let payload = payload();
        let wire = payload.wire_json().unwrap();
        let payload_debug = format!("{payload:?}");

        // Non-vacuous control: the secret really is in the serialized payload we hand in.
        assert!(wire.contains(KNOWN_SECRET), "test setup lost the known secret");
        assert!(!payload_debug.contains(KNOWN_SECRET));
        assert_eq!(payload_debug, "PaymentPayload(<redacted>)");
    }

    #[test]
    fn payment_send_fails_closed_when_no_relay_accepts() {
        let error = payment_sent(
            "event".into(),
            Vec::new(),
            vec![PaymentRelayFailure {
                relay: "wss://relay.example".into(),
                error: "rejected".into(),
            }],
        )
        .unwrap_err();

        assert_eq!(
            error,
            PaymentSendError::Transport("no relay accepted the NIP-17 payment".into())
        );
    }

    #[test]
    fn payment_send_preserves_partial_relay_outcome() {
        let failed = PaymentRelayFailure {
            relay: "wss://failed.example".into(),
            error: "rejected".into(),
        };
        let delivered = payment_sent(
            "event".into(),
            vec!["wss://accepted.example".into()],
            vec![failed.clone()],
        )
        .unwrap();

        assert_eq!(delivered.relay_success, ["wss://accepted.example"]);
        assert_eq!(delivered.relay_failed, [failed]);
    }

    #[cfg(feature = "gateway")]
    #[test]
    fn gift_wrap_timestamp_tweak_stays_inside_relay_freshness_window() {
        let now = nostr_sdk::prelude::Timestamp::now();
        let created_at = fresh_gift_wrap_created_at();
        assert!(created_at <= now);
        assert!(
            now.as_secs().saturating_sub(created_at.as_secs()) < GIFT_WRAP_TIMESTAMP_TWEAK_MAX_SECS
        );
    }

    #[cfg(all(feature = "gateway", feature = "wallet"))]
    #[test]
    fn corrupt_wire_payload_fails_closed() {
        let buyer = nostr_sdk::prelude::Keys::generate().public_key();

        // Not a NUT-18 payload at all.
        let result = parse_nip17_payment_payload("{\"not\":\"a payload\"}", buyer);
        assert!(matches!(
            result,
            Err(PaymentSendError::Transport(message))
                if message.contains("invalid NIP-17 NUT-18 payment payload JSON")
        ));

        // Well-formed NUT-18 shape but missing the job-correlation `id`.
        let mut no_id = nut18_payload("job");
        no_id.id = None;
        let json = serde_json::to_string(&no_id).unwrap();
        let result = parse_nip17_payment_payload(&json, buyer);
        assert!(matches!(
            result,
            Err(PaymentSendError::Transport(message)) if message.contains("missing its `id`")
        ));
    }

    #[cfg(all(feature = "gateway", feature = "wallet"))]
    #[test]
    fn payment_payload_round_trips_through_the_nut18_wire() {
        let payload = payload();
        let buyer = nostr_sdk::prelude::Keys::generate().public_key();
        let json = payload.wire_json().unwrap();

        let parsed = parse_nip17_payment_payload(&json, buyer).unwrap();

        assert_eq!(parsed.payload.payload, payload.payload);
        assert_eq!(parsed.payload.job_id(), "job");
        assert_eq!(parsed.payload.to_token(), payload.to_token());
        assert_eq!(parsed.buyer_pubkey, buyer);
    }

    #[cfg(all(feature = "gateway", feature = "wallet"))]
    #[test]
    fn gift_wrap_never_leaks_proof_secret_or_payload_plaintext() {
        use nostr_sdk::{
            nips::nip59::UnwrappedGift,
            prelude::{JsonUtil, Keys, Kind, PublicKey, Tag},
        };

        const SECRET_JOB_ID: &str = "job-secret-corr-42";

        let sender = Keys::generate();
        let receiver = Keys::generate();

        // Secret-class material rides ONLY inside the encrypted NIP-17 rumor: the proof secret is
        // bearer ecash (a spendable secret on a public relay if leaked); the whole NUT-18 payload
        // and the job correlation id travel inside the same encrypted envelope.
        let payload = PaymentPayload {
            seller_pubkey: receiver.public_key().to_hex(),
            payload: nut18_payload(SECRET_JOB_ID),
        };
        let plaintext = payload.wire_json().unwrap();

        // Non-vacuous guard: the secret really is present in the plaintext we hand in.
        assert!(
            plaintext.contains(KNOWN_SECRET),
            "test setup broken: proof secret missing from NUT-18 payload"
        );

        let recipient = PublicKey::parse(&payload.seller_pubkey).expect("valid recipient pubkey");
        let gift_wrap =
            block_on(payment_send_gift_wrap(&sender, recipient, plaintext.clone()))
                .expect("gift wrap builds without network");

        assert_eq!(gift_wrap.kind, Kind::GiftWrap);
        assert_eq!(gift_wrap.kind.as_u16(), 1059);

        // Unwrap the exact publishable artifact as the recipient does: proves the encrypted rumor
        // is a decryptable kind-14 DM addressed to the seller.
        let unwrapped = block_on(UnwrappedGift::from_gift_wrap(&receiver, &gift_wrap))
            .expect("recipient decrypts and authenticates the gift wrap");
        assert_eq!(unwrapped.sender, sender.public_key());
        assert_eq!(unwrapped.rumor.kind, Kind::PrivateDirectMessage);
        assert_eq!(unwrapped.rumor.content, plaintext);
        assert!(
            unwrapped
                .rumor
                .tags
                .iter()
                .any(|tag| tag == &Tag::public_key(receiver.public_key())),
            "kind-14 rumor is missing its recipient p-tag"
        );

        // Serialize the ENTIRE publishable event exactly as it goes on the wire and search it.
        let wire = gift_wrap.as_json();
        assert!(
            !wire.contains(KNOWN_SECRET),
            "proof secret leaked in plaintext on the wire: {wire}"
        );
        assert!(
            !wire.contains(&plaintext),
            "NUT-18 payload leaked in plaintext on the wire: {wire}"
        );
        assert!(
            !wire.contains(SECRET_JOB_ID),
            "job correlation id leaked in plaintext on the wire: {wire}"
        );

        // Non-vacuous guard: there is real ciphertext and it is what got serialized.
        assert!(!gift_wrap.content.is_empty(), "gift wrap content is empty");
        assert!(
            wire.contains(&gift_wrap.content),
            "serialized event does not contain its own encrypted content"
        );

        // Negative control: a second wrap of the SAME payload differs on the wire (ephemeral
        // wrapping key + randomized nip44 nonce), proving the absence checks above are not
        // passing vacuously against a constant or empty artifact.
        let second = block_on(payment_send_gift_wrap(&sender, recipient, plaintext.clone()))
            .expect("second gift wrap builds without network");
        assert_ne!(
            wire,
            second.as_json(),
            "two wraps of the same payload are byte-identical; encryption is not randomized"
        );
    }

    // Minimal single-threaded executor: the gift-wrap path is pure CPU crypto (no I/O), so a
    // busy-poll with a no-op waker drives it to completion without pulling in an async runtime.
    #[cfg(feature = "gateway")]
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::pin::Pin;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        unsafe fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match Pin::new(&mut future).poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
