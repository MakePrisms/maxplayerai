use std::fmt;

use crate::receipt::DeliveryKind;

/// A full Git commit object id advertised by a seller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitOid(String);

impl CommitOid {
    /// Parses a full SHA-1 or SHA-256 Git object id.
    pub fn parse(value: impl Into<String>) -> Result<Self, DeliveryError> {
        let value = value.into();
        if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DeliveryError::InvalidCommitOid);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the canonical lowercase object id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Git delivery fields advertised by a result event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDelivery {
    repo: String,
    branch: String,
    commit_oid: CommitOid,
}

impl GitDelivery {
    /// Creates an advertised git delivery from result-event fields.
    pub fn new(
        repo: impl Into<String>,
        branch: impl Into<String>,
        commit_oid: CommitOid,
    ) -> Result<Self, DeliveryError> {
        let repo = repo.into();
        let branch = branch.into();
        if repo.is_empty() {
            return Err(DeliveryError::MissingRepo);
        }
        if branch.is_empty()
            || branch.starts_with('-')
            || branch.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(DeliveryError::InvalidBranch);
        }
        Ok(Self {
            repo,
            branch,
            commit_oid,
        })
    }

    /// Returns the repository locator carried by the event.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Returns the advertised branch name.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Returns the advertised full commit object id.
    pub fn commit_oid(&self) -> &CommitOid {
        &self.commit_oid
    }

    /// Wire delivery-kind for a git (fork-tip) delivery: [`DeliveryKind::Fork`] (`"fork"`).
    pub fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Fork
    }
}

/// Proof that the advertised branch tip was fetched into the buyer store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedDelivery {
    commit_oid: CommitOid,
}

impl VerifiedDelivery {
    /// Binds a fetched tip to the advertisement or refuses a mismatch.
    pub fn from_fetched_tip(
        advertised: &GitDelivery,
        fetched_tip: CommitOid,
    ) -> Result<Self, DeliveryError> {
        if &fetched_tip != advertised.commit_oid() {
            return Err(DeliveryError::TipMismatch {
                expected: advertised.commit_oid().clone(),
                actual: fetched_tip,
            });
        }
        Ok(Self {
            commit_oid: advertised.commit_oid().clone(),
        })
    }

    /// Returns the verified commit object id used by payment and receipt binding.
    pub fn commit_oid(&self) -> &CommitOid {
        &self.commit_oid
    }
}

/// Injected buyer-side delivery verification effect.
pub trait DeliveryVerifier {
    /// Fetches and verifies a delivery before payment intent is persisted.
    fn verify(&mut self, delivery: &GitDelivery) -> Result<VerifiedDelivery, DeliveryError>;
}

/// Fail-closed delivery verification errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryError {
    InvalidCommitOid,
    MissingRepo,
    InvalidBranch,
    Transport(crate::delivery_transport::TransportRefuse),
    GitUnavailable,
    /// A required child process (e.g. `git`) could not be spawned. Names the program
    /// and the `io::ErrorKind` so a missing binary is not misreported as "git unavailable".
    GitSpawnFailed {
        program: &'static str,
        kind: std::io::ErrorKind,
    },
    GitCommandFailed(&'static str),
    MissingFetchedTip,
    TipMismatch {
        expected: CommitOid,
        actual: CommitOid,
    },
    MissingCommitObject,
    /// Contribution: `base_oid` is not present in the PINNED target (base-from-pin
    /// fetch produced no such object). Fail-closed — the buyer never bases against a value it
    /// could not resolve from its own signed offer's target.
    MissingBaseObject,
    /// Contribution: the delivered `commit_oid` does NOT descend from `base_oid`
    /// (`git merge-base --is-ancestor` refused). Closes unrelated-history / swapped-base.
    NotDescendant {
        base_oid: String,
        commit_oid: String,
    },
    /// Contribution: the content gate refused (empty / out-of-scope / forbidden / too-large).
    ContentRefused(String),
    /// Contribution: merging the retained local `commit_oid` into the target failed.
    MergeFailed(&'static str),
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommitOid => {
                formatter.write_str("delivery commit oid must be 40 or 64 hex characters")
            }
            Self::MissingRepo => formatter.write_str("delivery repository is missing"),
            Self::InvalidBranch => formatter.write_str("delivery branch is invalid"),
            Self::Transport(refuse) => write!(formatter, "{refuse}"),
            Self::GitUnavailable => formatter.write_str("git executable is unavailable"),
            Self::GitSpawnFailed { program, kind } => {
                write!(formatter, "failed to spawn {program}: {kind:?}")
            }
            Self::GitCommandFailed(operation) => write!(formatter, "git {operation} failed"),
            Self::MissingFetchedTip => {
                formatter.write_str("git fetch did not produce one commit tip")
            }
            Self::TipMismatch { expected, actual } => {
                write!(
                    formatter,
                    "fetched tip {actual} does not match advertised {expected}"
                )
            }
            Self::MissingCommitObject => {
                formatter.write_str("fetched commit object is not in the buyer store")
            }
            Self::MissingBaseObject => {
                formatter.write_str("base_oid is not present in the pinned target repo (base-from-pin)")
            }
            Self::NotDescendant {
                base_oid,
                commit_oid,
            } => write!(
                formatter,
                "delivered commit {commit_oid} does not descend from base {base_oid}"
            ),
            Self::ContentRefused(reason) => write!(formatter, "content gate refused: {reason}"),
            Self::MergeFailed(operation) => {
                write!(formatter, "retained merge {operation} failed")
            }
        }
    }
}

impl std::error::Error for DeliveryError {}

impl From<crate::delivery_transport::TransportRefuse> for DeliveryError {
    fn from(value: crate::delivery_transport::TransportRefuse) -> Self {
        Self::Transport(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_oid_requires_full_hex() {
        assert_eq!(
            CommitOid::parse("abc"),
            Err(DeliveryError::InvalidCommitOid)
        );
        assert_eq!(
            CommitOid::parse("z".repeat(40)),
            Err(DeliveryError::InvalidCommitOid)
        );
        assert_eq!(
            CommitOid::parse("A".repeat(40)).expect("full oid").as_str(),
            "a".repeat(40)
        );
        assert_eq!(
            CommitOid::parse("B".repeat(64))
                .expect("full sha256 oid")
                .as_str(),
            "b".repeat(64)
        );
    }

    #[test]
    fn verified_delivery_refuses_a_different_fetched_tip() {
        let advertised = GitDelivery::new(
            "repo",
            "work",
            CommitOid::parse("1".repeat(40)).expect("advertised oid"),
        )
        .expect("delivery");

        assert!(matches!(
            VerifiedDelivery::from_fetched_tip(
                &advertised,
                CommitOid::parse("2".repeat(40)).expect("fetched oid")
            ),
            Err(DeliveryError::TipMismatch { .. })
        ));
    }

    fn commit_delivery(oid: &str) -> GitDelivery {
        GitDelivery::new(
            "https://example.invalid/repo.git",
            "main",
            CommitOid::parse(oid).expect("oid"),
        )
        .expect("delivery")
    }

    #[test]
    fn git_delivery_binds_commit_oid_and_derives_fork_kind() {
        // The bound-oid and delivery-kind derivations are load-bearing seams (breaking either
        // regresses the money path).
        let delivery = commit_delivery(&"1".repeat(40));
        assert_eq!(delivery.commit_oid().as_str(), &"1".repeat(40));
        assert_eq!(delivery.delivery_kind(), DeliveryKind::Fork);
        assert_eq!(delivery.delivery_kind().as_str(), "fork");
    }

}
