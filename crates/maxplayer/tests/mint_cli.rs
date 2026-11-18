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

/// The binary with no inherited `MAXPLAYER_*` variables (the config layer refuses unknown names)
/// and no proxy. The latest-release check is pointed at a file that does not exist, so no test ever
/// reaches GitHub; a test that wants a reply sets `MAXPLAYER_RELEASES_LATEST_URL` itself.
fn maxplayer() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("MAXPLAYER_")
            || name.eq_ignore_ascii_case("https_proxy")
            || name.eq_ignore_ascii_case("http_proxy")
            || name.eq_ignore_ascii_case("all_proxy")
        {
            cmd.env_remove(key);
        }
    }
    cmd.env(
        "MAXPLAYER_RELEASES_LATEST_URL",
        "file:///nonexistent/maxplayer-test/releases-latest.json",
    );
    cmd.stdin(Stdio::null());
    cmd
}

const OLD_LINE: &str = "maxplayer-mint 0.6.0 (0123456789abcdef0123456789abcdef01234567 release)";

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
               echo seed > \"$MAXPLAYER_HOME/mint/seed\"; : > \"$MAXPLAYER_HOME/mint/mint.sqlite\"\n\
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
            &self.script(&format!("maxplayer-mint {VERSION} ({STAMP} release)")),
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

    fn upgrade(&self, extra: &[&str]) -> (i32, String, String) {
        run(maxplayer()
            .args(["mint", "upgrade", "--from", &self.from()])
            .arg("--home")
            .arg(&self.home)
            .arg("--bin-dir")
            .arg(&self.bin_dir)
            .args(extra))
    }

    fn uninstall(&self) -> (i32, String, String) {
        run(maxplayer()
            .args(["mint", "uninstall"])
            .arg("--home")
            .arg(&self.home)
            .arg("--bin-dir")
            .arg(&self.bin_dir)
            .env("PATH", "/usr/bin:/bin"))
    }

    /// An installed stand-in that reports `line` (written directly, not through install).
    fn put_installed(&self, line: &str) -> Vec<u8> {
        fs::create_dir_all(&self.bin_dir).unwrap();
        let bytes = self.script(line);
        fs::write(self.target(), &bytes).unwrap();
        fs::set_permissions(self.target(), fs::Permissions::from_mode(0o755)).unwrap();
        bytes
    }

    /// A mint home as `init` makes it, without installing anything.
    fn make_mint_home(&self) {
        let mint = self.home.join("mint");
        fs::create_dir_all(mint.join("issued")).unwrap();
        fs::write(
            mint.join("nostr.key"),
            self.mint_keys.secret_key().to_secret_hex(),
        )
        .unwrap();
        fs::write(mint.join("seed"), b"seed").unwrap();
        fs::write(mint.join("mint.sqlite"), b"db").unwrap();
    }

    /// A `releases/latest` reply naming `tag`, as a file:// URL.
    fn latest(&self, tag: &str) -> String {
        let path = self.release.parent().unwrap().join("latest.json");
        fs::write(&path, format!("{{\"tag_name\":\"{tag}\",\"draft\":false}}")).unwrap();
        format!("file://{}", path.display())
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
        out.contains(&format!(
            "Installed maxplayer-mint {VERSION} ({STAMP} release)"
        )),
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
        out.contains("it emails you a code, then run the same command with --code"),
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
        &w.script(&format!("maxplayer-mint {VERSION} ({STAMP} release)")),
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
        "maxplayer-mint 0.0.1 (0123456789abcdef0123456789abcdef01234567 release)".to_owned(),
        format!("maxplayer-mint {VERSION}-rc1 ({STAMP} release)"),
        format!("maxplayer {VERSION} ({STAMP} release)"),
        // Source builds: not what a release ships (review M1).
        format!("maxplayer-mint {VERSION}"),
        format!("maxplayer-mint {VERSION} ({STAMP}-dirty)"),
        format!("maxplayer-mint {VERSION} (unknown)"),
    ] {
        let w = World::new();
        w.publish(&w.script(&line), None);
        let (code, out, err) = w.install(&[]);
        assert_eq!(code, 2, "{line}: {out}\n{err}");
        assert!(err.contains("the mint must match this maxplayer"), "{err}");
        assert!(!w.target().exists(), "{line}: installed anyway");
        assert!(!w.bin_dir.exists(), "{line}: bin dir left behind");
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
    assert!(!w.bin_dir.exists(), "bin dir left behind");
    assert!(!w.home.exists());
}

#[test]
fn an_existing_binary_is_not_replaced_without_force() {
    let w = World::new();
    w.good_release();
    fs::create_dir_all(&w.bin_dir).unwrap();
    let old = w.script("maxplayer-mint 0.6.0 (0123456789abcdef0123456789abcdef01234567 release)");
    fs::write(w.target(), &old).unwrap();
    fs::set_permissions(w.target(), fs::Permissions::from_mode(0o755)).unwrap();

    let (code, out, err) = w.install(&[]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("not replacing it"), "{err}");
    // An older release is what `upgrade` is for: install points there, not at --force.
    assert!(err.contains("maxplayer mint upgrade --bin-dir"), "{err}");
    assert!(!err.contains("--force"), "{err}");
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
            .contains(&format!("maxplayer-mint {VERSION} ({STAMP} release)"))
    );
    assert_eq!(w.bin_entries(), ["maxplayer-mint"]);
}

#[test]
fn a_refused_force_install_keeps_the_old_binary_intact() {
    // --force with a bad download must not have touched the old binary.
    let w = World::new();
    w.publish(
        &w.script("maxplayer-mint 9.9.9 (0123456789abcdef0123456789abcdef01234567 release)"),
        None,
    );
    fs::create_dir_all(&w.bin_dir).unwrap();
    let old = w.script("maxplayer-mint 0.6.0 (0123456789abcdef0123456789abcdef01234567 release)");
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
        w.script("maxplayer-mint 0.6.0 (0123456789abcdef0123456789abcdef01234567 release)"),
    )
    .unwrap();
    fs::set_permissions(w.target(), fs::Permissions::from_mode(0o755)).unwrap();
    let (code, out, _) = w.info(&["--offline"]);
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!("version 0.6.0 (maxplayer is {VERSION}")),
        "{out}"
    );
    assert!(
        out.contains(&format!("mint upgrade --bin-dir {}", w.bin_dir.display())),
        "{out}"
    );
    assert!(!w.home.exists());

    // Anything that is not an older release still needs install --force.
    w.put_installed("maxplayer-mint 0.6.0");
    let (_, out, _) = w.info(&["--offline"]);
    assert!(out.contains("mint install --force --bin-dir"), "{out}");
}

#[test]
fn only_a_stamp_carrying_the_release_marker_installs() {
    // A clean source build prints the bare commit; only `(<sha> release)` (release.yml build-mint)
    // is a release (#1346 review 2).
    for stamp in [format!("({STAMP})"), format!("({STAMP} 2026-11-18)")] {
        let w = World::new();
        w.publish(
            &w.script(&format!("maxplayer-mint {VERSION} {stamp}")),
            None,
        );
        let (code, out, err) = w.install(&[]);
        assert_eq!(code, 2, "{stamp}: {out}\n{err}");
        assert!(!w.target().exists(), "{stamp}");
    }
    let w = World::new();
    w.good_release();
    let (code, out, err) = w.install(&[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(w.target().is_file());
}

#[test]
fn a_half_made_mint_home_is_refused_not_reported_as_set_up() {
    // An `init` that failed after creating <home>/mint/ (review M6): install must not print the
    // link step for a mint with no key, and must install nothing.
    for leave in ["empty", "key-only"] {
        let w = World::new();
        w.good_release();
        fs::create_dir_all(w.home.join("mint")).unwrap();
        if leave == "key-only" {
            fs::write(
                w.home.join("mint/nostr.key"),
                w.mint_keys.secret_key().to_secret_hex(),
            )
            .unwrap();
        }
        let (code, out, err) = w.install(&[]);
        assert_eq!(code, 2, "{leave}: {out}\n{err}");
        assert!(err.contains("is not a usable mint"), "{leave}: {err}");
        assert!(!out.contains("account link"), "{leave}: {out}");
        assert!(!w.bin_dir.exists(), "{leave}: installed anyway");
    }
}

#[test]
fn a_failed_init_says_the_binary_is_installed_and_how_to_recover() {
    let w = World::new();
    // init creates mint/ and then fails, like a database error after the key was written.
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n--version) echo 'maxplayer-mint {VERSION} ({STAMP} release)' ;;\n\
         init) mkdir \"$MAXPLAYER_HOME/mint\"; echo 'db error' >&2; exit 1 ;;\nesac\n"
    );
    w.publish(script.as_bytes(), None);
    let (code, out, err) = w.install(&[]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("the binary is installed"), "{err}");
    assert!(err.contains("move it aside"), "{err}");
    assert!(w.target().is_file());
    // A rerun refuses the half-made mint instead of reporting success.
    let (code, _, err) = w.install(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("is not a usable mint"), "{err}");
}

#[test]
fn force_never_runs_the_binary_it_replaces_and_reports_a_replaced_symlink() {
    let w = World::new();
    w.good_release();
    fs::create_dir_all(&w.bin_dir).unwrap();
    let marker = w.bin_dir.parent().unwrap().join("old-ran");
    let old = w.bin_dir.parent().unwrap().join("npm-maxplayer-mint");
    fs::write(
        &old,
        format!(
            "#!/bin/sh\ntouch '{}'\necho 'maxplayer-mint 0.6.0'\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&old, w.target()).unwrap();
    let (code, out, err) = w.install(&["--force"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        !marker.exists(),
        "--force ran the binary it replaced (review L3)"
    );
    assert!(out.contains("Replaced the symlink"), "{out}");
    assert!(
        !fs::symlink_metadata(w.target())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(old.exists(), "the link's target was touched");
}

#[test]
fn stages_left_by_an_interrupted_install_are_cleaned_up() {
    // A killed install leaves `.maxplayer-mint.install.<pid>.*` (review M3); the next run removes it.
    let w = World::new();
    w.good_release();
    fs::create_dir_all(&w.bin_dir).unwrap();
    let stale = w
        .bin_dir
        .join(format!(".maxplayer-mint.install.{}.0", i32::MAX));
    fs::write(&stale, b"half").unwrap();
    let (code, out, err) = w.install(&[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(!stale.exists());
    assert_eq!(w.bin_entries(), ["maxplayer-mint"]);
}

#[test]
fn an_installed_binary_is_not_reported_as_a_failure_when_the_dir_cannot_be_synced() {
    // Write+search but no read on the bin dir: the directory fsync fails after the file is in
    // place (review M5). That is a warning, not "could not install", and init still runs.
    let w = World::new();
    w.good_release();
    fs::create_dir_all(&w.bin_dir).unwrap();
    fs::set_permissions(&w.bin_dir, fs::Permissions::from_mode(0o300)).unwrap();
    let (code, out, err) = w.install(&[]);
    fs::set_permissions(&w.bin_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        err.contains("warning: installed, but could not sync"),
        "{err}"
    );
    assert!(w.target().is_file());
    assert!(w.home.join("mint/nostr.key").is_file());
}

#[test]
fn bad_arguments_are_usage_errors_before_anything_happens() {
    let w = World::new();
    for (args, want) in [
        (vec!["--bin-dir", ""], "--bin-dir must not be empty"),
        (
            vec!["--from", "https://u:p@example.com/r"],
            "no credentials",
        ),
        (
            vec!["--from", "https://example.com/r?sig=1"],
            "no credentials",
        ),
    ] {
        let (code, _, err) = run(maxplayer()
            .args(["mint", "install"])
            .arg("--home")
            .arg(&w.home)
            .args(&args));
        assert_eq!(code, 1, "{args:?}: {err}");
        assert!(err.contains(want), "{args:?}: {err}");
        assert!(!w.home.exists());
    }
}

// ── maxplayer mint upgrade ───────────────────────────────────────────────────────────────────

#[test]
fn upgrade_replaces_an_older_release_and_keeps_the_mint_home() {
    let w = World::new();
    w.good_release();
    w.put_installed(OLD_LINE);
    w.make_mint_home();
    let before = snapshot(&w.home);
    let (code, out, err) = w.upgrade(&[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("Verified sha256"), "{out}");
    assert!(
        out.contains(&format!(
            "from maxplayer-mint 0.6.0 to maxplayer-mint {VERSION} ({STAMP} release)"
        )),
        "{out}"
    );
    assert!(
        fs::read_to_string(w.target())
            .unwrap()
            .contains(&format!("maxplayer-mint {VERSION} ({STAMP} release)"))
    );
    assert_eq!(w.bin_entries(), ["maxplayer-mint"], "no staging file left");
    // <home>/mint/ untouched, and `init` never ran (it would have failed on the existing dir).
    assert_eq!(snapshot(&w.home), before, "upgrade changed the mint home");
    assert!(!out.contains("Created"), "{out}");
    assert!(!out.contains("is serving"), "{out}");
}

#[test]
fn upgrade_on_an_absent_home_creates_nothing_there() {
    let w = World::new();
    w.good_release();
    w.put_installed(OLD_LINE);
    let (code, out, err) = w.upgrade(&[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(!w.home.exists(), "upgrade created the home (init ran?)");
}

#[test]
fn upgrade_at_this_version_is_a_no_op() {
    let w = World::new();
    w.good_release();
    let bytes = w.put_installed(&format!("maxplayer-mint {VERSION} ({STAMP} release)"));
    // Even a broken release would not be touched: nothing is downloaded.
    fs::remove_file(w.release.join("SHA256SUMS")).unwrap();
    let (code, out, err) = w.upgrade(&[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("nothing to upgrade"), "{out}");
    assert!(!out.contains("Downloading"), "{out}");
    assert_eq!(fs::read(w.target()).unwrap(), bytes);
}

#[test]
fn upgrade_refuses_newer_source_built_and_unknown_binaries() {
    for (line, want) in [
        (
            "maxplayer-mint 999.0.0 (0123456789abcdef0123456789abcdef01234567 release)".to_owned(),
            "newer than this maxplayer",
        ),
        ("maxplayer-mint 0.6.0".to_owned(), "a source build"),
        // A clean checkout's build: the bare commit, no release marker (#1346 review 2).
        (format!("maxplayer-mint 0.6.0 ({STAMP})"), "a source build"),
        (
            format!("maxplayer-mint {VERSION} ({STAMP}-dirty)"),
            "a source build",
        ),
        (
            "something else 1.2.3".to_owned(),
            "not a maxplayer-mint release line",
        ),
    ] {
        let w = World::new();
        w.good_release();
        let bytes = w.put_installed(&line);
        let (code, out, err) = w.upgrade(&[]);
        assert_eq!(code, 2, "{line}: {out}\n{err}");
        assert!(err.contains(want), "{line}: {err}");
        assert!(
            err.contains("maxplayer mint install --force --bin-dir"),
            "{line}: {err}"
        );
        assert!(!out.contains("Downloading"), "{line}: {out}");
        assert_eq!(
            fs::read(w.target()).unwrap(),
            bytes,
            "{line}: replaced anyway"
        );
        assert_eq!(w.bin_entries(), ["maxplayer-mint"]);
    }
    // A binary that does not run at all is refused the same way.
    let w = World::new();
    w.good_release();
    fs::create_dir_all(&w.bin_dir).unwrap();
    fs::write(w.target(), b"\x7fELF nope").unwrap();
    fs::set_permissions(w.target(), fs::Permissions::from_mode(0o755)).unwrap();
    let (code, _, err) = w.upgrade(&[]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("install --force"), "{err}");
    assert_eq!(fs::read(w.target()).unwrap(), b"\x7fELF nope");
    // --force is not an upgrade flag.
    let (code, _, err) = w.upgrade(&["--force"]);
    assert_eq!(code, 1, "{err}");
}

#[test]
fn upgrade_with_nothing_installed_points_at_install() {
    let w = World::new();
    w.good_release();
    let (code, _, err) = w.upgrade(&[]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("maxplayer mint install --bin-dir"), "{err}");
    assert!(!w.bin_dir.exists());
}

#[test]
fn a_refused_upgrade_keeps_the_old_binary() {
    let w = World::new();
    w.publish(
        &w.script(&format!("maxplayer-mint {VERSION} ({STAMP} release)")),
        Some(&"d".repeat(64)),
    );
    let old = w.put_installed(OLD_LINE);
    let (code, _, err) = w.upgrade(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("does not match the release"), "{err}");
    assert_eq!(fs::read(w.target()).unwrap(), old);
    assert_eq!(w.bin_entries(), ["maxplayer-mint"]);
}

#[test]
fn upgrading_under_a_running_mint_says_it_keeps_the_old_version_until_restarted() {
    let w = World::new();
    w.good_release();
    w.put_installed(OLD_LINE);
    w.make_mint_home();
    let held = fs::File::open(w.home.join("mint")).unwrap();
    held.try_lock().unwrap();
    let (code, out, err) = w.upgrade(&[]);
    drop(held);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.contains("keeps running 0.6.0 until it is restarted"),
        "{out}"
    );
    assert!(
        out.contains("systemctl --user restart maxplayer-mint"),
        "{out}"
    );
}

// ── maxplayer mint uninstall ─────────────────────────────────────────────────────────────────

#[test]
fn uninstall_removes_the_binary_and_never_the_mint_home() {
    let w = World::new();
    w.put_installed(&format!("maxplayer-mint {VERSION} ({STAMP} release)"));
    w.make_mint_home();
    let before = snapshot(&w.home);
    let (code, out, err) = w.uninstall();
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(!w.target().exists());
    assert_eq!(snapshot(&w.home), before, "uninstall touched <home>/mint/");
    assert!(out.contains("nostr.key"), "{out}");
    assert!(out.contains("seed"), "{out}");
    assert!(out.contains("mint.sqlite"), "{out}");
    assert!(out.contains("destroys the mint"), "{out}");
    assert!(out.contains("back it up first"), "{out}");

    // Nothing left to uninstall: said, exit 0, still nothing touched.
    let (code, out, err) = w.uninstall();
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("nothing to uninstall"), "{out}");
    assert_eq!(snapshot(&w.home), before);
}

#[test]
fn uninstall_refuses_while_the_mint_is_serving() {
    let w = World::new();
    let bytes = w.put_installed(&format!("maxplayer-mint {VERSION} ({STAMP} release)"));
    w.make_mint_home();
    let held = fs::File::open(w.home.join("mint")).unwrap();
    held.try_lock().unwrap();
    let (code, out, err) = w.uninstall();
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("is serving"), "{err}");
    assert!(
        err.contains("systemctl --user disable --now maxplayer-mint"),
        "{err}"
    );
    assert!(err.contains("kill -INT"), "{err}");
    assert_eq!(
        fs::read(w.target()).unwrap(),
        bytes,
        "removed while serving"
    );
    drop(held);
    let (code, _, err) = w.uninstall();
    assert_eq!(code, 0, "{err}");
}

#[test]
fn uninstall_removes_a_symlink_not_its_target() {
    let w = World::new();
    fs::create_dir_all(&w.bin_dir).unwrap();
    let real = w.bin_dir.parent().unwrap().join("npm-maxplayer-mint");
    fs::write(&real, w.script(OLD_LINE)).unwrap();
    std::os::unix::fs::symlink(&real, w.target()).unwrap();
    let (code, out, err) = w.uninstall();
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        fs::symlink_metadata(w.target()).is_err(),
        "link still there"
    );
    assert!(real.is_file(), "the link's target was removed");
    assert!(out.contains("Removed the symlink"), "{out}");
    assert!(out.contains(&real.display().to_string()), "{out}");
}

#[test]
fn uninstall_refuses_what_is_not_a_file_or_symlink() {
    let w = World::new();
    fs::create_dir_all(w.target()).unwrap();
    let (code, _, err) = w.uninstall();
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("not a regular file or a symlink"), "{err}");
    assert!(w.target().is_dir());
}

// ── the newer-version notice in `maxplayer mint` ─────────────────────────────────────────────

#[test]
fn status_says_when_the_installed_mint_is_older() {
    let w = World::new();
    w.put_installed(OLD_LINE);
    let (code, out, err) = w.info(&["--offline"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.contains(&format!(
            "update:    maxplayer-mint 0.6.0 is older than this maxplayer ({VERSION}): run `maxplayer mint upgrade --bin-dir {}",
            w.bin_dir.display()
        )),
        "{out}"
    );
    assert!(out.contains("latest:    not checked (--offline)"), "{out}");
    let (_, out, _) = w.info(&["--offline", "--json"]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["update"]["relation"], "older", "{out}");
    assert_eq!(v["update"]["mint_upgrade_available"], true, "{out}");
    assert_eq!(v["update"]["latest_checked"], false, "{out}");
}

#[test]
fn status_says_when_a_newer_maxplayer_is_released() {
    let w = World::new();
    w.put_installed(&format!("maxplayer-mint {VERSION} ({STAMP} release)"));
    let latest = w.latest("v999.0.0");
    let info = |extra: &[&str]| {
        run(maxplayer()
            .arg("mint")
            .arg("--home")
            .arg(&w.home)
            .arg("--bin-dir")
            .arg(&w.bin_dir)
            .args(extra)
            .env("PATH", "/usr/bin:/bin")
            .env("MAXPLAYER_RELEASES_LATEST_URL", &latest))
    };
    let (code, out, err) = info(&[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.contains(&format!("latest:    maxplayer 999.0.0 (this is {VERSION})")),
        "{out}"
    );
    assert!(
        out.contains("update:    maxplayer 999.0.0 is released"),
        "{out}"
    );
    assert!(
        out.contains("then run `maxplayer mint upgrade --bin-dir"),
        "{out}"
    );
    let (_, out, _) = info(&["--json"]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["update"]["latest_maxplayer"], "999.0.0", "{out}");
    assert_eq!(v["update"]["maxplayer_update_available"], true, "{out}");
    assert_eq!(v["update"]["mint_upgrade_available"], false, "{out}");
    assert!(!w.home.exists(), "status created the home");

    // The same release as this one: nothing to say.
    let same = w.latest(&format!("v{VERSION}"));
    let (_, out, _) = run(maxplayer()
        .arg("mint")
        .arg("--home")
        .arg(&w.home)
        .arg("--bin-dir")
        .arg(&w.bin_dir)
        .env("PATH", "/usr/bin:/bin")
        .env("MAXPLAYER_RELEASES_LATEST_URL", &same));
    assert!(!out.contains("update:"), "{out}");
    assert!(out.contains("up to date"), "{out}");
}

#[test]
fn a_failed_latest_check_is_shown_not_an_error_and_offline_asks_nobody() {
    // A loopback listener that counts connections and never speaks TLS: the check fails.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    std::thread::spawn(move || {
        loop {
            if let Ok((stream, _)) = listener.accept() {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let url = format!("https://127.0.0.1:{port}/releases/latest");
    let w = World::new();
    w.put_installed(&format!("maxplayer-mint {VERSION} ({STAMP} release)"));
    let info = |extra: &[&str]| {
        run(maxplayer()
            .arg("mint")
            .arg("--home")
            .arg(&w.home)
            .arg("--bin-dir")
            .arg(&w.bin_dir)
            .args(extra)
            .env("PATH", "/usr/bin:/bin")
            .env("MAXPLAYER_RELEASES_LATEST_URL", &url))
    };

    let (code, out, err) = info(&["--offline"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("latest:    not checked (--offline)"), "{out}");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "--offline made a request"
    );

    let (code, out, err) = info(&[]);
    assert_eq!(code, 0, "a failed check is not an error: {out}\n{err}");
    assert!(out.contains("latest:    could not check"), "{out}");
    assert!(!out.contains("update:"), "{out}");
    assert!(
        connections.load(Ordering::SeqCst) >= 1,
        "the check never ran"
    );
    let (code, out, _) = info(&["--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["update"]["latest_checked"], false, "{out}");
    assert!(
        v["update"]["latest_error"]
            .as_str()
            .unwrap()
            .starts_with("could not check"),
        "{out}"
    );
    assert!(!w.home.exists(), "status created the home");
}

// ── review r1 (#1346) ────────────────────────────────────────────────────────────────────────

#[test]
fn upgrade_refuses_when_the_binary_changes_during_the_download() {
    // Review 3: the asset is a FIFO, so the download blocks until fed; meanwhile a NEWER mint is
    // put at the target. The upgrade must not overwrite it on the strength of the old probe.
    let w = World::new();
    let archive = w.publish(
        &w.script(&format!("maxplayer-mint {VERSION} ({STAMP} release)")),
        None,
    );
    let asset = w.release.join(asset());
    fs::remove_file(&asset).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(&asset)
            .status()
            .unwrap()
            .success()
    );
    w.put_installed(OLD_LINE);
    let child = maxplayer()
        .args(["mint", "upgrade", "--from", &w.from()])
        .arg("--home")
        .arg(&w.home)
        .arg("--bin-dir")
        .arg(&w.bin_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    let newer = w.script(&format!("maxplayer-mint 999.0.0 ({STAMP} release)"));
    let tmp = w.bin_dir.join("swap");
    fs::write(&tmp, &newer).unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&tmp, w.target()).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&asset)
        .unwrap()
        .write_all(&archive)
        .unwrap();
    let Output { status, stderr, .. } = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&stderr);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(err.contains("changed while"), "{err}");
    assert_eq!(
        fs::read(w.target()).unwrap(),
        newer,
        "the newer binary was overwritten"
    );
    assert_eq!(w.bin_entries(), ["maxplayer-mint"], "staging file left");
}

#[test]
fn uninstall_refuses_when_it_cannot_tell_whether_the_mint_is_serving() {
    // Review 5: an unsearchable home used to skip the running-mint guard.
    if unsafe { libc::geteuid() } == 0 {
        return; // root reads through any mode
    }
    let w = World::new();
    let bytes = w.put_installed(&format!("maxplayer-mint {VERSION} ({STAMP} release)"));
    w.make_mint_home();
    fs::set_permissions(&w.home, fs::Permissions::from_mode(0o000)).unwrap();
    let (code, out, err) = w.uninstall();
    fs::set_permissions(&w.home, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("could not tell whether"), "{err}");
    assert_eq!(fs::read(w.target()).unwrap(), bytes);
}

#[test]
fn upgrade_and_uninstall_point_at_the_mint_on_path_instead_of_reporting_nothing() {
    // Review 6: status finds the mint on PATH; without --bin-dir these default to ~/.local/bin.
    let w = World::new();
    w.good_release();
    let user_home = w.home.parent().unwrap().join("userhome");
    fs::create_dir_all(&user_home).unwrap();
    let pathbin = w.home.parent().unwrap().join("pathbin");
    fs::create_dir_all(&pathbin).unwrap();
    let bytes = w.script(OLD_LINE);
    fs::write(pathbin.join("maxplayer-mint"), &bytes).unwrap();
    fs::set_permissions(
        pathbin.join("maxplayer-mint"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let path = format!("{}:/usr/bin:/bin", pathbin.display());
    for sub in ["upgrade", "uninstall"] {
        let (code, out, err) = run(maxplayer()
            .args(["mint", sub, "--from", &w.from()])
            .arg("--home")
            .arg(&w.home)
            .env("HOME", &user_home)
            .env("PATH", &path));
        let (code, err) = if sub == "uninstall" && code == 1 {
            // uninstall takes no --from
            let (c, o, e) = run(maxplayer()
                .args(["mint", sub])
                .arg("--home")
                .arg(&w.home)
                .env("HOME", &user_home)
                .env("PATH", &path));
            let _ = o;
            (c, e)
        } else {
            let _ = out;
            (code, err)
        };
        assert_eq!(code, 2, "{sub}: {err}");
        assert!(
            err.contains(&format!(
                "rerun `maxplayer mint {sub} --bin-dir {}",
                pathbin.display()
            )),
            "{sub}: {err}"
        );
        assert_eq!(
            fs::read(pathbin.join("maxplayer-mint")).unwrap(),
            bytes,
            "{sub}"
        );
        assert!(
            !user_home.join(".local/bin").exists(),
            "{sub}: installed a second copy"
        );
    }
}

#[test]
fn uninstall_warns_about_a_symlinked_mint_dir_too() {
    // Review 10: <home>/mint -> elsewhere is still the mint.
    let w = World::new();
    w.put_installed(&format!("maxplayer-mint {VERSION} ({STAMP} release)"));
    let real = w.home.parent().unwrap().join("data-mint");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("seed"), b"s").unwrap();
    fs::create_dir_all(&w.home).unwrap();
    std::os::unix::fs::symlink(&real, w.home.join("mint")).unwrap();
    let (code, out, err) = w.uninstall();
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("destroys the mint"), "{out}");
    assert!(out.contains("seed"), "{out}");
    assert!(
        out.contains(&format!(
            "maxplayer mint install --bin-dir {}",
            w.bin_dir.display()
        )),
        "the reinstall hint keeps --bin-dir/--home: {out}"
    );
    assert!(real.join("seed").exists());
}

#[test]
fn status_never_advises_force_installing_an_older_mint_over_a_newer_one() {
    // Review 9a.
    let w = World::new();
    w.put_installed(&format!("maxplayer-mint 999.0.0 ({STAMP} release)"));
    let (code, out, _) = w.info(&["--offline"]);
    assert_eq!(code, 0);
    assert!(!out.contains("install --force"), "{out}");
    assert!(out.contains("update maxplayer"), "{out}");
}
