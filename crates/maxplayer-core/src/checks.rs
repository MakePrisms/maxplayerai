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
    use std::collections::BTreeMap;

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
        for argv in &declaration.commands {
            let is_test = argv.get(1).map(String::as_str) == Some("test");
            let scoped = argv.iter().any(|part| part == "-p");
            assert!(
                !is_test || scoped,
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
        // feature list and (b) fails.
        // Reads `--features a,b,c` in argv position, which is the only form this file uses.
        let has_feature = |argv: &[String], name: &str| {
            argv.windows(2)
                .filter(|pair| pair[0] == "--features")
                .any(|pair| pair[1].split(',').any(|feature| feature == name))
        };
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
    }
}
