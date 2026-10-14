//! The commit this binary is built from, for `serve`'s startup line (`shutdown::BUILD_COMMIT`).
//!
//! Same rules as `crates/maxplayer/build.rs` (#818), kept to what this crate needs: written to a
//! file in `OUT_DIR` (no environment surface for child processes), resolved from the
//! `MAXPLAYER_BUILD_COMMIT` build override, else `HEAD` read as files (worktrees included), else
//! the literal `unknown`. A sha is emitted only when one was read, never synthesised.
use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    println!("cargo:rerun-if-env-changed=MAXPLAYER_BUILD_COMMIT");
    let stamp = match env::var("MAXPLAYER_BUILD_COMMIT") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => head(&PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()))
            .unwrap_or_else(|| "unknown".into()),
    };
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("build_commit");
    fs::write(&out, stamp).unwrap();
}

fn head(start: &Path) -> Option<String> {
    let git = start.ancestors().find_map(|dir| {
        let c = dir.join(".git");
        if c.is_dir() {
            Some(c)
        } else if c.is_file() {
            let target = read(&c)?.strip_prefix("gitdir:")?.trim().to_string();
            Some(dir.join(target))
        } else {
            None
        }
    })?;
    let common = read(&git.join("commondir")).map_or(git.clone(), |rel| git.join(rel));
    let head = read(&watch(git.join("HEAD")))?;
    let Some(name) = head.strip_prefix("ref:").map(str::trim) else {
        return sha(&head);
    };
    if let Some(v) = read(&watch(common.join(name))) {
        return sha(&v);
    }
    let packed = read(&watch(common.join("packed-refs")))?;
    packed.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let (value, r) = (parts.next()?, parts.next()?);
        (r == name).then(|| sha(value)).flatten()
    })
}
fn read(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}
/// Re-run when this file changes; only for files that exist (a missing path reruns every build).
fn watch(p: PathBuf) -> PathBuf {
    if p.exists() {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    p
}
fn sha(v: &str) -> Option<String> {
    (v.len() == 40 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| v.to_string())
}
