//! Protocol objects for per-project checks and their deterministic attestation.
//!
//! This module deliberately contains no runner or buyer/seller policy. It only defines the bytes
//! both sides parse from the pinned base and delivered trees.

use std::fmt;

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const DECLARATION_PATH: &str = ".maxplayer/checks.toml";
pub const CHECKS_ATTESTATION_FILE: &str = "MAXPLAYER_CHECKS_ATTESTATION";
pub const CHECKS_ATTESTATION_MARKER: &str = "maxplayer-checks-attestation/v1";
pub const DECLARATION_SIZE_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvKind {
    NixFlake,
    ContainerImage,
}

impl EnvKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NixFlake => "nix-flake",
            Self::ContainerImage => "container-image",
        }
    }

    pub fn from_wire(text: &str) -> Option<Self> {
        match text {
            "nix-flake" => Some(Self::NixFlake),
            "container-image" => Some(Self::ContainerImage),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecksDeclaration {
    pub schema: u32,
    pub env_kind: EnvKind,
    pub flake_path: String,
    pub devshell: Option<String>,
    pub image: Option<String>,
    pub prepare: Vec<Vec<String>>,
    pub commands: Vec<Vec<String>>,
    pub timeout_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttestedCheck {
    pub argv: Vec<String>,
    pub exit_code: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecksAttestation {
    pub job_hash: String,
    pub raw_tree: String,
    pub declaration: String,
    pub env_kind: EnvKind,
    pub env_ref: String,
    pub net: String,
    pub checks: Vec<AttestedCheck>,
    pub verdict: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckRunOutcome {
    Pass,
    Fail { index: usize, exit_code: i32 },
    Indeterminate { cause: IndeterminateCause },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndeterminateCause {
    Timeout,
    SignalTerminated,
    LauncherFault,
    ProvisionFailed,
    ControlFailed,
    PostureMismatch,
    ResourceLimit,
    Io,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReasonCode {
    VerifyNotDescendant,
    VerifyTipMismatch,
    VerifyContentRefused,
    VerifyNoSentinel,
    VerifyReservedPath,
    VerifyAttestationMissing,
    VerifyAttestationMismatch,
    ChecksFailed,
}

impl RejectReasonCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VerifyNotDescendant => "verify_not_descendant",
            Self::VerifyTipMismatch => "verify_tip_mismatch",
            Self::VerifyContentRefused => "verify_content_refused",
            Self::VerifyNoSentinel => "verify_no_sentinel",
            Self::VerifyReservedPath => "verify_reserved_path",
            Self::VerifyAttestationMissing => "verify_attestation_missing",
            Self::VerifyAttestationMismatch => "verify_attestation_mismatch",
            Self::ChecksFailed => "checks_failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChecksError {
    TooLarge,
    Malformed(String),
    UnsupportedSchema(u32),
    InvalidFlakePath,
    InvalidImage,
    EmptyCommand,
    MissingEnvLock(String),
    MissingFlake(String),
    ReservedPath(String),
    TreeRead(String),
}

impl fmt::Display for ChecksError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "checks declaration exceeds 64 KiB"),
            Self::Malformed(error) => write!(f, "malformed checks protocol object: {error}"),
            Self::UnsupportedSchema(schema) => write!(f, "unsupported checks schema {schema}"),
            Self::InvalidFlakePath => {
                write!(f, "flake_path is not a clean repository-relative path")
            }
            Self::InvalidImage => write!(f, "container image is not digest-pinned"),
            Self::EmptyCommand => write!(f, "check argv arrays must be non-empty"),
            Self::MissingEnvLock(path) => write!(f, "required environment lock is absent: {path}"),
            Self::MissingFlake(path) => write!(f, "required flake is absent: {path}"),
            Self::ReservedPath(path) => write!(f, "reserved protocol path is occupied: {path}"),
            Self::TreeRead(error) => write!(f, "could not read base tree: {error}"),
        }
    }
}

impl std::error::Error for ChecksError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDeclaration {
    schema: u32,
    env: RawEnv,
    checks: RawChecks,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum RawEnv {
    NixFlake {
        #[serde(default = "root_flake_path")]
        flake_path: String,
        #[serde(default)]
        devshell: Option<String>,
    },
    ContainerImage {
        image: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChecks {
    prepare: Vec<Vec<String>>,
    commands: Vec<Vec<String>>,
    timeout_secs: u64,
}

fn root_flake_path() -> String {
    ".".to_owned()
}

pub fn parse_declaration(bytes: &[u8]) -> Result<Option<ChecksDeclaration>, ChecksError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > DECLARATION_SIZE_LIMIT {
        return Err(ChecksError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|e| ChecksError::Malformed(e.to_string()))?;
    let raw: RawDeclaration =
        toml::from_str(text).map_err(|e| ChecksError::Malformed(e.to_string()))?;
    if raw.schema != 1 {
        return Err(ChecksError::UnsupportedSchema(raw.schema));
    }
    if raw.checks.commands.is_empty()
        || raw.checks.commands.iter().any(Vec::is_empty)
        || raw.checks.prepare.iter().any(Vec::is_empty)
    {
        return Err(ChecksError::EmptyCommand);
    }
    let (env_kind, flake_path, devshell, image) = match raw.env {
        RawEnv::NixFlake {
            flake_path,
            devshell,
        } => {
            if !clean_relative_path(&flake_path) {
                return Err(ChecksError::InvalidFlakePath);
            }
            (EnvKind::NixFlake, flake_path, devshell, None)
        }
        RawEnv::ContainerImage { image } => {
            if !digest_pinned_image(&image) {
                return Err(ChecksError::InvalidImage);
            }
            (EnvKind::ContainerImage, ".".to_owned(), None, Some(image))
        }
    };
    Ok(Some(ChecksDeclaration {
        schema: raw.schema,
        env_kind,
        flake_path,
        devshell,
        image,
        prepare: raw.checks.prepare,
        commands: raw.checks.commands,
        timeout_secs: raw.checks.timeout_secs,
    }))
}

fn clean_relative_path(path: &str) -> bool {
    if path == "." {
        return true;
    }
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.as_bytes().get(1) != Some(&b':')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn digest_pinned_image(image: &str) -> bool {
    let Some((name, digest)) = image.split_once("@sha256:") else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-_/".contains(&b))
        && digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !digest.contains("@sha256:")
}

/// Minimal deterministic view of a pinned git tree. Implementations must return bytes only for
/// blobs; directories and missing paths are `None`.
pub trait BaseTree {
    fn blob_at(&self, path: &str) -> Result<Option<Vec<u8>>, ChecksError>;
}

#[cfg(feature = "git-delivery")]
impl BaseTree for (&git2::Repository, &git2::Tree<'_>) {
    fn blob_at(&self, path: &str) -> Result<Option<Vec<u8>>, ChecksError> {
        let entry = match self.1.get_path(std::path::Path::new(path)) {
            Ok(entry) => entry,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(ChecksError::TreeRead(error.to_string())),
        };
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return Ok(None);
        }
        let blob = self
            .0
            .find_blob(entry.id())
            .map_err(|e| ChecksError::TreeRead(e.to_string()))?;
        Ok(Some(blob.content().to_vec()))
    }
}

pub fn validate_against_base<T: BaseTree + ?Sized>(
    declaration: &ChecksDeclaration,
    base_tree: &T,
) -> Result<(), ChecksError> {
    for path in [
        crate::delivery_sentinel::SENTINEL_FILE,
        CHECKS_ATTESTATION_FILE,
    ] {
        if base_tree.blob_at(path)?.is_some() {
            return Err(ChecksError::ReservedPath(path.to_owned()));
        }
    }
    if declaration.env_kind == EnvKind::NixFlake {
        let flake = joined_path(&declaration.flake_path, "flake.nix");
        if base_tree.blob_at(&flake)?.is_none() {
            return Err(ChecksError::MissingFlake(flake));
        }
    }
    env_lock_ref(declaration, base_tree).map(|_| ())
}

pub fn env_lock_ref<T: BaseTree + ?Sized>(
    declaration: &ChecksDeclaration,
    base_tree: &T,
) -> Result<String, ChecksError> {
    match declaration.env_kind {
        EnvKind::NixFlake => {
            let path = joined_path(&declaration.flake_path, "flake.lock");
            let bytes = base_tree
                .blob_at(&path)?
                .ok_or_else(|| ChecksError::MissingEnvLock(path))?;
            Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
        }
        EnvKind::ContainerImage => declaration.image.clone().ok_or(ChecksError::InvalidImage),
    }
}

fn joined_path(parent: &str, leaf: &str) -> String {
    if parent == "." {
        leaf.to_owned()
    } else {
        format!("{parent}/{leaf}")
    }
}

pub fn render_attestation(attestation: &ChecksAttestation) -> String {
    let mut out = format!(
        "{CHECKS_ATTESTATION_MARKER} job-hash={}\nraw-tree: {}\ndeclaration: {}\nenv-kind: {}\nenv-ref: {}\nnet: {}\n",
        attestation.job_hash,
        attestation.raw_tree,
        attestation.declaration,
        attestation.env_kind.as_str(),
        attestation.env_ref,
        attestation.net,
    );
    for (index, check) in attestation.checks.iter().enumerate() {
        let argv =
            serde_json::to_string(&check.argv).expect("string argv is always JSON serializable");
        out.push_str(&format!(
            "check[{index}]: {argv} exit={}\n",
            check.exit_code
        ));
    }
    out.push_str(&format!("verdict: {}\n", attestation.verdict));
    out
}

pub fn parse_attestation(content: &str) -> Result<ChecksAttestation, ChecksError> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() < 7 || !content.ends_with('\n') {
        return Err(ChecksError::Malformed(
            "attestation has missing lines".to_owned(),
        ));
    }
    let job_hash = prefixed(lines[0], &format!("{CHECKS_ATTESTATION_MARKER} job-hash="))?;
    let raw_tree = prefixed(lines[1], "raw-tree: ")?;
    let declaration = prefixed(lines[2], "declaration: ")?;
    let env_kind = EnvKind::from_wire(prefixed(lines[3], "env-kind: ")?)
        .ok_or_else(|| ChecksError::Malformed("unknown env-kind".to_owned()))?;
    let env_ref = prefixed(lines[4], "env-ref: ")?.to_owned();
    let net = prefixed(lines[5], "net: ")?.to_owned();
    if !matches!(net.as_str(), "denied" | "open") {
        return Err(ChecksError::Malformed("bad net posture".to_owned()));
    }
    if !hex_of_len(job_hash, 64)
        || !hex_of_len(raw_tree, 40)
        || !hex_of_len(declaration, 64)
        || env_ref.is_empty()
        || env_ref.chars().any(char::is_control)
    {
        return Err(ChecksError::Malformed(
            "invalid attestation binding".to_owned(),
        ));
    }
    let mut checks = Vec::new();
    for line in &lines[6..lines.len() - 1] {
        let prefix = format!("check[{}]: ", checks.len());
        let rest = prefixed(line, &prefix)?;
        let (json, exit) = rest
            .rsplit_once(" exit=")
            .ok_or_else(|| ChecksError::Malformed("bad check line".to_owned()))?;
        let argv: Vec<String> =
            serde_json::from_str(json).map_err(|e| ChecksError::Malformed(e.to_string()))?;
        if argv.is_empty() {
            return Err(ChecksError::EmptyCommand);
        }
        if serde_json::to_string(&argv).ok().as_deref() != Some(json) || exit != "0" {
            return Err(ChecksError::Malformed(
                "non-canonical or failed check line".to_owned(),
            ));
        }
        let exit_code = 0;
        checks.push(AttestedCheck { argv, exit_code });
    }
    if checks.is_empty() {
        return Err(ChecksError::Malformed(
            "attestation has no checks".to_owned(),
        ));
    }
    let verdict = prefixed(lines[lines.len() - 1], "verdict: ")?.to_owned();
    if verdict != "pass" {
        return Err(ChecksError::Malformed("unsupported verdict".to_owned()));
    }
    Ok(ChecksAttestation {
        job_hash: job_hash.to_owned(),
        raw_tree: raw_tree.to_owned(),
        declaration: declaration.to_owned(),
        env_kind,
        env_ref,
        net,
        checks,
        verdict,
    })
}

fn prefixed<'a>(line: &'a str, prefix: &str) -> Result<&'a str, ChecksError> {
    line.strip_prefix(prefix)
        .ok_or_else(|| ChecksError::Malformed(format!("expected {prefix}")))
}

fn hex_of_len(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn content_carries_attestation(content: &str, job_hash: &str, subtract_path: &str) -> bool {
    let token = format!("{CHECKS_ATTESTATION_MARKER} job-hash={job_hash}");
    if subtract_path.is_empty() {
        content.contains(&token)
    } else {
        content.replace(subtract_path, "").contains(&token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    const NIX: &str = r#"schema = 1
[env]
kind = "nix-flake"
[checks]
prepare = [["cargo", "fetch", "--locked"]]
commands = [["cargo", "build", "--locked"], ["cargo", "test", "--locked"]]
timeout_secs = 1200
"#;

    #[derive(Default)]
    struct FakeTree(BTreeMap<String, Vec<u8>>);
    impl BaseTree for FakeTree {
        fn blob_at(&self, path: &str) -> Result<Option<Vec<u8>>, ChecksError> {
            Ok(self.0.get(path).cloned())
        }
    }

    #[test]
    fn declaration_accepts_nix_default_and_non_root_flake_path() {
        let root = parse_declaration(NIX.as_bytes()).unwrap().unwrap();
        assert_eq!(
            root.flake_path, ".",
            "omitted flake_path defaults to repository root"
        );
        let nested = NIX.replace(
            "kind = \"nix-flake\"",
            "kind = \"nix-flake\"\nflake_path = \".maxplayer\"",
        );
        assert_eq!(
            parse_declaration(nested.as_bytes())
                .unwrap()
                .unwrap()
                .flake_path,
            ".maxplayer",
            "a clean nested flake_path is accepted"
        );
    }

    #[test]
    fn declaration_refuses_absolute_and_parent_flake_paths() {
        for (path, label) in [
            ("/tmp/x", "absolute"),
            ("C:/tmp/x", "drive-absolute"),
            ("../x", "parent"),
            ("x/../y", "embedded parent"),
        ] {
            let input = NIX.replace(
                "kind = \"nix-flake\"",
                &format!("kind = \"nix-flake\"\nflake_path = \"{path}\""),
            );
            assert_eq!(
                parse_declaration(input.as_bytes()),
                Err(ChecksError::InvalidFlakePath),
                "{label} flake_path must be refused"
            );
        }
    }

    #[test]
    fn declaration_refuses_tagged_image_shell_unknown_schema_and_empty_argv() {
        let container = NIX.replace(
            "kind = \"nix-flake\"",
            "kind = \"container-image\"\nimage = \"docker.io/library/rust:latest\"",
        );
        assert_eq!(
            parse_declaration(container.as_bytes()),
            Err(ChecksError::InvalidImage),
            "tagged image must be refused"
        );
        let shell = NIX.replace(
            "[[\"cargo\", \"build\", \"--locked\"], [\"cargo\", \"test\", \"--locked\"]]",
            "[\"cargo build --locked\"]",
        );
        assert!(
            matches!(
                parse_declaration(shell.as_bytes()),
                Err(ChecksError::Malformed(_))
            ),
            "shell-string command must be refused"
        );
        assert!(
            matches!(
                parse_declaration(format!("{NIX}surprise = true\n").as_bytes()),
                Err(ChecksError::Malformed(_))
            ),
            "unknown field must be refused"
        );
        assert_eq!(
            parse_declaration(NIX.replace("schema = 1", "schema = 2").as_bytes()),
            Err(ChecksError::UnsupportedSchema(2)),
            "schema other than one must be refused"
        );
        let empty = NIX.replace(
            "[[\"cargo\", \"build\", \"--locked\"], [\"cargo\", \"test\", \"--locked\"]]",
            "[[]]",
        );
        assert_eq!(
            parse_declaration(empty.as_bytes()),
            Err(ChecksError::EmptyCommand),
            "empty argv must be refused"
        );
        assert_eq!(
            parse_declaration(b""),
            Ok(None),
            "absent declaration preserves v0.2.0 behavior"
        );
        assert_eq!(
            parse_declaration(&vec![b'x'; DECLARATION_SIZE_LIMIT + 1]),
            Err(ChecksError::TooLarge),
            "a declaration over 64 KiB is refused before parsing"
        );
    }

    #[test]
    fn container_digest_is_accepted_and_missing_digest_notation_refused() {
        let good = NIX.replace(
            "kind = \"nix-flake\"",
            &format!(
                "kind = \"container-image\"\nimage = \"docker.io/library/rust@sha256:{}\"",
                "a".repeat(64)
            ),
        );
        assert_eq!(
            parse_declaration(good.as_bytes())
                .unwrap()
                .unwrap()
                .env_kind,
            EnvKind::ContainerImage,
            "digest-pinned image is accepted"
        );
        let missing = good.replace("@sha256:", "@");
        assert_eq!(
            parse_declaration(missing.as_bytes()),
            Err(ChecksError::InvalidImage),
            "missing sha256 digest notation must be refused"
        );
    }

    #[test]
    fn base_validation_refuses_each_reserved_blob_and_nested_lock_is_hashed() {
        let nested = NIX.replace(
            "kind = \"nix-flake\"",
            "kind = \"nix-flake\"\nflake_path = \".maxplayer\"",
        );
        let declaration = parse_declaration(nested.as_bytes()).unwrap().unwrap();
        let mut tree = FakeTree::default();
        tree.0.insert(".maxplayer/flake.nix".into(), b"{}".to_vec());
        tree.0
            .insert(".maxplayer/flake.lock".into(), b"pinned".to_vec());
        assert_eq!(
            env_lock_ref(&declaration, &tree).unwrap(),
            format!("sha256:{}", hex::encode(Sha256::digest(b"pinned"))),
            "non-root declared lock bytes determine env-ref"
        );
        for path in [
            crate::delivery_sentinel::SENTINEL_FILE,
            CHECKS_ATTESTATION_FILE,
        ] {
            tree.0.insert(path.into(), b"occupied".to_vec());
            assert_eq!(
                validate_against_base(&declaration, &tree),
                Err(ChecksError::ReservedPath(path.into())),
                "declaring base must refuse occupied reserved path {path}"
            );
            tree.0.remove(path);
        }
        assert_eq!(
            validate_against_base(&declaration, &tree),
            Ok(()),
            "pinned clean nested flake validates"
        );
        tree.0.remove(".maxplayer/flake.lock");
        assert_eq!(
            env_lock_ref(&declaration, &tree),
            Err(ChecksError::MissingEnvLock(".maxplayer/flake.lock".into())),
            "a nix environment without its declared lock is refused"
        );
        tree.0
            .insert(".maxplayer/flake.lock".into(), b"pinned".to_vec());
        tree.0.remove(".maxplayer/flake.nix");
        assert_eq!(
            validate_against_base(&declaration, &tree),
            Err(ChecksError::MissingFlake(".maxplayer/flake.nix".into())),
            "the declared flake itself must exist at the pinned base"
        );
    }

    fn attestation() -> ChecksAttestation {
        ChecksAttestation {
            job_hash: "a".repeat(64),
            raw_tree: "b".repeat(40),
            declaration: "c".repeat(64),
            env_kind: EnvKind::NixFlake,
            env_ref: format!("sha256:{}", "d".repeat(64)),
            net: "denied".into(),
            checks: vec![AttestedCheck {
                argv: vec!["cargo".into(), "test".into(), "--locked".into()],
                exit_code: 0,
            }],
            verdict: "pass".into(),
        }
    }

    #[test]
    fn attestation_round_trip_is_deterministic_and_job_bound() {
        let value = attestation();
        let first = render_attestation(&value);
        let second = render_attestation(&value);
        assert_eq!(
            first.as_bytes(),
            second.as_bytes(),
            "two renders are byte deterministic"
        );
        assert_eq!(
            parse_attestation(&first),
            Ok(value.clone()),
            "render then parse preserves every field including net posture"
        );
        assert!(
            content_carries_attestation(&first, &value.job_hash, ""),
            "attestation binds its own job"
        );
        assert!(
            !content_carries_attestation(&first, &"e".repeat(64), ""),
            "a different job hash cannot reuse the attestation"
        );
    }

    #[test]
    fn malformed_attestation_is_an_error() {
        let missing = render_attestation(&attestation()).replace("net: denied\n", "");
        assert!(
            parse_attestation(&missing).is_err(),
            "a present attestation missing the net line is refused"
        );
    }

    #[test]
    fn outcome_consumers_match_every_variant_and_cause() {
        fn classify(outcome: CheckRunOutcome) -> &'static str {
            match outcome {
                CheckRunOutcome::Pass => "pass",
                CheckRunOutcome::Fail {
                    index: _,
                    exit_code: _,
                } => "fail",
                CheckRunOutcome::Indeterminate { cause } => match cause {
                    IndeterminateCause::Timeout
                    | IndeterminateCause::SignalTerminated
                    | IndeterminateCause::LauncherFault
                    | IndeterminateCause::ProvisionFailed
                    | IndeterminateCause::ControlFailed
                    | IndeterminateCause::PostureMismatch
                    | IndeterminateCause::ResourceLimit
                    | IndeterminateCause::Io => "indeterminate",
                },
            }
        }
        assert_eq!(
            classify(CheckRunOutcome::Fail {
                index: 1,
                exit_code: 2
            }),
            "fail",
            "ordinary nonzero child exit is a check failure"
        );
        assert_eq!(
            classify(CheckRunOutcome::Indeterminate {
                cause: IndeterminateCause::SignalTerminated
            }),
            "indeterminate",
            "signal termination is never classified as a command failure"
        );
        assert_eq!(
            classify(CheckRunOutcome::Indeterminate {
                cause: IndeterminateCause::LauncherFault
            }),
            "indeterminate",
            "launcher fault is never classified as a command failure"
        );
    }

    #[test]
    fn reject_reason_vocabulary_is_closed_and_exact() {
        let codes = [
            RejectReasonCode::VerifyNotDescendant,
            RejectReasonCode::VerifyTipMismatch,
            RejectReasonCode::VerifyContentRefused,
            RejectReasonCode::VerifyNoSentinel,
            RejectReasonCode::VerifyReservedPath,
            RejectReasonCode::VerifyAttestationMissing,
            RejectReasonCode::VerifyAttestationMismatch,
            RejectReasonCode::ChecksFailed,
        ]
        .map(RejectReasonCode::as_str);
        assert_eq!(
            codes,
            [
                "verify_not_descendant",
                "verify_tip_mismatch",
                "verify_content_refused",
                "verify_no_sentinel",
                "verify_reserved_path",
                "verify_attestation_missing",
                "verify_attestation_mismatch",
                "checks_failed",
            ],
            "the buyer rejection enum emits only the protocol vocabulary"
        );
    }

    /// Every feature name an argv turns on, collected across cargo's spellings of one flag.
    ///
    /// #737. Cargo accepts `--features a,b`, `--features=a,b`, `-F a,b`, `-F=a,b` and `-Fa,b`
    /// interchangeably, and treats spaces inside the value like commas. The guard below asserts
    /// that `live-mints` is not in the declared set; a reader that knows only the argv-position
    /// form asserts that only about rows spelled that way, so `--features=live-mints` would pass a
    /// check whose whole purpose is to refuse it. Normalise first, then test membership once —
    /// the membership question is about the feature, not about how the flag was written.
    fn declared_features(argv: &[String]) -> BTreeSet<String> {
        let mut features = BTreeSet::new();
        let mut parts = argv.iter();
        while let Some(part) = parts.next() {
            let value = if part == "--features" {
                // Argv-position: the value is the next element, and may be absent at the end.
                parts.next().cloned()
            } else if let Some(value) = part.strip_prefix("--features=") {
                Some(value.to_owned())
            } else if let Some(value) = part.strip_prefix("-F") {
                match value {
                    "" => parts.next().cloned(),
                    _ => Some(value.strip_prefix('=').unwrap_or(value).to_owned()),
                }
            } else {
                None
            };
            let Some(value) = value else { continue };
            features.extend(
                value
                    .split([',', ' '])
                    .filter(|feature| !feature.is_empty())
                    .map(str::to_owned),
            );
        }
        features
    }

    // #737. The normaliser is what the guard below stands on, so it is proven against every
    // spelling directly rather than only against the one spelling this repo happens to use. Each
    // row here is a form cargo would accept and act on; if any stops being read, a declaration
    // written that way becomes invisible to the `live-mints` ban.
    #[test]
    fn feature_flag_reader_sees_every_cargo_spelling_of_one_feature() {
        fn argv(parts: &[&str]) -> Vec<String> {
            parts.iter().map(|part| (*part).to_owned()).collect()
        }

        // Each row is a spelling cargo accepts and acts on. Every one of them turns `live-mints`
        // ON, so every one of them must be visible to the ban below.
        let live: [(&[&str], &str); 8] = [
            (&["cargo", "--features", "live-mints"], "argv-position"),
            (&["cargo", "--features=live-mints"], "equals"),
            (&["cargo", "-F", "live-mints"], "short argv-position"),
            (&["cargo", "-F=live-mints"], "short equals"),
            (&["cargo", "-Flive-mints"], "short attached"),
            (&["cargo", "--features=a,live-mints"], "equals list"),
            (&["cargo", "--features", "a live-mints"], "space list"),
            (&["cargo", "-Fa", "--features=live-mints"], "two flags"),
        ];
        for (parts, label) in live {
            assert!(
                declared_features(&argv(parts)).contains("live-mints"),
                "the {label} spelling turns `live-mints` on and must read as such: {parts:?}"
            );
        }

        let no_flag = argv(&["cargo", "test", "-p", "maxplayer-core"]);
        assert_eq!(
            declared_features(&no_flag),
            BTreeSet::new(),
            "a row that names no feature flag turns no feature on"
        );
        let wallet_only = argv(&["cargo", "--features=wallet"]);
        assert!(
            !declared_features(&wallet_only).contains("live-mints"),
            "reading a feature list must not invent members that are not in it"
        );
        let dangling = argv(&["cargo", "test", "--features"]);
        assert_eq!(
            declared_features(&dangling),
            BTreeSet::new(),
            "a trailing `--features` carrying no value is read as naming no feature"
        );
        let wallet_row = argv(&["cargo", "--features", "wallet"]);
        assert!(
            declared_features(&wallet_row).contains("wallet"),
            "the form this repo actually declares still reads: that is half (a) of the guard"
        );
    }

    /// Every path a declared argv points at, normalised across the spellings that reach one
    /// directory.
    ///
    /// #709. A row reaches a Node suite by naming its directory, and there is more than one way to
    /// write that name: `--prefix web/app`, `--prefix=web/app`, `./web/app`, `web/app/`, or a file
    /// path underneath it. A reader that knows only the bare argv-position spelling asserts
    /// coverage only about rows written that way, so `--prefix=web/app` would fail a check whose
    /// whole purpose is to find it — the same exposure `declared_features` was widened for in
    /// #737. Normalise first, then ask the membership question once.
    ///
    /// Flags are dropped, and an `=`-attached flag contributes its VALUE: that is what makes the
    /// attached and detached spellings read alike, since the detached form is already its own
    /// token. Non-path operands (`maxplayer-core`, `wallet`) survive normalisation as harmless
    /// entries — the question asked of this set is always about one named directory.
    fn declared_paths(argv: &[String]) -> BTreeSet<String> {
        argv.iter()
            .filter_map(|part| {
                let value = match part.split_once('=') {
                    Some((flag, value)) if flag.starts_with('-') => value,
                    _ if part.starts_with('-') => return None,
                    _ => part.as_str(),
                };
                let value = value.strip_prefix("./").unwrap_or(value);
                let value = value.trim_end_matches('/');
                (!value.is_empty()).then(|| value.to_owned())
            })
            .collect()
    }

    /// Whether a declared argv reaches inside `dir`, ignoring what it does there.
    ///
    /// #709. Segment boundaries, not string prefixes: `web/apparel` starts with `web/app` and is
    /// not inside it. A `starts_with` on the bare name would read a sibling directory as coverage.
    ///
    /// This answers HALF the question and is never asked alone — `argv_runs_suite_in` is the
    /// guard. Kept separate because "does this argv point at the directory" and "does this argv
    /// run its tests" fail in different ways and are worth failing separately.
    fn argv_reaches(argv: &[String], dir: &str) -> bool {
        let inside = format!("{dir}/");
        declared_paths(argv)
            .iter()
            .any(|path| path == dir || path.starts_with(&inside))
    }

    /// Whether a declared argv actually RUNS a Node test suite in `dir`.
    ///
    /// #709 re-grade. The first version of this guard asked only `argv_reaches`, so it certified
    /// any argv that merely MENTIONED the directory: `["true", "web/app"]` satisfied it, and no
    /// test ran. That is #709's own defect — a suite reading as covered while nothing executes it
    /// — rebuilt inside the fix for #709.
    ///
    /// Three things must hold TOGETHER, because each one alone is satisfiable by an argv that runs
    /// nothing: a runner that can execute a Node suite, an argument that makes that runner TEST
    /// rather than build or install, and the directory. The failure was treating the third as
    /// sufficient.
    ///
    /// What this deliberately does NOT decide is whether the argv succeeds. `node --test
    /// web/app/test/` names the runner, the action and the directory and still runs nothing on
    /// this tree, because nothing loads tsx — measured, rc=1. No reader of argv can know that, and
    /// a reader that pretended to would be guessing. That is what the header rule above covers:
    /// every argv here must be PROVEN, not merely well-shaped.
    fn argv_runs_suite_in(argv: &[String], dir: &str) -> bool {
        let Some(runner) = argv.first().map(String::as_str) else {
            return false;
        };
        let tests = match runner {
            // `npm test` and `npm run test` both reach the package's own `test` script.
            "npm" => argv.iter().any(|part| part == "test"),
            // node's own test runner, in both spellings it accepts for the flag.
            "node" => argv
                .iter()
                .any(|part| part == "--test" || part.starts_with("--test=")),
            _ => false,
        };
        tests && argv_reaches(argv, dir)
    }

    // #709. The reader is what the web-suite guard stands on, so it is proven against every
    // spelling that reaches the directory rather than only against the one this repo declares.
    // Each row here is a form that runs the suite; if any stops being read, a declaration written
    // that way becomes invisible to the guard and the suite reads as covered by nothing.
    #[test]
    fn node_suite_reader_sees_every_spelling_of_one_suite() {
        fn argv(parts: &[&str]) -> Vec<String> {
            parts.iter().map(|part| (*part).to_owned()).collect()
        }

        // Every row runs `web/app`'s suite, so every row must read as running it.
        //
        // `node --test web/app/test/` is deliberately NOT in this set. It is well-shaped and it
        // runs nothing on this tree (rc=1, no suite loaded, because nothing imports tsx), and a
        // reader that certifies an argv already measured as dead is the same silent-green in a new
        // place. It is left unasserted rather than asserted false: its deadness is a property of
        // this tree's dependencies, not of its argv, and this reader only reads argv.
        let running: [(&[&str], &str); 6] = [
            (&["npm", "--prefix", "web/app", "test"], "argv-position prefix"),
            (&["npm", "--prefix=web/app", "test"], "equals prefix"),
            (&["npm", "test", "--prefix", "web/app"], "prefix after the command"),
            (&["npm", "run", "test", "--prefix", "web/app"], "npm run test"),
            (&["npm", "--prefix", "./web/app", "test"], "dot-slash prefix"),
            (&["npm", "--prefix", "web/app/", "test"], "trailing-slash prefix"),
        ];
        for (parts, label) in running {
            assert!(
                argv_runs_suite_in(&argv(parts), "web/app"),
                "the {label} spelling runs web/app's suite and must read as such: {parts:?}"
            );
        }

        // A runner and an action without the directory is not coverage of THIS suite.
        assert!(
            !argv_runs_suite_in(&argv(&["npm", "--prefix", "web/network", "test"]), "web/app"),
            "the two web suites are different directories and one must never satisfy the other"
        );
        assert!(
            !argv_runs_suite_in(&argv(&["npm", "--prefix", "web/apparel", "test"]), "web/app"),
            "`web/apparel` is not inside `web/app` — the question is segment boundaries, never a \
             string prefix"
        );
        assert!(
            !argv_runs_suite_in(&argv(&["npm", "--prefix", "web", "test"]), "web/app"),
            "the parent directory is not the suite: `web` does not run what is under `web/app`"
        );

        // #709 re-grade. The directory alone must NEVER be enough. Every row below names
        // `web/app` and runs no test in it; the guard's first version accepted all of them.
        let not_running: [(&[&str], &str); 6] = [
            (&["true", "web/app"], "no runner at all"),
            (&["echo", "web/app/test/"], "a runner that only prints"),
            (&["npm", "--prefix", "web/app", "install"], "npm, but installing"),
            (&["npm", "--prefix", "web/app", "run", "build"], "npm, but building"),
            (&["node", "--check", "web/app/test/spot.test.ts"], "node, but syntax-checking"),
            (
                &["cargo", "test", "-p", "maxplayer-core", "--locked", "--offline"],
                "a cargo row, which runs no Node suite: the premise of #709",
            ),
        ];
        for (parts, label) in not_running {
            assert!(
                !argv_runs_suite_in(&argv(parts), "web/app"),
                "{label}: naming the directory is not running its suite, and reading it as \
                 coverage is the #709 defect rebuilt inside the #709 fix: {parts:?}"
            );
        }
    }

    /// Whether a declared argv installs `dir`'s npm dependencies.
    ///
    /// #709 re-grade, maxplayer's finding. The suite guard reads `commands` only, so the `prepare`
    /// row that installs web/app's node_modules was covered by nothing: delete it and every test
    /// still passed, while the declared web/app row could no longer execute — measured, rc=127
    /// `tsc: command not found`. That is the same declared-row-that-cannot-run defect this whole
    /// change exists to close, one field over in the same file.
    fn argv_installs_in(argv: &[String], dir: &str) -> bool {
        let Some(runner) = argv.first().map(String::as_str) else {
            return false;
        };
        let installs = runner == "npm"
            && argv
                .iter()
                .any(|part| part == "ci" || part == "install" || part == "i");
        installs && argv_reaches(argv, dir)
    }

    // #709 re-grade. Proven against argv the same way the suite reader is, so the prepare guard
    // below cannot be the only thing that has ever read this predicate.
    #[test]
    fn install_reader_sees_the_install_spellings_and_nothing_else() {
        fn argv(parts: &[&str]) -> Vec<String> {
            parts.iter().map(|part| (*part).to_owned()).collect()
        }

        for parts in [
            &["npm", "ci", "--prefix", "web/app"][..],
            &["npm", "install", "--prefix", "web/app"][..],
            &["npm", "--prefix=web/app", "ci"][..],
        ] {
            assert!(
                argv_installs_in(&argv(parts), "web/app"),
                "this spelling installs web/app's dependencies and must read as such: {parts:?}"
            );
        }

        assert!(
            !argv_installs_in(&argv(&["npm", "--prefix", "web/app", "test"]), "web/app"),
            "running the suite is not installing it: the two rows answer different questions"
        );
        assert!(
            !argv_installs_in(&argv(&["npm", "ci", "--prefix", "web/network"]), "web/app"),
            "installing a different package is not installing this one"
        );
        assert!(
            !argv_installs_in(&argv(&["cargo", "fetch", "--locked"]), "web/app"),
            "cargo fetch installs no npm dependency"
        );
    }

    /// Whether a declared argv is allowed by the cargo-test `-p` rule.
    ///
    /// #753. The rule is cargo feature-unification: a `cargo test` row must name the one package
    /// it proves, so a red says which configuration failed. A node runner has no feature graph
    /// and no `-p`, so the predicate is gated on the toolchain it reasons about — not on whether
    /// the second token happens to be `test`.
    fn cargo_test_is_package_scoped(argv: &[String]) -> bool {
        let is_cargo = argv.first().map(String::as_str) == Some("cargo");
        let is_test = is_cargo && argv.get(1).map(String::as_str) == Some("test");
        let scoped = argv.iter().any(|part| part == "-p");
        !is_test || scoped
    }

    // #753. The narrowing must not weaken the cargo property, and must stop judging non-cargo
    // runners by cargo's shape. Proven here against argv, not against this repo's current
    // declaration — adding a web row that the guard polices is a separate change.
    #[test]
    fn cargo_test_scope_rule_is_about_cargo_not_the_second_token() {
        fn argv(parts: &[&str]) -> Vec<String> {
            parts.iter().map(|part| (*part).to_owned()).collect()
        }

        assert!(
            cargo_test_is_package_scoped(&argv(&["cargo", "test", "-p", "x"])),
            "a -p scoped cargo test is the form the rule requires"
        );
        assert!(
            !cargo_test_is_package_scoped(&argv(&["cargo", "test"])),
            "workspace-wide cargo test must still fail: that is the property being protected"
        );
        assert!(
            cargo_test_is_package_scoped(&argv(&["npm", "test"])),
            "[\"npm\", \"test\"] is not a cargo row and must not be rejected for lacking -p"
        );
        assert!(
            cargo_test_is_package_scoped(&argv(&["node", "--test", "web/app/test/"])),
            "node --test passes because it is not cargo, not because argv[1] starts with a dash"
        );
    }

    // THIS repository's own declaration, parsed by the real parser. Presence is fail-closed at
    // runtime — a malformed declaration refuses the job with `ENV_UNPROVISIONABLE` — so shipping
    // one that nothing has ever parsed would hand every contributor an execution failure. A
    // declaration no test reads is a claim.
    #[test]
    fn this_repos_own_declaration_parses_and_stays_hermetic() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../", ".maxplayer/checks.toml");
        let bytes = std::fs::read(path).expect("this repo ships .maxplayer/checks.toml");
        let declaration = parse_declaration(&bytes)
            .expect("our own declaration must parse")
            .expect("our own declaration must not be empty");

        assert_eq!(declaration.env_kind, EnvKind::NixFlake);
        assert_eq!(
            declaration.flake_path, ".",
            "the flake this repo declares is the one at its root"
        );
        assert!(!declaration.commands.is_empty());

        // §9.1: every declared command runs with NO network, and every declared TEST command names
        // the one package (and so the one feature configuration) it proves. A workspace-wide row
        // would re-unify features across crates and report a red without saying which
        // configuration produced it. This assertion exists so the declaration cannot be
        // "simplified" back to the whole-workspace form.
        //
        // #753. That intent is cargo feature-unification. `is_test` is therefore cargo's
        // `argv[1] == "test"`, not "the second token happens to be test": a node runner has no
        // `-p` and must not be accepted or rejected on the accident of that token. The predicate
        // is proven against cargo and non-cargo shapes in
        // `cargo_test_scope_rule_is_about_cargo_not_the_second_token`.
        for argv in &declaration.commands {
            assert!(
                cargo_test_is_package_scoped(argv),
                "a declared test command must be -p scoped, never workspace-wide: {argv:?}"
            );
        }

        // #720. Two halves of one guard, and neither is worth anything alone.
        //
        // (a) The money path must be DECLARED. `wallet` gates collect (tests/collect_integrity.rs),
        // the seller node and its store, buyer_fund, wallet_ops, payment. With no wallet row, all
        // of it ran zero times under the declared set and collect/settle integrity could be broken
        // with every check green — that was the state this repo shipped in.
        //
        // (b) `live-mints` must stay OUT of the declared set. It is the opt-in that carries the
        // four tests which reach a live third-party mint; enabling it here is exactly how (a) gets
        // "fixed" back into a suite that cannot pass without a network. Those four run in the
        // money-path CI job instead (.github/workflows/ci.yml), which has one.
        //
        // Red-on-revert, measured: drop the wallet row and (a) fails; append "live-mints" to its
        // feature list — in any spelling cargo accepts — and (b) fails.
        // #737. Both halves ask the same question — is this feature ON for this row — so both go
        // through `declared_features`, which normalises cargo's several spellings of the flag
        // before the membership test. Reading only the argv-position form left half (b) blind to
        // `--features=live-mints`: the guard names its subject in one spelling, and the subject
        // has more than one. The normaliser is proven spelling-by-spelling in
        // `feature_flag_reader_sees_every_cargo_spelling_of_one_feature`.
        let has_feature = |argv: &[String], name: &str| declared_features(argv).contains(name);
        let wallet_row = declaration.commands.iter().any(|argv| {
            argv.get(1).map(String::as_str) == Some("test")
                && argv.iter().any(|part| part == "maxplayer-core")
                && has_feature(argv, "wallet")
        });
        assert!(
            wallet_row,
            "the declared set must run maxplayer-core's wallet configuration — without it collect \
             and the seller node are covered by nothing: {:?}",
            declaration.commands
        );
        for argv in &declaration.commands {
            assert!(
                !has_feature(argv, "live-mints"),
                "`live-mints` reaches real mints over the network and can never be declared here \
                 (§9.1 runs every command with net denied): {argv:?}"
            );
        }

        // #709. This file is the CONTRIBUTION gate — the set a seller's attestation runs from the
        // pinned base with the network denied — and it is what a delivery is PAID against. It
        // declared five cargo rows and no Node row, so every test in web/app and web/network ran
        // zero times under the declared set: a delivery could regress the market terminal and
        // attest GREEN. CI catches that break on a pull request to main; the gate that pays did
        // not. Same shape as the #720 wallet gap, and a money defect for the same reason.
        //
        // Both suites, not one. They are separate npm packages with separate runners: web/app is
        // TypeScript run through tsx, web/network is plain ESM with zero dependencies. A guard
        // that named only one would leave the other exactly as uncovered as before.
        //
        // Asked through `argv_runs_suite_in`, which requires a Node RUNNER, a TEST action and the
        // directory together — the reader is proven spelling-by-spelling, and against six argv
        // that name the directory and run nothing, in
        // `node_suite_reader_sees_every_spelling_of_one_suite`.
        for suite in ["web/app", "web/network"] {
            assert!(
                declaration
                    .commands
                    .iter()
                    .any(|argv| argv_runs_suite_in(argv, suite)),
                "the declared set must run {suite}'s Node suite — without it a delivery can \
                 regress the web tree and still attest green: {:?}",
                declaration.commands
            );
        }

        // #709 re-grade, maxplayer's finding. web/app's suite runs its TypeScript through tsx, a
        // lockfile-pinned devDependency, so the declared row cannot execute unless something
        // installs node_modules first. `prepare` MAY use the network and the commands MAY NOT
        // (protocol-v1 §9.1), so the install can only live there. Guarding `commands` alone left
        // that row covered by nothing: deleting it kept every test green while the web/app row
        // stopped being able to run at all — rc=127, `tsc: command not found`, measured.
        assert!(
            declaration
                .prepare
                .iter()
                .any(|argv| argv_installs_in(argv, "web/app")),
            "web/app's suite needs its node_modules installed in `prepare`, or the declared row \
             cannot execute — a declared row that cannot run is worse than no row: {:?}",
            declaration.prepare
        );
    }

    // #709 re-grade. The red this change was written against, made MECHANICAL.
    //
    // The original proof that the guard catches the defect was a terminal paste in a pull request
    // body: true when it was taken, unreadable afterwards, and impossible for CI to re-check. This
    // reconstructs the exact shape this repo shipped at d4ccc7f — five cargo rows, no Node row —
    // and asserts the guard rejects it. That turns a one-time claim into a property CI re-proves
    // on every run, which is what the red was ever supposed to buy.
    //
    // It is built from a literal rather than read from git, so it keeps holding after the base
    // commit ages out of anyone's memory.
    #[test]
    fn the_five_cargo_row_declaration_this_repo_shipped_is_rejected() {
        let shipped: Vec<Vec<String>> = [
            &["cargo", "build", "--locked"][..],
            &["cargo", "test", "-p", "maxplayer-core", "--locked", "--offline"][..],
            &[
                "cargo",
                "test",
                "-p",
                "maxplayer-core",
                "--features",
                "acp",
                "--locked",
                "--offline",
            ][..],
            &[
                "cargo",
                "test",
                "-p",
                "maxplayer-core",
                "--features",
                "wallet",
                "--locked",
                "--offline",
            ][..],
            &["cargo", "test", "-p", "maxplayer", "--locked", "--offline"][..],
        ]
        .iter()
        .map(|argv| argv.iter().map(|part| (*part).to_owned()).collect())
        .collect();

        for suite in ["web/app", "web/network"] {
            assert!(
                !shipped.iter().any(|argv| argv_runs_suite_in(argv, suite)),
                "the five-cargo-row set runs no Node suite — if the guard accepts it, the guard \
                 has stopped catching the defect it was written for ({suite})"
            );
        }

        // And the same set carries no npm install either, so the prepare guard has a red too.
        let no_prepare: Vec<Vec<String>> = vec![vec!["cargo".to_owned(), "fetch".to_owned()]];
        assert!(
            !no_prepare
                .iter()
                .any(|argv| argv_installs_in(argv, "web/app")),
            "`cargo fetch` alone installs no npm dependency: the prepare guard must fail on it"
        );
    }

    // #709 re-grade. The declared npm rows run inside the flake devshell (§9.1), and at d4ccc7f
    // that shell held a Rust toolchain and nothing else — no node, no npm. So the rows this change
    // adds could not have executed there, and a declared row that cannot execute is worse than no
    // row: it reports as an environment failure rather than as the coverage gap it replaces.
    //
    // Delete `nodejs_22` from flake.nix and, without this test, every other test here still
    // passes while the declared rows quietly become unrunnable — #709's own defect class, one file
    // over. This closes that.
    //
    // BOUND, and it is the honest limit of this assertion: this reads flake.nix as TEXT. It proves
    // the devshell DECLARES node; it does not prove the devshell provides it. Nothing in this tree
    // can prove that — `nix` is not required to build this repo and no CI job enters the devshell
    // — so that residual is named in the pull request rather than papered over here.
    #[test]
    fn the_devshell_declares_the_node_the_check_rows_need() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../", "flake.nix");
        let text = std::fs::read_to_string(path).expect("this repo ships flake.nix at its root");

        let devshells = text
            .find("devShells")
            .expect("flake.nix must declare devShells: §9.1 runs every declared command in one");
        assert!(
            text[devshells..].contains("nodejs_22"),
            "the default devshell must declare nodejs_22, or .maxplayer/checks.toml's npm rows \
             cannot execute in the environment they are declared against"
        );
    }
}
