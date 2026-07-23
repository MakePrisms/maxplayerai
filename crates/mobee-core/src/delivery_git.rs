//! Buyer-side delivery verification, in-process via libgit2.
//!
//! The pay path fetches the seller's delivered fork tip (and the pinned base) into a buyer-owned
//! local bare repository (the buyer store), all through [`crate::git_transport`]'s rustls smart-HTTP subtransport
//! (NIP-98 auth for relay-git reads), then runs the contribution gates entirely in-process:
//! tip-match, base-from-pin, descendant, and the content policy. A hung fetch fails CLOSED under a
//! short per-leg HTTP timeout so `authorize_pay` never burns budget on a stalled verify.

use std::fs;
use std::path::{Path, PathBuf};

use git2::{DiffOptions, Oid, Repository};

use crate::delivery::{CommitOid, DeliveryError, DeliveryVerifier, GitDelivery, VerifiedDelivery};
use crate::delivery_transport::AllowlistedDeliveryVerifier;
use crate::git_transport;

/// Real git-backed verifier that retains fetched objects in a buyer-owned repository.
///
/// Crate-private on purpose: bare git verify can fetch `ext::` (RCE). The pay path
/// must use [`PayPathDeliveryVerifier`] — the only public factory that hands out a
/// fetch-capable verifier, with the transport allowlist sealed in.
pub(crate) struct GitDeliveryVerifier {
    repository: PathBuf,
    /// Buyer secret (hex) for NIP-98 on relay-git READS. `None` ⇒ anonymous fetch (public https
    /// bases, and the local-path fixtures in tests). Never logged, never leaves the process.
    buyer_secret_hex: Option<String>,
}

impl GitDeliveryVerifier {
    /// Creates a verifier whose fetched object database lives at `repository`.
    pub(crate) fn new(repository: impl Into<PathBuf>) -> Self {
        Self {
            repository: repository.into(),
            buyer_secret_hex: None,
        }
    }

    /// Attach the buyer secret used to sign NIP-98 on relay-git reads.
    pub(crate) fn with_buyer_secret(mut self, buyer_secret_hex: Option<String>) -> Self {
        self.buyer_secret_hex = buyer_secret_hex;
        self
    }

    /// Returns the local repository that holds verified delivery objects.
    pub(crate) fn repository(&self) -> &Path {
        &self.repository
    }

    /// NIP-98 auth to present on a read of `remote_url`: the buyer secret for relay-git targets,
    /// `None` for public https / local-path targets (`git_transport` gates on `is_relay_git`).
    fn read_auth(&self) -> Option<&str> {
        self.buyer_secret_hex.as_deref()
    }

    /// Open the buyer store (a buyer-owned local bare repository).
    fn open_store(&self) -> Result<Repository, DeliveryError> {
        Repository::open_bare(&self.repository)
            .map_err(|_| DeliveryError::GitCommandFailed("open-store"))
    }

    /// Ensure the buyer store exists. mobee sellers always emit sha1 objects (they `init` without
    /// `--object-format`), so the store is sha1; a sha256 (64-hex) delivery is not reachable in the
    /// mobee system and git2 0.19 cannot init a sha256 odb — so it fails CLOSED here rather than
    /// silently mishandling one.
    fn ensure_repository(&self, oid: &CommitOid) -> Result<(), DeliveryError> {
        if oid.as_str().len() == 64 {
            return Err(DeliveryError::GitCommandFailed("object-format"));
        }
        if self.repository.join("HEAD").is_file() {
            // Already a git dir — confirm it opens; a corrupt store fails closed.
            self.open_store()?;
            return Ok(());
        }
        if let Some(parent) = self.repository.parent() {
            fs::create_dir_all(parent).map_err(|_| DeliveryError::GitCommandFailed("init"))?;
        }
        Repository::init_bare(&self.repository).map_err(|_| DeliveryError::GitCommandFailed("init"))?;
        Ok(())
    }

    /// Validate a branch name (mirrors `git check-ref-format --branch`): the `refs/heads/<branch>`
    /// form must be a valid ref name. Fail-closed to [`DeliveryError::InvalidBranch`].
    fn check_branch(&self, branch: &str) -> Result<(), DeliveryError> {
        if branch.is_empty() || !Reference_is_valid_branch(branch) {
            return Err(DeliveryError::InvalidBranch);
        }
        Ok(())
    }

    fn parse_oid(oid: &CommitOid) -> Result<Oid, DeliveryError> {
        Oid::from_str(oid.as_str()).map_err(|_| DeliveryError::InvalidCommitOid)
    }

    fn fetch(&self, delivery: &GitDelivery) -> Result<CommitOid, DeliveryError> {
        let repo = self.open_store()?;
        let fetched_ref = format!("refs/mobee/deliveries/{}", delivery.commit_oid().as_str());
        let refspec = format!("+refs/heads/{}:{fetched_ref}", delivery.branch());
        // short_timeout=true: a hung fetch must not own the MCP stdio loop past the client timeout,
        // and must fail CLOSED before authorize_pay burns budget (verify-before-pay).
        git_transport::fetch_refspecs(&repo, delivery.repo(), &[&refspec], self.read_auth(), true)
            .map_err(|_| DeliveryError::GitCommandFailed("fetch"))?;

        let fetched_object = format!("{fetched_ref}^{{commit}}");
        let commit = repo
            .revparse_single(&fetched_object)
            .and_then(|object| object.peel_to_commit())
            .map_err(|_| DeliveryError::MissingFetchedTip)?;
        CommitOid::parse(commit.id().to_string()).map_err(|_| DeliveryError::MissingFetchedTip)
    }

    fn require_local_object(&self, oid: &CommitOid) -> Result<(), DeliveryError> {
        let repo = self.open_store()?;
        let parsed = Self::parse_oid(oid)?;
        repo.find_commit(parsed)
            .map(|_| ())
            .map_err(|_| DeliveryError::MissingCommitObject)
    }

    /// Contribution: fetch `base_oid` from the **pinned target** clone URL into the SAME
    /// buyer store, then prove `base_oid` is present in that target — fail-closed if absent.
    ///
    /// FETCH DEPTH: this is a FULL fetch of the base branch (no depth limit), so
    /// `base_oid` and the ancestry chain up to the fork tip are all in the store. A shallow / tip-only
    /// fetch would make the later descendant gate FALSE-REFUSE an honest deep contribution.
    fn fetch_base(
        &self,
        base_clone_url: &str,
        base_branch: &str,
        base_oid: &CommitOid,
    ) -> Result<(), DeliveryError> {
        self.check_branch(base_branch)?;
        let repo = self.open_store()?;
        let fetched_ref = format!("refs/mobee/bases/{}", base_oid.as_str());
        let refspec = format!("+refs/heads/{base_branch}:{fetched_ref}");
        git_transport::fetch_refspecs(&repo, base_clone_url, &[&refspec], self.read_auth(), true)
            .map_err(|_| DeliveryError::GitCommandFailed("fetch-base"))?;
        // The pinned target MUST actually contain base_oid — resolve it as a commit in the store.
        let parsed = Self::parse_oid(base_oid)?;
        repo.find_commit(parsed)
            .map(|_| ())
            .map_err(|_| DeliveryError::MissingBaseObject)
    }

    /// Contribution: refuse unless `commit_oid` descends from `base_oid`, in-process via
    /// `graph_descendant_of`. Equal oids count as ancestor (parity with `merge-base --is-ancestor`,
    /// exit 0) — the empty diff is then refused by the content gate. Both oids must be in the store.
    fn assert_descendant(
        &self,
        base_oid: &CommitOid,
        commit_oid: &CommitOid,
    ) -> Result<(), DeliveryError> {
        let repo = self.open_store()?;
        let base = Self::parse_oid(base_oid)?;
        let commit = Self::parse_oid(commit_oid)?;
        if base == commit {
            return Ok(());
        }
        match repo.graph_descendant_of(commit, base) {
            Ok(true) => Ok(()),
            Ok(false) => Err(DeliveryError::NotDescendant {
                base_oid: base_oid.as_str().to_owned(),
                commit_oid: commit_oid.as_str().to_owned(),
            }),
            Err(_) => Err(DeliveryError::GitCommandFailed("merge-base")),
        }
    }

    /// Contribution: the changed paths of the fork-vs-base diff, computed by the BUYER in
    /// the store (never trusted from the seller). `bytes` per path is the numstat churn
    /// (added+deleted lines; binary files count as 1) — a deterministic size proxy for the policy
    /// cap. An empty result means an empty diff (the content-gate floor refuses it).
    fn changed_paths(
        &self,
        base_oid: &CommitOid,
        commit_oid: &CommitOid,
    ) -> Result<Vec<crate::contribution::ChangedPath>, DeliveryError> {
        let repo = self.open_store()?;
        let base_tree = repo
            .find_commit(Self::parse_oid(base_oid)?)
            .and_then(|c| c.tree())
            .map_err(|_| DeliveryError::GitCommandFailed("diff-numstat"))?;
        let commit_tree = repo
            .find_commit(Self::parse_oid(commit_oid)?)
            .and_then(|c| c.tree())
            .map_err(|_| DeliveryError::GitCommandFailed("diff-numstat"))?;
        // Default git2 diff detects NO renames (parity with `--no-renames`).
        let mut opts = DiffOptions::new();
        let diff = repo
            .diff_tree_to_tree(Some(&base_tree), Some(&commit_tree), Some(&mut opts))
            .map_err(|_| DeliveryError::GitCommandFailed("diff-numstat"))?;

        let mut changed = Vec::new();
        let deltas = diff.deltas().len();
        for idx in 0..deltas {
            let delta = diff
                .get_delta(idx)
                .ok_or(DeliveryError::GitCommandFailed("diff-numstat"))?;
            // Path: new side for adds/mods, old side for deletions.
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().into_owned());
            let Some(path) = path.filter(|p| !p.is_empty()) else {
                continue;
            };
            let is_binary = delta.flags().is_binary();
            let churn = match git2::Patch::from_diff(&diff, idx) {
                Ok(Some(patch)) => {
                    let (_context, additions, deletions) = patch
                        .line_stats()
                        .map_err(|_| DeliveryError::GitCommandFailed("diff-numstat"))?;
                    (additions + deletions) as u64
                }
                // A binary delta has no textual patch (None); count it as churn 1 below.
                Ok(None) => 0,
                Err(_) => return Err(DeliveryError::GitCommandFailed("diff-numstat")),
            };
            // Binary files report `-`/`-` in `git diff --numstat`; register them as churn 1 so a
            // binary-only change is neither invisible (empty-diff false-pass) nor unbounded.
            let churn = if is_binary { churn.max(1) } else { churn };
            changed.push(crate::contribution::ChangedPath { path, bytes: churn });
        }
        Ok(changed)
    }

    /// Contribution buyer verify-path orchestration (the ONE state machine, all pre-pay). Bare —
    /// the transport allowlist is applied by [`PayPathDeliveryVerifier::verify_contribution`] BEFORE
    /// this runs. In order: fetch the fork tip into the store + tip-match, base-from-pin, descendant gate,
    /// content gate. Returns the store proof + local ref for the later merge.
    fn contribution_verify(
        &mut self,
        fork: &GitDelivery,
        base_clone_url: &str,
        base_branch: &str,
        base_oid: &CommitOid,
        policy: &crate::contribution::ContentPolicy,
    ) -> Result<VerifiedContribution, DeliveryError> {
        // 1. fetch fork tip into the store + tip-match (retains the object under refs/mobee/deliveries/…).
        let verified = self.verify(fork)?;
        // 2. base-from-pin into the same buyer store (fail-closed if absent from the pinned target).
        self.fetch_base(base_clone_url, base_branch, base_oid)?;
        // 3. descendant gate.
        self.assert_descendant(base_oid, verified.commit_oid())?;
        // 4. content gate + buyer policy hook.
        let changed = self.changed_paths(base_oid, verified.commit_oid())?;
        policy
            .evaluate(&changed)
            .map_err(|refusal| DeliveryError::ContentRefused(refusal.to_string()))?;
        Ok(VerifiedContribution {
            store_ref: PayPathDeliveryVerifier::store_ref_for(verified.commit_oid().as_str()),
            verified,
            changed_paths: changed,
        })
    }
}

/// `git2::Reference::is_valid_name` on the `refs/heads/<branch>` form — the branch analog of
/// `git check-ref-format --branch`. Split out so the verifier method reads cleanly.
#[allow(non_snake_case)]
fn Reference_is_valid_branch(branch: &str) -> bool {
    git2::Reference::is_valid_name(&format!("refs/heads/{branch}"))
}

/// Proof that a contribution fork tip is in the buyer store AND descends from the pinned base, with
/// the content gate satisfied. Carries the LOCAL store ref so the merge step operates on the
/// retained oid, never the live fork branch (retention).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedContribution {
    verified: VerifiedDelivery,
    store_ref: String,
    changed_paths: Vec<crate::contribution::ChangedPath>,
}

impl VerifiedContribution {
    /// The verified fork-tip delivery (the object the money path binds — unchanged `Commit`).
    pub fn verified(&self) -> &VerifiedDelivery {
        &self.verified
    }

    /// The BUYER-CONTROLLED local ref holding the retained fork tip (recorded in the accept-bind;
    /// merge uses THIS, never the live fork branch name).
    pub fn store_ref(&self) -> &str {
        &self.store_ref
    }

    /// The changed paths the content gate evaluated (buyer-computed in the store).
    pub fn changed_paths(&self) -> &[crate::contribution::ChangedPath] {
        &self.changed_paths
    }
}

impl DeliveryVerifier for GitDeliveryVerifier {
    fn verify(&mut self, delivery: &GitDelivery) -> Result<VerifiedDelivery, DeliveryError> {
        self.check_branch(delivery.branch())?;
        self.ensure_repository(delivery.commit_oid())?;
        let fetched_tip = self.fetch(delivery)?;
        let verified = VerifiedDelivery::from_fetched_tip(delivery, fetched_tip)?;
        self.require_local_object(verified.commit_oid())?;
        Ok(verified)
    }
}

/// Pay-path delivery verifier: transport allowlist sealed around git verify/fetch.
///
/// This is the only public constructor that yields a fetch-capable verifier for
/// `authorize_pay` / MCP. There is no peel API — the bare git verifier cannot be
/// reached from outside `mobee-core`.
pub struct PayPathDeliveryVerifier {
    inner: AllowlistedDeliveryVerifier<GitDeliveryVerifier>,
}

impl PayPathDeliveryVerifier {
    /// Build the allowlisted git verifier used on the authorize_pay path. `buyer_secret_hex` signs
    /// NIP-98 for relay-git reads (`None` ⇒ anonymous — public https bases / local-path tests).
    pub fn new(repository: impl Into<PathBuf>, buyer_secret_hex: Option<String>) -> Self {
        Self {
            inner: AllowlistedDeliveryVerifier::new(
                GitDeliveryVerifier::new(repository).with_buyer_secret(buyer_secret_hex),
            ),
        }
    }

    /// Returns the local repository that holds verified delivery objects.
    pub fn repository(&self) -> &Path {
        self.inner.inner().repository()
    }

    /// The buyer-controlled store ref a fork tip is retained under (retention).
    pub fn store_ref_for(commit_oid: &str) -> String {
        format!("refs/mobee/deliveries/{commit_oid}")
    }

    /// Contribution buyer verify-path — the ONE state machine, ALL pre-pay, ALL against
    /// BUYER-CONTROLLED inputs (`fork` = the delivered object; `base_*` come from the buyer's
    /// SIGNED offer pin, NEVER the seller echo). In order:
    ///   1. fetch the fork tip into the store + tip-match (allowlisted);
    ///   2. base-from-pin: fetch `base_oid` from the PINNED target clone URL into the same buyer
    ///      store (allowlisted) — fail-closed if absent from the pinned target;
    ///   3. descendant gate: `commit_oid` must descend from `base_oid`;
    ///   4. content gate + policy hook: refuse an empty / out-of-scope / forbidden / too-large diff.
    /// Returns the store proof + LOCAL store ref for the later merge. Authorship (the seller's
    /// signed tuple) + echo-equality are checked by the caller at the pre-pay seam; pay then binds
    /// the fork-tip `commit_oid` via the unchanged money path.
    pub fn verify_contribution(
        &mut self,
        fork: &GitDelivery,
        base_clone_url: &str,
        base_branch: &str,
        base_oid: &CommitOid,
        policy: &crate::contribution::ContentPolicy,
    ) -> Result<VerifiedContribution, DeliveryError> {
        // BOTH the fork fetch AND the base-from-pin fetch go through the SAME transport allowlist
        // (https + relay-git; `ext::`/file/ssh refused) — defense-in-depth even though the base pin
        // is buyer-signed. Enforced here BEFORE the bare orchestration touches git.
        crate::delivery_transport::assert_allowed_repo_locator(fork.repo())?;
        crate::delivery_transport::assert_allowed_repo_locator(base_clone_url)?;
        self.inner
            .inner_mut()
            .contribution_verify(fork, base_clone_url, base_branch, base_oid, policy)
    }

    /// Merge the RETAINED local `commit_oid` into a buyer-owned target working clone, FF-preferred
    /// ("accept the PR"). Buyer-store action (NOT what payment binds): it fetches the object from
    /// the buyer's own store by its LOCAL ref and fast-forwards to it — so a seller that
    /// deletes or moves the fork AFTER pay cannot strand the buyer (retention). No transport
    /// allowlist applies: both the store and the target clone are buyer-owned LOCAL repos.
    pub fn merge_retained_commit(
        &self,
        target_workdir: &Path,
        commit_oid: &CommitOid,
    ) -> Result<(), DeliveryError> {
        let store = self.repository();
        let store_url = store.to_string_lossy().into_owned();
        let local_ref = Self::store_ref_for(commit_oid.as_str());
        let merge_ref = format!("refs/mobee/merge/{}", commit_oid.as_str());
        let fetch_spec = format!("+{local_ref}:{merge_ref}");

        let target = Repository::open(target_workdir)
            .map_err(|_| DeliveryError::GitCommandFailed("merge-open"))?;
        // Local store→target fetch (no network, no auth, no allowlist — both are buyer-owned).
        git_transport::fetch_refspecs(&target, &store_url, &[&fetch_spec], None, false)
            .map_err(|_| DeliveryError::GitCommandFailed("fetch-from-store"))?;

        let merged = GitDeliveryVerifier::parse_oid(commit_oid)?;
        // Fast-forward-only: the retained commit must be the current HEAD or a descendant of it.
        let annotated = target
            .find_annotated_commit(merged)
            .map_err(|_| DeliveryError::MergeFailed("ff-only"))?;
        let (analysis, _) = target
            .merge_analysis(&[&annotated])
            .map_err(|_| DeliveryError::MergeFailed("ff-only"))?;
        if analysis.is_up_to_date() {
            return Ok(());
        }
        if !analysis.is_fast_forward() {
            return Err(DeliveryError::MergeFailed("ff-only"));
        }
        let object = target
            .find_object(merged, None)
            .map_err(|_| DeliveryError::MergeFailed("ff-only"))?;
        target
            .checkout_tree(&object, Some(git2::build::CheckoutBuilder::new().safe()))
            .map_err(|_| DeliveryError::MergeFailed("ff-only"))?;
        let mut head = target
            .head()
            .map_err(|_| DeliveryError::MergeFailed("ff-only"))?;
        head.set_target(merged, "mobee ff-only merge of retained delivery")
            .map_err(|_| DeliveryError::MergeFailed("ff-only"))?;
        Ok(())
    }
}

impl DeliveryVerifier for PayPathDeliveryVerifier {
    fn verify(&mut self, delivery: &GitDelivery) -> Result<VerifiedDelivery, DeliveryError> {
        self.inner.verify(delivery)
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_REPO: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        root: PathBuf,
        work: PathBuf,
        remote: PathBuf,
        store: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let id = NEXT_REPO.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("mobee-delivery-{}-{id}", std::process::id()));
            let work = root.join("work");
            let remote = root.join("remote.git");
            let store = root.join("store.git");
            fs::create_dir_all(&work).expect("create fixture");
            run(["init", "--initial-branch=main"], Some(&work));
            run(["config", "user.name", "Mobee Test"], Some(&work));
            run(
                ["config", "user.email", "mobee@example.invalid"],
                Some(&work),
            );
            fs::write(work.join("delivery.txt"), "one\n").expect("write delivery");
            run(["add", "delivery.txt"], Some(&work));
            run(["commit", "-m", "delivery one"], Some(&work));
            run(
                ["init", "--bare", remote.to_str().expect("remote path")],
                None,
            );
            run(
                [
                    "remote",
                    "add",
                    "origin",
                    remote.to_str().expect("remote path"),
                ],
                Some(&work),
            );
            run(["push", "origin", "main"], Some(&work));
            Self {
                root,
                work,
                remote,
                store,
            }
        }

        fn head(&self) -> CommitOid {
            let output = Command::new("git")
                .args([
                    "-C",
                    self.work.to_str().expect("work path"),
                    "rev-parse",
                    "HEAD",
                ])
                .output()
                .expect("rev-parse");
            assert!(output.status.success());
            CommitOid::parse(String::from_utf8(output.stdout).expect("utf8").trim())
                .expect("head oid")
        }

        fn delivery(&self, oid: CommitOid) -> GitDelivery {
            GitDelivery::new(self.remote.to_str().expect("remote path"), "main", oid)
                .expect("delivery")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn run<const N: usize>(args: [&str; N], cwd: Option<&Path>) {
        let mut command = Command::new("git");
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output().expect("run git fixture command");
        assert!(
            output.status.success(),
            "git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn fetch_tip_match_returns_retained_commit() {
        let fixture = Fixture::new();
        let advertised = fixture.delivery(fixture.head());
        let mut verifier = GitDeliveryVerifier::new(&fixture.store);

        let verified = verifier.verify(&advertised).expect("verify delivery");

        assert_eq!(verified.commit_oid(), advertised.commit_oid());
        let object = format!("{}^{{commit}}", verified.commit_oid());
        let status = Command::new("git")
            .args([
                "-C",
                fixture.store.to_str().expect("store path"),
                "cat-file",
                "-e",
                &object,
            ])
            .status()
            .expect("inspect store");
        assert!(status.success());
    }

    #[test]
    fn moved_tip_refuses_the_stale_advertised_oid() {
        let fixture = Fixture::new();
        let advertised = fixture.delivery(fixture.head());
        let mut verifier = GitDeliveryVerifier::new(&fixture.store);
        verifier
            .verify(&advertised)
            .expect("initial delivery verifies");
        fs::write(fixture.work.join("delivery.txt"), "two\n").expect("advance delivery");
        run(["add", "delivery.txt"], Some(&fixture.work));
        run(["commit", "-m", "delivery two"], Some(&fixture.work));
        run(["push", "origin", "main"], Some(&fixture.work));

        assert!(matches!(
            verifier.verify(&advertised),
            Err(DeliveryError::TipMismatch { .. })
        ));
    }

    #[test]
    fn malformed_branch_refuses_before_creating_the_store_repo() {
        let fixture = Fixture::new();
        let delivery = GitDelivery::new(
            fixture.remote.to_str().expect("remote path"),
            "bad..branch",
            fixture.head(),
        )
        .expect("delivery shape");
        let mut verifier = GitDeliveryVerifier::new(&fixture.store);

        assert_eq!(
            verifier.verify(&delivery),
            Err(DeliveryError::InvalidBranch)
        );
        assert!(!fixture.store.exists());
    }

    #[test]
    fn pay_path_verifier_refuses_ext_before_fetch() {
        use crate::delivery_transport::TransportRefuse;

        let fixture = Fixture::new();
        let delivery = GitDelivery::new(
            "ext::sh -c evil",
            "main",
            fixture.head(),
        )
        .expect("delivery shape");
        let mut verifier = PayPathDeliveryVerifier::new(&fixture.store, None);
        let err = verifier.verify(&delivery).expect_err("refuse ext");
        assert!(matches!(
            err,
            DeliveryError::Transport(TransportRefuse::ForbiddenScheme(_))
        ));
        assert!(!fixture.store.exists());
    }

    #[test]
    fn pay_path_verifier_allows_local_path_only_via_bare_inner_in_tests() {
        // Hermetic tip-match tests still use bare GitDeliveryVerifier (pub(crate)).
        // Pay path must not be that type — factory always allowlists first.
        let fixture = Fixture::new();
        let advertised = fixture.delivery(fixture.head());
        let mut pay_path = PayPathDeliveryVerifier::new(&fixture.store, None);
        let err = pay_path.verify(&advertised).expect_err("local path refused");
        assert!(matches!(
            err,
            DeliveryError::Transport(crate::delivery_transport::TransportRefuse::LocalPath)
        ));
    }

    /// PATH-stripped proof: build the delivery source repo with git2 ONLY (no `Command`), then
    /// verify through the product path (git2 fetch + tip-match + store retention). Run with `git`
    /// absent from PATH to prove the buyer verify money path has no hidden shell-out.
    #[test]
    fn verify_delivery_without_system_git() {
        use git2::{Repository, Signature};
        let id = NEXT_REPO.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("mobee-nogit-{}-{id}", std::process::id()));
        let src = root.join("src");
        let store = root.join("store.git");
        fs::create_dir_all(&src).expect("mkdir src");
        let repo = Repository::init(&src).expect("git2 init source");
        fs::write(src.join("d.txt"), "one\n").expect("write");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new("d.txt")).expect("add");
        index.write().expect("index write");
        let tree = repo.find_tree(index.write_tree().expect("write tree")).expect("tree");
        let sig = Signature::now("Mobee Test", "t@t.invalid").expect("sig");
        let oid = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "one", &tree, &[])
            .expect("git2 commit");

        let delivery = GitDelivery::new(
            src.to_str().expect("src path"),
            "main",
            CommitOid::parse(oid.to_string()).expect("oid"),
        )
        .expect("delivery");
        let mut verifier = GitDeliveryVerifier::new(&store);
        let verified = verifier.verify(&delivery).expect("git2 verify without system git");
        assert_eq!(verified.commit_oid().as_str(), oid.to_string());
        let cust = Repository::open_bare(&store).expect("open store");
        assert!(cust.find_commit(oid).is_ok(), "store must retain the object");
        let _ = fs::remove_dir_all(&root);
    }

    // NOTE: the old `hanging_remote_fetch_fails_closed_via_in_process_timeout` test drove the
    // removed `git_output_timed` subprocess-kill helper against a `git://` hang server. The buyer
    // fetch is now in-process libgit2 over the rustls subtransport, and fail-closed on a hung remote
    // is enforced BY CONSTRUCTION via `git_transport::client_short`'s connect+read timeout (a read
    // can never exceed it — no reliance on kill/reap succeeding). The end-to-end auth/read path is
    // exercised against a real TLS git server in `tests/relay_git_http_auth.rs`.
}

#[cfg(test)]
mod contribution_tests {
    use super::*;
    use crate::contribution::{ChangedPath, ContentPolicy};
    use std::process::{Command, Output};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(1);

    fn git<const N: usize>(args: [&str; N], cwd: &Path) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.invalid")
            .output()
            .expect("git")
    }

    fn ok<const N: usize>(args: [&str; N], cwd: &Path) {
        let out = git(args, cwd);
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn oid(cwd: &Path, rev: &str) -> CommitOid {
        let out = git(["rev-parse", rev], cwd);
        assert!(out.status.success(), "rev-parse {rev}");
        CommitOid::parse(String::from_utf8(out.stdout).unwrap().trim()).expect("oid")
    }

    fn commit(cwd: &Path, path: &str, content: &str, msg: &str) {
        let full = cwd.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&full, content).expect("write");
        ok(["add", "-A"], cwd);
        ok(["commit", "-m", msg], cwd);
    }

    /// A contribution scenario: a PINNED target repo (base branch with `base_oid` as an ancestor of
    /// its tip) + a seller FORK descending from `base_oid` with `depth` agent commits on top.
    struct Fx {
        root: PathBuf,
        target_git: PathBuf,
        fork_git: PathBuf,
        store: PathBuf,
        base_oid: CommitOid,
        fork_tip: CommitOid,
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn scenario(depth: usize, change_path: &str) -> Fx {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("mobee-contrib-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let target_work = root.join("target_work");
        let target_git = root.join("target.git");
        let fork_work = root.join("fork_work");
        let fork_git = root.join("fork.git");
        let store = root.join("store.git");
        fs::create_dir_all(&target_work).expect("mk");
        ok(["init", "--initial-branch=main"], &target_work);
        commit(&target_work, "README.md", "base one\n", "base one");
        let base_oid = oid(&target_work, "HEAD");
        // Advance main PAST base_oid so base_oid is an ANCESTOR (base-from-pin must find it in history).
        commit(&target_work, "README.md", "base two\n", "base two");
        ok(["init", "--bare", target_git.to_str().unwrap()], &root);
        ok(["remote", "add", "origin", target_git.to_str().unwrap()], &target_work);
        ok(["push", "origin", "main"], &target_work);

        // Fork: clone target, check out base_oid, add `depth` agent commits on top, push a branch.
        ok(["clone", target_git.to_str().unwrap(), fork_work.to_str().unwrap()], &root);
        ok(["checkout", "-B", "contribution", base_oid.as_str()], &fork_work);
        for i in 0..depth.max(1) {
            commit(&fork_work, change_path, &format!("agent work {i}\n"), &format!("agent {i}"));
        }
        let fork_tip = oid(&fork_work, "HEAD");
        ok(["init", "--bare", fork_git.to_str().unwrap()], &root);
        ok(["remote", "add", "fork", fork_git.to_str().unwrap()], &fork_work);
        ok(["push", "fork", "contribution"], &fork_work);

        Fx { root, target_git, fork_git, store, base_oid, fork_tip }
    }

    fn fork_delivery(fx: &Fx) -> GitDelivery {
        GitDelivery::new(fx.fork_git.to_str().unwrap(), "contribution", fx.fork_tip.clone())
            .expect("fork delivery")
    }

    // ── Descendant + tip-match + base-from-pin: a valid DEEP-history contribution passes (item i:
    //    a full-depth fetch into the store reaches base_oid, so the descendant gate never false-refuses). ──
    #[test]
    fn deep_history_contribution_verifies() {
        let fx = scenario(5, "src/feature.rs");
        let mut v = GitDeliveryVerifier::new(&fx.store);
        let verified = v
            .contribution_verify(
                &fork_delivery(&fx),
                fx.target_git.to_str().unwrap(),
                "main",
                &fx.base_oid,
                &ContentPolicy::floor(),
            )
            .expect("deep-history contribution must verify");
        assert_eq!(verified.verified().commit_oid(), &fx.fork_tip);
        assert_eq!(verified.store_ref(), PayPathDeliveryVerifier::store_ref_for(fx.fork_tip.as_str()));
        assert!(!verified.changed_paths().is_empty());
    }

    // ── Descendant gate refuses unrelated history / swapped base ──────────────────────────────
    #[test]
    fn unrelated_history_refused_as_not_descendant() {
        let fx = scenario(1, "src/feature.rs");
        // An ORPHAN base_oid (a commit created independently, not in the fork's history).
        let orphan_work = fx.root.join("orphan");
        fs::create_dir_all(&orphan_work).unwrap();
        ok(["init", "--initial-branch=main"], &orphan_work);
        commit(&orphan_work, "x.txt", "orphan\n", "orphan");
        let orphan_oid = oid(&orphan_work, "HEAD");
        // Put the orphan object into the store (as if a base fetch produced it) so the descendant
        // check is the thing under test, not object-presence.
        let mut v = GitDeliveryVerifier::new(&fx.store);
        v.verify(&fork_delivery(&fx)).expect("fetch fork tip");
        ok(["init", "--bare", fx.store.join("dummy").to_str().unwrap_or("dummy")], &fx.root);
        // fetch the orphan commit into the store directly
        let spec = format!("+refs/heads/main:refs/mobee/bases/{}", orphan_oid.as_str());
        ok(["-C", fx.store.to_str().unwrap(), "fetch", orphan_work.to_str().unwrap(), &spec], &fx.root);
        assert!(matches!(
            v.assert_descendant(&orphan_oid, &fx.fork_tip),
            Err(DeliveryError::NotDescendant { .. })
        ));
    }

    #[test]
    fn swapped_base_that_is_not_an_ancestor_refused() {
        // base = the fork TIP's child? No — use a sibling: a second fork off base with different work.
        let fx = scenario(1, "src/a.rs");
        // Build a sibling commit off base in another clone (a DIFFERENT descendant of base).
        let sib = fx.root.join("sib");
        ok(["clone", fx.target_git.to_str().unwrap(), sib.to_str().unwrap()], &fx.root);
        ok(["checkout", "-B", "sib", fx.base_oid.as_str()], &sib);
        commit(&sib, "src/b.rs", "sibling\n", "sibling");
        let sib_oid = oid(&sib, "HEAD");
        let mut v = GitDeliveryVerifier::new(&fx.store);
        v.verify(&fork_delivery(&fx)).expect("fetch fork tip");
        let spec = format!("+refs/heads/sib:refs/mobee/x/{}", sib_oid.as_str());
        ok(["-C", fx.store.to_str().unwrap(), "fetch", sib.to_str().unwrap(), &spec], &fx.root);
        // The sibling is NOT an ancestor of the fork tip ⇒ swapped-base refuse.
        assert!(matches!(
            v.assert_descendant(&sib_oid, &fx.fork_tip),
            Err(DeliveryError::NotDescendant { .. })
        ));
    }

    // ── Base-from-pin: base_oid absent from the pinned target ⇒ fail-closed refuse ────────────
    #[test]
    fn base_absent_from_pinned_target_fails_closed() {
        let fx = scenario(1, "src/feature.rs");
        // An ORPHAN base_oid — a commit that is NOT on the target's main branch AND not in the
        // fork's history (so it is absent from the store after both the fork and the base fetch).
        let orphan = fx.root.join("orphan_base");
        fs::create_dir_all(&orphan).unwrap();
        ok(["init", "--initial-branch=main"], &orphan);
        commit(&orphan, "z.txt", "orphan base\n", "orphan base");
        let bogus_base = oid(&orphan, "HEAD");
        let mut v = GitDeliveryVerifier::new(&fx.store);
        v.verify(&fork_delivery(&fx)).expect("fetch fork tip");
        // base-from-pin fetches the target's main (base + base_two) but the orphan is not there ⇒
        // fail-closed MissingBaseObject (the buyer never bases against an oid absent from the pin).
        assert!(matches!(
            v.fetch_base(fx.target_git.to_str().unwrap(), "main", &bogus_base),
            Err(DeliveryError::MissingBaseObject)
        ));
    }

    #[test]
    fn base_from_pin_finds_ancestor_base() {
        let fx = scenario(2, "src/feature.rs");
        let mut v = GitDeliveryVerifier::new(&fx.store);
        v.verify(&fork_delivery(&fx)).expect("fetch fork tip");
        // base_oid is an ANCESTOR of the target's main tip — base-from-pin must resolve it.
        v.fetch_base(fx.target_git.to_str().unwrap(), "main", &fx.base_oid)
            .expect("base-from-pin must find the ancestor base_oid in the pinned target");
        v.assert_descendant(&fx.base_oid, &fx.fork_tip).expect("descends");
    }

    // ── Content gate: empty / out-of-scope refuse; in-scope passes ────────────────────────────
    #[test]
    fn content_gate_refuses_empty_diff() {
        // A fork whose tip has the SAME tree as base (an empty commit) ⇒ empty diff.
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("mobee-contrib-empty-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let tw = root.join("tw");
        let tg = root.join("t.git");
        let fw = root.join("fw");
        let fg = root.join("f.git");
        let store = root.join("c.git");
        fs::create_dir_all(&tw).unwrap();
        ok(["init", "--initial-branch=main"], &tw);
        commit(&tw, "README.md", "base\n", "base");
        let base_oid = oid(&tw, "HEAD");
        ok(["init", "--bare", tg.to_str().unwrap()], &root);
        ok(["remote", "add", "origin", tg.to_str().unwrap()], &tw);
        ok(["push", "origin", "main"], &tw);
        ok(["clone", tg.to_str().unwrap(), fw.to_str().unwrap()], &root);
        ok(["checkout", "-B", "contribution", base_oid.as_str()], &fw);
        ok(["commit", "--allow-empty", "-m", "empty"], &fw);
        let fork_tip = oid(&fw, "HEAD");
        ok(["init", "--bare", fg.to_str().unwrap()], &root);
        ok(["remote", "add", "fork", fg.to_str().unwrap()], &fw);
        ok(["push", "fork", "contribution"], &fw);
        let mut v = GitDeliveryVerifier::new(&store);
        let err = v
            .contribution_verify(
                &GitDelivery::new(fg.to_str().unwrap(), "contribution", fork_tip).unwrap(),
                tg.to_str().unwrap(),
                "main",
                &base_oid,
                &ContentPolicy::floor(),
            )
            .expect_err("empty diff must refuse");
        assert!(matches!(err, DeliveryError::ContentRefused(_)), "got {err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn content_gate_scope_out_of_scope_refuses_in_scope_passes() {
        // out-of-scope: change under "other/", policy allows only "src".
        let fx = scenario(1, "other/x.rs");
        let mut v = GitDeliveryVerifier::new(&fx.store);
        let policy = ContentPolicy { allowed_paths: vec!["src".into()], ..Default::default() };
        let err = v
            .contribution_verify(&fork_delivery(&fx), fx.target_git.to_str().unwrap(), "main", &fx.base_oid, &policy)
            .expect_err("out-of-scope refuse");
        assert!(matches!(err, DeliveryError::ContentRefused(_)), "got {err}");

        // in-scope: change under "src/", same policy → pass.
        let fx2 = scenario(1, "src/ok.rs");
        let mut v2 = GitDeliveryVerifier::new(&fx2.store);
        v2.contribution_verify(&fork_delivery(&fx2), fx2.target_git.to_str().unwrap(), "main", &fx2.base_oid, &policy)
            .expect("in-scope must pass");
    }

    // ── Retention (the STRAND-PROOF): after verify, the object is in the buyer store; the
    //    fork moving/deleting cannot strand the buyer — merge succeeds from the retained local oid. ─
    #[test]
    fn retention_survives_fork_deletion_and_merges_from_local_oid() {
        let fx = scenario(1, "src/feature.rs");
        let pay = PayPathDeliveryVerifier::new(&fx.store, None);
        // Fetch into the store via the bare verifier (local path; allowlist tested separately).
        let mut bare = GitDeliveryVerifier::new(&fx.store);
        let verified = bare
            .contribution_verify(&fork_delivery(&fx), fx.target_git.to_str().unwrap(), "main", &fx.base_oid, &ContentPolicy::floor())
            .expect("verify");
        // Simulate the seller DELETING the fork remote after accept.
        fs::remove_dir_all(&fx.fork_git).expect("delete fork");
        // The object is RETAINED in the store (a deleted fork cannot strand the buyer).
        bare.require_local_object(&fx.fork_tip).expect("retained object survives fork deletion");
        // Merge the RETAINED local oid into a target working clone, FF-preferred.
        let target_clone = fx.root.join("accept_pr");
        ok(["clone", fx.target_git.to_str().unwrap(), target_clone.to_str().unwrap()], &fx.root);
        ok(["checkout", fx.base_oid.as_str()], &target_clone);
        pay.merge_retained_commit(&target_clone, &fx.fork_tip)
            .expect("merge from the store must succeed by LOCAL oid even after fork deletion");
        assert_eq!(oid(&target_clone, "HEAD"), fx.fork_tip, "FF merge lands the paid fork-tip oid");
        assert_eq!(verified.verified().commit_oid(), &fx.fork_tip);
    }

    // ── Allowlist: the pay-path contribution verify refuses ext:: for BOTH fork and base ──────
    #[test]
    fn pay_path_contribution_verify_refuses_ext_transport() {
        use crate::delivery_transport::TransportRefuse;
        let fx = scenario(1, "src/feature.rs");
        let mut pay = PayPathDeliveryVerifier::new(&fx.store, None);
        // ext:: fork repo refused before any fetch.
        let ext_fork = GitDelivery::new("ext::sh -c evil", "contribution", fx.fork_tip.clone()).unwrap();
        assert!(matches!(
            pay.verify_contribution(&ext_fork, fx.target_git.to_str().unwrap(), "main", &fx.base_oid, &ContentPolicy::floor()),
            Err(DeliveryError::Transport(TransportRefuse::ForbiddenScheme(_)))
        ));
        // ext:: base clone url refused too — the fork here is an allowlisted https URL so the base
        // is the value under test (neither fetch runs; both URLs are allowlist-checked up front).
        let https_fork =
            GitDelivery::new("https://example.invalid/fork.git", "contribution", fx.fork_tip.clone())
                .unwrap();
        assert!(matches!(
            pay.verify_contribution(&https_fork, "ext::sh -c evil", "main", &fx.base_oid, &ContentPolicy::floor()),
            Err(DeliveryError::Transport(TransportRefuse::ForbiddenScheme(_)))
        ));
    }
}
