//! Transport-scheme allowlist for seller-supplied git locators.
//!
//! Lands with the delivery-verify caller: allow `https` and relay-git (https + `/git/…`);
//! refuse `ext::` / `file` / `ssh` and related bypass shapes (case tricks, creds-in-URL,
//! local paths). Redirect following is out of scope here — this gates the locator string
//! before any fetch.
//!
//! By construction on the pay path: use [`crate::delivery_git::PayPathDeliveryVerifier`].
//! Bare `GitDeliveryVerifier` is `pub(crate)` (test-only inside core); peel/`into_inner`
//! on the allowlist wrapper are crate-private so the allowlist cannot be stripped outside.

use std::fmt;

use crate::delivery::{DeliveryError, DeliveryVerifier, GitDelivery, VerifiedDelivery};

/// Fail-closed transport refusal for a seller-supplied repo locator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportRefuse {
    Empty,
    ForbiddenScheme(String),
    CredentialsInUrl,
    LocalPath,
    ScpLikeSsh,
    MissingHost,
}

impl fmt::Display for TransportRefuse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("delivery repo locator is empty"),
            Self::ForbiddenScheme(scheme) => write!(
                formatter,
                "delivery repo transport {scheme:?} refused (allowlist: https + relay-git)"
            ),
            Self::CredentialsInUrl => {
                formatter.write_str("delivery repo locator must not embed credentials")
            }
            Self::LocalPath => formatter.write_str(
                "delivery repo local/path locator refused (allowlist: https + relay-git)",
            ),
            Self::ScpLikeSsh => formatter.write_str(
                "delivery repo scp-like ssh locator refused (allowlist: https + relay-git)",
            ),
            Self::MissingHost => formatter.write_str("delivery repo locator is missing a host"),
        }
    }
}

impl std::error::Error for TransportRefuse {}

/// Allow only `https` (incl. relay-git `https://…/git/<owner>/<repo>`); refuse unsafe schemes.
///
/// Case is normalized on the scheme only (`HTTPS://` → allow). Userinfo (`user:pass@`) is
/// refused. Scheme-less paths and `git@host:path` scp forms are refused (git would treat
/// them as local/ssh).
pub fn assert_allowed_repo_locator(repo: &str) -> Result<(), TransportRefuse> {
    let trimmed = repo.trim();
    if trimmed.is_empty() {
        return Err(TransportRefuse::Empty);
    }

    // `ext::` is not a URL scheme git accepts via `://`, but git remote helpers use it.
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("ext::") {
        return Err(TransportRefuse::ForbiddenScheme("ext".into()));
    }

    // scp-like: git@host:path (no ://)
    if !trimmed.contains("://") {
        if trimmed.contains('@') {
            return Err(TransportRefuse::ScpLikeSsh);
        }
        return Err(TransportRefuse::LocalPath);
    }

    let (scheme_raw, rest) = trimmed
        .split_once("://")
        .expect(":// present after contains check");
    if scheme_raw.is_empty() || scheme_raw.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(TransportRefuse::ForbiddenScheme(scheme_raw.to_owned()));
    }
    let scheme = scheme_raw.to_ascii_lowercase();
    match scheme.as_str() {
        "https" => {}
        "http" | "file" | "ssh" | "git" | "ext" => {
            return Err(TransportRefuse::ForbiddenScheme(scheme));
        }
        other => return Err(TransportRefuse::ForbiddenScheme(other.to_owned())),
    }

    // Authority ends at first `/`, `?`, or `#`.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(TransportRefuse::MissingHost);
    }
    // Credentials: userinfo before final `@` in authority (host may be IPv6 in [...]).
    if let Some((userinfo, hostport)) = authority.rsplit_once('@') {
        if !userinfo.is_empty() {
            return Err(TransportRefuse::CredentialsInUrl);
        }
        if hostport.is_empty() {
            return Err(TransportRefuse::MissingHost);
        }
    }

    Ok(())
}

/// True when an allowed https locator looks like relay-git (`…/git/<owner>/<repo>`).
///
/// Informational helper for callers; allowlist itself accepts any credential-free https.
pub fn is_relay_git_locator(repo: &str) -> bool {
    if assert_allowed_repo_locator(repo).is_err() {
        return false;
    }
    repo.to_ascii_lowercase().contains("/git/")
}

/// Delivery verifier that enforces the transport allowlist before the inner verify/fetch.
///
/// Crate-visible composition helper. Outside core, obtain a fetch-capable verifier only via
/// [`crate::delivery_git::PayPathDeliveryVerifier`] — peel/`into_inner` stay crate-private.
pub(crate) struct AllowlistedDeliveryVerifier<V> {
    inner: V,
}

impl<V> AllowlistedDeliveryVerifier<V> {
    pub(crate) fn new(inner: V) -> Self {
        Self { inner }
    }

    #[allow(dead_code)] // kept crate-private so peel/`into_inner` cannot strip allowlist outside core
    pub(crate) fn into_inner(self) -> V {
        self.inner
    }

    pub(crate) fn inner(&self) -> &V {
        &self.inner
    }

    #[allow(dead_code)] // kept crate-private so peel/`into_inner` cannot strip allowlist outside core
    pub(crate) fn inner_mut(&mut self) -> &mut V {
        &mut self.inner
    }
}

impl<V: DeliveryVerifier> DeliveryVerifier for AllowlistedDeliveryVerifier<V> {
    fn verify(&mut self, delivery: &GitDelivery) -> Result<VerifiedDelivery, DeliveryError> {
        assert_allowed_repo_locator(delivery.repo())?;
        self.inner.verify(delivery)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::{CommitOid, DeliveryError, GitDelivery};

    #[test]
    fn allows_https_and_relay_git() {
        assert_allowed_repo_locator("https://example.invalid/repo.git").expect("https");
        assert_allowed_repo_locator(
            "https://relay.example/git/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789/job.git",
        )
        .expect("relay-git");
        assert_allowed_repo_locator("HTTPS://Example.INVALID/Repo.GIT").expect("case");
    }

    #[test]
    fn refuses_ext_file_ssh_and_local() {
        assert_eq!(
            assert_allowed_repo_locator("ext::sh -c evil"),
            Err(TransportRefuse::ForbiddenScheme("ext".into()))
        );
        assert_eq!(
            assert_allowed_repo_locator("EXT::sh"),
            Err(TransportRefuse::ForbiddenScheme("ext".into()))
        );
        assert_eq!(
            assert_allowed_repo_locator("file:///tmp/repo.git"),
            Err(TransportRefuse::ForbiddenScheme("file".into()))
        );
        assert_eq!(
            assert_allowed_repo_locator("FILE:///tmp/repo.git"),
            Err(TransportRefuse::ForbiddenScheme("file".into()))
        );
        assert_eq!(
            assert_allowed_repo_locator("ssh://git@host/repo.git"),
            Err(TransportRefuse::ForbiddenScheme("ssh".into()))
        );
        assert_eq!(
            assert_allowed_repo_locator("git@github.com:org/repo.git"),
            Err(TransportRefuse::ScpLikeSsh)
        );
        assert_eq!(
            assert_allowed_repo_locator("/absolute/path/repo.git"),
            Err(TransportRefuse::LocalPath)
        );
        assert_eq!(
            assert_allowed_repo_locator("http://example.invalid/repo.git"),
            Err(TransportRefuse::ForbiddenScheme("http".into()))
        );
    }

    #[test]
    fn refuses_credentials_in_url() {
        assert_eq!(
            assert_allowed_repo_locator("https://user:pass@example.invalid/repo.git"),
            Err(TransportRefuse::CredentialsInUrl)
        );
        assert_eq!(
            assert_allowed_repo_locator("https://token@example.invalid/repo.git"),
            Err(TransportRefuse::CredentialsInUrl)
        );
    }

    #[test]
    fn relay_git_helper_detects_path_shape() {
        assert!(is_relay_git_locator(
            "https://relay.example/git/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789/job.git"
        ));
        assert!(!is_relay_git_locator("https://example.invalid/repo.git"));
        assert!(!is_relay_git_locator("ext::nope"));
    }

    struct BoomVerifier;

    impl DeliveryVerifier for BoomVerifier {
        fn verify(
            &mut self,
            _delivery: &GitDelivery,
        ) -> Result<VerifiedDelivery, DeliveryError> {
            panic!("inner verify must not run on refused transport");
        }
    }

    #[test]
    fn allowlisted_wrapper_refuses_ext_before_inner() {
        let delivery = GitDelivery::new(
            "ext::sh -c evil",
            "main",
            CommitOid::parse("1".repeat(40)).expect("oid"),
        )
        .expect("delivery");
        let mut verifier = AllowlistedDeliveryVerifier::new(BoomVerifier);
        let err = verifier.verify(&delivery).expect_err("refuse");
        assert!(matches!(
            err,
            DeliveryError::Transport(TransportRefuse::ForbiddenScheme(_))
        ));
    }
}
