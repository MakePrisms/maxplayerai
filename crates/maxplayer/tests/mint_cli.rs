//! `maxplayer mint` / `maxplayer mint install` driven as the shipped binary, with no network: the
//! "release" is a `file://` directory this test writes (a `.tar.gz` holding a stand-in
//! `maxplayer-mint` plus a `SHA256SUMS`), shaped exactly like the GitHub release `install.sh` reads.
//! The stand-in answers `--version` with a chosen line and `init` by creating `<home>/mint/` with a
//! real nostr key, which is all `maxplayer` relies on.
#![cfg(feature = "wallet")]

#[path = "support/fake_registration.rs"]
mod fake_registration;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fake_registration::FakeRegistration;
use nostr_sdk::prelude::{Keys, ToBech32};
use sha2::{Digest, Sha256};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const STAMP: &str = "0123456789abcdef0123456789abcdef01234567";
const HTTP_REFUSED: &str =
    "http:// download sources are not supported; use https:// (or file:// for a local mirror)";

fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        other => panic!("no release platform for {other:?}"),
    }
}

fn asset() -> String {
    format!("maxplayer-mint-{VERSION}-{}.tar.gz", platform())
}

/// The binary with no inherited `MAXPLAYER_*` variables (the config layer refuses unknown names).
fn maxplayer() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MAXPLAYER_") {
            cmd.env_remove(key);
        }
    }
    cmd.stdin(Stdio::null());
    cmd
}

fn run(cmd: &mut Command) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = cmd.output().expect("run maxplayer");
    (
        status.code().unwrap_or(-1),
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

/// One scratch world: a release directory, a bin dir and a home, all absent until written.
struct World {
    _tmp: tempfile::TempDir,
    release: PathBuf,
    bin_dir: PathBuf,
    home: PathBuf,
    mint_keys: Keys,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let release = tmp.path().join("release");
        fs::create_dir(&release).unwrap();
        Self {
            release,
            bin_dir: tmp.path().join("bin"),
            home: tmp.path().join("home"),
            mint_keys: Keys::generate(),
            _tmp: tmp,
        }
    }

    fn from(&self) -> String {
        format!("file://{}", self.release.display())
    }

    /// A stand-in `maxplayer-mint` that reports `version_line`.
    fn script(&self, version_line: &str) -> Vec<u8> {
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             --version|version) echo '{version_line}' ;;\n\
             init)\n\
               mkdir -m 700 \"$MAXPLAYER_HOME/mint\" || exit 1\n\
               umask 077\n\
               printf '%s' '{secret}' > \"$MAXPLAYER_HOME/mint/nostr.key\"\n\
               if [ \"$2\" = --name ]; then printf 'name = \"%s\"\\n' \"$3\" > \"$MAXPLAYER_HOME/mint/mint.toml\"; fi\n\
               echo \"Created $MAXPLAYER_HOME/mint\" ;;\n\
             *) exit 2 ;;\n\
             esac\n",
            secret = self.mint_keys.secret_key().to_secret_hex(),
        )
        .into_bytes()
    }

    /// Publish a release holding `binary` and a SHA256SUMS line with `digest` (the real one when
    /// `None`). Returns the archive bytes.
    fn publish(&self, binary: &[u8], digest: Option<&str>) -> Vec<u8> {
        let stem = format!("maxplayer-mint-{VERSION}-{}", platform());
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (name, bytes, mode) in [
            ("maxplayer-mint", binary, 0o755),
            ("LICENSE-MIT", &b"mit"[..], 0o644),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{stem}/{name}"), bytes)
                .unwrap();
        }
        let archive = builder.into_inner().unwrap().finish().unwrap();
        fs::write(self.release.join(asset()), &archive).unwrap();
        let real = hex::encode(Sha256::digest(&archive));
        let other = format!("maxplayer-{VERSION}-{}.tar.gz", platform());
        fs::write(
            self.release.join("SHA256SUMS"),
            format!(
                "{}  {other}\n{}  {}\n",
                "c".repeat(64),
                digest.unwrap_or(&real),
                asset()
            ),
        )
        .unwrap();
        archive
    }

    fn good_release(&self) {
        self.publish(
            &self.script(&format!("maxplayer-mint {VERSION} ({STAMP})")),
            None,
        );
    }

    fn install(&self, extra: &[&str]) -> (i32, String, String) {
        run(maxplayer()
            .args(["mint", "install", "--from", &self.from()])
            .arg("--home")
            .arg(&self.home)
            .arg("--bin-dir")
            .arg(&self.bin_dir)
            .args(extra))
    }

    fn info(&self, extra: &[&str]) -> (i32, String, String) {
        run(maxplayer()
            .args(["mint"])
            .arg("--home")
            .arg(&self.home)
            .arg("--bin-dir")
            .arg(&self.bin_dir)
            .args(extra)
            .env("PATH", "/usr/bin:/bin"))
    }

    fn target(&self) -> PathBuf {
        self.bin_dir.join("maxplayer-mint")
    }

    /// Every entry left in the bin dir (staging files included).
    fn bin_entries(&self) -> Vec<String> {
        match fs::read_dir(&self.bin_dir) {
            Ok(dir) => {
                let mut names: Vec<String> = dir
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                names
            }
            Err(_) => Vec::new(),
        }
    }

    fn npub(&self) -> String {
        self.mint_keys.public_key().to_bech32().unwrap()
    }
}

/// Every path under `root` with its mode and length, for "nothing changed" comparisons.
fn snapshot(root: &Path) -> Vec<(PathBuf, u32, u64)> {
    let mut all = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        all.push((path.clone(), meta.permissions().mode(), meta.len()));
        if meta.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
        }
    }
    all.sort();
    all
}

#[test]
fn install_verifies_installs_sets_up_the_mint_home_and_prints_the_link_step() {
    let w = World::new();
    w.good_release();
    let (code, out, err) = w.install(&["--name", "Bob's credits"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("Verified sha256"), "{out}");
    assert!(
        out.contains(&format!("Installed maxplayer-mint {VERSION} ({STAMP})")),
        "{out}"
    );
    let mode = fs::metadata(w.target()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(w.bin_entries(), ["maxplayer-mint"], "no staging file left");
    // `init` ran with this home and the name.
    assert!(w.home.join("mint/nostr.key").is_file());
    assert!(
        fs::read_to_string(w.home.join("mint/mint.toml"))
            .unwrap()
            .contains("Bob's credits")
    );
    assert!(
        out.contains(&format!("Mint URL: nostr://{}", w.npub())),
        "{out}"
    );
    assert!(
        out.contains("no agent key, so it emails you a code"),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "maxplayer account link --mint --home {}",
            w.home.display()
        )),
        "{out}"
    );

    // A second install of the same version is a no-op that keeps the mint.
    let before = snapshot(&w.home);
    let (code, out, err) = w.install(&[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("already installed"), "{out}");
    assert!(out.contains("Kept the existing mint"), "{out}");
    assert_eq!(snapshot(&w.home), before);
}

#[test]
fn a_checksum_mismatch_is_refused_and_leaves_nothing_installed() {
    let w = World::new();
    let wrong = "d".repeat(64);
    w.publish(
        &w.script(&format!("maxplayer-mint {VERSION} ({STAMP})")),
        Some(&wrong),
    );
    let (code, out, err) = w.install(&[]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("does not match the release"), "{err}");
    assert!(err.contains(&wrong), "{err}");
    assert!(
        !w.bin_dir.exists(),
        "bin dir created: {:?}",
        w.bin_entries()
    );
    assert!(!w.home.exists(), "home created");

    // A release whose SHA256SUMS does not list the mint asset is refused the same way.
    fs::write(
        w.release.join("SHA256SUMS"),
        format!("{}  other.tar.gz\n", "e".repeat(64)),
    )
    .unwrap();
    let (code, _, err) = w.install(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("does not list"), "{err}");
    assert!(!w.bin_dir.exists() && !w.home.exists());

    // And so is a release with no SHA256SUMS at all.
    fs::remove_file(w.release.join("SHA256SUMS")).unwrap();
    let (code, _, err) = w.install(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("unverified"), "{err}");
    assert!(!w.bin_dir.exists() && !w.home.exists());
}

#[test]
fn a_binary_reporting_another_version_is_refused_and_leaves_nothing() {
    for line in [
        "maxplayer-mint 0.0.1 (0123456789abcdef0123456789abcdef01234567)".to_owned(),
        format!("maxplayer-mint {VERSION}-rc1 ({STAMP})"),
        format!("maxplayer {VERSION} ({STAMP})"),
    ] {
        let w = World::new();
        w.publish(&w.script(&line), None);
        let (code, out, err) = w.install(&[]);
        assert_eq!(code, 2, "{line}: {out}\n{err}");
        assert!(err.contains("the mint must match it"), "{err}");
        assert!(!w.target().exists(), "{line}: installed anyway");
        assert_eq!(w.bin_entries(), Vec::<String>::new(), "{line}: leftovers");
        assert!(!w.home.exists(), "{line}: home created");
    }
}

#[test]
fn a_binary_that_does_not_run_leaves_no_half_installed_file() {
    // A truncated/foreign payload: the checksum matches (the release vouches for it), but it does
    // not run. Nothing may appear at the target, not even briefly-written bytes.
    let w = World::new();
    w.publish(b"\x7fELF truncated", None);
    let (code, _, err) = w.install(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("did not run here"), "{err}");
    assert_eq!(w.bin_entries(), Vec::<String>::new());
    assert!(!w.home.exists());
}

#[test]
fn an_existing_binary_is_not_replaced_without_force() {
    let w = World::new();
    w.good_release();
    fs::create_dir_all(&w.bin_dir).unwrap();
    let old = w.script("maxplayer-mint 0.6.0 (0123456789abcdef0123456789abcdef01234567)");
    fs::write(w.target(), &old).unwrap();
    fs::set_permissions(w.target(), fs::Permissions::from_mode(0o755)).unwrap();

    let (code, out, err) = w.install(&[]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("not replacing it"), "{err}");
    assert!(err.contains("--force"), "{err}");
    assert!(err.contains("0.6.0"), "{err}");
    assert_eq!(fs::read(w.target()).unwrap(), old, "old binary changed");
    assert_eq!(w.bin_entries(), ["maxplayer-mint"]);
    assert!(!w.home.exists(), "refused install set up the home");

    // With --force it is replaced (atomically: one directory entry, the new contents).
    let (code, out, err) = w.install(&["--force"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        fs::read_to_string(w.target())
            .unwrap()
            .contains(&format!("maxplayer-mint {VERSION} ({STAMP})"))
    );
    assert_eq!(w.bin_entries(), ["maxplayer-mint"]);
}

#[test]
fn a_refused_force_install_keeps_the_old_binary_intact() {
    // --force with a bad download must not have touched the old binary.
    let w = World::new();
    w.publish(
        &w.script("maxplayer-mint 9.9.9 (0123456789abcdef0123456789abcdef01234567)"),
        None,
    );
    fs::create_dir_all(&w.bin_dir).unwrap();
    let old = w.script("maxplayer-mint 0.6.0 (0123456789abcdef0123456789abcdef01234567)");
    fs::write(w.target(), &old).unwrap();
    let (code, _, _) = w.install(&["--force"]);
    assert_eq!(code, 2);
    assert_eq!(fs::read(w.target()).unwrap(), old);
    assert_eq!(w.bin_entries(), ["maxplayer-mint"]);
}

#[test]
fn http_sources_are_refused_before_anything_is_contacted_or_created() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    std::thread::spawn(move || {
        loop {
            if let Ok((mut stream, _)) = listener.accept() {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    for from in [
        format!("http://127.0.0.1:{port}/release"),
        format!("http://localhost:{port}/release"),
        format!("http://[::1]:{port}/release"),
        format!("HTTP://127.0.0.1:{port}/release"),
    ] {
        let w = World::new();
        let (code, out, err) = run(maxplayer()
            .args(["mint", "install", "--from", &from])
            .arg("--home")
            .arg(&w.home)
            .arg("--bin-dir")
            .arg(&w.bin_dir));
        assert_eq!(code, 1, "{from}: {out}\n{err}");
        assert!(err.contains(HTTP_REFUSED), "{from}: {err}");
        assert!(!w.home.exists() && !w.bin_dir.exists(), "{from}: created");
    }
    let w = World::new();
    let (code, _, err) = run(maxplayer()
        .args(["mint", "install", "--from", "ftp://127.0.0.1/x"])
        .arg("--home")
        .arg(&w.home)
        .arg("--bin-dir")
        .arg(&w.bin_dir));
    assert_eq!(code, 1, "{err}");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "a refused source was contacted"
    );
}

#[test]
fn info_on_an_absent_home_creates_nothing() {
    let w = World::new();
    for json in [false, true] {
        let mut extra = vec![];
        if json {
            extra.push("--json");
        }
        let (code, out, err) = w.info(&extra);
        assert_eq!(code, 0, "{out}\n{err}");
        assert!(!w.home.exists(), "maxplayer mint created the home");
        assert!(!w.bin_dir.exists());
        if json {
            let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            assert_eq!(v["binary"]["installed"], false);
            assert_eq!(v["mint"]["exists"], false);
            assert_eq!(v["maxplayer_version"], VERSION);
        } else {
            assert!(out.contains("maxplayer-mint: not installed"), "{out}");
            assert!(out.contains("mint:      none at"), "{out}");
        }
    }
    // Through MAXPLAYER_HOME too (no --home), the way most users call it.
    let (code, out, err) = run(maxplayer()
        .arg("mint")
        .arg("--offline")
        .env("MAXPLAYER_HOME", &w.home)
        .env("PATH", "/usr/bin:/bin"));
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(!w.home.exists(), "MAXPLAYER_HOME was created");
}

#[test]
fn info_on_a_mint_home_reads_only_and_reports_key_version_running_and_link() {
    let w = World::new();
    w.good_release();
    let (code, out, err) = w.install(&[]);
    assert_eq!(code, 0, "{out}\n{err}");
    // A key left group-readable: a read must not "fix" (chmod) it, nor create the agent key,
    // config.toml or wallet/ beside the mint.
    fs::set_permissions(
        w.home.join("mint/nostr.key"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let before = snapshot(&w.home);

    let fake = FakeRegistration::start();
    let info = |extra: &[&str]| {
        let mut cmd = maxplayer();
        cmd.arg("mint")
            .arg("--home")
            .arg(&w.home)
            .arg("--bin-dir")
            .arg(&w.bin_dir)
            .args(extra)
            .env("MAXPLAYER_REGISTRATION_URL", fake.base_url());
        run(&mut cmd)
    };
    let (code, out, err) = info(&[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.contains(&format!("version {VERSION} (matches maxplayer)")),
        "{out}"
    );
    assert!(out.contains(&format!("npub:      {}", w.npub())), "{out}");
    assert!(
        out.contains(&format!("url:       nostr://{}", w.npub())),
        "{out}"
    );
    assert!(out.contains("running:   no"), "{out}");
    assert!(
        out.contains(&format!(
            "linked:    no (run `maxplayer account link --mint --home {}`)",
            w.home.display()
        )),
        "{out}"
    );
    assert_eq!(snapshot(&w.home), before, "maxplayer mint changed the home");

    // Linked, and served: `maxplayer-mint run` holds an exclusive flock on <home>/mint/.
    fake.link_existing(&w.mint_keys.public_key().to_hex(), "op@example.com");
    let held = fs::File::open(w.home.join("mint")).unwrap();
    held.try_lock().unwrap();
    let (code, out, err) = info(&["--json"]);
    assert_eq!(code, 0, "{out}\n{err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["linked"], true, "{out}");
    assert_eq!(v["mint"]["npub"], w.npub());
    assert_eq!(v["binary"]["version_matches"], true);
    if cfg!(target_os = "linux") {
        assert_eq!(v["mint"]["running"], true, "{out}");
        assert_eq!(v["mint"]["pid"], std::process::id(), "{out}");
    }
    drop(held);
    assert_eq!(snapshot(&w.home), before, "maxplayer mint changed the home");

    // --offline asks nobody.
    let calls = fake.requests().len();
    let (code, out, _) = info(&["--offline"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("linked:    unknown (not checked (--offline))"),
        "{out}"
    );
    assert_eq!(fake.requests().len(), calls);
}

#[test]
fn info_flags_a_binary_of_another_version() {
    let w = World::new();
    fs::create_dir_all(&w.bin_dir).unwrap();
    fs::write(
        w.target(),
        w.script("maxplayer-mint 0.6.0 (0123456789abcdef0123456789abcdef01234567)"),
    )
    .unwrap();
    fs::set_permissions(w.target(), fs::Permissions::from_mode(0o755)).unwrap();
    let (code, out, _) = w.info(&["--offline"]);
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!("version 0.6.0 (maxplayer is {VERSION}")),
        "{out}"
    );
    assert!(out.contains("mint install --force"), "{out}");
    assert!(!w.home.exists());
}
