//! Named agent presets → ACP stdio argv arrays.
//!
//! Sellers pick `--agent claude|cursor|codex` or any name from the config `[agents]` table;
//! raw `--agent-argv` remains the power-user hatch. A custom `[agents]` entry named after a
//! built-in OVERRIDES that built-in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mobee_core::home::AgentPresetConfig;

/// Built-in preset names, in the order they are suggested/detected.
pub const BUILTIN_PRESETS: [&str; 3] = ["claude", "cursor", "codex"];

/// Resolve a preset name to an argv array suitable for the seller ACP driver.
///
/// Config-defined presets win over built-ins; the returned label is the preset name.
pub fn resolve_agent_preset(
    name: &str,
    custom: &BTreeMap<String, AgentPresetConfig>,
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
        "claude" => resolve_claude().map(|argv| ("claude".into(), argv)),
        "cursor" => resolve_cursor().map(|argv| ("cursor".into(), argv)),
        "codex" => resolve_codex().map(|argv| ("codex".into(), argv)),
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

fn resolve_claude() -> Result<Vec<String>, String> {
    match which("claude-agent-acp") {
        Some(bin) => Ok(vec![bin.to_string_lossy().into_owned()]),
        None => Err(
            "claude ACP adapter not found on PATH: install it \
             (npm i -g @agentclientprotocol/claude-agent-acp) or put claude-agent-acp on PATH"
                .into(),
        ),
    }
}

fn resolve_cursor() -> Result<Vec<String>, String> {
    match which("cursor-agent").or_else(|| which("agent")) {
        Some(bin) => Ok(vec![bin.to_string_lossy().into_owned(), "acp".into()]),
        None => Err(
            "cursor ACP adapter not found on PATH: install the cursor agent and put \
             cursor-agent (or agent) on PATH"
                .into(),
        ),
    }
}

fn resolve_codex() -> Result<Vec<String>, String> {
    match which("codex-acp") {
        Some(bin) => Ok(vec![bin.to_string_lossy().into_owned()]),
        None => Err(
            "codex ACP adapter not found on PATH: install it \
             (npm i -g @agentclientprotocol/codex-acp) or put codex-acp on PATH"
                .into(),
        ),
    }
}

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
            "mobee-agent-preset-detect-{}",
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
            &["mobee-test-binary-that-definitely-does-not-exist-4c1f"],
        )]);
        let detected = detect_available_agents(&table);
        assert!(!detected.contains(&"ghost".to_owned()), "{detected:?}");
    }
}
