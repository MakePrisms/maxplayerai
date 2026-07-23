//! Seller-side git — ALL in-process via libgit2; no system `git` on any product path.
//!
//! Base fetch, fork checkout, and delivery push run through [`crate::git_transport`]'s rustls
//! smart-HTTP subtransport, which injects the seller's NIP-98 `Authorization` on relay-git requests.
//! The agent edits files in the job workdir but never commits; at delivery the daemon snapshots the
//! final tree into ONE commit under the delivery identity via [`snapshot_delivery`] (git2 only, no
//! `git` subprocess) and pushes that.
//!
//! ## Why the old scrub machinery is gone (and this is safe)
//! The previous implementation shelled out to `git` and had to defend against ambient config: empty
//! `GIT_CONFIG_GLOBAL`/`XDG_CONFIG_HOME`, `GIT_CONFIG_NOSYSTEM`, `protocol.*.allow=never`, scrubbed
//! `GIT_SSH*`/`insteadOf`. In-process git2 needs NONE of that:
//! - **`insteadOf` immunity is structural:** every remote is [`Repository::remote_anonymous`], which
//!   uses the literal URL and applies NO `url.*.insteadOf` config rewrite — an agent-planted
//!   `.git/config` (or poisoned `$HOME/.gitconfig`) can never redirect an allowlisted `https` push
//!   onto `ssh`/`file`/`ext`.
//! - **Transport allowlist:** every entry asserts [`assert_allowed_repo_locator`] and only `https`
//!   is registered as a subtransport — `ext:`/`file:`/`ssh:` are refused before any remote exists.
//! - **Key hygiene:** the seller secret signs the NIP-98 event in-process only — never on argv,
//!   never in child env, no subprocess.

use std::path::{Path, PathBuf};

use git2::build::CheckoutBuilder;
use git2::{Commit, Direction, IndexAddOption, Oid, Repository, Signature};

use crate::delivery_transport::{assert_allowed_repo_locator, TransportRefuse};
use crate::git_transport::{self, TransportError};

/// Seller push failure (maps to feedback-kind error in the daemon).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellerGitError {
    Transport(String),
    Unavailable,
    CommandFailed(&'static str),
    AuthFailed(String),
    Io(String),
}

impl std::fmt::Display for SellerGitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "seller git transport refused: {message}"),
            Self::Unavailable => write!(f, "seller git unavailable"),
            Self::CommandFailed(op) => write!(f, "seller git {op} failed"),
            Self::AuthFailed(message) => write!(f, "seller git auth failed: {message}"),
            Self::Io(message) => write!(f, "seller git io error: {message}"),
        }
    }
}

impl std::error::Error for SellerGitError {}

impl From<TransportRefuse> for SellerGitError {
    fn from(value: TransportRefuse) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<TransportError> for SellerGitError {
    fn from(value: TransportError) -> Self {
        match value {
            TransportError::Transport(m) => Self::Transport(m),
            // A rejected ref or an auth/permission signal is a fail-closed auth failure (parity with
            // the old system-git path, which mapped both to AuthFailed).
            TransportError::Auth(m) | TransportError::Rejected(m) => Self::AuthFailed(m),
            TransportError::Io(m) => Self::Io(m),
        }
    }
}

/// The deterministic identity every delivery commit carries: `mobee-seller-<pubkey16>` /
/// `<pubkey16>@seller.mobee.invalid`. The daemon authors the snapshot commit under this identity,
/// so the delivered commit is provably the seat's — the seller's signature over the result and
/// receipt is the binding attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryAgentIdentity {
    pub name: String,
    pub email: String,
}

impl DeliveryAgentIdentity {
    /// Derive a stable seller-run identity from the seller pubkey hex.
    pub fn for_seller(seller_pubkey_hex: &str) -> Self {
        let short = seller_pubkey_hex
            .get(..16)
            .unwrap_or(seller_pubkey_hex)
            .to_ascii_lowercase();
        Self {
            name: format!("mobee-seller-{short}"),
            email: format!("{short}@seller.mobee.invalid"),
        }
    }

    /// Env that overrides ambient git identity for commits made during the agent run (the AGENT
    /// process makes those commits with its own git — out of scope for the seller daemon's git2).
    pub fn git_env(&self) -> Vec<(String, String)> {
        vec![
            ("GIT_AUTHOR_NAME".into(), self.name.clone()),
            ("GIT_AUTHOR_EMAIL".into(), self.email.clone()),
            ("GIT_COMMITTER_NAME".into(), self.name.clone()),
            ("GIT_COMMITTER_EMAIL".into(), self.email.clone()),
        ]
    }
}

/// Initialise `workdir` as a fresh repo (`main` as the initial branch) with the delivery identity
/// in `.git/config`. The agent works from an empty tree here; the daemon snapshots the result at
/// delivery. The config identity only spares the agent's optional scratch commits a missing-identity
/// error — those commits are never delivered.
pub fn init_empty_delivery_workdir(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
) -> Result<(), SellerGitError> {
    let repo = init_repo_with_identity(workdir, identity)?;
    drop(repo);
    Ok(())
}

/// `git init --initial-branch=main` + `git config user.name/email` via git2.
fn init_repo_with_identity(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
) -> Result<Repository, SellerGitError> {
    if !workdir.exists() {
        std::fs::create_dir_all(workdir).map_err(|error| SellerGitError::Io(error.to_string()))?;
    }
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head("main");
    let repo = Repository::init_opts(workdir, &opts)
        .map_err(|error| SellerGitError::Io(format!("init: {error}")))?;
    {
        let mut cfg = repo
            .config()
            .map_err(|error| SellerGitError::Io(format!("open config: {error}")))?;
        cfg.set_str("user.name", &identity.name)
            .map_err(|_| SellerGitError::CommandFailed("config-user-name"))?;
        cfg.set_str("user.email", &identity.email)
            .map_err(|_| SellerGitError::CommandFailed("config-user-email"))?;
    }
    Ok(repo)
}

/// Contribution fork-from-base: initialise `workdir` as a working clone of the PINNED
/// target `base_clone_url` at `base_oid`, on a per-job unique `branch` carrying the FULL job_id.
/// The agent then edits the tree; at delivery the daemon snapshots the result into one commit
/// parented on `base_oid` (see [`snapshot_delivery`]) and pushes `branch` to the seller's OWN
/// relay-git namespace. Transport-allowlisted (https + relay-git; `ext::`/file/ssh refused).
///
/// FULL-depth fetch (no depth limit) so the fork carries `base_oid` + ancestry — a shallow fork
/// would make the BUYER's descendant gate false-refuse an honest contribution.
pub fn init_contribution_workdir(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
    base_clone_url: &str,
    base_branch: &str,
    base_oid: &str,
    branch: &str,
    auth: Option<&PushAuth>,
) -> Result<(), SellerGitError> {
    assert_allowed_repo_locator(base_clone_url)?;
    let repo = init_repo_with_identity(workdir, identity)?;
    // Full-depth fetch of the base branch from the pinned target into a local ref. mobee relay-git
    // requires NIP-98 auth for READS, so present the seller secret for relay-git bases; public /
    // anonymous https bases fetch without it (git_transport gates the header on is_relay_git).
    let refspec = format!("+refs/heads/{base_branch}:refs/mobee/base");
    git_transport::fetch_refspecs(
        &repo,
        base_clone_url,
        &[&refspec],
        auth.map(|a| a.secret_key_hex.as_str()),
        false,
    )?;
    drop(repo);
    // Check out base_oid onto the per-job unique branch (the fork tip the agent extends).
    checkout_base_branch(workdir, branch, base_oid)
}

/// `git checkout -B <branch> <base_oid>` via git2 — the fork tip the agent extends. Force-creates
/// the branch at `base_oid`, checks out its tree, and points HEAD at it.
fn checkout_base_branch(
    workdir: &Path,
    branch: &str,
    base_oid: &str,
) -> Result<(), SellerGitError> {
    let repo =
        Repository::open(workdir).map_err(|error| SellerGitError::Io(format!("open: {error}")))?;
    let oid = Oid::from_str(base_oid).map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    let commit = repo
        .find_commit(oid)
        .map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    repo.branch(branch, &commit, true)
        .map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    repo.checkout_tree(commit.as_object(), Some(CheckoutBuilder::new().force()))
        .map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    repo.set_head(&format!("refs/heads/{branch}"))
        .map_err(|_| SellerGitError::CommandFailed("checkout-base"))?;
    Ok(())
}

/// Snapshot the final workdir tree into ONE delivery commit under `identity` and point `branch`
/// at it. This is the whole delivery step: the agent never commits (any commits it makes are
/// scratch and ignored), so the daemon authors the deliverable itself from the workdir contents.
///
/// - `base_oid = Some(oid)` (contribution): the commit is parented on the buyer-pinned base, so it
///   descends from `base_oid` by construction (the buyer's descendant gate holds). The snapshot is
///   refused if its tree equals the base tree — there is nothing to deliver.
/// - `base_oid = None` (from-scratch): a root commit whose tree is the whole workdir. Refused if the
///   tree is empty. No foreign history is ever adopted — we only ever deliver a tree we snapshot
///   ourselves — so there is no clone-then-deliver laundering to guard against.
///
/// The tree is staged with `add_all`/`update_all` (tracked modifications, new non-ignored files,
/// and deletions), so `.gitignore`d files and `.git` internals are never included and file modes
/// (the executable bit) are preserved. libgit2's `commit()` never signs (no `gpgsig`, regardless of
/// `commit.gpgsign`) and runs no hooks — both structural, so no config scrub is needed. Returns the
/// delivery commit oid.
pub fn snapshot_delivery(
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
    base_oid: Option<&str>,
    branch: &str,
    message: &str,
) -> Result<String, SellerGitError> {
    let repo = Repository::open(workdir)
        .map_err(|error| SellerGitError::Io(format!("snapshot: open workdir: {error}")))?;

    let base = match base_oid {
        Some(hex) => {
            let oid = Oid::from_str(hex)
                .map_err(|_| SellerGitError::Io("delivery refused: bad base oid".into()))?;
            let commit = repo.find_commit(oid).map_err(|_| {
                SellerGitError::Io("delivery refused: base_oid absent from workdir".into())
            })?;
            Some(commit)
        }
        None => None,
    };

    // Stage the full workdir tree: new + modified tracked files (add_all skips ignored) and
    // removals of tracked files gone from the workdir (update_all). `.git` is never walked.
    let mut index = repo
        .index()
        .map_err(|_| SellerGitError::CommandFailed("snapshot-index"))?;
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .map_err(|_| SellerGitError::CommandFailed("snapshot-add"))?;
    index
        .update_all(["*"].iter(), None)
        .map_err(|_| SellerGitError::CommandFailed("snapshot-update"))?;
    let tree_oid = index
        .write_tree()
        .map_err(|_| SellerGitError::CommandFailed("snapshot-write-tree"))?;
    let tree = repo
        .find_tree(tree_oid)
        .map_err(|_| SellerGitError::CommandFailed("snapshot-tree"))?;

    // Completion gate: there must be something to deliver.
    match base.as_ref().map(|c| c.tree_id()) {
        Some(base_tree) if base_tree == tree_oid => {
            return Err(SellerGitError::Io(
                "delivery refused: nothing to deliver (workdir identical to base)".into(),
            ));
        }
        None if tree.is_empty() => {
            return Err(SellerGitError::Io(
                "delivery refused: nothing to deliver (empty tree)".into(),
            ));
        }
        _ => {}
    }

    index
        .write()
        .map_err(|_| SellerGitError::CommandFailed("snapshot-index-write"))?;
    let signature = Signature::now(&identity.name, &identity.email)
        .map_err(|_| SellerGitError::CommandFailed("snapshot-signature"))?;
    let parents: Vec<&Commit> = base.iter().collect();
    let commit = repo
        .commit(None, &signature, &signature, message, &tree, &parents)
        .map_err(|_| SellerGitError::CommandFailed("snapshot-commit"))?;

    // Point the delivery branch (and HEAD) at the snapshot, regardless of whatever scratch state
    // the agent left HEAD in. Force-update the ref directly (`Repository::branch` refuses to move
    // the checked-out branch, which is the common case).
    let refname = format!("refs/heads/{branch}");
    repo.reference(&refname, commit, true, "mobee delivery snapshot")
        .map_err(|_| SellerGitError::CommandFailed("snapshot-branch"))?;
    repo.set_head(&refname)
        .map_err(|_| SellerGitError::CommandFailed("snapshot-set-head"))?;
    Ok(commit.to_string())
}

/// Optional NIP-98 auth for relay-git push/fetch (key never logged / never on argv).
///
/// `secret_key_hex` signs the NIP-98 event in-process for relay-git remotes. Public / anonymous
/// https remotes present no auth. Callers must not print it.
#[derive(Debug, Clone)]
pub struct PushAuth {
    pub secret_key_hex: String,
}

/// Push `branch` from `workdir` to `remote_url` (allowlisted https / relay-git only), with
/// optional NIP-98 auth for relay-git. Always in-process libgit2 — there is no system-git fallback.
/// Returns the pushed commit OID (full hex). Unauthenticated / prompt-needing remotes
/// fail closed.
pub fn push_branch_with_auth(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    auth: Option<&PushAuth>,
) -> Result<String, SellerGitError> {
    assert_allowed_repo_locator(remote_url)?;
    if branch.trim().is_empty() {
        return Err(SellerGitError::Io("branch must be non-empty".into()));
    }
    let oid =
        git_transport::push_branch(workdir, remote_url, branch, auth.map(|a| a.secret_key_hex.as_str()))?;
    eprintln!("seller push path=inprocess remote={remote_url} branch={branch} ok");
    Ok(oid)
}

/// Boot-time WRITE-auth probe: connect to `remote_url` in the PUSH direction and read the
/// receive-pack ref advertisement (the auth-gated leg) WITHOUT transferring a pack or mutating the
/// remote. Surfaces a broken write path — missing/invalid credential, unannounced/unreachable
/// relay-git repo, write-scoped auth failure — at daemon boot instead of at job-delivery time.
///
/// git2 has no `push --dry-run`; `connect(Push) + list` is the faithful equivalent (it performs
/// exactly the receive-pack advertisement mobee-relay NIP-98 auth-gates, then stops). Allowlisted
/// (https + relay-git; `ext::`/file/ssh refused).
pub fn preflight_push_probe(
    remote_url: &str,
    auth: Option<&PushAuth>,
) -> Result<(), SellerGitError> {
    assert_allowed_repo_locator(remote_url)?;
    git_transport::list_remote(
        remote_url,
        auth.map(|a| a.secret_key_hex.as_str()),
        Direction::Push,
    )
    .map(|_| ())
    .map_err(SellerGitError::from)
}

/// Resolve `git-credential-nostr` absolute path (`MOBEE_GIT_CREDENTIAL_NOSTR` override, then PATH).
///
/// Retained for `mobee doctor`'s informational check only — the seller itself no longer needs the
/// helper (all git legs are in-process libgit2 with NIP-98 signed in this process).
pub fn resolve_git_credential_nostr() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("MOBEE_GIT_CREDENTIAL_NOSTR") {
        let path = PathBuf::from(override_path);
        if path.is_file() {
            return Some(path);
        }
    }
    which_bin("git-credential-nostr").ok()
}

fn which_bin(name: &str) -> Result<PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-git-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).expect("mkdir");
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(path)
            .status()
            .expect("git init");
        assert!(status.success());
        let _ = Command::new("git")
            .args(["config", "user.name", "Mobee Seller Test"])
            .current_dir(path)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.email", "seller@example.invalid"])
            .current_dir(path)
            .status();
        fs::write(path.join("out.txt"), "hello\n").expect("write");
        assert!(Command::new("git")
            .args(["add", "out.txt"])
            .current_dir(path)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-m", "seller delivery"])
            .current_dir(path)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn push_refuses_ssh_and_local_paths() {
        let root = temp("refuse");
        let _ = fs::remove_dir_all(&root);
        init_repo(&root);
        assert!(matches!(
            push_branch_with_auth(&root, "git@example.invalid:repo.git", "main", None),
            Err(SellerGitError::Transport(_))
        ));
        assert!(matches!(
            push_branch_with_auth(&root, "/tmp/local.git", "main", None),
            Err(SellerGitError::Transport(_))
        ));
        assert!(matches!(
            push_branch_with_auth(&root, "ssh://example.invalid/repo.git", "main", None),
            Err(SellerGitError::Transport(_))
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn push_to_local_https_style_bare_via_file_is_refused() {
        let root = temp("file-refuse");
        let _ = fs::remove_dir_all(&root);
        init_repo(&root);
        let err = push_branch_with_auth(
            &root,
            &format!("file://{}/remote.git", root.display()),
            "main",
            None,
        )
        .expect_err("file refused");
        assert!(matches!(err, SellerGitError::Transport(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn preflight_push_probe_refuses_non_allowlisted_remote() {
        for bad in [
            "git@example.invalid:repo.git",
            "ssh://example.invalid/repo.git",
            "/tmp/local.git",
            "ext::sh -c evil",
        ] {
            assert!(
                matches!(
                    preflight_push_probe(bad, None),
                    Err(SellerGitError::Transport(_))
                ),
                "expected transport refuse for {bad}"
            );
        }
    }

    #[test]
    fn preflight_push_probe_fails_closed_on_unreachable_https_remote() {
        let err = preflight_push_probe("https://mobee-preflight.invalid/git/owner/repo.git", None)
            .expect_err("unreachable remote must fail closed");
        assert!(
            matches!(
                err,
                SellerGitError::AuthFailed(_)
                    | SellerGitError::CommandFailed(_)
                    | SellerGitError::Io(_)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn allowlist_https_accepted_by_locator_gate() {
        assert_allowed_repo_locator("https://example.invalid/git/owner/repo.git").unwrap();
        assert_allowed_repo_locator("https://example.invalid/repo.git").unwrap();
    }

    #[test]
    fn init_contribution_workdir_refuses_ext_base() {
        let root = temp("contrib-ext");
        let identity = DeliveryAgentIdentity::for_seller(&"aa".repeat(32));
        let err = init_contribution_workdir(
            &root,
            &identity,
            "ext::sh -c evil",
            "main",
            &"a".repeat(40),
            "mobee/contribution/job",
            None,
        )
        .expect_err("ext base must be refused by the transport allowlist");
        assert!(matches!(err, SellerGitError::Transport(_)), "got {err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_base_branch_from_oid_creates_fork_tip() {
        let root = temp("checkout-base");
        let _ = fs::remove_dir_all(&root);
        init_repo(&root);
        let base_oid = {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };

        checkout_base_branch(&root, "mobee/contribution/job", &base_oid)
            .expect("checkout of a valid base_oid onto the fork branch must succeed");

        let branch = Command::new("git")
            .args(["symbolic-ref", "--short", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(branch.stdout).unwrap().trim(),
            "mobee/contribution/job"
        );
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), base_oid);
        let _ = fs::remove_dir_all(&root);
    }
}

/// Snapshot delivery: the daemon authors ONE commit from the final workdir tree, whatever git
/// state the agent left. Setup uses system `git` (fixtures only); every assertion reads the
/// delivered objects through git2. `snapshot_without_system_git` proves the delivery path itself
/// has no shell-out.
#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn workdir(label: &str) -> PathBuf {
        let id = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("mobee-snapshot-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir workdir");
        dir
    }

    fn identity() -> DeliveryAgentIdentity {
        DeliveryAgentIdentity::for_seller(&"ab".repeat(32))
    }

    fn git<const N: usize>(dir: &Path, args: [&str; N]) {
        run_env(dir, args, None);
    }

    fn run_env<const N: usize>(dir: &Path, args: [&str; N], who: Option<(&str, &str)>) {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(dir);
        if let Some((name, email)) = who {
            cmd.env("GIT_AUTHOR_NAME", name)
                .env("GIT_AUTHOR_EMAIL", email)
                .env("GIT_COMMITTER_NAME", name)
                .env("GIT_COMMITTER_EMAIL", email);
        }
        let out = cmd.output().expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one base commit (foreign upstream identity), checked out on `branch`. Returns base oid.
    fn init_with_base(dir: &Path, branch: &str) -> String {
        git(dir, ["init", "--initial-branch=main"]);
        fs::write(dir.join("README.md"), "base\n").expect("write base");
        git(dir, ["add", "-A"]);
        run_env(
            dir,
            ["commit", "-m", "base"],
            Some(("Upstream", "upstream@example.invalid")),
        );
        let base = head(dir);
        git(dir, ["checkout", "-B", branch, &base]);
        base
    }

    fn head(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("rev-parse");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn write(dir: &Path, path: &str, content: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(&full, content).expect("write file");
    }

    fn tree_paths(dir: &Path, oid: &str) -> Vec<String> {
        let repo = Repository::open(dir).expect("open");
        let tree = repo
            .find_commit(Oid::from_str(oid).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        let mut paths = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                paths.push(format!("{root}{}", entry.name().unwrap_or("")));
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        paths
    }

    fn commit(dir: &Path, oid: &str) -> git2::Commit<'static> {
        // Leak the repo so the returned commit's lifetime is convenient in asserts.
        let repo = Box::leak(Box::new(Repository::open(dir).expect("open")));
        repo.find_commit(Oid::from_str(oid).unwrap()).expect("commit")
    }

    // ── Field case: agent edits, never commits — the daemon snapshots the workdir ──────────────
    #[test]
    fn snapshots_uncommitted_workdir_onto_base() {
        let dir = workdir("uncommitted");
        let base = init_with_base(&dir, "mobee/job");
        write(&dir, "src/feature.rs", "agent work, never committed\n");
        let id = identity();

        let oid = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "mobee delivery: task")
            .expect("snapshot");

        let c = commit(&dir, &oid);
        assert_eq!(c.parent_count(), 1, "delivery is one commit on top of base");
        assert_eq!(c.parent_id(0).unwrap().to_string(), base, "parented on the pinned base");
        assert_eq!(c.author().email(), Some(id.email.as_str()));
        assert_eq!(c.committer().email(), Some(id.email.as_str()));
        assert!(tree_paths(&dir, &oid).contains(&"src/feature.rs".to_owned()));
        let _ = fs::remove_dir_all(&dir);
    }

    // ── Agent scratch commits (foreign identity, extra commits) are ignored ────────────────────
    #[test]
    fn ignores_agent_scratch_commits_delivers_single_commit_on_base() {
        let dir = workdir("scratch");
        let base = init_with_base(&dir, "mobee/job");
        // Agent makes two scratch commits under a foreign identity.
        write(&dir, "a.rs", "one\n");
        git(&dir, ["add", "-A"]);
        run_env(&dir, ["commit", "-m", "scratch 1"], Some(("Claude", "c@anthropic.invalid")));
        write(&dir, "b.rs", "two\n");
        git(&dir, ["add", "-A"]);
        run_env(&dir, ["commit", "-m", "scratch 2"], Some(("Claude", "c@anthropic.invalid")));
        let id = identity();

        let oid = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg").expect("snapshot");

        let c = commit(&dir, &oid);
        assert_eq!(c.parent_id(0).unwrap().to_string(), base, "parented on base, not the scratch tip");
        assert_eq!(c.author().email(), Some(id.email.as_str()), "delivery identity, not the agent's");
        // Exactly one commit between base and the delivery tip.
        let repo = Repository::open(&dir).unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push(Oid::from_str(&oid).unwrap()).unwrap();
        walk.hide(Oid::from_str(&base).unwrap()).unwrap();
        assert_eq!(walk.count(), 1, "the delivery collapses to a single commit");
        // Both files the agent produced are present.
        let paths = tree_paths(&dir, &oid);
        assert!(paths.contains(&"a.rs".to_owned()) && paths.contains(&"b.rs".to_owned()));
        let _ = fs::remove_dir_all(&dir);
    }

    // ── From-scratch: root commit whose tree is the whole workdir ──────────────────────────────
    #[test]
    fn snapshots_from_scratch_as_root_commit() {
        let dir = workdir("scratch-base");
        let id = identity();
        init_empty_delivery_workdir(&dir, &id).expect("init");
        write(&dir, "out.rs", "work\n");

        let oid = snapshot_delivery(&dir, &id, None, "mobee/job", "msg").expect("snapshot");

        let c = commit(&dir, &oid);
        assert_eq!(c.parent_count(), 0, "from-scratch delivery is a root commit");
        assert_eq!(c.author().email(), Some(id.email.as_str()));
        assert!(tree_paths(&dir, &oid).contains(&"out.rs".to_owned()));
        let _ = fs::remove_dir_all(&dir);
    }

    // ── Completion gate: nothing to deliver refuses cleanly (both floors) ──────────────────────
    #[test]
    fn nothing_to_deliver_contribution_refuses() {
        let dir = workdir("empty-contrib");
        let base = init_with_base(&dir, "mobee/job");
        let id = identity();
        // Workdir untouched — identical to base.
        let err = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg")
            .expect_err("must refuse");
        assert!(err.to_string().contains("identical to base"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_to_deliver_from_scratch_refuses() {
        let dir = workdir("empty-scratch");
        let id = identity();
        init_empty_delivery_workdir(&dir, &id).expect("init");
        let err = snapshot_delivery(&dir, &id, None, "mobee/job", "msg").expect_err("must refuse");
        assert!(err.to_string().contains("empty tree"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    // ── .gitignore'd files are never delivered ─────────────────────────────────────────────────
    #[test]
    fn snapshot_excludes_gitignored_files() {
        let dir = workdir("ignore");
        let base = init_with_base(&dir, "mobee/job");
        write(&dir, ".gitignore", "secret.txt\n");
        write(&dir, "secret.txt", "do not deliver\n");
        write(&dir, "real.rs", "delivered\n");
        let id = identity();

        let oid = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg").expect("snapshot");

        let paths = tree_paths(&dir, &oid);
        assert!(paths.contains(&"real.rs".to_owned()));
        assert!(!paths.contains(&"secret.txt".to_owned()), "ignored file must not be delivered");
        let _ = fs::remove_dir_all(&dir);
    }

    // ── `.git` internals are never delivered ───────────────────────────────────────────────────
    #[test]
    fn snapshot_never_includes_git_internals() {
        let dir = workdir("gitinternals");
        let base = init_with_base(&dir, "mobee/job");
        write(&dir, "work.rs", "work\n");
        let id = identity();

        let oid = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg").expect("snapshot");

        for path in tree_paths(&dir, &oid) {
            assert!(!path.starts_with(".git/") && path != ".git", "git internals leaked: {path}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ── Host commit.gpgsign is ignored — libgit2 never signs ───────────────────────────────────
    #[test]
    fn snapshot_commit_is_never_signed() {
        let dir = workdir("gpgsign");
        let base = init_with_base(&dir, "mobee/job");
        git(&dir, ["config", "commit.gpgsign", "true"]);
        git(&dir, ["config", "user.signingkey", "DEADBEEF"]);
        write(&dir, "work.rs", "work\n");
        let id = identity();

        let oid = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg").expect("snapshot");

        assert!(
            !commit(&dir, &oid).raw_header().unwrap().contains("gpgsig"),
            "delivery commit must carry no signature"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ── A blocking pre-commit hook cannot stop the snapshot ────────────────────────────────────
    #[test]
    fn snapshot_bypasses_base_repo_hooks() {
        let dir = workdir("hooks");
        let base = init_with_base(&dir, "mobee/job");
        let hook = dir.join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("write hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");
        write(&dir, "work.rs", "work\n");
        let id = identity();

        snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg")
            .expect("libgit2 runs no hooks, so a failing pre-commit cannot block delivery");
        let _ = fs::remove_dir_all(&dir);
    }

    // ── Executable bit is preserved ────────────────────────────────────────────────────────────
    #[test]
    fn snapshot_preserves_executable_bit() {
        let dir = workdir("execbit");
        let base = init_with_base(&dir, "mobee/job");
        let script = dir.join("run.sh");
        fs::write(&script, "#!/bin/sh\necho hi\n").expect("write script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        let id = identity();

        let oid = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg").expect("snapshot");

        let repo = Repository::open(&dir).unwrap();
        let entry = repo
            .find_commit(Oid::from_str(&oid).unwrap())
            .unwrap()
            .tree()
            .unwrap()
            .get_path(Path::new("run.sh"))
            .unwrap();
        assert_eq!(entry.filemode(), 0o100755, "executable bit must be preserved");
        let _ = fs::remove_dir_all(&dir);
    }

    // ── Deletions are honored: a file removed from the workdir is absent from the delivery ─────
    #[test]
    fn snapshot_reflects_deletions() {
        let dir = workdir("delete");
        let base = init_with_base(&dir, "mobee/job");
        // README.md exists in base; the agent deletes it and adds a replacement.
        fs::remove_file(dir.join("README.md")).expect("rm");
        write(&dir, "new.rs", "replacement\n");
        let id = identity();

        let oid = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg").expect("snapshot");

        let paths = tree_paths(&dir, &oid);
        assert!(!paths.contains(&"README.md".to_owned()), "deleted file must not be delivered");
        assert!(paths.contains(&"new.rs".to_owned()));
        let _ = fs::remove_dir_all(&dir);
    }

    /// PATH-stripped proof: build the base with git2 ONLY and snapshot — no `Command`. Run with
    /// `git` absent from PATH to prove the delivery path has no hidden shell-out.
    #[test]
    fn snapshot_without_system_git() {
        use git2::{Repository, Signature};
        let dir = workdir("nogit");
        let repo = Repository::init(&dir).expect("git2 init");
        // Base commit via git2.
        fs::write(dir.join("README.md"), "base\n").expect("write");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new("README.md")).expect("add");
        index.write().expect("index write");
        let tree = repo.find_tree(index.write_tree().expect("wt")).expect("tree");
        let sig = Signature::now("Upstream", "u@u.invalid").expect("sig");
        let base = repo
            .commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .expect("git2 commit")
            .to_string();
        // Agent edit, uncommitted. (snapshot_delivery opens its own repo handle.)
        fs::write(dir.join("feature.rs"), "work\n").expect("write");
        let id = identity();

        let oid = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg")
            .expect("git2-only snapshot");

        assert_eq!(commit(&dir, &oid).parent_id(0).unwrap().to_string(), base);
        let _ = fs::remove_dir_all(&dir);
    }

    // ── End-to-end: snapshot → push the delivery branch to a bare remote ───────────────────────
    #[test]
    fn deliver_after_snapshot_pushes_branch() {
        let dir = workdir("e2e");
        let base = init_with_base(&dir, "mobee/job");
        // Agent left scratch commits AND uncommitted edits — the daemon ignores all of it.
        write(&dir, "feature.rs", "impl\n");
        git(&dir, ["add", "-A"]);
        run_env(&dir, ["commit", "-m", "scratch"], Some(("Claude", "c@anthropic.invalid")));
        write(&dir, "extra.rs", "more, uncommitted\n");
        let id = identity();

        let oid = snapshot_delivery(&dir, &id, Some(&base), "mobee/job", "msg").expect("snapshot");

        let remote = workdir("e2e-remote.git");
        git(&remote, ["init", "--bare", "--initial-branch=main"]);
        git(&dir, ["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&dir, ["push", "origin", "mobee/job"]);
        let out = Command::new("git")
            .args(["rev-parse", "refs/heads/mobee/job"])
            .current_dir(&remote)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), oid);
        let paths = tree_paths(&dir, &oid);
        assert!(paths.contains(&"feature.rs".to_owned()) && paths.contains(&"extra.rs".to_owned()));

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&remote);
    }
}
