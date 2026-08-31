//! Can the AGENT redirect the seller's delivery push by writing the job workdir's own `.git/config`?
//!
//! Under `[sandbox] mode = "docker"` the whole job workdir is bind-mounted read-write into the
//! container, `.git` included, so the agent can write any file in it. libgit2 applies
//! `url.<other>.insteadOf` from the config of the repository that RUNS an operation, at CONNECT time,
//! for `remote_anonymous` too. `git_transport::ensure_registered` empties the GLOBAL/XDG/SYSTEM
//! config search paths (#610), but a repository's OWN `.git/config` is not reached through a search
//! path — so a push straight from the agent's workdir would follow a planted `insteadOf`, sending the
//! push (and the seller's token) to a host the agent chose.
//!
//! The fix: before the delivery push, `seller_git::neutralize_push_config` REPLACES the workdir's
//! `.git/config` with a fixed, minimal, redirect-free file — a whole-file replacement, so
//! `insteadOf`, `pushInsteadOf`, and any `[include]` the agent planted are all gone at once. This
//! test drives that production primitive and asserts the push lands at the URL the seller named. A
//! positive control at the end RE-PLANTS the rule and shows the push then does redirect — proving the
//! rule is real and that neutralising it is what protected the delivery.
#![cfg(feature = "git-delivery")]

use maxplayer_core::seller_git::neutralize_push_config;
use std::path::{Path, PathBuf};

fn temp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "maxplayer-hostile-localcfg-{label}-{}",
        std::process::id()
    ))
}

fn bare(path: &Path) -> git2::Repository {
    git2::Repository::init_bare(path).expect("init bare")
}

fn push_workdir_to(workdir: &Path, url: &str, branch: &str) {
    let repo = git2::Repository::open(workdir).expect("open workdir");
    let mut remote = repo.remote_anonymous(url).expect("remote_anonymous");
    let _ = remote.push(&[&format!("refs/heads/{branch}:refs/heads/{branch}")], None);
}

fn plant_insteadof(workdir: &Path, attacker_url: &str, intended_url: &str) {
    let repo = git2::Repository::open(workdir).expect("open workdir");
    let mut cfg = repo.config().expect("repo config");
    cfg.set_str(&format!("url.{attacker_url}.insteadOf"), intended_url)
        .expect("plant insteadOf");
}

#[test]
fn config_rewrite_defeats_a_local_insteadof_redirect() {
    let root = temp("root");
    let _ = std::fs::remove_dir_all(&root);
    let intended = root.join("intended.git");
    let attacker = root.join("attacker.git");
    let workdir = root.join("workdir");
    std::fs::create_dir_all(&workdir).expect("workdir");
    bare(&intended);
    bare(&attacker);

    // Mirror production: ambient config is isolated, local config is NOT reachable that way.
    for level in [
        git2::ConfigLevel::System,
        git2::ConfigLevel::Global,
        git2::ConfigLevel::XDG,
        git2::ConfigLevel::ProgramData,
    ] {
        let _ = unsafe { git2::opts::set_search_path(level, "") };
    }

    let branch = "job";
    let commit_oid = {
        let repo = git2::Repository::init(&workdir).expect("init workdir");
        let mut index = repo.index().expect("index");
        std::fs::write(workdir.join("deliverable.txt"), b"work\n").expect("write");
        index.add_path(Path::new("deliverable.txt")).expect("add");
        index.write().expect("write index");
        let tree_oid = index.write_tree().expect("tree");
        let tree = repo.find_tree(tree_oid).expect("find tree");
        let sig = git2::Signature::now("s", "s@s").expect("sig");
        repo.commit(Some(&format!("refs/heads/{branch}")), &sig, &sig, "delivery", &tree, &[])
            .expect("commit")
    };

    let intended_url = intended.to_str().expect("utf8").to_owned();
    let attacker_url = attacker.to_str().expect("utf8").to_owned();

    // ── The attack: the agent writes the workdir's own .git/config ──────────────────────────
    plant_insteadof(&workdir, &attacker_url, &intended_url);

    // ── The fix: neutralise the config, then push from the workdir ──────────────────────────
    neutralize_push_config(&workdir).expect("neutralize");
    push_workdir_to(&workdir, &intended_url, branch);

    // ── The property: the commit is at the intended host, and NOT at the attacker's ─────────
    let intended_repo = git2::Repository::open_bare(&intended).expect("open intended");
    assert!(
        intended_repo.find_commit(commit_oid).is_ok(),
        "the push after neutralise must reach the intended remote"
    );
    let attacker_repo = git2::Repository::open_bare(&attacker).expect("open attacker");
    assert!(
        attacker_repo.find_commit(commit_oid).is_err(),
        "SECURITY: the push reached the attacker — neutralising the config did not defeat the redirect"
    );

    // ── Positive control: RE-PLANT the rule and push again — now it DOES redirect to the attacker.
    //    Proves the insteadOf genuinely redirects, so neutralising it is what protected the delivery
    //    above (the fix assertion was not vacuous). Runs last, since it delivers to the attacker. ──
    plant_insteadof(&workdir, &attacker_url, &intended_url);
    push_workdir_to(&workdir, &intended_url, branch);
    let attacker_repo = git2::Repository::open_bare(&attacker).expect("reopen attacker");
    assert!(
        attacker_repo.find_commit(commit_oid).is_ok(),
        "fixture broken: re-planting the insteadOf did NOT redirect the push, so the rule is inert \
         and the fix assertion proved nothing"
    );

    let _ = std::fs::remove_dir_all(&root);
}
