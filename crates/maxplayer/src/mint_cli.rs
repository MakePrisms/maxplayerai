//! `maxplayer mint` (read-only info) and `maxplayer mint install` (fetch the released
//! `maxplayer-mint` for this platform and set up `<home>/mint/`).
//!
//! Mint onboarding is three pieces (Petar, #merge-binaries, 2026-11-16):
//!
//! 1. `maxplayer mint` says whether a mint binary is installed, its version, whether this home has
//!    a mint (`<home>/mint/`), its key, whether `maxplayer-mint run` is serving it, and whether the
//!    mint key is linked to an account. It CREATES NOTHING: no home, no `config.toml`, no agent key,
//!    no `wallet/`, and no chmod of the mint key (the bootstrap rule: a read never mints an identity).
//! 2. `maxplayer mint install` downloads the release asset `maxplayer-mint-<v>-<platform>.tar.gz`
//!    and the release's `SHA256SUMS`, exactly the way `install.sh` installs `maxplayer` itself:
//!    same release, same naming, same checksum file. `<v>` is THIS binary's version — the mint ships
//!    in lockstep with `maxplayer` — and the unpacked binary must report it before it is installed.
//!    Installing is atomic (staging file in the target directory, then link/rename), refuses to
//!    replace an existing binary without `--force`, and finishes by running the installed
//!    `maxplayer-mint init`, which owns the `<home>/mint/` layout (crates/maxplayer-mint/src/home.rs).
//!    An existing `<home>/mint/` is never touched.
//! 3. The user links the mint with `maxplayer account link --mint`; `install` prints that step.
//!
//! The download source is `https://` (the GitHub release) or `file://` (an offline mirror, and the
//! test fixtures). Plain `http://` is refused before anything is contacted or created, on loopback
//! too: the user build talks to no plain-HTTP money endpoint (scripts/check-user-build-mint-fence.sh),
//! and a redirect away from https is refused the same way.

use std::io::Write;
use std::path::PathBuf;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
#[cfg(feature = "wallet")]
const RUNTIME_ERROR: i32 = 2;

/// The executable the mint release ships, and the stem of its release asset.
pub const MINT_BIN: &str = "maxplayer-mint";
/// Where a release's assets are downloaded from; `/v<version>` is appended (as in `install.sh`).
pub const RELEASE_DOWNLOAD_BASE: &str =
    "https://github.com/MakePrisms/maxplayerai/releases/download";
/// The refusal for a plain-HTTP `--from`. Scripts and the fence check key on it.
#[cfg(feature = "wallet")]
pub const HTTP_REFUSED: &str =
    "http:// download sources are not supported; use https:// (or file:// for a local mirror)";

fn write_usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n\
         \x20 maxplayer mint [--home <dir>] [--bin-dir <dir>] [--json] [--offline]\n\
         \x20 maxplayer mint install [--home <dir>] [--bin-dir <dir>] [--name <name>] [--force] [--from <url>]\n\
         \n\
         `maxplayer mint` shows this home's credit mint: whether `{MINT_BIN}` is installed and its\n\
         version, the mint key (npub and nostr:// URL), whether `{MINT_BIN} run` is serving it, and\n\
         whether the mint key is linked to an account (asks the registration service; --offline\n\
         skips that). It only reads: it creates no home, key or file.\n\
         \n\
         `maxplayer mint install` downloads the released `{MINT_BIN}` for this platform at THIS\n\
         maxplayer's version ({version}), checks it against the release's SHA256SUMS and the version\n\
         it reports, installs it atomically into --bin-dir (default ~/.local/bin), then runs\n\
         `{MINT_BIN} init` to create <home>/mint/ (an existing <home>/mint/ is kept as it is).\n\
         Then link the mint to your account: maxplayer account link --mint\n\
         --force: replace an existing {MINT_BIN} in --bin-dir (refused otherwise).\n\
         --name: the mint name wallets show (passed to `{MINT_BIN} init --name`).\n\
         --from: download from this release directory instead of\n\
         \x20        {RELEASE_DOWNLOAD_BASE}/v{version} (https:// or file://).\n\
         <home> is --home, else $MAXPLAYER_HOME, else ~/.maxplayer.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error (the message says whether the binary\n\
         was installed: a refused download installs nothing; a failed `init` leaves it installed)",
        version = env!("CARGO_PKG_VERSION"),
    );
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Opts {
    home: Option<PathBuf>,
    bin_dir: Option<PathBuf>,
    json: bool,
    offline: bool,
    force: bool,
    name: Option<String>,
    from: Option<String>,
}

const INFO_FLAGS: &[&str] = &["--home", "--bin-dir", "--json", "--offline"];
const INSTALL_FLAGS: &[&str] = &["--home", "--bin-dir", "--force", "--name", "--from"];

fn parse(args: &[String], allowed: &[&str]) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if !allowed.contains(&flag) {
            return Err(format!("unknown argument {flag}"));
        }
        let mut value = || {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag {
            "--home" => opts.home = Some(PathBuf::from(value()?)),
            "--bin-dir" => opts.bin_dir = Some(PathBuf::from(value()?)),
            "--name" => opts.name = Some(value()?),
            "--from" => opts.from = Some(value()?),
            "--json" => opts.json = true,
            "--offline" => opts.offline = true,
            "--force" => opts.force = true,
            _ => return Err(format!("unknown argument {flag}")),
        }
        index += 1;
    }
    Ok(opts)
}

/// Entry from `cli::run` for `maxplayer mint ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` prints usage to STDOUT and exits 0 before any parse, home or network.
    if crate::cli::is_help_request(args) {
        write_usage(out);
        return SUCCESS;
    }
    let (install, rest) = match args.first().map(String::as_str) {
        Some("install") => (true, &args[1..]),
        _ => (false, args),
    };
    let opts = match parse(rest, if install { INSTALL_FLAGS } else { INFO_FLAGS }) {
        Ok(opts) => opts,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            write_usage(err);
            return USAGE_ERROR;
        }
    };
    #[cfg(not(feature = "wallet"))]
    {
        let _ = (opts, install, out);
        let _ = writeln!(
            err,
            "maxplayer mint requires the wallet feature (rebuild with default features)"
        );
        USAGE_ERROR
    }
    #[cfg(feature = "wallet")]
    {
        if install {
            wallet::install(&opts, out, err)
        } else {
            wallet::info(&opts, out, err)
        }
    }
}

#[cfg(feature = "wallet")]
mod wallet {
    use super::*;
    use std::fs;
    use std::io::{self, BufRead, Read};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use maxplayer_core::home;
    use maxplayer_core::registration::RegistrationClient;
    use nostr_sdk::prelude::{Keys, ToBech32};
    use sha2::{Digest, Sha256};

    /// The version this `maxplayer` is; the mint it installs must report the same.
    pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");
    /// Upper bound on the downloaded archive (the mint is ~20 MB) and on `SHA256SUMS`.
    const MAX_ARCHIVE_BYTES: u64 = 512 << 20;
    const MAX_SUMS_BYTES: u64 = 1 << 20;
    /// How long a `--version` probe of a downloaded or installed binary may take.
    const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
    /// The registration status check `maxplayer mint` makes (never blocks the info for long).
    const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
    /// Staging files in the bin dir: `.maxplayer-mint.install.<pid>.<suffix>`.
    pub(super) const STAGE_PREFIX: &str = ".maxplayer-mint.install.";
    /// A staging file this old is left over from an interrupted install even if its pid was reused.
    const STALE_STAGE_AGE: Duration = Duration::from_secs(3600);

    // ── shared pieces ────────────────────────────────────────────────────────────────────────

    /// The release platform name for this build (`install.sh`'s `detect_platform`, at compile
    /// time: the mint is installed for the platform this `maxplayer` was built for).
    pub(super) fn release_platform() -> Option<&'static str> {
        platform_for(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub(super) fn platform_for(os: &str, arch: &str) -> Option<&'static str> {
        match (os, arch) {
            ("linux", "x86_64") => Some("linux-x64"),
            ("linux", "aarch64") => Some("linux-arm64"),
            ("macos", "aarch64") => Some("darwin-arm64"),
            ("macos", "x86_64") => Some("darwin-x64"),
            _ => None,
        }
    }

    /// `maxplayer-mint-<version>-<platform>.tar.gz`, as `scripts/package-release-asset.sh` names it.
    pub(super) fn asset_name(version: &str, platform: &str) -> String {
        format!("{MINT_BIN}-{version}-{platform}.tar.gz")
    }

    /// The archive member holding the executable: `<asset stem>/maxplayer-mint`.
    pub(super) fn archive_member(version: &str, platform: &str) -> String {
        format!("{MINT_BIN}-{version}-{platform}/{MINT_BIN}")
    }

    /// Exactly a RELEASE line for `version`: `maxplayer-mint <version> (<40-hex commit>[ <text>])`,
    /// where `<text>` holds no `)`. This is the grammar `scripts/verify-release-version.sh` holds
    /// every released artifact to (`STAMP_RE='^\(([0-9a-f]{40})( [^)]*)?\)$'`); keep the two in
    /// step. A bare `maxplayer-mint <version>`, or a `-dirty`/`unknown` stamp, is a source build.
    pub(super) fn is_release_line(line: &str, version: &str) -> bool {
        let Some(stamp) = line.strip_prefix(&format!("{MINT_BIN} {version} ")) else {
            return false;
        };
        let Some(inner) = stamp.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
            return false;
        };
        let bytes = inner.as_bytes();
        if bytes.len() < 40
            || !bytes[..40]
                .iter()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return false;
        }
        let rest = &inner[40..];
        rest.is_empty() || (rest.starts_with(' ') && !rest.contains(')'))
    }

    /// The version field of a `maxplayer-mint <version> ...` line.
    pub(super) fn reported_version(line: &str) -> Option<&str> {
        line.strip_prefix(&format!("{MINT_BIN} "))?
            .split_whitespace()
            .next()
    }

    /// The digest `SHA256SUMS` lists for exactly `asset` (a `*` binary-mode prefix is accepted).
    /// Refuses a missing entry, a malformed digest and conflicting duplicate entries.
    pub(super) fn expected_sha256(sums: &str, asset: &str) -> Result<String, String> {
        let mut found: Option<String> = None;
        for line in sums.lines() {
            let mut parts = line.split_whitespace();
            let (Some(digest), Some(name), None) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if name.strip_prefix('*').unwrap_or(name) != asset {
                continue;
            }
            let digest = digest.to_ascii_lowercase();
            if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!(
                    "the SHA256SUMS entry for {asset} is not a sha256 digest; refusing to install"
                ));
            }
            match &found {
                Some(previous) if *previous != digest => {
                    return Err(format!(
                        "SHA256SUMS lists {asset} twice with different digests; refusing to install"
                    ));
                }
                _ => found = Some(digest),
            }
        }
        found.ok_or_else(|| {
            format!(
                "the release's SHA256SUMS does not list {asset}; refusing to install an artifact \
                 the release does not vouch for"
            )
        })
    }

    pub(super) fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// The executable `member` out of a `.tar.gz`, read into memory (nothing is unpacked to disk,
    /// so no archive path can escape anywhere). It must be a regular file and appear once.
    pub(super) fn extract_member(archive: &[u8], member: &str) -> Result<Vec<u8>, String> {
        let bad = |e: io::Error| format!("could not read the archive: {e}");
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        let mut found: Option<Vec<u8>> = None;
        for entry in tar.entries().map_err(bad)? {
            let mut entry = entry.map_err(bad)?;
            if entry.path().map_err(bad)? != Path::new(member) {
                continue;
            }
            if found.is_some() {
                return Err(format!(
                    "the archive holds {member} more than once; refusing it"
                ));
            }
            if entry.header().entry_type() != tar::EntryType::Regular {
                return Err(format!("{member} in the archive is not a regular file"));
            }
            let mut bytes = Vec::new();
            entry
                .by_ref()
                .take(MAX_ARCHIVE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(bad)?;
            found = Some(bytes);
        }
        found.ok_or_else(|| {
            format!(
                "the archive does not contain {member}; this is not the asset layout maxplayer \
                 expects"
            )
        })
    }

    /// Where the release is fetched from.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Source {
        Https(String),
        File(PathBuf),
    }

    impl Source {
        /// `https://` or `file://` naming a directory, nothing else: `http://` gets
        /// [`HTTP_REFUSED`]; credentials, a query or a fragment are refused (they would be glued
        /// in front of the asset name and printed).
        pub(super) fn parse(base: &str) -> Result<Self, String> {
            let trimmed = base.trim();
            let url = reqwest::Url::parse(trimmed)
                .map_err(|_| format!("--from {base:?} is not a URL"))?;
            match url.scheme() {
                "https" | "file" => {}
                "http" => return Err(HTTP_REFUSED.to_owned()),
                other => {
                    return Err(format!(
                        "--from must be an https:// or file:// URL (got {other}://)"
                    ));
                }
            }
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(
                    "--from must name a release directory with no credentials, query or fragment"
                        .to_owned(),
                );
            }
            if url.scheme() == "https" {
                Ok(Self::Https(trimmed.trim_end_matches('/').to_owned()))
            } else {
                url.to_file_path()
                    .map(Self::File)
                    .map_err(|()| format!("--from {base:?} is not a local file:// path"))
            }
        }

        fn describe(&self, name: &str) -> String {
            match self {
                Self::Https(base) => format!("{base}/{name}"),
                Self::File(dir) => dir.join(name).display().to_string(),
            }
        }

        fn fetch(&self, name: &str, limit: u64) -> Result<Vec<u8>, String> {
            let where_ = self.describe(name);
            match self {
                Self::File(dir) => {
                    let mut bytes = Vec::new();
                    fs::File::open(dir.join(name))
                        .and_then(|f| f.take(limit + 1).read_to_end(&mut bytes))
                        .map_err(|e| format!("could not read {where_}: {e}"))?;
                    if bytes.len() as u64 > limit {
                        return Err(format!(
                            "{where_} is larger than {limit} bytes; refusing it"
                        ));
                    }
                    Ok(bytes)
                }
                Self::Https(_) => fetch_https(&http_client(&[], false)?, &where_, limit),
            }
        }
    }

    /// Why a redirect to a URL with `scheme`, after `hops` earlier redirects, is refused; `None`
    /// follows it. GitHub answers a release download with a redirect to its object store, so
    /// redirects are followed, but only while they stay on https (the http fence).
    pub(super) fn redirect_refusal(scheme: &str, hops: usize) -> Option<&'static str> {
        if scheme != "https" {
            Some("redirect away from https refused")
        } else if hops >= 10 {
            Some("too many redirects")
        } else {
            None
        }
    }

    /// The download client. `roots` and `loopback` exist for the TLS test fixture only.
    pub(super) fn http_client(
        roots: &[reqwest::Certificate],
        loopback: bool,
    ) -> Result<reqwest::blocking::Client, String> {
        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::custom(
                |attempt| match redirect_refusal(attempt.url().scheme(), attempt.previous().len()) {
                    Some(why) => attempt.error(why),
                    None => attempt.follow(),
                },
            ))
            .user_agent(concat!("maxplayer/", env!("CARGO_PKG_VERSION")));
        for root in roots {
            builder = builder.add_root_certificate(root.clone());
        }
        if loopback {
            builder = builder.no_proxy();
        }
        builder.build().map_err(|e| format!("http client: {e}"))
    }

    pub(super) fn fetch_https(
        client: &reqwest::blocking::Client,
        url: &str,
        limit: u64,
    ) -> Result<Vec<u8>, String> {
        let response = client
            .get(url)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("could not download {url}: {e}"))?;
        let mut bytes = Vec::new();
        response
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("could not download {url}: {e}"))?;
        if bytes.len() as u64 > limit {
            return Err(format!("{url} is larger than {limit} bytes; refusing it"));
        }
        Ok(bytes)
    }

    fn resolve_root(opts: &Opts) -> Result<PathBuf, String> {
        match &opts.home {
            Some(path) => Ok(path.clone()),
            None => home::default_home_dir().map_err(|e| e.to_string()),
        }
    }

    fn default_bin_dir() -> Result<PathBuf, String> {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".local").join("bin"))
            .ok_or_else(|| "HOME is not set, so there is no default --bin-dir; pass one".to_owned())
    }

    /// `--bin-dir` (else ~/.local/bin), made absolute so a staged file is never run via PATH.
    fn resolve_bin_dir(opts: &Opts) -> Result<PathBuf, String> {
        let dir = match &opts.bin_dir {
            Some(dir) => dir.clone(),
            None => default_bin_dir()?,
        };
        if dir.is_absolute() {
            return Ok(dir);
        }
        std::env::current_dir()
            .map(|cwd| cwd.join(dir))
            .map_err(|e| format!("could not resolve --bin-dir: {e}"))
    }

    /// `maxplayer-mint` as the user's shell would find it: on PATH, else in ~/.local/bin.
    fn locate_binary(opts: &Opts) -> Option<PathBuf> {
        if opts.bin_dir.is_some() {
            let path = resolve_bin_dir(opts).ok()?.join(MINT_BIN);
            return fs::symlink_metadata(&path).ok().map(|_| path);
        }
        path_lookup().or_else(|| {
            let path = default_bin_dir().ok()?.join(MINT_BIN);
            fs::symlink_metadata(&path).ok().map(|_| path)
        })
    }

    fn path_lookup() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(MINT_BIN))
            .find(|candidate| {
                fs::metadata(candidate)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    }

    /// `path` as one shell word: unchanged when it is plain, else single-quoted.
    pub(super) fn shell_quote(path: &Path) -> String {
        let text = path.display().to_string();
        if !text.is_empty()
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%,=".contains(&b))
        {
            text
        } else {
            format!("'{}'", text.replace('\'', "'\\''"))
        }
    }

    /// The child with no inherited `MAXPLAYER_*` variable except the ones set afterwards.
    fn command(bin: &Path) -> Command {
        let mut cmd = Command::new(bin);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("MAXPLAYER_") {
                cmd.env_remove(key);
            }
        }
        cmd
    }

    /// `spawn`, retrying ETXTBSY (26 on linux and macOS): a binary written a moment ago can be
    /// briefly "busy" while a concurrently forked child still holds an inherited write handle.
    fn spawn(cmd: &mut Command) -> io::Result<Child> {
        let mut delay = Duration::from_millis(10);
        for _ in 0..8 {
            match cmd.spawn() {
                Err(error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(delay);
                    delay *= 2;
                }
                other => return other,
            }
        }
        cmd.spawn()
    }

    /// The first stdout line of `<bin> --version`, bounded by [`VERSION_PROBE_TIMEOUT`]. Only the
    /// first line is read, so a background process the binary left holding its stdout cannot hold
    /// this up.
    fn probe_version(bin: &Path) -> Result<String, String> {
        let mut child = spawn(
            command(bin)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .map_err(|e| format!("could not run {}: {e}", bin.display()))?;
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(stdout) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut line = String::new();
                let _ = io::BufReader::new(stdout.take(64 << 10)).read_line(&mut line);
                let _ = tx.send(line);
            });
        }
        let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{} --version did not answer", bin.display()));
                }
                Err(e) => return Err(format!("waiting for {}: {e}", bin.display())),
            }
        };
        if !status.success() {
            return Err(format!("{} --version failed ({status})", bin.display()));
        }
        let line = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
        Ok(line.trim().to_owned())
    }

    /// The mint's PUBLIC key, from `<mint dir>/nostr.key`. Read-only: unlike the home-key reader,
    /// this never re-chmods the file. `Ok(None)` when there is no key file.
    fn read_mint_keys(mint_dir: &Path) -> Result<Option<Keys>, String> {
        let path = mint_dir.join("nostr.key");
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        if !meta.is_file() {
            return Err(format!(
                "{} is not a regular file; refusing to read a key through it",
                path.display()
            ));
        }
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Keys::parse(text.trim())
            .map(Some)
            .map_err(|_| format!("{} is not a valid key", path.display()))
    }

    /// `name` from `mint.toml`, else the mint's default name (`maxplayer-mint-<8 hex>`, as
    /// crates/maxplayer-mint/src/home.rs derives it).
    fn mint_name(mint_dir: &Path, keys: &Keys) -> String {
        fs::read_to_string(mint_dir.join("mint.toml"))
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|table| table.get("name")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{MINT_BIN}-{}", &keys.public_key().to_hex()[..8]))
    }

    /// An existing `<home>/mint/` is kept only when it is a mint: a directory holding `nostr.key`,
    /// `seed` and `mint.sqlite`. A half-made one (an `init` that failed part-way) is refused, so
    /// `install` never reports a mint with no key as set up.
    fn check_existing_mint(mint_dir: &Path) -> Result<(), String> {
        let meta =
            fs::symlink_metadata(mint_dir).map_err(|e| format!("{}: {e}", mint_dir.display()))?;
        let is_file =
            |name: &str| fs::symlink_metadata(mint_dir.join(name)).is_ok_and(|m| m.is_file());
        let problem = if !meta.is_dir() {
            Some("it is not a directory".to_owned())
        } else {
            match read_mint_keys(mint_dir) {
                Ok(None) => Some("it has no nostr.key".to_owned()),
                Err(e) => Some(e),
                Ok(Some(_)) if !is_file("seed") => Some("it has no seed".to_owned()),
                Ok(Some(_)) if !is_file("mint.sqlite") => Some("it has no mint.sqlite".to_owned()),
                Ok(Some(_)) => None,
            }
        };
        match problem {
            None => Ok(()),
            Some(problem) => Err(format!(
                "{} exists but is not a usable mint ({problem}); nothing was installed. If an \
                 earlier setup stopped half-way, move it aside (never delete a mint that has \
                 issued credits) and rerun.",
                mint_dir.display()
            )),
        }
    }

    /// Whether `maxplayer-mint run` holds its single-listener lock (an exclusive `flock` on the
    /// mint directory). `Ok(Some(pid))` running (pid 0 when unknown), `Ok(None)` not running.
    fn mint_runner(mint_dir: &Path) -> Result<Option<u32>, String> {
        let proc_locks = if cfg!(target_os = "linux") {
            fs::read_to_string("/proc/locks").ok()
        } else {
            None
        };
        mint_runner_with(mint_dir, proc_locks.as_deref())
    }

    /// [`mint_runner`] with the `/proc/locks` text given. A holder listed there is found without
    /// touching the lock and names its pid. Anything else — not listed (btrfs reports another
    /// device there than `stat`, and another PID namespace's locks are hidden) or no
    /// `/proc/locks` (macOS) — is asked of the lock itself, with a shared probe released at once.
    /// That probe cannot stop a starting `maxplayer-mint run`: its `lock_run` retries for a
    /// second before deciding another `run` is serving.
    pub(super) fn mint_runner_with(
        mint_dir: &Path,
        proc_locks: Option<&str>,
    ) -> Result<Option<u32>, String> {
        let meta = fs::metadata(mint_dir).map_err(|e| format!("{}: {e}", mint_dir.display()))?;
        if let Some(locks) = proc_locks
            && let Some(pid) = flock_holder(locks, meta.dev(), meta.ino())
        {
            return Ok(Some(pid));
        }
        let dir = fs::File::open(mint_dir).map_err(|e| format!("{}: {e}", mint_dir.display()))?;
        match dir.try_lock_shared() {
            Ok(()) => Ok(None),
            Err(fs::TryLockError::WouldBlock) => Ok(Some(0)),
            Err(fs::TryLockError::Error(e)) => Err(format!("{}: {e}", mint_dir.display())),
        }
    }

    /// The pid holding a WRITE `FLOCK` on (`dev`, `ino`) in `/proc/locks` text (0 when the pid
    /// field is not a plain number). Blocked waiters (`->` lines) do not count.
    pub(super) fn flock_holder(locks: &str, dev: u64, ino: u64) -> Option<u32> {
        // glibc's major()/minor() decoding of st_dev; /proc/locks prints `%02x:%02x:%lu`.
        let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
        let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
        let want = format!("{major:02x}:{minor:02x}:{ino}");
        locks.lines().find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.contains(&"->") {
                return None;
            }
            let at = fields.iter().position(|f| *f == "FLOCK")?;
            (fields.get(at + 2) == Some(&"WRITE") && fields.get(at + 4) == Some(&want.as_str()))
                .then(|| fields.get(at + 3).and_then(|p| p.parse().ok()).unwrap_or(0))
        })
    }

    // ── maxplayer mint (info) ────────────────────────────────────────────────────────────────

    pub fn info(opts: &Opts, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        let root = match resolve_root(opts) {
            Ok(root) => root,
            Err(message) => {
                let _ = writeln!(err, "{message}");
                return RUNTIME_ERROR;
            }
        };
        let flags = home_flag(opts);

        let binary = locate_binary(opts);
        let (bin_line, bin_error) = match binary.as_deref().map(probe_version) {
            Some(Ok(line)) => (Some(line), None),
            Some(Err(e)) => (None, Some(e)),
            None => (None, None),
        };
        let bin_version = bin_line
            .as_deref()
            .and_then(reported_version)
            .map(str::to_owned);
        let bin_matches = bin_version.as_deref() == Some(VERSION);
        let bin_release = bin_line
            .as_deref()
            .is_some_and(|line| is_release_line(line, VERSION));

        let mint_dir = root.join("mint");
        let mint_exists = fs::symlink_metadata(&mint_dir).is_ok();
        let keys = if mint_exists {
            read_mint_keys(&mint_dir)
        } else {
            Ok(None)
        };
        let (keys, key_error) = match keys {
            Ok(keys) => (keys, None),
            Err(e) => (None, Some(e)),
        };
        let npub = keys.as_ref().and_then(|k| k.public_key().to_bech32().ok());
        let url = npub
            .as_ref()
            .map(|n| format!("{}{n}", maxplayer_core::mint_wire::NOSTR_MINT_SCHEME));
        let name = keys.as_ref().map(|k| mint_name(&mint_dir, k));
        let (running, run_pid, run_error) = if mint_exists {
            match mint_runner(&mint_dir) {
                Ok(Some(pid)) => (Some(true), (pid != 0).then_some(pid), None),
                Ok(None) => (Some(false), None, None),
                Err(e) => (None, None, Some(e)),
            }
        } else {
            (Some(false), None, None)
        };

        let (linked, link_error) = match (&keys, opts.offline) {
            (None, _) => (None, None),
            (Some(_), true) => (None, Some("not checked (--offline)".to_owned())),
            (Some(keys), false) => link_status(&root, keys),
        };

        if opts.json {
            let value = serde_json::json!({
                "maxplayer_version": VERSION,
                "binary": {
                    "installed": binary.is_some(),
                    "path": binary.as_ref().map(|p| p.display().to_string()),
                    "line": bin_line,
                    "version": bin_version,
                    "version_matches": bin_matches,
                    "release_build": bin_release,
                    "error": bin_error,
                },
                "home": root.display().to_string(),
                "mint": {
                    "exists": mint_exists,
                    "dir": mint_dir.display().to_string(),
                    "npub": npub,
                    "url": url,
                    "name": name,
                    "key_error": key_error,
                    "running": running,
                    "pid": run_pid,
                    "running_error": run_error,
                },
                "linked": linked,
                "link_error": link_error,
            });
            let _ = writeln!(out, "{value}");
            return SUCCESS;
        }

        let binary_line = match (&binary, &bin_error) {
            (None, _) => format!("not installed (run `maxplayer mint install{flags}`)"),
            (Some(path), Some(e)) => format!("{} (version unknown: {e})", path.display()),
            (Some(path), None) if bin_matches && bin_release => {
                format!("{} version {VERSION} (matches maxplayer)", path.display())
            }
            (Some(path), None) if bin_matches => format!(
                "{} version {VERSION} (matches maxplayer; a source build: {})",
                path.display(),
                bin_line.as_deref().unwrap_or("")
            ),
            (Some(path), None) => {
                // Name the directory it was found in: the default --bin-dir may not be the one
                // on PATH, and advice that installs elsewhere would never clear this line.
                let dir = path.parent().unwrap_or(Path::new("."));
                format!(
                    "{} version {} (maxplayer is {VERSION}; run `maxplayer mint install --force \
                     --bin-dir {}{flags}`)",
                    path.display(),
                    bin_version.as_deref().unwrap_or("unknown"),
                    shell_quote(dir)
                )
            }
        };
        let _ = writeln!(out, "{MINT_BIN}: {binary_line}");
        let _ = writeln!(out, "home:      {}", root.display());
        if !mint_exists {
            let _ = writeln!(
                out,
                "mint:      none at {} (`maxplayer mint install{flags}` sets it up)",
                mint_dir.display()
            );
        } else {
            let _ = writeln!(out, "mint:      {}", mint_dir.display());
            match (&npub, &key_error) {
                (Some(npub), _) => {
                    let _ = writeln!(out, "npub:      {npub}");
                    let _ = writeln!(out, "url:       {}", url.as_deref().unwrap_or(""));
                    let _ = writeln!(out, "name:      {}", name.as_deref().unwrap_or(""));
                }
                (None, Some(e)) => {
                    let _ = writeln!(out, "key:       unreadable ({e})");
                }
                (None, None) => {
                    let _ = writeln!(
                        out,
                        "key:       none (no nostr.key in {})",
                        mint_dir.display()
                    );
                }
            }
        }
        let running_line = match (running, run_pid, &run_error) {
            (Some(true), Some(pid), _) => format!("yes (pid {pid})"),
            (Some(true), None, _) => "yes".to_owned(),
            (Some(false), _, _) => "no".to_owned(),
            (None, _, e) => format!("unknown ({})", e.as_deref().unwrap_or("")),
        };
        let _ = writeln!(out, "running:   {running_line}");
        let linked_line = match (linked, &link_error, &keys) {
            (_, _, None) => "no mint key".to_owned(),
            (Some(true), _, _) => "yes".to_owned(),
            (Some(false), _, _) => format!("no (run `maxplayer account link --mint{flags}`)"),
            (None, e, _) => format!("unknown ({})", e.as_deref().unwrap_or("")),
        };
        let _ = writeln!(out, "linked:    {linked_line}");
        SUCCESS
    }

    /// The registration service's answer for the mint key: `(Some(registered), None)`, or
    /// `(None, Some(reason))` when it could not be asked.
    fn link_status(root: &Path, keys: &Keys) -> (Option<bool>, Option<String>) {
        // Read-only config resolution: no home, key or config.toml is created.
        let config = match home::resolve_config_read_only(root) {
            Ok(config) => config,
            Err(e) => return (None, Some(e.to_string())),
        };
        match RegistrationClient::with_timeout(
            &config.registration_url,
            keys.clone(),
            STATUS_TIMEOUT,
        )
        .and_then(|client| client.status())
        {
            Ok(status) => (Some(status.registered), None),
            Err(e) => (None, Some(e.to_string())),
        }
    }

    /// ` --home <dir>` (shell-quoted) when the user named one, so printed commands act on the
    /// same home.
    fn home_flag(opts: &Opts) -> String {
        opts.home
            .as_ref()
            .map(|h| format!(" --home {}", shell_quote(h)))
            .unwrap_or_default()
    }

    // ── maxplayer mint install ───────────────────────────────────────────────────────────────

    pub fn install(opts: &Opts, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        // Every refusal of the arguments happens before anything is resolved, created or contacted.
        let default_base = format!("{RELEASE_DOWNLOAD_BASE}/v{VERSION}");
        let source = match Source::parse(opts.from.as_deref().unwrap_or(&default_base)) {
            Ok(source) => source,
            Err(message) => {
                let _ = writeln!(err, "{message}");
                return USAGE_ERROR;
            }
        };
        if opts
            .bin_dir
            .as_ref()
            .is_some_and(|dir| dir.as_os_str().is_empty())
        {
            let _ = writeln!(err, "--bin-dir must not be empty");
            return USAGE_ERROR;
        }
        if let Some(name) = &opts.name
            && !maxplayer_core::mint_wire::valid_mint_name(name)
        {
            let _ = writeln!(
                err,
                "mint name {name:?} must be 1-{} characters, with no leading or trailing spaces, \
                 control characters, '=', '\"' or '\\'",
                maxplayer_core::mint_wire::MAX_MINT_NAME_CHARS
            );
            return USAGE_ERROR;
        }
        match install_inner(opts, &source, out, err) {
            Ok(()) => SUCCESS,
            Err(message) => {
                let _ = writeln!(err, "{message}");
                RUNTIME_ERROR
            }
        }
    }

    fn install_inner(
        opts: &Opts,
        source: &Source,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> Result<(), String> {
        let platform = release_platform().ok_or_else(|| {
            format!(
                "no {MINT_BIN} release for {}-{}: the released platforms are linux x86_64/aarch64 \
                 and macOS arm64/x86_64",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        let root = resolve_root(opts)?;
        let bin_dir = resolve_bin_dir(opts)?;
        let target = bin_dir.join(MINT_BIN);
        let flags = home_flag(opts);
        let mint_dir = root.join("mint");

        // An existing <home>/mint/ is kept as it is, but only a real one; checked before anything
        // is downloaded or installed.
        let mint_exists = fs::symlink_metadata(&mint_dir).is_ok();
        if mint_exists {
            check_existing_mint(&mint_dir)?;
        }

        // Staging files an interrupted install left behind (Ctrl-C, a kill, a crash).
        remove_stale_stages(&bin_dir);

        // An existing binary is replaced only with --force, and then it is not run at all. Without
        // --force, one that already reports this version is left alone and counts as installed.
        let mut already = false;
        let mut replaced_link = None;
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if opts.force {
                if meta.file_type().is_symlink() {
                    replaced_link = Some(fs::read_link(&target).unwrap_or_default());
                }
            } else {
                let reported = probe_version(&target);
                match &reported {
                    Ok(line) if reported_version(line) == Some(VERSION) => {
                        let note = if is_release_line(line, VERSION) {
                            String::new()
                        } else {
                            format!(
                                "; a source build — `maxplayer mint install --force{flags}` \
                                 replaces it with the release"
                            )
                        };
                        let _ = writeln!(
                            out,
                            "{line} is already installed at {} (left as it is{note}).",
                            target.display()
                        );
                        already = true;
                    }
                    _ => {
                        let what = match &reported {
                            Ok(line) if !line.is_empty() => format!("it reports {line:?}"),
                            Ok(_) => "it reports no version".to_owned(),
                            Err(e) => e.clone(),
                        };
                        return Err(format!(
                            "{} already exists ({what}); not replacing it. Rerun with --force to \
                             replace it with {MINT_BIN} {VERSION}.",
                            target.display()
                        ));
                    }
                }
            }
        }

        if !already {
            let asset = asset_name(VERSION, platform);
            let _ = writeln!(
                out,
                "Downloading {MINT_BIN} {VERSION} for {platform} from {}",
                source.describe(&asset)
            );
            let archive = source.fetch(&asset, MAX_ARCHIVE_BYTES)?;
            let sums = source.fetch("SHA256SUMS", MAX_SUMS_BYTES).map_err(|e| {
                format!("{e} — refusing to install an unverified download; nothing was installed")
            })?;
            let sums = String::from_utf8(sums)
                .map_err(|_| "SHA256SUMS is not text; refusing to install".to_owned())?;
            let expected = expected_sha256(&sums, &asset)?;
            let actual = sha256_hex(&archive);
            if actual != expected {
                return Err(format!(
                    "the download does not match the release — nothing was installed\n  \
                     expected (from SHA256SUMS): {expected}\n  actual (of {asset}):        {actual}"
                ));
            }
            let _ = writeln!(out, "Verified sha256 {actual} ({asset})");
            let binary = extract_member(&archive, &archive_member(VERSION, platform))?;
            if binary.len() as u64 > MAX_ARCHIVE_BYTES {
                return Err("the unpacked binary is implausibly large; refusing it".to_owned());
            }

            // Directories made here are removed again if the install is refused (dropped
            // without `keep`), after the staging file (declared later, dropped first).
            let created = CreatedDirs::create(&bin_dir)?;
            let staged = Staged::write(&bin_dir, &binary)
                .map_err(|e| format!("could not write into {}: {e}", bin_dir.display()))?;
            // Prove it before it is installed: a binary for the wrong platform or version leaves
            // nothing behind (the staging file is removed when `staged` drops).
            let line = probe_version(&staged.path).map_err(|e| {
                format!("the downloaded {MINT_BIN} did not run here ({e}) — nothing was installed")
            })?;
            if !is_release_line(&line, VERSION) {
                return Err(format!(
                    "the downloaded binary reports {line:?}, not a `{MINT_BIN} {VERSION} \
                     (<commit>)` release line; the mint must match this maxplayer ({VERSION}) — \
                     nothing was installed"
                ));
            }
            let warnings = staged.commit(&target, opts.force).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    format!(
                        "{} appeared while installing; not replacing it (rerun with --force) — \
                         nothing was installed",
                        target.display()
                    )
                } else {
                    format!(
                        "could not install {}: {e} — nothing was installed",
                        target.display()
                    )
                }
            })?;
            created.keep();
            // The binary is in place from here on: what follows can warn, never undo it.
            for warning in warnings {
                let _ = writeln!(err, "warning: {warning}");
            }
            let _ = writeln!(out, "Installed {line} -> {}", target.display());
            if let Some(link) = replaced_link {
                let _ = writeln!(
                    out,
                    "Replaced the symlink {} (it pointed to {}) with the installed file; a \
                     package manager that owned the link (npm) no longer updates it.",
                    target.display(),
                    link.display()
                );
            }
        }

        // <home>/mint/: created by the installed binary's own `init` (it owns the layout); an
        // existing one is never touched.
        if mint_exists {
            let _ = writeln!(
                out,
                "Kept the existing mint at {} (not re-initialised).",
                mint_dir.display()
            );
            if opts.name.is_some() {
                let _ = writeln!(
                    out,
                    "Ignored --name: an existing mint keeps its name; change it with \
                     `{MINT_BIN} name <name>`."
                );
            }
        } else {
            init_mint_home(&target, &root, opts.name.as_deref(), &flags, out, err)?;
        }

        if !already && let Ok(Some(pid)) = mint_runner(&mint_dir) {
            let pid = if pid == 0 {
                String::new()
            } else {
                format!(" (pid {pid})")
            };
            let _ = writeln!(
                out,
                "A `{MINT_BIN} run` is serving this home{pid}: restart it to run {VERSION}."
            );
        }
        if let Ok(Some(keys)) = read_mint_keys(&mint_dir)
            && let Ok(npub) = keys.public_key().to_bech32()
        {
            let _ = writeln!(
                out,
                "Mint URL: {}{npub}",
                maxplayer_core::mint_wire::NOSTR_MINT_SCHEME
            );
        }
        if path_lookup().is_none_or(|found| found != target) {
            let _ = writeln!(
                out,
                "Note: `{MINT_BIN}` on your PATH is not {}; add {} to PATH (or run it by its full \
                 path).",
                target.display(),
                bin_dir.display()
            );
        }
        let _ = writeln!(out);
        // `account link --mint` cosigns with this home's agent key when that key is linked (PR 4,
        // #1340); otherwise it links the mint by email code.
        let how = if fs::symlink_metadata(root.join("key")).is_ok() {
            "if this home's agent key is already linked, both keys sign and no email is sent; \
             otherwise it emails you a code"
        } else {
            "this home has no agent key, so it emails you a code"
        };
        let _ = writeln!(
            out,
            "Next: link the mint to your Maxplayer account ({how}):"
        );
        let _ = writeln!(out, "  maxplayer account link --mint{flags}");
        Ok(())
    }

    fn init_mint_home(
        bin: &Path,
        root: &Path,
        name: Option<&str>,
        flags: &str,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> Result<(), String> {
        if fs::symlink_metadata(root).is_err() {
            // Owner-only, like a home `maxplayer` creates: the mint's key and seed land under it.
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root)
                .map_err(|e| format!("could not create {}: {e}", root.display()))?;
        }
        let mut cmd = command(bin);
        cmd.arg("init");
        if let Some(name) = name {
            cmd.args(["--name", name]);
        }
        cmd.env("MAXPLAYER_HOME", root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = spawn(&mut cmd)
            .and_then(Child::wait_with_output)
            .map_err(|e| format!("could not run {} init: {e}", bin.display()))?;
        let _ = out.write_all(&output.stdout);
        let _ = err.write_all(&output.stderr);
        if !output.status.success() {
            let mint_dir = root.join("mint");
            let fix = if fs::symlink_metadata(&mint_dir).is_ok() {
                format!(
                    "it left a partial {}: move it aside (never delete a mint that has issued \
                     credits), then rerun `maxplayer mint install{flags}`",
                    mint_dir.display()
                )
            } else {
                format!("fix the error above, then rerun `maxplayer mint install{flags}`")
            };
            return Err(format!(
                "{} init failed ({}); the binary is installed but <home>/mint/ is not set up: \
                 {fix}",
                bin.display(),
                output.status
            ));
        }
        Ok(())
    }

    /// Remove `.maxplayer-mint.install.<pid>.*` regular files in `dir` whose process is gone or
    /// that are older than [`STALE_STAGE_AGE`]. Best effort: a live install's stage is kept.
    pub(super) fn remove_stale_stages(dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(STAGE_PREFIX)) else {
                continue;
            };
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let dead = rest
                .split('.')
                .next()
                .and_then(|pid| pid.parse::<i32>().ok())
                .is_some_and(|pid| !pid_alive(pid));
            let old = meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > STALE_STAGE_AGE);
            if dead || old {
                let _ = fs::remove_file(&path);
            }
        }
    }

    fn pid_alive(pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 only checks that the process exists; nothing is delivered.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// The directories [`Self::create`] made for the bin dir, removed again (if still empty)
    /// when dropped without [`Self::keep`].
    struct CreatedDirs {
        /// Deepest first.
        made: Vec<PathBuf>,
        keep: bool,
    }

    impl CreatedDirs {
        fn create(dir: &Path) -> Result<Self, String> {
            let mut made = Vec::new();
            let mut cursor = Some(dir);
            while let Some(path) = cursor {
                if fs::symlink_metadata(path).is_ok() {
                    break;
                }
                made.push(path.to_path_buf());
                cursor = path.parent();
            }
            fs::create_dir_all(dir)
                .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
            Ok(Self { made, keep: false })
        }

        fn keep(mut self) {
            self.keep = true;
        }
    }

    impl Drop for CreatedDirs {
        fn drop(&mut self) {
            if !self.keep {
                for dir in &self.made {
                    let _ = fs::remove_dir(dir);
                }
            }
        }
    }

    /// A fully written, fsynced copy of the binary under a temporary name in the target directory,
    /// owner-only until it is committed. Dropped without [`Self::commit`] it is removed, so a
    /// refused install leaves nothing behind.
    pub(super) struct Staged {
        pub(super) path: PathBuf,
        /// A read-only handle on the inode that was written: permissions are changed through it,
        /// never by path.
        handle: fs::File,
        armed: bool,
    }

    impl Staged {
        pub(super) fn write(dir: &Path, bytes: &[u8]) -> io::Result<Self> {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            for attempt in 0..32u32 {
                let path = dir.join(format!(
                    "{STAGE_PREFIX}{}.{nanos:x}{attempt}",
                    std::process::id()
                ));
                let mut file = match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o700)
                    .open(&path)
                {
                    Ok(file) => file,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                };
                let handle = match fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                    .and_then(|handle| {
                        let (a, b) = (file.metadata()?, handle.metadata()?);
                        if (a.dev(), a.ino()) == (b.dev(), b.ino()) {
                            Ok(handle)
                        } else {
                            Err(io::Error::other(
                                "the staging file was replaced while writing",
                            ))
                        }
                    }) {
                    Ok(handle) => handle,
                    Err(e) => {
                        let _ = fs::remove_file(&path);
                        return Err(e);
                    }
                };
                let staged = Self {
                    path,
                    handle,
                    armed: true,
                };
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
                staged
                    .handle
                    .set_permissions(fs::Permissions::from_mode(0o700))?;
                return Ok(staged);
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "no free staging name",
            ))
        }

        /// Put the staged file at `target` in one step and return non-fatal warnings. With
        /// `replace`: a rename over the old entry, so a running `maxplayer-mint` keeps its old
        /// inode and no reader ever sees a partial file. Without: a rename that refuses an
        /// existing target atomically (`renameat2(RENAME_NOREPLACE)` / `renamex_np(RENAME_EXCL)`),
        /// else a hard link, which refuses it the same way; a filesystem offering neither is
        /// refused rather than risk replacing a file that appeared meanwhile. Once the file is in
        /// place, nothing after it (directory fsync, stage cleanup) can fail the install.
        pub(super) fn commit(mut self, target: &Path, replace: bool) -> io::Result<Vec<String>> {
            self.handle
                .set_permissions(fs::Permissions::from_mode(0o755))?;
            let mut warnings = Vec::new();
            if replace {
                fs::rename(&self.path, target)?;
                self.armed = false;
            } else {
                match rename_noreplace(&self.path, target) {
                    Ok(()) => self.armed = false,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
                    Err(e) if rename_unsupported(&e) => match fs::hard_link(&self.path, target) {
                        Ok(()) => {
                            self.armed = false;
                            if let Err(e) = fs::remove_file(&self.path) {
                                warnings.push(format!(
                                    "could not remove the staging file {}: {e}",
                                    self.path.display()
                                ));
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
                        Err(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "this filesystem cannot install without possibly replacing a file \
                                 that appears meanwhile; rerun with --force to replace whatever is \
                                 there",
                            ));
                        }
                    },
                    Err(e) => return Err(e),
                }
            }
            if let Err(e) = sync_parent(target) {
                warnings.push(format!(
                    "installed, but could not sync the directory of {}: {e}",
                    target.display()
                ));
            }
            Ok(warnings)
        }
    }

    impl Drop for Staged {
        fn drop(&mut self) {
            if self.armed {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    fn rename_unsupported(e: &io::Error) -> bool {
        matches!(
            e.raw_os_error(),
            Some(code) if code == libc::EINVAL || code == libc::ENOSYS || code == libc::ENOTSUP
        )
    }

    /// `rename(from, to)` that fails with `AlreadyExists` instead of replacing `to`.
    #[cfg(target_os = "linux")]
    pub(super) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: two valid NUL-terminated paths that outlive the call; renameat2 only reads them.
        // The raw syscall rather than libc's wrapper: musl (the release build) has no wrapper.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// `rename(from, to)` that fails with `AlreadyExists` instead of replacing `to`.
    #[cfg(target_os = "macos")]
    pub(super) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: two valid NUL-terminated paths that outlive the call.
        let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }

    fn sync_parent(path: &Path) -> io::Result<()> {
        match path.parent() {
            Some(dir) => fs::File::open(dir)?.sync_all(),
            None => Ok(()),
        }
    }
}

#[cfg(all(test, feature = "wallet"))]
mod tests {
    use super::wallet::*;
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn help_is_stdout_and_success() {
        for form in [&["--help"][..], &["install", "--help"][..]] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            assert_eq!(run(&args(form), &mut out, &mut err), SUCCESS);
            assert!(String::from_utf8_lossy(&out).contains("maxplayer mint install"));
            assert!(err.is_empty());
        }
    }

    #[test]
    fn flags_are_per_subcommand() {
        assert!(parse(&args(&["--force"]), INFO_FLAGS).is_err());
        assert!(parse(&args(&["--offline"]), INSTALL_FLAGS).is_err());
        assert!(parse(&args(&["--home"]), INFO_FLAGS).is_err());
        let opts = parse(
            &args(&[
                "--home",
                "/h",
                "--force",
                "--name",
                "n",
                "--from",
                "file:///x",
            ]),
            INSTALL_FLAGS,
        )
        .unwrap();
        assert_eq!(opts.home, Some(PathBuf::from("/h")));
        assert!(opts.force);
        assert_eq!(opts.from.as_deref(), Some("file:///x"));
    }

    #[test]
    fn platforms_match_the_release_set() {
        assert_eq!(platform_for("linux", "x86_64"), Some("linux-x64"));
        assert_eq!(platform_for("linux", "aarch64"), Some("linux-arm64"));
        assert_eq!(platform_for("macos", "aarch64"), Some("darwin-arm64"));
        assert_eq!(platform_for("macos", "x86_64"), Some("darwin-x64"));
        assert_eq!(platform_for("windows", "x86_64"), None);
        assert_eq!(
            asset_name("0.7.3", "linux-x64"),
            "maxplayer-mint-0.7.3-linux-x64.tar.gz"
        );
        assert_eq!(
            archive_member("0.7.3", "linux-x64"),
            "maxplayer-mint-0.7.3-linux-x64/maxplayer-mint"
        );
    }

    #[test]
    fn only_release_lines_of_this_version_install() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        // The grammar of scripts/verify-release-version.sh: `(<40 hex>[ <text without ')'>])`.
        assert!(is_release_line(
            &format!("maxplayer-mint 0.7.3 ({sha})"),
            "0.7.3"
        ));
        assert!(is_release_line(
            &format!("maxplayer-mint 0.7.3 ({sha} 2026-11-18T10:00Z)"),
            "0.7.3"
        ));
        assert!(!is_release_line("maxplayer-mint 0.7.3", "0.7.3"));
        assert!(!is_release_line(
            &format!("maxplayer-mint 0.7.3 ({sha}-dirty)"),
            "0.7.3"
        ));
        assert!(!is_release_line("maxplayer-mint 0.7.3 (unknown)", "0.7.3"));
        assert!(!is_release_line(
            &format!("maxplayer-mint 0.7.2 ({sha})"),
            "0.7.3"
        ));
        assert!(!is_release_line(
            &format!("maxplayer-mint 0.7.3-rc1 ({sha})"),
            "0.7.3"
        ));
        assert!(!is_release_line(
            &format!("maxplayer 0.7.3 ({sha})"),
            "0.7.3"
        ));
        assert!(!is_release_line(
            &format!("maxplayer-mint 0.7.3 ({sha}) x"),
            "0.7.3"
        ));
        assert!(!is_release_line(
            &format!("maxplayer-mint 0.7.3 ({sha} a)b)"),
            "0.7.3"
        ));
        assert!(!is_release_line(
            &format!("maxplayer-mint 0.7.3 ({})", sha.to_uppercase()),
            "0.7.3"
        ));
        assert_eq!(
            reported_version(&format!("maxplayer-mint 0.6.0 ({sha})")),
            Some("0.6.0")
        );
        assert_eq!(reported_version("maxplayer-mint 0.7.3"), Some("0.7.3"));
    }

    #[test]
    fn redirects_stay_on_https() {
        assert_eq!(redirect_refusal("https", 0), None);
        assert_eq!(
            redirect_refusal("http", 0),
            Some("redirect away from https refused")
        );
        assert_eq!(
            redirect_refusal("file", 0),
            Some("redirect away from https refused")
        );
        assert_eq!(redirect_refusal("https", 10), Some("too many redirects"));
    }

    /// The real download client against a loopback https server that redirects to plain http:
    /// the redirect is refused and the http listener is never contacted (review M7).
    #[test]
    fn the_download_client_refuses_a_redirect_to_http() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let plain = TcpListener::bind("127.0.0.1:0").unwrap();
        let plain_port = plain.local_addr().unwrap().port();
        plain.set_nonblocking(true).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            loop {
                if plain.accept().is_ok() {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });

        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let pem = certified.cert.pem();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()),
        );
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key)
            .unwrap(),
        );
        let tls = TcpListener::bind("127.0.0.1:0").unwrap();
        let tls_port = tls.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in tls.incoming().flatten() {
                let conn = rustls::ServerConnection::new(config.clone()).unwrap();
                let mut stream = rustls::StreamOwned::new(conn, stream);
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let _ = write!(
                    stream,
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{plain_port}/x.tar.gz\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.flush();
            }
        });

        let root = reqwest::Certificate::from_pem(pem.as_bytes()).unwrap();
        let client = http_client(&[root], true).unwrap();
        let err = fetch_https(
            &client,
            &format!("https://127.0.0.1:{tls_port}/x.tar.gz"),
            1 << 20,
        )
        .unwrap_err();
        assert!(err.contains("could not download"), "{err}");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the http listener was contacted"
        );
    }

    #[test]
    fn duplicate_archive_members_are_refused() {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for body in [&b"one"[..], &b"two"[..]] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "d/maxplayer-mint", body)
                .unwrap();
        }
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let err = extract_member(&archive, "d/maxplayer-mint").unwrap_err();
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn printed_paths_are_one_shell_word() {
        assert_eq!(shell_quote(Path::new("/srv/mint-home")), "/srv/mint-home");
        assert_eq!(shell_quote(Path::new("/a/my home")), "'/a/my home'");
        assert_eq!(shell_quote(Path::new("/a/it's")), "'/a/it'\\''s'");
    }

    #[test]
    fn sums_entry_is_exact_and_well_formed() {
        let d = "a".repeat(64);
        let e = "b".repeat(64);
        let sums = format!(
            "{e}  maxplayer-0.7.3-linux-x64.tar.gz\n{d} *maxplayer-mint-0.7.3-linux-x64.tar.gz\n"
        );
        assert_eq!(
            expected_sha256(&sums, "maxplayer-mint-0.7.3-linux-x64.tar.gz").unwrap(),
            d
        );
        assert!(expected_sha256(&sums, "maxplayer-mint-0.7.3-linux-arm64.tar.gz").is_err());
        assert!(expected_sha256("zz  x.tar.gz\n", "x.tar.gz").is_err());
        let twice = format!("{d}  x.tar.gz\n{e}  x.tar.gz\n");
        assert!(expected_sha256(&twice, "x.tar.gz").is_err());
    }

    #[test]
    fn sources_are_https_or_file_only() {
        assert_eq!(
            Source::parse("https://example.com/r/").unwrap(),
            Source::Https("https://example.com/r".into())
        );
        assert_eq!(
            Source::parse("file:///tmp/rel").unwrap(),
            Source::File(PathBuf::from("/tmp/rel"))
        );
        assert_eq!(
            Source::parse("http://127.0.0.1:9/x").unwrap_err(),
            HTTP_REFUSED
        );
        assert_eq!(
            Source::parse("HTTP://localhost/x").unwrap_err(),
            HTTP_REFUSED
        );
        assert!(Source::parse("ftp://example.com/x").is_err());
        assert!(Source::parse("/tmp/rel").is_err());
        for bad in [
            "https://user:secret@example.com/r",
            "https://example.com/r?sig=abc",
            "https://example.com/r#frag",
        ] {
            assert!(
                Source::parse(bad).unwrap_err().contains("no credentials"),
                "{bad}"
            );
        }
    }

    #[test]
    fn proc_locks_holder_is_found_by_dev_and_inode() {
        // st_dev 0xfd01 = major 0xfd, minor 0x01.
        let locks = "1: FLOCK  ADVISORY  WRITE 4242 fd:01:777 0 EOF\n\
                     1: -> FLOCK  ADVISORY  WRITE 99 fd:01:778 0 EOF\n\
                     2: POSIX  ADVISORY  WRITE 7 fd:01:779 0 EOF\n\
                     3: FLOCK  ADVISORY  READ 8 fd:01:780 0 EOF\n";
        assert_eq!(flock_holder(locks, 0xfd01, 777), Some(4242));
        assert_eq!(flock_holder(locks, 0xfd01, 778), None);
        assert_eq!(flock_holder(locks, 0xfd01, 779), None);
        assert_eq!(flock_holder(locks, 0xfd01, 780), None);
        assert_eq!(flock_holder(locks, 0xfd02, 777), None);
        // A holder whose pid the kernel cannot attribute still counts as running.
        assert_eq!(
            flock_holder("1: FLOCK ADVISORY WRITE -1 fd:01:9 0 EOF\n", 0xfd01, 9),
            Some(0)
        );
    }

    #[test]
    fn a_holder_missing_from_proc_locks_is_still_seen() {
        // btrfs subvolumes and other PID namespaces leave /proc/locks without the line; the
        // fallback probe must still see the exclusive lock (and release its own at once).
        let dir = tempfile::tempdir().unwrap();
        let held = std::fs::File::open(dir.path()).unwrap();
        held.try_lock().unwrap();
        assert_eq!(mint_runner_with(dir.path(), Some("")).unwrap(), Some(0));
        assert_eq!(mint_runner_with(dir.path(), None).unwrap(), Some(0));
        drop(held);
        assert_eq!(mint_runner_with(dir.path(), Some("")).unwrap(), None);
        let after = std::fs::File::open(dir.path()).unwrap();
        after.try_lock().expect("the probe released its lock");
    }

    #[test]
    fn a_dropped_stage_leaves_nothing_and_commit_never_clobbers() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Staged::write(dir.path(), b"new").unwrap();
        let mode = std::fs::metadata(&staged.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "a stage is owner-only until committed");
        drop(staged);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        let target = dir.path().join(MINT_BIN);
        std::fs::write(&target, b"old").unwrap();
        let staged = Staged::write(dir.path(), b"new").unwrap();
        let err = staged.commit(&target, false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "stage removed"
        );

        let staged = Staged::write(dir.path(), b"new").unwrap();
        assert!(staged.commit(&target, true).unwrap().is_empty());
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn rename_noreplace_refuses_an_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let err = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&b).unwrap(), b"b");
        std::fs::remove_file(&b).unwrap();
        rename_noreplace(&a, &b).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"a");
        assert!(!a.exists());
    }

    #[test]
    fn stale_stages_of_dead_processes_are_removed_live_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        // pid 0 is never a process we can own; i32::MAX is never allocated.
        let dead = dir.path().join(format!("{STAGE_PREFIX}{}.0", i32::MAX));
        let live = dir
            .path()
            .join(format!("{STAGE_PREFIX}{}.0", std::process::id()));
        let other = dir.path().join(".maxplayer-mint.other");
        for path in [&dead, &live, &other] {
            std::fs::write(path, b"x").unwrap();
        }
        remove_stale_stages(dir.path());
        assert!(!dead.exists());
        assert!(live.exists());
        assert!(other.exists());
    }
}
