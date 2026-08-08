//! #610 regression: the in-process git transport must be immune to ambient git config.
//!
//! libgit2 applies `url.*.insteadOf` from the global/XDG/system config at CONNECT time — even for
//! anonymous remotes (`Repository::remote_anonymous` does NOT prevent it). If an ambient insteadOf
//! rewrites an allowlisted `https` URL onto `ssh` — a scheme the `default-features = false` build has
//! no transport for — the leg fails with the opaque libgit2 "unsupported URL protocol". That is the
//! exact live #610 failure (a developer's `url."git@github.com:".insteadOf = "https://github.com/"`).
//!
//! `git_transport` defends against this by emptying libgit2's global/XDG/system config search path
//! at first use, so no ambient config — hence no insteadOf — is ever read. This test plants such an
//! ambient insteadOf and asserts an allowlisted `https` URL is NOT rewritten. RED-ON-REVERT: remove
//! the search-path isolation and this fails with "unsupported URL protocol".
//!
//! Gated on `git-delivery` (which compiles `git_transport` in); a bare `cargo test` compiles this out.

use std::fs;
use std::path::PathBuf;

fn temp(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "maxplayer-cfgiso-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn ambient_insteadof_cannot_rewrite_allowlisted_https() {
    // Plant an ambient config home whose insteadOf rewrites our test https host onto an ssh URL.
    let home = temp("home");
    let xdg = home.join(".config");
    fs::create_dir_all(xdg.join("git")).expect("xdg git dir");
    let rewrite = "[url \"git@rewrite.invalid:\"]\n\tinsteadOf = \"https://127.0.0.1:1/\"\n";
    fs::write(home.join(".gitconfig"), rewrite).expect("global config");
    fs::write(xdg.join("git").join("config"), rewrite).expect("xdg config");

    // SAFETY (edition-2024 set_var): set before ANY libgit2/config use in this dedicated single-test
    // process, so libgit2 derives its global/XDG config search from the planted home. No other thread
    // reads the env concurrently.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        std::env::remove_var("GIT_CONFIG_GLOBAL");
        std::env::remove_var("GIT_CONFIG_SYSTEM");
        std::env::remove_var("GIT_CONFIG_NOSYSTEM");
    }

    // Allowlisted https URL to a CLOSED port. With ambient-config isolation the URL is NOT rewritten:
    // it reaches the rustls subtransport and fails at CONNECT (refused), never at transport-find.
    // Without isolation the planted insteadOf rewrites it onto `git@rewrite.invalid:...` (ssh), which
    // the build has no transport for, and libgit2 returns "unsupported URL protocol".
    let err = maxplayer_core::git_transport::ls_remote("https://127.0.0.1:1/git/o/r.git", None)
        .expect_err("closed port must fail to connect");
    let msg = err.to_string();
    assert!(
        !msg.contains("unsupported URL protocol"),
        "ambient insteadOf must NOT rewrite the allowlisted https URL onto ssh — \
         config isolation regressed (got: {msg})"
    );
}
