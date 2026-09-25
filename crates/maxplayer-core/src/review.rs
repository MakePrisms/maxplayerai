//! Optional execution-safety reviews. Probability is not payment authority.
//! Relay service and client gates share the signed immutable review contract.
#[cfg(feature = "wallet")]
pub mod state;
#[cfg(feature = "wallet")]
pub mod private;
use crate::gateway::{EventDraft, MAXPLAYER_TAG, PROTOCOL_VERSION, TagSpec};
use crate::kinds::{JOB_OFFER_KIND, JOB_RESULT_KIND, REVIEW_KIND, REVIEW_REQUEST_KIND};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_INPUT_BYTES: usize = 128 * 1024;
pub const MAX_FILES: usize = 256;
pub const MAX_REVIEW_BYTES: usize = 16 * 1024;
pub const CLASSIFIER: &str = "execution-safety";
pub const CLASSIFIER_VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    pub seller_offer: bool,
    pub buyer_delivery: bool,
    pub skip_buyer_pubkeys: Vec<String>,
    pub skip_seller_pubkeys: Vec<String>,
    /// Relay URL -> reviewer public key. Never learned from incoming events.
    pub reviewers: BTreeMap<String, String>,
    /// Probability in millionths, inclusive. Keeps persisted config exact.
    /// Provisional mock-stage default; calibration is required before release.
    pub reject_at_or_above_ppm: u32,
    pub timeout_seconds: u64,
}
impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            seller_offer: true,
            buyer_delivery: true,
            skip_buyer_pubkeys: vec![],
            skip_seller_pubkeys: vec![],
            reviewers: BTreeMap::new(),
            reject_at_or_above_ppm: 500_000,
            timeout_seconds: 30,
        }
    }
}
impl ReviewConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.reject_at_or_above_ppm > 1_000_000 || !(1..=300).contains(&self.timeout_seconds) {
            return Err("review: invalid probability threshold or timeout (1..300 seconds)".into());
        }
        if self
            .skip_buyer_pubkeys
            .iter()
            .chain(&self.skip_seller_pubkeys)
            .chain(self.reviewers.values())
            .any(|s| !hex_id(s))
        {
            return Err("review: public keys must be lowercase 64-character hex".into());
        }
        Ok(())
    }
    pub fn enabled(&self, kind: u16, counterparty: &str) -> Result<bool, String> {
        self.validate()?;
        match kind {
            JOB_OFFER_KIND => {
                Ok(self.seller_offer && !self.skip_buyer_pubkeys.iter().any(|p| p == counterparty))
            }
            JOB_RESULT_KIND => {
                Ok(self.buyer_delivery
                    && !self.skip_seller_pubkeys.iter().any(|p| p == counterparty))
            }
            _ => Err("review: unsupported subject kind".into()),
        }
    }
}
fn hex_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub offer: String,
    pub event: String,
    pub kind: u16,
    pub commit: Option<String>,
}
impl Subject {
    pub fn validate(&self) -> Result<(), String> {
        if !hex_id(&self.offer) || !hex_id(&self.event) {
            return Err("review: invalid subject id".into());
        }
        match self.kind {
            JOB_OFFER_KIND if self.offer == self.event && self.commit.is_none() => Ok(()),
            JOB_RESULT_KIND
                if self.commit.as_ref().is_none_or(|c| {
                    (c.len() == 40 || c.len() == 64)
                        && c.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                }) =>
            {
                Ok(())
            }
            _ => Err("review: invalid subject/commit binding".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Classification {
    pub classifier: String,
    pub version: String,
    pub label: String,
    pub probabilities: BTreeMap<String, f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub schema: u16,
    pub subject: Subject,
    pub input_sha256: String,
    pub status: String,
    pub results: Vec<Classification>,
    pub error_code: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Passed,
    Refused,
    Error(String),
}
impl Review {
    pub fn decision(&self, subject: &Subject, threshold: u32) -> Result<Decision, String> {
        subject.validate()?;
        if self.schema != 1
            || &self.subject != subject
            || threshold > 1_000_000
            || !hex_id(&self.input_sha256)
        {
            return Err("review: schema, subject or input binding invalid".into());
        }
        if self.status == "error" && self.results.is_empty() {
            if !self.error_code.as_ref().is_some_and(|code| {
                !code.is_empty()
                    && code.len() <= 64
                    && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            }) {
                return Err("review: invalid error code".into());
            }
            return Ok(Decision::Error(
                self.error_code
                    .clone()
                    .ok_or("review: missing error code")?,
            ));
        }
        if self.status != "ok" || self.error_code.is_some() {
            return Err("review: invalid status".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for item in &self.results {
            if !ids.insert(&item.classifier) {
                return Err("review: duplicate classifier".into());
            }
        }
        let r = self
            .results
            .iter()
            .find(|r| r.classifier == CLASSIFIER)
            .ok_or("review: missing execution-safety classifier")?;
        if r.version != CLASSIFIER_VERSION || r.probabilities.len() != 2 {
            return Err("review: unsupported classifier".into());
        }
        let safe = *r
            .probabilities
            .get("safe")
            .ok_or("review: missing safe probability")?;
        let unsafe_p = *r
            .probabilities
            .get("unsafe")
            .ok_or("review: missing unsafe probability")?;
        if !safe.is_finite()
            || !unsafe_p.is_finite()
            || !(0.0..=1.0).contains(&safe)
            || !(0.0..=1.0).contains(&unsafe_p)
            || (safe + unsafe_p - 1.0).abs() > 0.000001
            || !matches!(r.label.as_str(), "safe" | "unsafe")
            || r.probabilities[&r.label] < safe.max(unsafe_p)
        {
            return Err("review: invalid probability distribution".into());
        }
        Ok(if unsafe_p >= threshold as f64 / 1_000_000.0 {
            Decision::Refused
        } else {
            Decision::Passed
        })
    }
}
fn tags(subject: &Subject) -> Vec<TagSpec> {
    vec![
        TagSpec::new(["t", MAXPLAYER_TAG]),
        TagSpec::new(["v", PROTOCOL_VERSION]),
        TagSpec::new(["e", &subject.offer, "", "root"]),
        TagSpec::new(["e", &subject.event, "", "reply"]),
    ]
}
pub fn request_draft(subject: &Subject, reviewer: &str) -> Result<EventDraft, String> {
    subject.validate()?;
    if !hex_id(reviewer) {
        return Err("review: invalid reviewer public key".into());
    }
    let mut t = tags(subject);
    t.push(TagSpec::new(["p", reviewer]));
    t.push(TagSpec::new(["classifier", CLASSIFIER, CLASSIFIER_VERSION]));
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| "review: random source unavailable")?;
    t.push(TagSpec::new(["request_nonce", &hex::encode(nonce)]));
    Ok(EventDraft::new(
        REVIEW_REQUEST_KIND,
        t,
        serde_json::to_string(subject).map_err(|e| e.to_string())?,
    ))
}
pub fn review_draft(review: &Review) -> Result<EventDraft, String> {
    review.decision(&review.subject, 500_000)?;
    let content = serde_json::to_string(review).map_err(|e| e.to_string())?;
    if content.len() > MAX_REVIEW_BYTES {
        return Err("review: response too large".into());
    }
    Ok(EventDraft::new(REVIEW_KIND, tags(&review.subject), content))
}

/// Canonical bounded review input: sorted UTF-8 paths and contents, no filesystem execution.
/// The caller must obtain blobs from the verified immutable git commit, not a worktree.
#[derive(Serialize)]
struct Input<'a> {
    subject: &'a Subject,
    task: &'a str,
    files: BTreeMap<String, String>,
}
pub fn input_bytes(
    subject: &Subject,
    task: &str,
    files: Vec<(String, Vec<u8>)>,
) -> Result<Vec<u8>, String> {
    subject.validate()?;
    if files.len() > MAX_FILES || task.len() > MAX_INPUT_BYTES {
        return Err("review: input too large".into());
    }
    if subject.kind == JOB_OFFER_KIND && !files.is_empty() {
        return Err("review: offer cannot carry delivery files".into());
    }
    let mut sorted = BTreeMap::new();
    let mut total = task.len();
    for (path, bytes) in files {
        total = total
            .checked_add(path.len())
            .and_then(|n| n.checked_add(bytes.len()))
            .ok_or("review: input too large")?;
        if total > MAX_INPUT_BYTES {
            return Err("review: input too large".into());
        }
        if path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path
                .split('/')
                .any(|p| p == ".." || p == "." || p.is_empty())
        {
            return Err("review: invalid input path".into());
        }
        let content = String::from_utf8(bytes).map_err(|_| "review: binary input unsupported")?;
        if content.contains('\0') || sorted.insert(path, content).is_some() {
            return Err("review: binary or duplicate input".into());
        }
    }
    let bytes = serde_json::to_vec(&Input {
        subject,
        task,
        files: sorted,
    })
    .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err("review: input too large".into());
    }
    Ok(bytes)
}
pub fn input_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(feature = "gateway")]
pub mod wire {
    use super::*;
    use nostr_sdk::prelude::*;
    /// Authenticated results only. Malformed trusted events fail closed; foreign authors are ignored.
    pub fn verify(
        event: &Event,
        reviewer: &PublicKey,
        subject: &Subject,
    ) -> Result<Review, String> {
        event
            .verify()
            .map_err(|_| "review: invalid event signature")?;
        if event.pubkey != *reviewer
            || event.kind != Kind::Custom(REVIEW_KIND)
            || event.content.len() > MAX_REVIEW_BYTES
        {
            return Err("review: wrong signer, kind or size".into());
        }
        let actual: Vec<Vec<String>> = event.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        // Exact singleton markers prevent ambiguous root/subject selection.
        for expected in tags(subject) {
            let same: Vec<_> = actual
                .iter()
                .filter(|t| {
                    t.first() == expected.0.first()
                        && (t.first().map(String::as_str) != Some("e")
                            || t.get(3) == expected.0.get(3))
                })
                .collect();
            if same.len() != 1 || same[0] != &expected.0 {
                return Err("review: ambiguous or missing event tags".into());
            }
        }
        let review: Review =
            serde_json::from_str(&event.content).map_err(|_| "review: invalid JSON")?;
        review.decision(subject, 500_000)?;
        Ok(review)
    }

    /// Transport seam used by the real relay and offline mock tests.
    #[allow(async_fn_in_trait)]
    pub trait Transport {
        async fn fetch(
            &self,
            subject: &Subject,
            reviewer: &PublicKey,
        ) -> Result<Vec<Event>, String>;
        async fn request(&self, draft: EventDraft) -> Result<(), String>;
    }
    pub struct RelayTransport<'a> {
        pub client: &'a Client,
        pub relay: &'a str,
    }
    impl Transport for RelayTransport<'_> {
        async fn fetch(
            &self,
            subject: &Subject,
            reviewer: &PublicKey,
        ) -> Result<Vec<Event>, String> {
            let events = self
                .client
                .fetch_events(
                    Filter::new()
                        .kind(Kind::Custom(REVIEW_KIND))
                        .author(*reviewer)
                        .event(EventId::from_hex(&subject.event).map_err(|e| e.to_string())?)
                        .hashtag(MAXPLAYER_TAG)
                        .limit(100),
                    std::time::Duration::from_secs(2),
                )
                .await
                .map_err(|e| e.to_string())?;
            if events.len() >= 100 {
                return Err("review: too many responses".into());
            }
            Ok(events.into_iter().collect())
        }
        async fn request(&self, draft: EventDraft) -> Result<(), String> {
            let builder =
                crate::gateway::nostr::event_builder(&draft).map_err(|e| e.to_string())?;
            self.client
                .send_event_builder_to([self.relay], builder)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        }
    }
    /// Bounded wait, one request per attempt, reuse prior authenticated result.
    pub async fn check<T: Transport>(
        transport: &T,
        config: &ReviewConfig,
        relay: &str,
        subject: &Subject,
        counterparty: &str,
        private: bool,
    ) -> Result<Option<String>, String> {
        if !config.enabled(subject.kind, counterparty)? {
            return Ok(None);
        }
        subject.validate()?;
        // No public fallback for private input, even if a cached public result exists.
        if private {
            return Err("review: private transport unavailable; no public request sent".into());
        }
        let reviewer = config
            .reviewers
            .get(relay)
            .ok_or("review: configure a reviewer key for this relay, or explicitly skip review")?;
        let key = PublicKey::from_hex(reviewer).map_err(|_| "review: invalid reviewer key")?;
        let mut last_error: Option<String> = None;
        let future = async {
            let mut requested = false;
            let mut prior_errors = std::collections::BTreeSet::new();
            loop {
                let events = transport.fetch(subject, &key).await?;
                let mut accepted: Option<(Review, String)> = None;
                let mut provider_error = None;
                for event in events {
                    if event.pubkey != key {
                        continue;
                    }
                    let r = verify(&event, &key, subject)?;
                    // Availability errors do not revoke an immutable successful assessment.
                    if r.status == "error" {
                        last_error = r.error_code.clone();
                        if !prior_errors.contains(&event.id) {
                            provider_error = r.error_code.clone();
                        }
                        if !requested {
                            prior_errors.insert(event.id);
                        }
                        continue;
                    }
                    if let Some((previous, _)) = &accepted {
                        if previous != &r {
                            return Err("review: conflicting results".into());
                        }
                    } else {
                        accepted = Some((r, event.id.to_hex()));
                    }
                }
                if let Some((r, id)) = accepted {
                    return match r.decision(subject, config.reject_at_or_above_ppm)? {
                        Decision::Passed => Ok(Some(id)),
                        Decision::Refused => Err(
                            "review: execution-safety probability exceeds local threshold".into(),
                        ),
                        Decision::Error(code) => {
                            Err(format!("review: reviewer error ({code}); retry available"))
                        }
                    };
                }
                if let Some(code) = provider_error {
                    // A deliberate new check requests recovery; never invent a safe result.
                    if requested {
                        return Err(format!(
                            "review: reviewer error ({code}); next step blocked; explicit retry available"
                        ));
                    }
                    // An old error is not a response to this new attempt. Request once below
                    // and wait for a new signed result instead of returning the stale error.
                }
                if !requested {
                    transport.request(request_draft(subject, reviewer)?).await?;
                    requested = true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(config.timeout_seconds),
            future,
        )
        .await
        .map_err(|_| match last_error {
            Some(code) => format!(
                "review: timeout; last reviewer error ({code}); next step blocked; retry available"
            ),
            None => "review: timeout; next step blocked; retry available".to_string(),
        })?
    }
}

#[cfg(feature = "wallet")]
pub async fn check_buyer(
    home: &crate::home::MaxplayerHome,
    keys: &nostr_sdk::Keys,
    subject: &Subject,
    counterparty: &str,
    evidence: Option<&crate::private_content::evidence::PrivateEvidence>,
) -> Result<Option<String>, String> {
    if !home.config.review.enabled(subject.kind, counterparty)? {
        state::write(
            &home.root,
            subject,
            "disabled",
            &format!("explicit local skip for counterparty public key {counterparty}"),
        )?;
        return Ok(None);
    }
    // Validate trust before opening a connection.
    if !home
        .config
        .review
        .reviewers
        .contains_key(&home.config.relay_url)
    {
        return Err("review: configure reviewer key or explicitly skip delivery review".into());
    }
    let client = nostr_sdk::Client::new(keys.clone());
    client.automatic_authentication(true);
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|e| e.to_string())?;
    client.connect().await;
    state::write(
        &home.root,
        subject,
        "pending",
        "Waiting for signed execution review",
    )?;
    let result = async {
        // A signed source determines routing, never a caller's unverified boolean.
        let private = if let Some(e) = evidence {
            e.offer.verify().map_err(|_| "review: invalid source offer")?;
            if e.offer.id.to_hex() != subject.offer || e.result.id.to_hex() != subject.event {
                return Err("review: mismatched evidence".into());
            }
            private::is_private(&e.offer)
        } else {
            use nostr_sdk::prelude::*;
            let rows = client.fetch_events(Filter::new().id(EventId::from_hex(&subject.offer).map_err(|_| "review: invalid offer id")?), std::time::Duration::from_secs(3))
                .await.map_err(|_| "review: offer unavailable")?;
            let offer = rows.into_iter().find(|e| e.id.to_hex() == subject.offer).ok_or("review: offer unavailable")?;
            offer.verify().map_err(|_| "review: invalid source offer")?;
            private::is_private(&offer)
        };
        if private {
            let evidence = evidence.ok_or("review: private evidence unavailable; no public fallback")?;
            let request = private::Request { subject: subject.clone(), offer: evidence.offer.clone(), task_envelope: evidence.task_envelope.clone(), delivery: Some(evidence.clone()) };
            private::check(home, &client, private::Identity::Buyer(keys), &request, counterparty).await
        } else {
            wire::check(
        &wire::RelayTransport {
            client: &client,
            relay: &home.config.relay_url,
        },
        &home.config.review,
        &home.config.relay_url,
        subject,
        counterparty,
        false,
    )
    .await
        }
    }.await;
    client.disconnect().await;
    state::completed(&home.root, subject, &result)?;
    result
        .map_err(|e| format!("{e}; retry the same collect or accept operation (no new job needed)"))
}

#[cfg(all(test, feature = "gateway"))]
mod tests {
    use super::*;
    use nostr_sdk::prelude::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    fn subject() -> Subject {
        Subject {
            offer: "a".repeat(64),
            event: "a".repeat(64),
            kind: JOB_OFFER_KIND,
            commit: None,
        }
    }
    fn answer(p: f64) -> Review {
        Review {
            schema: 1,
            subject: subject(),
            input_sha256: input_digest(b"fixture"),
            status: "ok".into(),
            results: vec![Classification {
                classifier: CLASSIFIER.into(),
                version: "1".into(),
                label: if p >= 0.5 { "unsafe" } else { "safe" }.into(),
                probabilities: BTreeMap::from([("safe".into(), 1.0 - p), ("unsafe".into(), p)]),
            }],
            error_code: None,
        }
    }
    fn signed(r: &Review, keys: &Keys) -> Event {
        crate::gateway::nostr::event_builder(&review_draft(r).unwrap())
            .unwrap()
            .sign_with_keys(keys)
            .unwrap()
    }
    struct Mock {
        events: Mutex<Vec<Event>>,
        on_request: Vec<Event>,
        requests: AtomicUsize,
    }
    impl wire::Transport for Mock {
        async fn fetch(&self, _: &Subject, _: &PublicKey) -> Result<Vec<Event>, String> {
            Ok(self.events.lock().unwrap().clone())
        }
        async fn request(&self, draft: EventDraft) -> Result<(), String> {
            assert_eq!(draft.kind, REVIEW_REQUEST_KIND);
            self.requests.fetch_add(1, Ordering::SeqCst);
            *self.events.lock().unwrap() = self.on_request.clone();
            Ok(())
        }
    }
    fn config(keys: &Keys) -> ReviewConfig {
        ReviewConfig {
            reviewers: BTreeMap::from([("wss://test".into(), keys.public_key().to_hex())]),
            timeout_seconds: 1,
            ..Default::default()
        }
    }
    fn mock(events: Vec<Event>, on_request: Vec<Event>) -> Mock {
        Mock {
            events: Mutex::new(events),
            on_request,
            requests: AtomicUsize::new(0),
        }
    }
    async fn check(m: &Mock, cfg: &ReviewConfig) -> Result<Option<String>, String> {
        wire::check(m, cfg, "wss://test", &subject(), &"b".repeat(64), false).await
    }
    #[tokio::test(start_paused = true)]
    async fn requests_once_then_allows_signed_safe_review() {
        let keys = Keys::generate();
        let event = signed(&answer(0.1), &keys);
        let m = mock(vec![], vec![event.clone()]);
        assert_eq!(
            check(&m, &config(&keys)).await.unwrap(),
            Some(event.id.to_hex())
        );
        assert_eq!(m.requests.load(Ordering::SeqCst), 1);
        assert!(check(&m, &config(&keys)).await.is_ok());
        assert_eq!(
            m.requests.load(Ordering::SeqCst),
            1,
            "cached review must avoid another request"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn review_retry_waits_for_recovery_instead_of_returning_old_error() {
        let keys = Keys::generate();
        let mut error = answer(0.1);
        error.status = "error".into();
        error.results.clear();
        error.error_code = Some("provider_timeout".into());
        let passed = signed(&answer(0.1), &keys);
        let m = mock(vec![signed(&error, &keys)], vec![passed.clone()]);
        assert_eq!(
            check(&m, &config(&keys)).await.unwrap(),
            Some(passed.id.to_hex())
        );
        assert_eq!(m.requests.load(Ordering::SeqCst), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn timeout_blocks_and_an_explicit_retry_can_recover() {
        let keys = Keys::generate();
        let m = mock(vec![], vec![]);
        assert!(
            check(&m, &config(&keys))
                .await
                .unwrap_err()
                .contains("timeout")
        );
        assert_eq!(m.requests.load(Ordering::SeqCst), 1);
        m.events.lock().unwrap().push(signed(&answer(0.1), &keys));
        assert!(check(&m, &config(&keys)).await.is_ok());
    }
    #[tokio::test]
    async fn skip_and_trusted_counterparty_do_not_contact_reviewer() {
        let m = mock(vec![], vec![]);
        let mut cfg = ReviewConfig {
            seller_offer: false,
            ..Default::default()
        };
        assert_eq!(check(&m, &cfg).await.unwrap(), None);
        cfg.seller_offer = true;
        cfg.skip_buyer_pubkeys.push("b".repeat(64));
        assert_eq!(check(&m, &cfg).await.unwrap(), None);
        assert_eq!(m.requests.load(Ordering::SeqCst), 0);
        cfg.skip_buyer_pubkeys.clear();
        assert!(check(&m, &cfg).await.unwrap_err().contains("configure"));
    }
    #[tokio::test]
    async fn private_review_never_falls_back_to_public_request() {
        let keys = Keys::generate();
        let m = mock(vec![], vec![]);
        assert!(
            wire::check(
                &m,
                &config(&keys),
                "wss://test",
                &subject(),
                &"b".repeat(64),
                true
            )
            .await
            .unwrap_err()
            .contains("private transport")
        );
        assert_eq!(m.requests.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn unsafe_error_and_conflict_block() {
        let keys = Keys::generate();
        let cfg = config(&keys);
        let m = mock(vec![signed(&answer(0.9), &keys)], vec![]);
        assert!(check(&m, &cfg).await.unwrap_err().contains("threshold"));
        let mut r = answer(0.1);
        r.status = "error".into();
        r.results.clear();
        r.error_code = Some("provider_unavailable".into());
        let m = mock(vec![signed(&r, &keys)], vec![]);
        assert!(
            check(&m, &cfg)
                .await
                .unwrap_err()
                .contains("provider_unavailable")
        );
        let m = mock(
            vec![signed(&answer(0.1), &keys), signed(&answer(0.9), &keys)],
            vec![],
        );
        assert!(check(&m, &cfg).await.unwrap_err().contains("conflicting"));
    }
    #[test]
    fn threshold_boundary_is_inclusive() {
        assert_eq!(
            answer(0.499999).decision(&subject(), 500_000).unwrap(),
            Decision::Passed
        );
        assert_eq!(
            answer(0.5).decision(&subject(), 500_000).unwrap(),
            Decision::Refused
        );
        assert_eq!(
            answer(0.500001).decision(&subject(), 500_000).unwrap(),
            Decision::Refused
        );
    }
    #[test]
    fn malformed_distributions_and_classifier_versions_block() {
        for p in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            assert!(answer(p).decision(&subject(), 500_000).is_err());
        }
        let mut r = answer(0.1);
        r.results[0].probabilities.insert("safe".into(), 0.1);
        assert!(r.decision(&subject(), 500_000).is_err());
        r = answer(0.1);
        r.results[0].version = "2".into();
        assert!(r.decision(&subject(), 500_000).is_err());
        r = answer(0.1);
        r.results.push(r.results[0].clone());
        assert!(r.decision(&subject(), 500_000).is_err());
        r = answer(0.1);
        r.results[0].label = "unsafe".into();
        assert!(r.decision(&subject(), 500_000).is_err());
    }
    #[test]
    fn optional_classifiers_are_independent() {
        let mut r = answer(0.1);
        let mut future = r.results[0].clone();
        future.classifier = "harmful-intent".into();
        future.label = "harmful".into();
        future.probabilities = BTreeMap::from([("harmful".into(), 1.0)]);
        r.results.push(future);
        assert_eq!(r.decision(&subject(), 500_000).unwrap(), Decision::Passed);
        r.results.remove(0);
        assert!(r.decision(&subject(), 500_000).is_err());
    }
    #[test]
    fn signature_subject_and_namespace_cannot_be_forged() {
        let keys = Keys::generate();
        let other = Keys::generate();
        let mut event = signed(&answer(0.1), &keys);
        assert!(wire::verify(&event, &other.public_key(), &subject()).is_err());
        let mut wrong = subject();
        wrong.offer = "c".repeat(64);
        wrong.event = wrong.offer.clone();
        assert!(wire::verify(&event, &keys.public_key(), &wrong).is_err());
        event.content.push(' ');
        assert!(wire::verify(&event, &keys.public_key(), &subject()).is_err());
        let mut draft = review_draft(&answer(0.1)).unwrap();
        draft.tags.push(TagSpec::new(["v", "2"]));
        let event = crate::gateway::nostr::event_builder(&draft)
            .unwrap()
            .sign_with_keys(&keys)
            .unwrap();
        assert!(wire::verify(&event, &keys.public_key(), &subject()).is_err());
    }
    #[test]
    fn changed_delivery_requires_new_review() {
        let mut r = answer(0.1);
        r.subject.event = "d".repeat(64);
        r.subject.kind = JOB_RESULT_KIND;
        r.subject.commit = Some("e".repeat(40));
        let mut changed = r.subject.clone();
        changed.commit = Some("f".repeat(40));
        assert!(r.decision(&changed, 500_000).is_err());
        changed = r.subject.clone();
        changed.event = "f".repeat(64);
        assert!(r.decision(&changed, 500_000).is_err());
    }
    #[test]
    fn bounded_input_is_deterministic_and_never_truncated() {
        let mut s = subject();
        s.event = "c".repeat(64);
        s.kind = JOB_RESULT_KIND;
        s.commit = Some("d".repeat(40));
        let a = input_bytes(
            &s,
            "task",
            vec![("b".into(), b"b".to_vec()), ("a".into(), b"a".to_vec())],
        )
        .unwrap();
        let b = input_bytes(
            &s,
            "task",
            vec![("a".into(), b"a".to_vec()), ("b".into(), b"b".to_vec())],
        )
        .unwrap();
        assert_eq!(a, b);
        assert_eq!(input_digest(&a), input_digest(&b));
        assert!(input_bytes(&s, "task", vec![("../secret".into(), vec![])]).is_err());
        assert!(input_bytes(&s, "task", vec![("a".into(), vec![255])]).is_err());
        assert!(input_bytes(&s, "task", vec![("a".into(), vec![b'x'; MAX_INPUT_BYTES])]).is_err());
    }
    #[test]
    fn mock_classifier_uses_bounded_input_and_maps_errors_without_leaking_them() {
        struct Fixture;
        impl Classifier for Fixture {
            fn classify(&self, input: &[u8]) -> Result<Classification, String> {
                assert!(
                    std::str::from_utf8(input)
                        .unwrap()
                        .contains("context-dump fixture")
                );
                Ok(answer(0.9).results.remove(0))
            }
        }
        let r = evaluate(&subject(), "context-dump fixture", vec![], &Fixture).unwrap();
        assert_eq!(r.decision(&subject(), 500_000).unwrap(), Decision::Refused);
        struct Failure;
        impl Classifier for Failure {
            fn classify(&self, _: &[u8]) -> Result<Classification, String> {
                Err("private provider detail must not be broadcast".into())
            }
        }
        let r = evaluate(&subject(), "task", vec![], &Failure).unwrap();
        assert_eq!(r.error_code.as_deref(), Some("provider_unavailable"));
        assert!(
            !serde_json::to_string(&r)
                .unwrap()
                .contains("private provider detail")
        );
    }

    #[test]
    fn default_config_requires_review_and_validates_bounds() {
        let cfg: ReviewConfig = toml::from_str("").unwrap();
        assert!(cfg.seller_offer && cfg.buyer_delivery);
        let bad = ReviewConfig {
            reject_at_or_above_ppm: 1_000_001,
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        let bad = ReviewConfig {
            timeout_seconds: 0,
            ..Default::default()
        };
        assert!(bad.validate().is_err());
    }
}

/// Provider seam: no implementation is selected implicitly, especially not a mock.
pub trait Classifier {
    fn classify(&self, input: &[u8]) -> Result<Classification, String>;
}
/// Turn a bounded immutable snapshot into the extensible result contract.
/// Provider errors contain codes only; do not put provider messages or input in events.
pub fn evaluate<C: Classifier>(
    subject: &Subject,
    task: &str,
    files: Vec<(String, Vec<u8>)>,
    classifier: &C,
) -> Result<Review, String> {
    let input = input_bytes(subject, task, files)?;
    let classified = classifier.classify(&input);
    let (status, results, error_code) = match classified {
        Ok(result) => ("ok".to_owned(), vec![result], None),
        Err(_) => (
            "error".to_owned(),
            vec![],
            Some("provider_unavailable".to_owned()),
        ),
    };
    let result = Review {
        schema: 1,
        subject: subject.clone(),
        input_sha256: input_digest(&input),
        status,
        results,
        error_code,
    };
    result.decision(subject, 500_000)?;
    Ok(result)
}

/// Local evidence is not a substitute for signature verification on a later attempt.
#[cfg(feature = "wallet")]
pub fn record_pass(
    home: &crate::home::MaxplayerHome,
    subject: &Subject,
    event_id: &str,
) -> Result<(), String> {
    subject.validate()?;
    if !hex_id(event_id) {
        return Err("review: invalid evidence id".into());
    }
    let dir = home.root.join("reviews");
    std::fs::create_dir_all(&dir).map_err(|e| format!("review: evidence directory: {e}"))?;
    let evidence = serde_json::json!({"subject": subject, "review_event": event_id,
        "relay": home.config.relay_url, "reviewer": home.config.review.reviewers.get(&home.config.relay_url),
        "reject_at_or_above_ppm": home.config.review.reject_at_or_above_ppm});
    let bytes = serde_json::to_vec(&evidence).map_err(|e| e.to_string())?;
    crate::durable::write_atomic(&dir, &dir.join(format!("{}.json", subject.event)), &bytes)
        .map_err(|e| format!("review: evidence write: {e}"))
}

/// Agent-facing read boundary. Failed/unreviewed delivery payloads never enter an MCP
/// agent context. Review only the newest result from the awarded seller per read; other
/// results remain metadata-only. Acceptance still independently verifies the signed result.
#[cfg(feature = "wallet")]
pub async fn protect_job_view(
    home: &crate::home::MaxplayerHome,
    view: &mut crate::job_lifecycle::JobView,
    awarded_seller: Option<&str>,
) -> BTreeMap<String, String> {
    let mut states = BTreeMap::new();
    let keys = crate::home::read_secret_key_hex(home)
        .ok()
        .and_then(|s| nostr_sdk::Keys::parse(&s).ok());
    let mut attempted = false;
    for result in &mut view.results {
        let subject = Subject {
            offer: view.job_id.clone(),
            event: result.result_id.clone(),
            kind: JOB_RESULT_KIND,
            commit: result.commit_oid.clone(),
        };
        let enabled = home
            .config
            .review
            .enabled(JOB_RESULT_KIND, &result.seller_pubkey);
        if enabled == Ok(false) {
            states.insert(result.result_id.clone(), "disabled".into());
            continue;
        }
        let decision = if !attempted && awarded_seller == Some(result.seller_pubkey.as_str()) {
            attempted = true;
            match &keys {
                Some(keys) => check_buyer(home, keys, &subject, &result.seller_pubkey, result.private_evidence.as_ref()).await,
                None => Err("review: local key unavailable".into()),
            }
        } else {
            Err("review: delivery content withheld; collect the selected delivery to request its review".into())
        };
        match decision {
            Ok(_) => {
                states.insert(result.result_id.clone(), "passed".into());
            }
            Err(error) => {
                states.insert(result.result_id.clone(), error);
                result.inline_answer = None;
                result.job_hash = None;
                result.seller_signature = None;
                result.repo = None;
                result.branch = None;
                result.harness = None;
                result.model = None;
                result.contribution = None;
                result.display_name = None;
            }
        }
    }
    // Accepted bind can itself contain the inline answer / repo strings. A prior acceptance
    // preserves payment recovery, not permission to expose unreviewed text to an agent.
    if let Some(bind) = &mut view.accepted {
        if !matches!(
            states.get(&bind.result_id).map(String::as_str),
            Some("passed" | "disabled")
        ) {
            bind.inline_answer = None;
            bind.repo.clear();
            bind.branch.clear();
            bind.agent_used = None;
            bind.model_used = None;
            bind.contribution = None;
        }
    }
    states
}
