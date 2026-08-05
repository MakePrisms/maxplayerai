//! Freelance-PR (contribution) fork path, additive to the from-scratch money path.
//!
//! A **contribution** job targets a buyer-owned repo (pinned by owner pubkey + clone URL) at an
//! exact `base_oid`; the seller forks that target, works, and delivers a fork tip that MUST
//! **descend** from `base_oid`. The buyer verify-path (ALL pre-pay, ALL against buyer-controlled
//! inputs) fetches the fork tip into the buyer store, resolves `base_oid` **from the PIN** (never the
//! seller echo), and asserts descendant + tip-match + **seller-signed tuple authorship** +
//! content-gate + echo-equality *before* the existing fork-tip commit money bind pays the
//! fork-tip `commit_oid`. Then the buyer merges the **retained local oid** (retention: a
//! fork moved/deleted post-accept cannot strand the buyer).
//!
//! This module holds the pure types + wire helpers. The git gates (fetch into the store, base-from-pin,
//! descendant, content diff) live on the pay-path verifier in `delivery_git.rs`; the authorship
//! tuple's schnorr verify EXTENDS the one pre-pay seam `ReceiptAuthority::verify_seller_prepay_cosig`
//! (`payment.rs`) — one seam, more binds, never a parallel pre-pay gate.

use std::fmt;

use sha2::{Digest, Sha256};

/// `job-class` tag value marking a contribution offer. Absent ⇒ from-scratch (back-compat).
pub const JOB_CLASS_CONTRIBUTION: &str = "contribution";

/// Only seller path shipped in v1 (`accepts=fork`).
pub const ACCEPTS_FORK: &str = "fork";

/// Domain separator for the seller's signed-result authorship tuple. DISTINCT from the
/// receipt-preimage domain (`mobee/v1/receipt-preimage`) so the two seller signatures can never
/// collide — the receipt cosig and the contribution cosig are independent binds at the one seam.
pub const CONTRIBUTION_TUPLE_DOMAIN: &str = "mobee/v1/contribution-tuple";

/// Offer/result tag names (additive; from-scratch offers carry none of them).
pub const TAG_JOB_CLASS: &str = "job-class";
pub const TAG_TARGET_REPO: &str = "target-repo";
pub const TAG_BASE: &str = "base";
pub const TAG_ACCEPTS: &str = "accepts";
pub const TAG_FORK_REF: &str = "fork-ref";
/// `["sig","seller-contribution",<hex>]` — the tuple schnorr signature label. Distinct from the
/// `["sig","seller",..]` receipt cosig so both ride the same result without ambiguity.
pub const SIG_SELLER_CONTRIBUTION: &str = "seller-contribution";

/// A target repo pinned by **owner pubkey + clone URL** — never a bare `d`-tag / name (the relay
/// `.names` registry is GLOBAL across owners, so a bare name is spoofable; `home.rs:105`). This
/// carries the security payload of a NIP-34 `naddr` (owner + locator); the owner segment scopes
/// the fetch so a different owner is a different repo. (Canonical NIP-19 bech32 rendering for
/// observatory interop is a deferred follow-up — the wire form here is the decoded payload.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetRepoPin {
    owner_pubkey: String,
    clone_url: String,
}

impl TargetRepoPin {
    /// Build + validate a pin: owner must be 64-hex; `clone_url` must be non-empty. The transport
    /// allowlist (`https` + relay-git) is enforced at fetch time on the pay path — kept there so
    /// this stays a pure type usable without the git-delivery feature.
    pub fn new(
        owner_pubkey: impl Into<String>,
        clone_url: impl Into<String>,
    ) -> Result<Self, ContributionError> {
        let owner_pubkey = owner_pubkey.into().trim().to_ascii_lowercase();
        let clone_url = clone_url.into().trim().to_owned();
        if owner_pubkey.len() != 64 || !owner_pubkey.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ContributionError::MalformedPin(
                "target-repo owner pubkey must be 64 hex chars".into(),
            ));
        }
        if clone_url.is_empty() {
            return Err(ContributionError::MalformedPin(
                "target-repo clone url is empty".into(),
            ));
        }
        Ok(Self {
            owner_pubkey,
            clone_url,
        })
    }

    /// The pinned owner (hex) — scopes the fetch so a bare global name cannot be substituted.
    pub fn owner_pubkey(&self) -> &str {
        &self.owner_pubkey
    }

    /// The buyer-controlled clone URL `base_oid` is fetched from (MUST-2: base-from-pin).
    pub fn clone_url(&self) -> &str {
        &self.clone_url
    }

    /// Positional tag values: `["target-repo", owner_pubkey, clone_url]`.
    pub fn to_tag_values(&self) -> [String; 3] {
        [
            TAG_TARGET_REPO.to_owned(),
            self.owner_pubkey.clone(),
            self.clone_url.clone(),
        ]
    }
}

impl fmt::Display for TargetRepoPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "naddr(owner={}, url={})", self.owner_pubkey, self.clone_url)
    }
}

/// The base a contribution must descend from: base branch + the exact `base_oid`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContributionBase {
    branch: String,
    oid: String,
}

impl ContributionBase {
    /// Validate: branch non-empty (no leading `-` / control bytes), `base_oid` EXACTLY 40 lowercase
    /// hex chars — a canonical git sha1 commit oid.
    ///
    /// Strict and canonical, refusing fail-closed: mobee repos are sha1 (40-hex), so a 64-hex
    /// (sha256) oid can never resolve — accepting one lets a seller claim an offer it can only fail
    /// at checkout. Uppercase is refused rather than silently normalized so the oid
    /// published on the wire is byte-identical to what the seller's `rev-parse`/verify produces.
    pub fn new(branch: impl Into<String>, oid: impl Into<String>) -> Result<Self, ContributionError> {
        let branch = branch.into().trim().to_owned();
        let oid = oid.into().trim().to_owned();
        if branch.is_empty()
            || branch.starts_with('-')
            || branch.bytes().any(|b| b.is_ascii_control())
        {
            return Err(ContributionError::MalformedBase("base branch is invalid".into()));
        }
        let is_canonical_sha1 = oid.len() == 40
            && oid
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !is_canonical_sha1 {
            return Err(ContributionError::MalformedBase(
                "base_oid must be exactly 40 lowercase hex chars (a git sha1 commit oid)".into(),
            ));
        }
        Ok(Self { branch, oid })
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub fn oid(&self) -> &str {
        &self.oid
    }

    /// Positional tag values: `["base", base_branch, base_oid]`.
    pub fn to_tag_values(&self) -> [String; 3] {
        [TAG_BASE.to_owned(), self.branch.clone(), self.oid.clone()]
    }
}

/// The seller's fork **repo + branch** in the seller's own relay-git namespace (what the buyer
/// fetches into the store and later merges). The fork tip `commit_oid` rides the existing result
/// `commit` tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkRef {
    repo: String,
    branch: String,
}

impl ForkRef {
    pub fn new(repo: impl Into<String>, branch: impl Into<String>) -> Result<Self, ContributionError> {
        let repo = repo.into().trim().to_owned();
        let branch = branch.into().trim().to_owned();
        if repo.is_empty() {
            return Err(ContributionError::MalformedForkRef("fork repo is empty".into()));
        }
        if branch.is_empty()
            || branch.starts_with('-')
            || branch.bytes().any(|b| b.is_ascii_control())
        {
            return Err(ContributionError::MalformedForkRef("fork branch is invalid".into()));
        }
        Ok(Self { repo, branch })
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// A per-job unique store/push ref carrying the **FULL** `job_id`. The
    /// `mobee/<job_id[:8]>` prefix collides — a real field collision already occurred between two
    /// sellers sharing a remote — so the full id + owner-scoped namespaces is the shape.
    pub fn unique_branch(job_id: &str) -> String {
        format!("mobee/contribution/{job_id}")
    }
}

/// A well-formed contribution offer (parsed from a offer-kind offer's additive tags).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContributionOffer {
    pub target: TargetRepoPin,
    pub base: ContributionBase,
    /// Positional multi-value `accepts` (v1 = `["fork"]`).
    pub accepts: Vec<String>,
}

impl ContributionOffer {
    /// True when the offer accepts the fork path.
    pub fn accepts_fork(&self) -> bool {
        self.accepts.iter().any(|a| a == ACCEPTS_FORK)
    }
}

/// The tuple the seller commits to in its schnorr-signed result-kind result. The seller's
/// own signature cryptographically ties `seller_pubkey → this job_id → this exact commit_oid`
/// against the pinned target + base + fork, so it **cannot be paid for a third party's commit**.
/// A git commit trailer is optional provenance only — NEVER this bind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorshipTuple {
    pub job_id: String,
    pub seller_pubkey: String,
    pub target: TargetRepoPin,
    pub base_oid: String,
    pub fork: ForkRef,
    pub commit_oid: String,
}

impl AuthorshipTuple {
    /// Domain-prefixed, fixed-order canonical JSON array — the signed bytes. Field order is
    /// LOCKED; changing it breaks every existing contribution signature.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(&serde_json::json!([
            CONTRIBUTION_TUPLE_DOMAIN,
            self.job_id,
            self.seller_pubkey,
            self.target.owner_pubkey(),
            self.target.clone_url(),
            self.base_oid,
            self.fork.repo(),
            self.fork.branch(),
            self.commit_oid,
        ]))
        .expect("authorship tuple is JSON-serializable")
    }

    /// SHA-256 digest the seller schnorr-signs and the buyer verifies at the pre-pay seam.
    pub fn digest_bytes(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_json().as_bytes());
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&hasher.finalize());
        bytes
    }

    /// Lowercase-hex form of [`Self::digest_bytes`] (for `sign_receipt_hash`-style signing).
    pub fn digest_hex(&self) -> String {
        hex::encode(self.digest_bytes())
    }
}

/// One changed path in a fork-vs-base diff, with an approximate churn size in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
    pub path: String,
    pub bytes: u64,
}

/// Buyer-side content policy hook (MUST-5). The FLOOR (default) refuses only EMPTY diffs; it is
/// **not** a quality gate — an in-scope-but-worthless diff can still pass (quality-judging is
/// deferred to the payment-and-reputation chapter). Path-scope lives here (the offer table has NO
/// paths tag): a buyer MAY tighten pre-pay with a path allowlist + forbidden paths + a max diff
/// size. All prefixes are matched against the diff's changed paths.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ContentPolicy {
    /// Empty ⇒ allow all paths (floor). Non-empty ⇒ every changed path MUST start with one prefix.
    pub allowed_paths: Vec<String>,
    /// A changed path starting with any of these is refused (checked before the allowlist).
    pub forbidden_paths: Vec<String>,
    /// Refuse when the summed churn exceeds this many bytes. `None` ⇒ no size cap.
    pub max_diff_bytes: Option<u64>,
}

impl ContentPolicy {
    /// The floor: allow all, forbid none, no size cap. Refuses ONLY empty diffs.
    pub fn floor() -> Self {
        Self::default()
    }

    /// Evaluate a fork-vs-base diff against the policy. Fail-closed: empty / out-of-scope /
    /// forbidden / too-large ⇒ refuse. `changed` is the set of changed paths (never trusted from
    /// the seller — computed by the buyer in the store).
    pub fn evaluate(&self, changed: &[ChangedPath]) -> Result<(), ContentRefusal> {
        if changed.is_empty() {
            return Err(ContentRefusal::Empty);
        }
        for entry in changed {
            if self
                .forbidden_paths
                .iter()
                .any(|prefix| path_matches(&entry.path, prefix))
            {
                return Err(ContentRefusal::Forbidden {
                    path: entry.path.clone(),
                });
            }
            if !self.allowed_paths.is_empty()
                && !self
                    .allowed_paths
                    .iter()
                    .any(|prefix| path_matches(&entry.path, prefix))
            {
                return Err(ContentRefusal::OutOfScope {
                    path: entry.path.clone(),
                });
            }
        }
        if let Some(cap) = self.max_diff_bytes {
            let total: u64 = changed.iter().map(|c| c.bytes).sum();
            if total > cap {
                return Err(ContentRefusal::TooLarge { total, cap });
            }
        }
        Ok(())
    }
}

/// True when `path` is exactly `prefix` or lies under it as a path segment.
fn path_matches(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    path == prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
}

/// Why the content gate refused (all fail-closed, pre-pay).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentRefusal {
    /// Diff vs base is empty (no changed paths).
    Empty,
    /// A changed path lies under a forbidden prefix.
    Forbidden { path: String },
    /// A changed path is outside the buyer's path allowlist (out-of-scope).
    OutOfScope { path: String },
    /// Summed churn exceeds the buyer's max-diff-size cap.
    TooLarge { total: u64, cap: u64 },
}

impl fmt::Display for ContentRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("contribution diff vs base is empty (content-gate floor)"),
            Self::Forbidden { path } => {
                write!(f, "contribution changes a forbidden path {path:?} (content policy)")
            }
            Self::OutOfScope { path } => write!(
                f,
                "contribution changes out-of-scope path {path:?} (not under the buyer path allowlist)"
            ),
            Self::TooLarge { total, cap } => write!(
                f,
                "contribution diff {total} bytes exceeds max-diff-size {cap} (content policy)"
            ),
        }
    }
}

impl std::error::Error for ContentRefusal {}

/// Fail-closed contribution parse / bind errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContributionError {
    MalformedPin(String),
    MalformedBase(String),
    MalformedForkRef(String),
    /// `job-class=contribution` but a required pin/base tag is missing or malformed.
    MalformedOffer(String),
    /// Result echoed a `{target_repo|base_oid|fork_ref}` that disagrees with the buyer's signed
    /// offer / accept-bind (MUST-4 equality-check; cross-check input, never authority).
    EchoMismatch(String),
    /// The seller-signed authorship tuple signature did not verify.
    Authorship(String),
    /// The seller-signed tuple was absent on a contribution result.
    MissingAuthorship,
}

impl fmt::Display for ContributionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedPin(m) => write!(f, "contribution target-repo pin invalid: {m}"),
            Self::MalformedBase(m) => write!(f, "contribution base invalid: {m}"),
            Self::MalformedForkRef(m) => write!(f, "contribution fork-ref invalid: {m}"),
            Self::MalformedOffer(m) => write!(f, "malformed contribution offer: {m}"),
            Self::EchoMismatch(m) => write!(f, "contribution echo mismatch: {m}"),
            Self::Authorship(m) => write!(f, "contribution authorship refused: {m}"),
            Self::MissingAuthorship => {
                f.write_str("contribution result is missing the seller-signed authorship tuple")
            }
        }
    }
}

impl std::error::Error for ContributionError {}

// ── Wire helpers over the gateway tag model (always available; gateway::TagSpec is not gated) ──

use crate::gateway::TagSpec;

fn tag_row<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a [String]> {
    tags.iter()
        .find(|t| t.0.first().map(String::as_str) == Some(name))
        .map(|t| t.0.as_slice())
}

/// True when an event's tags carry `["job-class","contribution"]`.
pub fn is_contribution_tags(tags: &[TagSpec]) -> bool {
    tag_row(tags, TAG_JOB_CLASS)
        .and_then(|row| row.get(1))
        .map(String::as_str)
        == Some(JOB_CLASS_CONTRIBUTION)
}

/// Additive contribution tags for a offer-kind offer OR a result-kind result echo:
/// `job-class`, `target-repo`, `base`, `accepts…`.
pub fn contribution_offer_tags(offer: &ContributionOffer) -> Vec<TagSpec> {
    let mut tags = vec![
        TagSpec::new([TAG_JOB_CLASS, JOB_CLASS_CONTRIBUTION]),
        TagSpec(offer.target.to_tag_values().to_vec()),
        TagSpec(offer.base.to_tag_values().to_vec()),
    ];
    let mut accepts = vec![TAG_ACCEPTS.to_owned()];
    accepts.extend(offer.accepts.iter().cloned());
    tags.push(TagSpec(accepts));
    tags
}

/// Parse the contribution class from an offer/result's tags. FAIL-CLOSED:
/// - `Ok(None)` when not a contribution (no `job-class=contribution`) ⇒ from-scratch (back-compat).
/// - `Ok(Some(..))` for a well-formed contribution.
/// - `Err(..)` when `job-class=contribution` but a pin/base is missing or malformed — a malformed
///   contribution offer is REFUSED, never silently run as from-scratch.
pub fn parse_contribution_offer(
    tags: &[TagSpec],
) -> Result<Option<ContributionOffer>, ContributionError> {
    if !is_contribution_tags(tags) {
        return Ok(None);
    }
    let target = parse_target_repo(tags)?;
    let base_row = tag_row(tags, TAG_BASE)
        .ok_or_else(|| ContributionError::MalformedOffer("missing base tag".into()))?;
    let branch = base_row
        .get(1)
        .ok_or_else(|| ContributionError::MalformedOffer("base missing branch".into()))?;
    let oid = base_row
        .get(2)
        .ok_or_else(|| ContributionError::MalformedOffer("base missing oid".into()))?;
    let base = ContributionBase::new(branch, oid)?;
    let accepts: Vec<String> = tag_row(tags, TAG_ACCEPTS)
        .map(|row| row[1..].to_vec())
        .unwrap_or_default();
    if accepts.is_empty() {
        return Err(ContributionError::MalformedOffer(
            "accepts tag missing a value (v1 requires accepts=fork)".into(),
        ));
    }
    if !accepts.iter().any(|a| a == ACCEPTS_FORK) {
        return Err(ContributionError::MalformedOffer(format!(
            "v1 supports only accepts=fork; offer accepts {accepts:?}"
        )));
    }
    Ok(Some(ContributionOffer {
        target,
        base,
        accepts,
    }))
}

fn parse_target_repo(tags: &[TagSpec]) -> Result<TargetRepoPin, ContributionError> {
    let row = tag_row(tags, TAG_TARGET_REPO)
        .ok_or_else(|| ContributionError::MalformedOffer("missing target-repo tag".into()))?;
    let owner = row
        .get(1)
        .ok_or_else(|| ContributionError::MalformedOffer("target-repo missing owner".into()))?;
    let clone_url = row
        .get(2)
        .ok_or_else(|| ContributionError::MalformedOffer("target-repo missing clone url".into()))?;
    TargetRepoPin::new(owner, clone_url)
}

/// Parse a seller result's contribution echo. `Ok(None)` when not a contribution; `Ok(Some((echo,
/// tuple_sig)))` for a well-formed contribution result; `Err` when `job-class=contribution` but the
/// echo is malformed or the `sig/seller-contribution` tag is absent (fail-closed — buyer refuses).
pub fn parse_contribution_result_echo(
    tags: &[TagSpec],
) -> Result<Option<(ContributionOffer, String)>, ContributionError> {
    match parse_contribution_offer(tags)? {
        None => Ok(None),
        Some(echo) => {
            let sig = contribution_sig_value(tags).ok_or(ContributionError::MissingAuthorship)?;
            Ok(Some((echo, sig)))
        }
    }
}

/// Read the seller's contribution authorship signature (`["sig","seller-contribution",<hex>]`).
pub fn contribution_sig_value(tags: &[TagSpec]) -> Option<String> {
    tags.iter()
        .find(|t| {
            t.0.first().map(String::as_str) == Some("sig")
                && t.0.get(1).map(String::as_str) == Some(SIG_SELLER_CONTRIBUTION)
        })
        .and_then(|t| t.0.get(2))
        .cloned()
}

/// The `["sig","seller-contribution",<hex>]` tag row.
pub fn contribution_sig_tag(sig_hex: &str) -> TagSpec {
    TagSpec::new(["sig", SIG_SELLER_CONTRIBUTION, sig_hex])
}

/// Seller-side: additive contribution echo + authorship-signature tags for a result-kind result.
/// The seller echoes the offer's `{job-class, target-repo, base, accepts}` and appends its
/// `sig/seller-contribution` over the authorship tuple. The echo is EQUALITY-CHECKED by the buyer
/// against its signed offer (MUST-4) — never trusted as authority.
pub fn contribution_result_tags(offer: &ContributionOffer, tuple_sig_hex: &str) -> Vec<TagSpec> {
    let mut tags = contribution_offer_tags(offer);
    tags.push(contribution_sig_tag(tuple_sig_hex));
    tags
}

#[cfg(feature = "gateway")]
mod schnorr {
    use super::AuthorshipTuple;
    use nostr_sdk::secp256k1::{Message, Secp256k1};
    use nostr_sdk::{Keys, PublicKey as NostrPublicKey};

    /// Seller-side: schnorr-sign the authorship tuple digest with the seller key (hex). This is the
    /// `sig/seller-contribution` value; the buyer verifies it at the pre-pay seam.
    pub fn sign_authorship_tuple(keys: &Keys, tuple: &AuthorshipTuple) -> String {
        keys.sign_schnorr(&Message::from_digest(tuple.digest_bytes()))
            .to_string()
    }

    /// Verify a seller's schnorr signature over the authorship tuple digest against `seller`.
    /// Any parse / verification failure is `Err(())` (fail closed) — the caller maps it to a
    /// zero-spend refusal. This is the SAME verification the pre-pay seam performs; exposed here
    /// for the seller-side round-trip test.
    pub fn verify_tuple_sig(
        tuple: &AuthorshipTuple,
        sig_hex: &str,
        seller: &NostrPublicKey,
    ) -> Result<(), ()> {
        use std::str::FromStr;
        let signature =
            nostr_sdk::secp256k1::schnorr::Signature::from_str(sig_hex).map_err(|_| ())?;
        let anchor = seller.xonly().map_err(|_| ())?;
        Secp256k1::verification_only()
            .verify_schnorr(&signature, &Message::from_digest(tuple.digest_bytes()), &anchor)
            .map_err(|_| ())
    }
}

#[cfg(feature = "gateway")]
pub use schnorr::{sign_authorship_tuple, verify_tuple_sig};

#[cfg(test)]
mod tests {
    use super::*;

    fn pin() -> TargetRepoPin {
        TargetRepoPin::new("aa".repeat(32), "https://mobee-relay.orveth.dev/git/owner/repo.git")
            .expect("pin")
    }

    #[test]
    fn pin_rejects_bare_name_without_owner() {
        assert!(matches!(
            TargetRepoPin::new("not-hex", "https://x/repo.git"),
            Err(ContributionError::MalformedPin(_))
        ));
        assert!(matches!(
            TargetRepoPin::new("aa".repeat(32), ""),
            Err(ContributionError::MalformedPin(_))
        ));
    }

    #[test]
    fn base_requires_full_oid() {
        assert!(ContributionBase::new("main", "abc").is_err());
        assert!(ContributionBase::new("main", "z".repeat(40)).is_err());
        assert!(ContributionBase::new("-x", "a".repeat(40)).is_err());
        assert!(ContributionBase::new("main", "a".repeat(40)).is_ok());
    }

    // base_oid must be EXACTLY 40 lowercase hex — refuse the malformed shapes that let a
    // seller claim an offer it can only fail at checkout (a 64-hex sha256 oid, a short oid), and
    // refuse uppercase rather than normalizing it (the wire oid must match the seller's rev-parse).
    #[test]
    fn base_oid_requires_exactly_40_lowercase_hex() {
        // 64-hex (sha256) refused — mobee repos are sha1, so it can never resolve.
        let err = ContributionBase::new("main", "a".repeat(64)).expect_err("64-hex must refuse");
        assert!(
            matches!(&err, ContributionError::MalformedBase(m) if m.contains("base_oid")),
            "refusal must name the base_oid field: {err:?}"
        );
        // 39-hex (short) refused.
        assert!(ContributionBase::new("main", "a".repeat(39)).is_err());
        // 41-hex (long) refused.
        assert!(ContributionBase::new("main", "a".repeat(41)).is_err());
        // Uppercase refused (not silently lowercased).
        assert!(ContributionBase::new("main", "A".repeat(40)).is_err());
        assert!(ContributionBase::new("main", "deadBEEF".to_owned() + &"a".repeat(32)).is_err());
        // Valid canonical 40 lowercase hex passes, unchanged on the way through.
        let ok = ContributionBase::new("main", "0123456789abcdef".to_owned() + &"a".repeat(24))
            .expect("canonical 40 lowercase hex must pass");
        assert_eq!(ok.oid().len(), 40);
    }

    #[test]
    fn unique_branch_carries_full_job_id_not_prefix() {
        let job_id = "b".repeat(64);
        let branch = ForkRef::unique_branch(&job_id);
        assert!(branch.contains(&job_id), "full job id must be in the ref (MUST-6)");
        // The colliding `[:8]` prefix must NOT be the whole leaf.
        assert_ne!(branch, format!("mobee/{}", &job_id[..8]));
    }

    #[test]
    fn tuple_digest_binds_every_field() {
        let base = AuthorshipTuple {
            job_id: "job".into(),
            seller_pubkey: "cc".repeat(32),
            target: pin(),
            base_oid: "a".repeat(40),
            fork: ForkRef::new("https://x/git/seller/fork.git", "mobee/contribution/job").unwrap(),
            commit_oid: "d".repeat(40),
        };
        let d0 = base.digest_hex();
        assert_eq!(d0.len(), 64);
        let mut swap_commit = base.clone();
        swap_commit.commit_oid = "e".repeat(40);
        assert_ne!(d0, swap_commit.digest_hex(), "commit_oid must be bound");
        let mut swap_base = base.clone();
        swap_base.base_oid = "f".repeat(40);
        assert_ne!(d0, swap_base.digest_hex(), "base_oid must be bound");
        let mut swap_target =
            AuthorshipTuple { target: TargetRepoPin::new("bb".repeat(32), pin().clone_url()).unwrap(), ..base.clone() };
        assert_ne!(d0, swap_target.digest_hex(), "target owner must be bound");
        swap_target.target = TargetRepoPin::new(pin().owner_pubkey(), "https://x/other.git").unwrap();
        assert_ne!(d0, swap_target.digest_hex(), "clone url must be bound");
    }

    #[test]
    fn tuple_canonical_json_is_domain_prefixed_locked_order() {
        let tuple = AuthorshipTuple {
            job_id: "j".into(),
            seller_pubkey: "cc".repeat(32),
            target: TargetRepoPin::new("aa".repeat(32), "https://x/repo.git").unwrap(),
            base_oid: "a".repeat(40),
            fork: ForkRef::new("https://x/fork.git", "wb").unwrap(),
            commit_oid: "d".repeat(40),
        };
        let json = tuple.canonical_json();
        assert!(json.starts_with(&format!("[\"{CONTRIBUTION_TUPLE_DOMAIN}\",\"j\",")));
        assert!(json.ends_with(&format!("\"{}\"]", "d".repeat(40))));
    }

    #[test]
    fn content_gate_floor_refuses_only_empty() {
        let floor = ContentPolicy::floor();
        assert_eq!(floor.evaluate(&[]), Err(ContentRefusal::Empty));
        assert!(floor
            .evaluate(&[ChangedPath { path: "anything/at/all.rs".into(), bytes: 1 }])
            .is_ok());
    }

    #[test]
    fn content_policy_scopes_paths_and_size() {
        let policy = ContentPolicy {
            allowed_paths: vec!["src".into(), "docs/".into()],
            forbidden_paths: vec!["src/secrets".into()],
            max_diff_bytes: Some(100),
        };
        // in-scope, under cap → pass
        assert!(policy
            .evaluate(&[ChangedPath { path: "src/lib.rs".into(), bytes: 50 }])
            .is_ok());
        // out-of-scope → refuse
        assert!(matches!(
            policy.evaluate(&[ChangedPath { path: "tests/x.rs".into(), bytes: 1 }]),
            Err(ContentRefusal::OutOfScope { .. })
        ));
        // forbidden prefix wins even though `src` is allowed
        assert!(matches!(
            policy.evaluate(&[ChangedPath { path: "src/secrets/key".into(), bytes: 1 }]),
            Err(ContentRefusal::Forbidden { .. })
        ));
        // over cap → refuse
        assert!(matches!(
            policy.evaluate(&[ChangedPath { path: "src/big.rs".into(), bytes: 101 }]),
            Err(ContentRefusal::TooLarge { .. })
        ));
        // path_matches must not treat `src` as a prefix of `src-other`
        assert!(matches!(
            policy.evaluate(&[ChangedPath { path: "src-other/x".into(), bytes: 1 }]),
            Err(ContentRefusal::OutOfScope { .. })
        ));
    }

    #[test]
    fn offer_tags_round_trip_through_parse() {
        let offer = ContributionOffer {
            target: pin(),
            base: ContributionBase::new("main", "a".repeat(40)).unwrap(),
            accepts: vec![ACCEPTS_FORK.to_owned()],
        };
        let tags = contribution_offer_tags(&offer);
        assert!(is_contribution_tags(&tags));
        let parsed = parse_contribution_offer(&tags).expect("parse ok").expect("is contribution");
        assert_eq!(parsed, offer);
        assert!(parsed.accepts_fork());
    }

    #[cfg(feature = "gateway")]
    #[test]
    fn tuple_sign_verify_round_trip_and_wrong_key_refused() {
        use nostr_sdk::Keys;
        let seller = Keys::generate();
        let attacker = Keys::generate();
        let tuple = AuthorshipTuple {
            job_id: "job".into(),
            seller_pubkey: seller.public_key().to_hex(),
            target: pin(),
            base_oid: "a".repeat(40),
            fork: ForkRef::new("https://x/git/seller/fork.git", "mobee/contribution/job").unwrap(),
            commit_oid: "d".repeat(40),
        };
        let sig = sign_authorship_tuple(&seller, &tuple);
        // Correct seller key verifies; an attacker key does not.
        verify_tuple_sig(&tuple, &sig, &seller.public_key()).expect("seller sig verifies");
        assert!(verify_tuple_sig(&tuple, &sig, &attacker.public_key()).is_err());
        // A sig over the honest tuple does not verify once a field is flipped.
        let mut tampered = tuple.clone();
        tampered.commit_oid = "e".repeat(40);
        assert!(verify_tuple_sig(&tampered, &sig, &seller.public_key()).is_err());
        // The seller's result-echo tags carry the sig + the offer echo, and round-trip through parse.
        let offer = ContributionOffer {
            target: pin(),
            base: ContributionBase::new("main", "a".repeat(40)).unwrap(),
            accepts: vec![ACCEPTS_FORK.to_owned()],
        };
        let tags = contribution_result_tags(&offer, &sig);
        let (echo, parsed_sig) = parse_contribution_result_echo(&tags)
            .expect("parse ok")
            .expect("is a contribution result");
        assert_eq!(echo, offer);
        assert_eq!(parsed_sig, sig);
    }

    #[test]
    fn non_contribution_tags_parse_to_none() {
        let tags = vec![TagSpec::new(["output", "text"]), TagSpec::new(["t", "mobee"])];
        assert_eq!(parse_contribution_offer(&tags), Ok(None));
    }

    #[test]
    fn malformed_contribution_offer_fails_closed_never_from_scratch() {
        // job-class=contribution but no target-repo / base → REFUSE (not silent from-scratch).
        let tags = vec![TagSpec::new([TAG_JOB_CLASS, JOB_CLASS_CONTRIBUTION])];
        assert!(matches!(
            parse_contribution_offer(&tags),
            Err(ContributionError::MalformedOffer(_))
        ));
        // present but malformed base oid → REFUSE.
        let tags = vec![
            TagSpec::new([TAG_JOB_CLASS, JOB_CLASS_CONTRIBUTION]),
            TagSpec(pin().to_tag_values().to_vec()),
            TagSpec::new([TAG_BASE, "main", "not-an-oid"]),
            TagSpec::new([TAG_ACCEPTS, ACCEPTS_FORK]),
        ];
        assert!(parse_contribution_offer(&tags).is_err());
        // patch-only accepts (no fork) → REFUSE in v1.
        let tags = vec![
            TagSpec::new([TAG_JOB_CLASS, JOB_CLASS_CONTRIBUTION]),
            TagSpec(pin().to_tag_values().to_vec()),
            TagSpec::new([TAG_BASE, "main", &"a".repeat(40)]),
            TagSpec::new([TAG_ACCEPTS, "patch"]),
        ];
        assert!(parse_contribution_offer(&tags).is_err());
    }
}
