//! Named agent presets → ACP stdio argv arrays.
//!
//! Sellers pick `--agent claude|cursor|codex` or any name from the config `[agents]` table;
//! raw `--agent-argv` remains the power-user hatch. A custom `[agents]` entry named after a
//! built-in OVERRIDES that built-in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::home::AgentPresetConfig;

/// Built-in preset names, in the order they are suggested/detected.
pub const BUILTIN_PRESETS: [&str; 3] = ["claude", "cursor", "codex"];

/// Where a resolved adapter will execute — which decides whether argv[0] is resolved against THIS
/// host's PATH or kept bare for a container's PATH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterHost {
    /// The adapter runs on this machine (pass-through / launcher sandbox). Resolve argv[0] to an
    /// absolute path via the host PATH and fail fast when it is absent.
    Host,
    /// The adapter runs inside the docker sandbox IMAGE (`[sandbox] mode = "docker"`). The host
    /// filesystem is irrelevant: keep argv[0] bare (e.g. `claude-agent-acp`) for the image's PATH,
    /// and never consult — or fail on — the host PATH. The image's contents are a BUILD-TIME
    /// guarantee (the sandbox Dockerfile bakes the adapters in), so there is nothing to probe.
    Container,
}

impl AdapterHost {
    /// Docker mode runs the adapter inside the image; every other executor runs it on the host.
    pub fn for_sandbox(sandbox: Option<&crate::home::SandboxConfig>) -> Self {
        match sandbox {
            Some(config) if matches!(config.mode, crate::home::SandboxMode::Docker) => {
                Self::Container
            }
            _ => Self::Host,
        }
    }
}

/// Resolve a preset name to an argv for the seller ACP driver, for an adapter that runs on the HOST.
/// The default for pass-through and launcher executors; docker mode calls [`resolve_agent_preset_in`]
/// with [`AdapterHost::Container`].
pub fn resolve_agent_preset(
    name: &str,
    custom: &BTreeMap<String, AgentPresetConfig>,
) -> Result<(String, Vec<String>), String> {
    resolve_agent_preset_in(name, custom, AdapterHost::Host)
}

/// Resolve a preset name to an argv for the seller ACP driver, for an adapter that runs on `host`.
///
/// Config-defined presets win over built-ins; the returned label is the preset name. A custom
/// preset's argv is the operator's and is returned verbatim in both modes. Only a built-in differs:
/// on the host it resolves argv[0] to an absolute path (fail-fast if absent), in a container it stays
/// a bare command for the image's PATH to resolve.
pub fn resolve_agent_preset_in(
    name: &str,
    custom: &BTreeMap<String, AgentPresetConfig>,
    host: AdapterHost,
) -> Result<(String, Vec<String>), String> {
    let trimmed = name.trim();
    let key = trimmed.to_ascii_lowercase();
    if let Some((configured, preset)) = custom
        .get_key_value(trimmed)
        .or_else(|| custom.get_key_value(key.as_str()))
    {
        if preset.argv.is_empty() {
            return Err(format!("agent preset {configured:?} has an empty argv"));
        }
        return Ok((configured.clone(), preset.argv.clone()));
    }
    match key.as_str() {
        "claude" => resolve_claude(host).map(|argv| ("claude".into(), argv)),
        "cursor" => resolve_cursor(host).map(|argv| ("cursor".into(), argv)),
        "codex" => resolve_codex(host).map(|argv| ("codex".into(), argv)),
        other => Err(format!(
            "unknown --agent {other:?} (want {}, or use --agent-argv)",
            preset_choices(custom)
        )),
    }
}

/// `claude|cursor|codex[|<custom>...]` — every accepted preset name, for messages.
pub fn preset_choices(custom: &BTreeMap<String, AgentPresetConfig>) -> String {
    let mut out = BUILTIN_PRESETS.join("|");
    for name in custom.keys() {
        if !BUILTIN_PRESETS.contains(&name.as_str()) {
            out.push('|');
            out.push_str(name);
        }
    }
    out
}

/// Which presets have a resolvable binary on PATH (custom: argv[0] on PATH or an existing
/// file path). A custom entry overriding a built-in name replaces that built-in's probe.
pub fn detect_available_agents(custom: &BTreeMap<String, AgentPresetConfig>) -> Vec<String> {
    let mut out = Vec::new();
    for name in BUILTIN_PRESETS {
        let available = match custom.get(name) {
            Some(preset) => custom_preset_available(preset),
            None => match name {
                // Available only when the ACP adapter binary the resolver actually launches is on
                // PATH — so doctor never reports an agent the seller cannot actually run.
                "claude" => which("claude-agent-acp").is_some(),
                "cursor" => which("cursor-agent").is_some() || which("agent").is_some(),
                "codex" => which("codex-acp").is_some(),
                _ => false,
            },
        };
        if available {
            out.push(name.to_owned());
        }
    }
    for (name, preset) in custom {
        if BUILTIN_PRESETS.contains(&name.as_str()) {
            continue;
        }
        if custom_preset_available(preset) {
            out.push(name.clone());
        }
    }
    out
}

fn custom_preset_available(preset: &AgentPresetConfig) -> bool {
    match preset.argv.first() {
        Some(argv0) => which(argv0).is_some() || Path::new(argv0).is_file(),
        None => false,
    }
}

/// Resolve a built-in ACP adapter for an adapter that runs on `host`. `candidates` are the accepted
/// argv[0] names in preference order; `tail` are the fixed args that follow (cursor's `acp`). On the
/// host the first candidate found on PATH is expanded to its absolute path, failing with `not_found`
/// when none resolve; in a container the FIRST candidate is kept bare for the image's PATH — no host
/// lookup, no host-absence failure.
fn resolve_adapter(
    candidates: &[&str],
    tail: &[&str],
    host: AdapterHost,
    not_found: &str,
) -> Result<Vec<String>, String> {
    let argv0 = match host {
        AdapterHost::Container => candidates[0].to_owned(),
        AdapterHost::Host => match candidates.iter().find_map(|name| which(name)) {
            Some(bin) => bin.to_string_lossy().into_owned(),
            None => return Err(not_found.to_owned()),
        },
    };
    let mut argv = vec![argv0];
    argv.extend(tail.iter().map(|arg| (*arg).to_owned()));
    Ok(argv)
}

fn resolve_claude(host: AdapterHost) -> Result<Vec<String>, String> {
    resolve_adapter(
        &["claude-agent-acp"],
        &[],
        host,
        "claude ACP adapter not found on PATH: install it \
         (npm i -g @agentclientprotocol/claude-agent-acp) or put claude-agent-acp on PATH",
    )
}

fn resolve_cursor(host: AdapterHost) -> Result<Vec<String>, String> {
    resolve_adapter(
        &["cursor-agent", "agent"],
        &["acp"],
        host,
        "cursor ACP adapter not found on PATH: install the cursor agent and put \
         cursor-agent (or agent) on PATH",
    )
}

fn resolve_codex(host: AdapterHost) -> Result<Vec<String>, String> {
    resolve_adapter(
        &["codex-acp"],
        &[],
        host,
        "codex ACP adapter not found on PATH: install it \
         (npm i -g @agentclientprotocol/codex-acp) or put codex-acp on PATH",
    )
}

/// What a built-in preset needs BEYOND its ACP adapter binary: the underlying agent CLI, installed
/// and authenticated (#488).
///
/// Resolving the adapter proves only that a binary exists. Every built-in adapter then drives a
/// separate agent CLI that carries its own credentials, so a seat can pass every resolution check
/// and still fail its first probe turn with an auth error. Returns `None` for custom `[agents]`
/// presets — their prerequisites are the operator's to know, and inventing one would be worse than
/// saying nothing.
///
/// This is operator guidance, NOT a check: nothing here reads the credential. The pre-advertise
/// probe remains the only thing that proves an authenticated turn is possible.
pub fn preset_prerequisite(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(
            "the `claude` CLI (npm i -g @anthropic-ai/claude-code), signed in — run `claude` once \
             and complete `/login`, or set ANTHROPIC_API_KEY in the daemon's environment",
        ),
        "cursor" => Some(
            "the Cursor agent CLI itself — `cursor-agent` is both the adapter and the agent, so it \
             needs no extra shim, but it must be signed in (`cursor-agent login`)",
        ),
        "codex" => Some(
            "the `codex` CLI (npm i -g @openai/codex), signed in — run `codex login`, or set \
             OPENAI_API_KEY in the daemon's environment",
        ),
        _ => None,
    }
}

/// The sentence that follows any [`preset_prerequisite`]: what happens if the prerequisite is not
/// met. Kept next to the prerequisites so the two never drift out of agreement.
pub const PREREQUISITE_ENFORCEMENT: &str =
    "the seat refuses to advertise until a probe turn actually succeeds, so an unauthenticated \
     CLI fails at boot, not silently mid-job";

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(entries: &[(&str, &[&str])]) -> BTreeMap<String, AgentPresetConfig> {
        entries
            .iter()
            .map(|(name, argv)| {
                (
                    (*name).to_owned(),
                    AgentPresetConfig {
                        argv: argv.iter().map(|a| (*a).to_owned()).collect(),
                    },
                )
            })
            .collect()
    }

    /// #488: the gap was a seat passing every resolution check and then failing its probe on
    /// `Authentication required`. Enumerating over `BUILTIN_PRESETS` (not a hand-listed three)
    /// means a preset added later cannot ship without saying how it is authenticated.
    #[test]
    fn every_builtin_preset_states_an_auth_prerequisite() {
        for name in BUILTIN_PRESETS {
            let prerequisite = preset_prerequisite(name)
                .unwrap_or_else(|| panic!("built-in preset {name} has no prerequisite line"));
            // Naming an install alone is what the docs already did and is exactly what let the
            // auth failure through — each line must name a way to authenticate.
            let names_auth = prerequisite.contains("login")
                || prerequisite.contains("signed in")
                || prerequisite.contains("API_KEY");
            assert!(
                names_auth,
                "prerequisite for {name} names no auth step: {prerequisite}"
            );
        }
    }

    /// Case and surrounding space are normalized exactly as `resolve_agent_preset` does, so the
    /// label a caller resolved always finds its own prerequisite.
    #[test]
    fn prerequisite_lookup_normalizes_like_the_resolver() {
        assert_eq!(preset_prerequisite("  CoDeX "), preset_prerequisite("codex"));
        assert!(preset_prerequisite("codex").is_some());
    }

    /// A custom `[agents]` entry gets no invented prerequisite — saying nothing beats guessing.
    #[test]
    fn custom_and_unknown_presets_have_no_prerequisite() {
        assert!(preset_prerequisite("my-own-agent").is_none());
        assert!(preset_prerequisite("").is_none());
    }

    #[test]
    fn builtin_presets_resolve_to_binary_or_install_hint() {
        // A built-in resolves to a non-empty argv only when its ACP adapter binary is on PATH;
        // otherwise it fails with an install hint (no npx auto-launch fallback).
        let none = BTreeMap::new();
        for name in BUILTIN_PRESETS {
            match resolve_agent_preset(name, &none) {
                Ok((label, argv)) => {
                    assert_eq!(label, name);
                    assert!(!argv.is_empty());
                    assert!(argv.iter().all(|p| !p.is_empty()));
                    assert!(
                        !argv.iter().any(|p| p == "npx"),
                        "{name} must not resolve to an npx fallback: {argv:?}"
                    );
                }
                Err(message) => assert!(
                    message.contains("install") && message.contains("PATH"),
                    "{name} missing-adapter error must carry an install hint: {message:?}"
                ),
            }
        }
    }

    #[test]
    fn unknown_preset_errors() {
        assert!(resolve_agent_preset("goose", &BTreeMap::new()).is_err());
    }

    #[test]
    fn custom_preset_resolves_to_configured_argv() {
        let table = custom(&[("grok", &["grok", "agent", "stdio"])]);
        let (label, argv) = resolve_agent_preset("grok", &table).expect("custom preset");
        assert_eq!(label, "grok");
        assert_eq!(argv, vec!["grok", "agent", "stdio"]);
    }

    #[test]
    fn custom_preset_overrides_builtin() {
        let table = custom(&[("codex", &["my-codex-acp", "--stdio"])]);
        let (label, argv) = resolve_agent_preset("codex", &table).expect("override");
        assert_eq!(label, "codex");
        assert_eq!(argv, vec!["my-codex-acp", "--stdio"]);
    }

    // In docker mode the adapter lives in the IMAGE, so resolution must NOT consult the host PATH and
    // must NOT fail when the host lacks the adapter (the bug that blocked every docker-mode seller on
    // a host without node). It returns the bare command for the container's PATH to resolve.
    #[test]
    fn container_mode_resolves_builtins_bare_without_touching_the_host() {
        let none = BTreeMap::new();
        let (label, argv) = resolve_agent_preset_in("claude", &none, AdapterHost::Container)
            .expect("container resolution never fails on host absence");
        assert_eq!(label, "claude");
        assert_eq!(argv, vec!["claude-agent-acp"], "bare command, no host absolute path");
        let (_, argv) =
            resolve_agent_preset_in("codex", &none, AdapterHost::Container).expect("codex");
        assert_eq!(argv, vec!["codex-acp"]);
        // A custom preset is the operator's argv verbatim in either mode.
        let table = custom(&[("grok", &["grok", "acp"])]);
        let (_, argv) =
            resolve_agent_preset_in("grok", &table, AdapterHost::Container).expect("custom");
        assert_eq!(argv, vec!["grok", "acp"]);
    }

    // The adapter runs in the image only under `[sandbox] mode = "docker"`; every other executor runs
    // it on the host, so the mode selects the resolution target.
    #[test]
    fn adapter_host_follows_the_sandbox_mode() {
        use crate::home::{SandboxConfig, SandboxMode};
        assert_eq!(AdapterHost::for_sandbox(None), AdapterHost::Host);
        let docker = SandboxConfig {
            mode: SandboxMode::Docker,
            image: Some("img".into()),
            ..Default::default()
        };
        assert_eq!(AdapterHost::for_sandbox(Some(&docker)), AdapterHost::Container);
        let launcher = SandboxConfig {
            mode: SandboxMode::Launcher,
            ..Default::default()
        };
        assert_eq!(AdapterHost::for_sandbox(Some(&launcher)), AdapterHost::Host);
    }

    #[test]
    fn unknown_preset_error_lists_builtins_and_configured_names() {
        let table = custom(&[("grok", &["grok", "agent", "stdio"])]);
        let message = resolve_agent_preset("goose", &table).expect_err("unknown");
        for name in ["claude", "cursor", "codex", "grok"] {
            assert!(message.contains(name), "{message:?} missing {name}");
        }
    }

    #[test]
    fn detect_includes_custom_preset_with_existing_file_path() {
        let file = std::env::temp_dir().join(format!(
            "maxplayer-agent-preset-detect-{}",
            std::process::id()
        ));
        std::fs::write(&file, "#!/bin/sh\n").expect("write probe file");
        let table = custom(&[("mine", &[file.to_str().expect("utf8 path"), "stdio"])]);
        let detected = detect_available_agents(&table);
        std::fs::remove_file(&file).ok();
        assert!(detected.contains(&"mine".to_owned()), "{detected:?}");
    }

    #[test]
    fn detect_excludes_custom_preset_with_unresolvable_argv0() {
        let table = custom(&[(
            "ghost",
            &["maxplayer-test-binary-that-definitely-does-not-exist-4c1f"],
        )]);
        let detected = detect_available_agents(&table);
        assert!(!detected.contains(&"ghost".to_owned()), "{detected:?}");
    }
}
