//! Capture the commit this binary is built from, at the one moment the information exists.
//!
//! #818: a deployed `maxplayer` carried no git provenance, so the only way to ask a seat what it was
//! running was to bracket its crate version against the commit window that produced it — a method
//! that works exactly until two releases land in one day. The stamp resolved here is written to
//! `$OUT_DIR/build_stamp`, `include_str!`'d by `src/build_stamp.rs`, and printed inside the
//! parentheses of `maxplayer <version> (<stamp>)`.
//!
//! ── Why a file and not `cargo::rustc-env` ───────────────────────────────────────────────────────
//! The obvious mechanism is `cargo:rustc-env=…`, and it is wrong HERE. Cargo applies those variables
//! to the processes it launches as well as to rustc, so a `MAXPLAYER_`-prefixed one lands in the
//! environment of every `cargo test`/`cargo run` child — and this repo reserves that whole prefix
//! for config and refuses an unmapped `MAXPLAYER_*` variable FAIL-CLOSED rather than ignoring it.
//! Measured, not reasoned: `cargo:rustc-env=MAXPLAYER_BUILD_STAMP` turned 21 tests in this crate red
//! with `unknown field 'build_stamp'` before this was a file. Naming the variable around the rule
//! would have satisfied the tripwire and still put a new variable in every child process; a file in
//! `OUT_DIR` adds no environment surface at all, and `include_str!` records it in rustc's dep-info
//! so a changed stamp still forces a recompile.
//!
//! Resolution order, and nothing else:
//!   1. `MAXPLAYER_BUILD_COMMIT` in the build environment. The channel for build paths that have no
//!      `.git` to read — the nix flake (`src = self`, so the store copy carries no git) and the
//!      docker image (`.dockerignore` strips `.git/`). Taken verbatim: it is the build
//!      environment's own statement about what it is building, and `verify-release-version.sh`
//!      fails a release closed if that statement is not a resolvable sha.
//!   2. `HEAD` of the source tree's `.git`, read as files. No `git` binary — issue #55 bans system
//!      git on product paths, and while a build script is not a product path, a plain file read
//!      reaches the same answer with nothing to ban.
//!   3. The literal `unknown`.
//!
//! Never anything else. #818 documents the failure mode this exists to avoid: a plausible 40-hex
//! string that resolves to no commit "looks fixed" while telling a future reader nothing, so a
//! sha is emitted only when it was read, never synthesised, padded or zeroed.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const UNKNOWN: &str = "unknown";
const ENV_OVERRIDE: &str = "MAXPLAYER_BUILD_COMMIT";

fn main() {
    // The override is an env var, so cargo cannot see it change by looking at files.
    println!("cargo:rerun-if-env-changed={ENV_OVERRIDE}");

    let stamp = match env::var(ENV_OVERRIDE) {
        // An empty value is an unset one: `MAXPLAYER_BUILD_COMMIT=` in a Dockerfile with no `--build-arg`
        // is the shape docker produces for "not supplied", and treating it as a statement would print
        // `()` — a stamp that claims a value and carries none.
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            let manifest_dir =
                PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
            read_head_commit(&manifest_dir).unwrap_or_else(|| UNKNOWN.to_string())
        }
    };

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("build_stamp");
    // No trailing newline: the file IS the stamp, and `include_str!` hands back exactly its bytes.
    fs::write(&out, &stamp).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
}

/// Resolve `HEAD` to a 40-hex sha by reading files, emitting a `rerun-if-changed` for every file the
/// answer depended on. `None` when any step is missing or malformed — never a partial guess.
///
/// The rerun set is the load-bearing half. A sha captured once and then kept across an incremental
/// build is the same defect as no sha: it names a commit the binary is not built from. `HEAD` moves
/// on checkout, the ref file moves on commit, and `packed-refs` moves when a loose ref is packed
/// away, so all three are watched — and only those, so a build with no git activity is a no-op.
fn read_head_commit(start: &Path) -> Option<String> {
    let git_dir = find_git_dir(start)?;
    // A linked worktree's git dir holds its own HEAD but no refs: those live in the common dir it
    // names. Resolving through `commondir` is what makes this work in a `git worktree` checkout,
    // which is how this change was itself developed.
    let common_dir = match read_trimmed(&git_dir.join("commondir")) {
        Some(rel) => resolve_relative(&git_dir, &rel),
        None => git_dir.clone(),
    };

    let head_path = git_dir.join("HEAD");
    let head = read_trimmed(&head_path)?;
    rerun_if_changed(&head_path);

    // Detached HEAD: the file is the sha itself.
    let Some(ref_name) = head.strip_prefix("ref:").map(str::trim) else {
        return sha(&head);
    };

    // Loose ref first — that is what `git commit` writes.
    let loose = common_dir.join(ref_name);
    if let Some(value) = read_trimmed(&loose) {
        rerun_if_changed(&loose);
        return sha(&value);
    }

    // Then the packed table, where a ref lands once it has been packed away.
    let packed_path = common_dir.join("packed-refs");
    let packed = fs::read_to_string(&packed_path).ok()?;
    rerun_if_changed(&packed_path);
    for line in packed.lines() {
        // `^<sha>` continuation lines carry the peeled tag target, not the ref, so skip anything
        // that is not `<sha> <refname>`.
        let mut parts = line.split_whitespace();
        let (Some(value), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name == ref_name {
            return sha(value);
        }
    }
    None
}

/// Walk up from the crate directory to the first `.git`, following the `gitdir:` pointer a linked
/// worktree (or a submodule) leaves in place of a directory.
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let pointer = read_trimmed(&candidate)?;
            let target = pointer.strip_prefix("gitdir:")?.trim();
            return Some(resolve_relative(dir, target));
        }
    }
    None
}

fn resolve_relative(base: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// A sha is 40 lowercase hex or it is not a sha. Anything else is dropped rather than printed: the
/// point of #818 is that the reader can trust what is in the parentheses.
fn sha(value: &str) -> Option<String> {
    let value = value.trim();
    // Lowercase only, deliberately: that is what git writes and what the verifiers match on.
    (value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    .then(|| value.to_string())
}

fn rerun_if_changed(path: &Path) {
    // Only for files that exist. A `rerun-if-changed` naming a missing path makes cargo re-run this
    // script on every single build, which is the rebuild loop the change must not introduce.
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
