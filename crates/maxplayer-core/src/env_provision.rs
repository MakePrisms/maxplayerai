//! Pure environment-backend resolution and command-line composition.
//!
//! Host availability checks, environment warming, and process spawning belong to later layers.

use std::fmt;
use std::path::PathBuf;

use crate::checks::{ChecksDeclaration, EnvKind};

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEnvRunner {
    pub container_runtime: String,
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
                    self.container_runtime.clone(),
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
            container_runtime: "podman".to_owned(),
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
    fn provisioned_env_prefix_comes_from_runner_for_the_same_posture() {
        let backend = EnvBackend::ContainerImage {
            digest: "image@sha256:digest".to_owned(),
        };
        for posture in [EnvPosture::Provision, EnvPosture::Checks] {
            let posture_prefix = runner().argv_prefix(&backend, posture);
            let provisioned = ProvisionedEnv {
                backend: backend.clone(),
                posture_prefix: posture_prefix.clone(),
                warmed: posture == EnvPosture::Provision,
            };
            assert_eq!(provisioned.posture_prefix, posture_prefix);
        }
    }
}
