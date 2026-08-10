//! Environment-backend resolution and effect-injected provisioning.

use std::fmt;
use std::path::PathBuf;

use crate::checks::{self, ChecksDeclaration, ChecksError, EnvKind};
use crate::gateway;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvBackend {
    NixFlake { workdir: PathBuf, devshell: String },
    ContainerImage { digest: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvPosture {
    Provision,
    Checks,
}

pub trait EnvRunner {
    fn argv_prefix(&self, backend: &EnvBackend, posture: EnvPosture) -> Vec<String>;
    fn container_runtime(&self) -> Option<&str>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEnvRunner {
    pub container_runtime: Option<String>,
    pub mount_dir: PathBuf,
}

impl EnvRunner for HostEnvRunner {
    fn argv_prefix(&self, backend: &EnvBackend, posture: EnvPosture) -> Vec<String> {
        match backend {
            EnvBackend::NixFlake { workdir, devshell } => vec![
                "nix".to_owned(),
                "develop".to_owned(),
                format!("{}#{devshell}", workdir.display()),
                "--command".to_owned(),
            ],
            EnvBackend::ContainerImage { digest } => {
                let mut prefix = vec![
                    self.container_runtime
                        .clone()
                        .unwrap_or_default(),
                    "run".to_owned(),
                    "--rm".to_owned(),
                    "-v".to_owned(),
                    format!("{}:/work", self.mount_dir.display()),
                    "-w".to_owned(),
                    "/work".to_owned(),
                ];
                match posture {
                    EnvPosture::Provision => {}
                    EnvPosture::Checks => prefix.push("--network=none".to_owned()),
                }
                prefix.push(digest.clone());
                prefix
            }
        }
    }

    fn container_runtime(&self) -> Option<&str> {
        self.container_runtime.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectOutput {
    pub status: i32,
    pub stdout: String,
}

pub trait EnvEffects {
    fn run(&self, argv: &[String]) -> Result<EffectOutput, EnvProvisionError>;
}

pub fn compose(
    policy_wrap: &dyn Fn(Vec<String>) -> Vec<String>,
    mut prefix: Vec<String>,
    argv: Vec<String>,
) -> Vec<String> {
    prefix.extend(argv);
    policy_wrap(prefix)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvProvisionError {
    BackendUnavailable { backend: String },
    EnvUnresolvable { detail: String },
    DigestMismatch { expected: String, actual: String },
    ProvisionCommandFailed { argv: Vec<String>, status: i32 },
}

impl fmt::Display for EnvProvisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendUnavailable { backend } => {
                write!(f, "environment backend is unavailable: {backend}")
            }
            Self::EnvUnresolvable { detail } => {
                write!(f, "environment declaration cannot be resolved: {detail}")
            }
            Self::DigestMismatch { expected, actual } => {
                write!(
                    f,
                    "environment digest mismatch: expected {expected}, got {actual}"
                )
            }
            Self::ProvisionCommandFailed { argv, status } => {
                write!(f, "environment provisioning command {argv:?} exited with status {status}")
            }
        }
    }
}

impl std::error::Error for EnvProvisionError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionedEnv {
    pub backend: EnvBackend,
    pub posture_prefix: Vec<String>,
    pub warmed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvisionOutcomeClass {
    Infra,
    Checks,
}

pub const ENV_UNPROVISIONABLE: &str = "env_unprovisionable";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionRefusal {
    DeclarationUnparsable { detail: String },
    ReservedPath { path: String },
    EnvLockMissing { detail: String },
    Unprovisionable(EnvProvisionError),
}

pub fn provision(
    effects: &dyn EnvEffects,
    runner: &dyn EnvRunner,
    backend: EnvBackend,
    posture: EnvPosture,
) -> Result<ProvisionedEnv, EnvProvisionError> {
    let warmed = match &backend {
        EnvBackend::NixFlake { .. } => {
            let mut argv = runner.argv_prefix(&backend, EnvPosture::Provision);
            argv.push("true".to_owned());
            require_success(effects, argv)?;
            true
        }
        EnvBackend::ContainerImage { digest } => {
            let runtime = runner.container_runtime().ok_or_else(|| {
                EnvProvisionError::BackendUnavailable {
                    backend: "container".to_owned(),
                }
            })?;
            require_success(
                effects,
                vec![runtime.to_owned(), "pull".to_owned(), digest.clone()],
            )?;
            let probe_argv = vec![
                runtime.to_owned(),
                "image".to_owned(),
                "inspect".to_owned(),
                "--format".to_owned(),
                "{{index .RepoDigests 0}}".to_owned(),
                digest.clone(),
            ];
            let output = require_success(effects, probe_argv)?;
            let actual = output.stdout.trim().to_owned();
            if actual != *digest {
                return Err(EnvProvisionError::DigestMismatch {
                    expected: digest.clone(),
                    actual,
                });
            }
            false
        }
    };
    let posture_prefix = runner.argv_prefix(&backend, posture);
    Ok(ProvisionedEnv {
        backend,
        posture_prefix,
        warmed,
    })
}

fn require_success(
    effects: &dyn EnvEffects,
    argv: Vec<String>,
) -> Result<EffectOutput, EnvProvisionError> {
    let output = effects.run(&argv)?;
    if output.status != 0 {
        return Err(EnvProvisionError::ProvisionCommandFailed {
            argv,
            status: output.status,
        });
    }
    Ok(output)
}

pub fn classify_provision_failure(error: &EnvProvisionError) -> ProvisionOutcomeClass {
    match error {
        EnvProvisionError::BackendUnavailable { .. }
        | EnvProvisionError::EnvUnresolvable { .. }
        | EnvProvisionError::DigestMismatch { .. }
        | EnvProvisionError::ProvisionCommandFailed { .. } => ProvisionOutcomeClass::Infra,
    }
}

pub fn refusal_feedback(refusal: &ProvisionRefusal) -> (gateway::ReasonCode, &'static str) {
    match refusal {
        ProvisionRefusal::DeclarationUnparsable { .. }
        | ProvisionRefusal::ReservedPath { .. }
        | ProvisionRefusal::EnvLockMissing { .. }
        | ProvisionRefusal::Unprovisionable(_) => {
            (gateway::ReasonCode::ExecutionFailed, ENV_UNPROVISIONABLE)
        }
    }
}

pub fn capture_job_checks(
    base_tree: &dyn checks::BaseTree,
) -> Result<(Vec<u8>, ChecksDeclaration), ChecksError> {
    let bytes = base_tree
        .blob_at(checks::DECLARATION_PATH)?
        .ok_or_else(|| ChecksError::Malformed("checks declaration is absent".to_owned()))?;
    let declaration = checks::parse_declaration(&bytes)?
        .ok_or_else(|| ChecksError::Malformed("checks declaration is empty".to_owned()))?;
    checks::validate_against_base(&declaration, base_tree)?;
    Ok((bytes, declaration))
}

pub fn resolve_backend(declaration: &ChecksDeclaration) -> Result<EnvBackend, EnvProvisionError> {
    match declaration.env_kind {
        EnvKind::NixFlake => Ok(EnvBackend::NixFlake {
            workdir: PathBuf::from(&declaration.flake_path),
            devshell: declaration
                .devshell
                .clone()
                .unwrap_or_else(|| "default".to_owned()),
        }),
        EnvKind::ContainerImage => declaration
            .image
            .clone()
            .map(|digest| EnvBackend::ContainerImage { digest })
            .ok_or_else(|| EnvProvisionError::EnvUnresolvable {
                detail: "container image declaration has no image".to_owned(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeEffects {
        outputs: RefCell<Vec<Result<EffectOutput, EnvProvisionError>>>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeEffects {
        fn succeeding(stdout: &[&str]) -> Self {
            Self {
                outputs: RefCell::new(
                    stdout
                        .iter()
                        .rev()
                        .map(|stdout| Ok(EffectOutput {
                            status: 0,
                            stdout: (*stdout).to_owned(),
                        }))
                        .collect(),
                ),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl EnvEffects for FakeEffects {
        fn run(&self, argv: &[String]) -> Result<EffectOutput, EnvProvisionError> {
            self.calls.borrow_mut().push(argv.to_vec());
            self.outputs.borrow_mut().pop().expect("fake output")
        }
    }

    fn declaration(env_kind: EnvKind) -> ChecksDeclaration {
        ChecksDeclaration {
            schema: 1,
            env_kind,
            flake_path: ".".to_owned(),
            devshell: None,
            image: None,
            prepare: Vec::new(),
            commands: vec![vec!["true".to_owned()]],
            timeout_secs: 60,
        }
    }

    fn runner() -> HostEnvRunner {
        HostEnvRunner {
            container_runtime: Some("podman".to_owned()),
            mount_dir: PathBuf::from("/materialized/job"),
        }
    }

    #[test]
    fn nix_prefix_is_exact_for_root_nested_default_and_named_devshells() {
        for (flake_path, devshell, flake_ref) in [
            (".", None, ".#default"),
            (".", Some("ci"), ".#ci"),
            ("tools/checks", None, "tools/checks#default"),
            ("tools/checks", Some("ci"), "tools/checks#ci"),
        ] {
            let mut declaration = declaration(EnvKind::NixFlake);
            declaration.flake_path = flake_path.to_owned();
            declaration.devshell = devshell.map(str::to_owned);
            let backend = resolve_backend(&declaration).unwrap();
            let expected = vec!["nix", "develop", flake_ref, "--command"];
            assert_eq!(
                runner().argv_prefix(&backend, EnvPosture::Provision),
                expected
            );
            assert_eq!(runner().argv_prefix(&backend, EnvPosture::Checks), expected);
        }
    }

    #[test]
    fn container_prefixes_are_exact_for_provision_and_checks() {
        let backend = EnvBackend::ContainerImage {
            digest: "registry.example/project/checks@sha256:abc123".to_owned(),
        };
        assert_eq!(
            runner().argv_prefix(&backend, EnvPosture::Provision),
            vec![
                "podman",
                "run",
                "--rm",
                "-v",
                "/materialized/job:/work",
                "-w",
                "/work",
                "registry.example/project/checks@sha256:abc123",
            ]
        );
        assert_eq!(
            runner().argv_prefix(&backend, EnvPosture::Checks),
            vec![
                "podman",
                "run",
                "--rm",
                "-v",
                "/materialized/job:/work",
                "-w",
                "/work",
                "--network=none",
                "registry.example/project/checks@sha256:abc123",
            ]
        );
    }

    #[test]
    fn compose_places_launcher_outermost_for_both_postures() {
        let wrap = |mut argv: Vec<String>| {
            argv.insert(0, "launcher".to_owned());
            argv
        };
        let backend = EnvBackend::ContainerImage {
            digest: "image@sha256:digest".to_owned(),
        };
        for posture in [EnvPosture::Provision, EnvPosture::Checks] {
            let prefix = runner().argv_prefix(&backend, posture);
            let composed = compose(&wrap, prefix.clone(), vec!["check".to_owned()]);
            assert_eq!(composed[0], "launcher");
            assert_eq!(&composed[1..1 + prefix.len()], prefix);
            assert_eq!(composed.last().map(String::as_str), Some("check"));
        }
    }

    #[test]
    fn resolve_backend_refuses_missing_image_and_preserves_pinned_image() {
        let missing = declaration(EnvKind::ContainerImage);
        assert!(matches!(
            resolve_backend(&missing),
            Err(EnvProvisionError::EnvUnresolvable { .. })
        ));

        let image = "registry.example/project/checks@sha256:not-revalidated".to_owned();
        let mut pinned = declaration(EnvKind::ContainerImage);
        pinned.image = Some(image.clone());
        assert_eq!(
            resolve_backend(&pinned),
            Ok(EnvBackend::ContainerImage { digest: image })
        );
    }

    #[test]
    fn posture_and_error_matches_are_exhaustive() {
        fn posture_name(posture: EnvPosture) -> &'static str {
            match posture {
                EnvPosture::Provision => "provision",
                EnvPosture::Checks => "checks",
            }
        }

        fn error_name(error: EnvProvisionError) -> &'static str {
            match error {
                EnvProvisionError::BackendUnavailable { .. } => "backend-unavailable",
                EnvProvisionError::EnvUnresolvable { .. } => "env-unresolvable",
                EnvProvisionError::DigestMismatch { .. } => "digest-mismatch",
                EnvProvisionError::ProvisionCommandFailed { .. } => "command-failed",
            }
        }

        assert_eq!(posture_name(EnvPosture::Provision), "provision");
        assert_eq!(posture_name(EnvPosture::Checks), "checks");
        assert_eq!(
            error_name(EnvProvisionError::EnvUnresolvable {
                detail: "missing".to_owned(),
            }),
            "env-unresolvable"
        );
    }

    #[test]
    fn nix_warms_once_with_the_provision_prefix_and_returns_requested_prefix() {
        let backend = EnvBackend::NixFlake {
            workdir: PathBuf::from("."),
            devshell: "ci".to_owned(),
        };
        let effects = FakeEffects::succeeding(&[""]);
        let provisioned = provision(&effects, &runner(), backend.clone(), EnvPosture::Checks)
            .expect("provision nix");
        let mut warm = runner().argv_prefix(&backend, EnvPosture::Provision);
        warm.push("true".to_owned());
        assert_eq!(&*effects.calls.borrow(), &[warm]);
        assert!(provisioned.warmed);
        assert_eq!(
            provisioned.posture_prefix,
            runner().argv_prefix(&backend, EnvPosture::Checks)
        );
    }

    #[test]
    fn container_pull_is_followed_by_digest_probe_and_exact_comparison() {
        let declared = "registry.example/checks@sha256:declared".to_owned();
        let backend = EnvBackend::ContainerImage {
            digest: declared.clone(),
        };
        let mismatch = FakeEffects::succeeding(&["", " registry.example/checks@sha256:actual\n"]);
        assert_eq!(
            provision(&mismatch, &runner(), backend.clone(), EnvPosture::Checks),
            Err(EnvProvisionError::DigestMismatch {
                expected: declared.clone(),
                actual: "registry.example/checks@sha256:actual".to_owned(),
            })
        );
        assert_eq!(mismatch.calls.borrow().len(), 2, "pull and probe both ran");
        assert_eq!(
            mismatch.calls.borrow()[1],
            vec![
                "podman",
                "image",
                "inspect",
                "--format",
                "{{index .RepoDigests 0}}",
                declared.as_str(),
            ]
        );

        let equal = FakeEffects::succeeding(&["", &format!(" {declared}\n")]);
        let provisioned = provision(&equal, &runner(), backend.clone(), EnvPosture::Provision)
            .expect("matching digest");
        assert_eq!(equal.calls.borrow().len(), 2, "probe happened after pull");
        assert_eq!(
            provisioned.posture_prefix,
            runner().argv_prefix(&backend, EnvPosture::Provision)
        );
        assert!(!provisioned.warmed);
    }

    #[test]
    fn unavailable_container_runtime_and_nonzero_commands_keep_exact_error_facts() {
        let backend = EnvBackend::ContainerImage {
            digest: "registry.example/checks@sha256:declared".to_owned(),
        };
        let unavailable = HostEnvRunner {
            container_runtime: None,
            mount_dir: PathBuf::from("/materialized/job"),
        };
        let unused = FakeEffects::succeeding(&[]);
        assert_eq!(
            provision(&unused, &unavailable, backend, EnvPosture::Checks),
            Err(EnvProvisionError::BackendUnavailable {
                backend: "container".to_owned(),
            })
        );
        assert!(unused.calls.borrow().is_empty());

        let backend = EnvBackend::NixFlake {
            workdir: PathBuf::from("."),
            devshell: "default".to_owned(),
        };
        let effects = FakeEffects {
            outputs: RefCell::new(vec![Ok(EffectOutput {
                status: 101,
                stdout: String::new(),
            })]),
            calls: RefCell::new(Vec::new()),
        };
        let mut expected_argv = runner().argv_prefix(&backend, EnvPosture::Provision);
        expected_argv.push("true".to_owned());
        assert_eq!(
            provision(&effects, &runner(), backend, EnvPosture::Checks),
            Err(EnvProvisionError::ProvisionCommandFailed {
                argv: expected_argv,
                status: 101,
            })
        );
    }

    #[test]
    fn provisioning_failures_are_infrastructure_even_for_status_101() {
        let errors = [
            EnvProvisionError::BackendUnavailable {
                backend: "container".to_owned(),
            },
            EnvProvisionError::EnvUnresolvable {
                detail: "bad declaration".to_owned(),
            },
            EnvProvisionError::DigestMismatch {
                expected: "expected".to_owned(),
                actual: "actual".to_owned(),
            },
            EnvProvisionError::ProvisionCommandFailed {
                argv: vec!["cargo".to_owned(), "test".to_owned()],
                status: 101,
            },
        ];
        for error in errors {
            assert_eq!(classify_provision_failure(&error), ProvisionOutcomeClass::Infra);
        }
    }

    #[test]
    fn every_refusal_has_execution_failed_unprovisionable_feedback() {
        let refusals = [
            ProvisionRefusal::DeclarationUnparsable {
                detail: "bad toml".to_owned(),
            },
            ProvisionRefusal::ReservedPath {
                path: "reserved".to_owned(),
            },
            ProvisionRefusal::EnvLockMissing {
                detail: "flake.lock".to_owned(),
            },
            ProvisionRefusal::Unprovisionable(EnvProvisionError::BackendUnavailable {
                backend: "container".to_owned(),
            }),
        ];
        for refusal in refusals {
            assert_eq!(
                refusal_feedback(&refusal),
                (gateway::ReasonCode::ExecutionFailed, ENV_UNPROVISIONABLE)
            );
        }
    }

    #[test]
    fn captured_declaration_bytes_are_the_exact_base_blob() {
        struct Tree(Vec<u8>);
        impl checks::BaseTree for Tree {
            fn blob_at(&self, path: &str) -> Result<Option<Vec<u8>>, ChecksError> {
                Ok(match path {
                    checks::DECLARATION_PATH => Some(self.0.clone()),
                    "flake.nix" | "flake.lock" => Some(b"base bytes".to_vec()),
                    _ => None,
                })
            }
        }
        let bytes = br#"schema = 1
[env]
kind = "nix-flake"
[checks]
prepare = []
commands = [["true"]]
timeout_secs = 60
"#
        .to_vec();
        let (captured, declaration) = capture_job_checks(&Tree(bytes.clone())).expect("capture");
        assert_eq!(captured, bytes);
        assert_eq!(declaration.env_kind, EnvKind::NixFlake);
    }
}
