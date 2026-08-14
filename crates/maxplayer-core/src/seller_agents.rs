//! The seller's harness registry: which agent harnesses this ONE node can run, and which one a
//! given job dispatches to.
//!
//! A node lists the harnesses it enables in `[seller] agents` (order = preference); each name is a
//! preset from [`crate::agent_presets`], so the same `claude|cursor|codex|<custom>` vocabulary the
//! CLI accepts is the vocabulary the wire advertises. The registry is resolved ONCE at boot —
//! every listed preset is checked for a launchable adapter and each gets its own PASS/FAIL verdict,
//! so a missing adapter is a named line rather than a runtime surprise mid-job.
//!
//! Two rules the rest of the node leans on:
//!
//! - **Advertise only what is dispatchable.** [`AgentRegistry::advertised`] is exactly the set
//!   [`AgentRegistry::dispatch`] can serve, so a buyer reading the wire and the node choosing a
//!   harness can never disagree. A raw `--agent-argv` seller carries no preset label, so it
//!   advertises nothing — there is no honest harness name to publish.
//! - **A named request is exact or nothing.** A job requesting harness `X` dispatches to `X` or is
//!   not served; there is no nearest-match fallback, because silently running a job on a harness
//!   the buyer did not ask for is the failure this registry exists to prevent.
//!
//! How many awarded jobs run at once is governed by the homogeneous node-level `[seller] slots`
//! (see [`crate::home::SellerConfig::slots`]) — every slot runs whichever harness the job asked for.
//! Issue #378 removed the per-entry `{ name, slots }` pool count: `agents` is a plain list of harness
//! names, and this homogeneous node-level count is the only concurrency knob.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::agent_presets::{resolve_agent_preset_in, AdapterHost};
use crate::home::{AgentPresetConfig, SellerConfig};

/// Wire tag naming the harnesses an event's author can run — `["agents", "claude", "codex"]`,
/// ordered by the seller's preference. Multi-value, so a single-harness seat publishes
/// `["agents", "claude"]`, the one-entry case of the same tag.
///
/// One constant, two emit sites: the kind-30340 seat announcement (§4.2) and the kind-3402 claim
/// (§6.2). Both spell it `agents` — issue #645 renamed it from the singular-sounding `mobee_agent`,
/// which the spec never used.
pub const AGENT_TAG: &str = "agents";

/// The offer parameter naming a requested harness: `["param", "agent", "claude"]`, a sibling of
/// `["param", "deadline", …]`. The value is opaque to the wire — an exact harness name today,
/// leaving room for a tier vocabulary later without a grammar change.
pub const AGENT_PARAM: &str = "agent";

/// The reserved "no preference" request value. Explicitly equivalent to omitting the parameter, so
/// a buyer can state indifference rather than only imply it.
pub const AGENT_ANY: &str = "any";

/// One harness the node can actually launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredAgent {
    /// The preset label — the public harness identity, advertised on the wire and matched against
    /// a job's request. `None` for the raw `--agent-argv` hatch: an argv with no preset label has
    /// no harness name we can honestly publish.
    pub name: Option<String>,
    /// The launch argv for the ACP driver (no shell).
    pub argv: Vec<String>,
}

/// One listed preset's boot verdict. A FAIL never names an argv — it names the reason, so the
/// degrade line tells the operator what to install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentVerdict {
    pub name: String,
    /// `Ok` ⇒ resolved and launchable. `Err` ⇒ the resolver's reason (missing adapter, empty argv,
    /// unknown preset name).
    pub outcome: Result<Vec<String>, String>,
}

impl AgentVerdict {
    /// `PASS <name> binary resolves argv0=<bin> (…)` / `FAIL <name>: <reason>` — one line per
    /// listed preset.
    ///
    /// The PASS wording names only what this check had access to: the adapter binary was found.
    /// It says nothing about whether the underlying agent CLI is authenticated — a resolvable
    /// adapter with no credentials still fails the pre-advertise probe (#488, the #252 class).
    /// Naming the limit here keeps a PASS from reading as "this seat can do work".
    pub fn line(&self) -> String {
        match &self.outcome {
            Ok(argv) => format!(
                "PASS {} binary resolves argv0={} (auth not checked here — proven at the pre-advertise probe)",
                self.name,
                argv.first().map(String::as_str).unwrap_or("")
            ),
            Err(reason) => format!("FAIL {}: {reason}", self.name),
        }
    }

    pub fn passed(&self) -> bool {
        self.outcome.is_ok()
    }
}

/// Why a registry could not be resolved at all. Both variants REFUSE the boot: a node that cannot
/// launch anything, or one whose config asks for capacity the engine does not have, must not serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// Every listed preset failed to resolve — there is nothing to run jobs with.
    AllFailed(Vec<AgentVerdict>),
    /// No harness at all: neither an `agents` list nor an `agent_command` to fall back to.
    Empty,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AllFailed(verdicts) => {
                write!(
                    formatter,
                    "no usable agent harness: every configured preset failed to resolve [{}]",
                    verdicts
                        .iter()
                        .map(AgentVerdict::line)
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            }
            Self::Empty => write!(
                formatter,
                "no agent harness configured: set [seller] agents = [\"claude\", …] or agent_command"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

/// The harnesses a booted node can run, in preference order. Built by [`resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRegistry {
    entries: Vec<RegisteredAgent>,
}

impl AgentRegistry {
    /// Build directly from resolved entries. Prefer [`resolve`]; this exists for tests and for
    /// callers holding an already-resolved set.
    pub fn new(entries: Vec<RegisteredAgent>) -> Self {
        Self { entries }
    }

    /// Every entry, in preference order.
    pub fn entries(&self) -> &[RegisteredAgent] {
        &self.entries
    }

    /// The harness names to advertise on the wire, in preference order. Exactly the set
    /// [`Self::dispatch`] can serve by name — unlabelled hatch entries are omitted (nothing honest
    /// to publish), so an empty list means "this seller states no harness", never "it has none".
    pub fn advertised(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|entry| entry.name.clone())
            .collect()
    }

    /// The harness that will run a job requesting `requested`.
    ///
    /// `None` or [`AGENT_ANY`] ⇒ the preferred (first) entry. A named request matches by exact
    /// preset name and NOTHING else: an unmatched name returns `None` so the caller declines,
    /// rather than running the job on a harness the buyer did not ask for.
    pub fn dispatch(&self, requested: Option<&str>) -> Option<&RegisteredAgent> {
        match normalize_request(requested) {
            None => self.entries.first(),
            Some(name) => self
                .entries
                .iter()
                .find(|entry| entry.name.as_deref() == Some(name.as_str())),
        }
    }

    /// Whether a job requesting `requested` can be served at all — the claim-decision predicate.
    pub fn serves(&self, requested: Option<&str>) -> bool {
        self.dispatch(requested).is_some()
    }
}

/// Canonicalise a requested harness: trims, lowercases, and maps both absence and [`AGENT_ANY`]
/// (and an empty value) to "no preference", so `None`, `""`, `"any"` and `" ANY "` are one case.
pub fn normalize_request(requested: Option<&str>) -> Option<String> {
    let value = requested?.trim().to_ascii_lowercase();
    if value.is_empty() || value == AGENT_ANY {
        return None;
    }
    Some(value)
}

/// The on-disk credential directory for a registered harness, if we know one.
///
/// Only the three built-in preset labels have a known directory under `user_home`:
/// `claude` → `<home>/.claude`, `cursor` → `<home>/.cursor`, `codex` → `<home>/.codex`.
/// A raw `--agent-argv` hatch (`name == None`) and any other label return `None`.
///
/// The argv is never consulted. Guessing a path from a binary name would inspect the wrong
/// directory and pass — worse than reporting that we cannot resolve (issue #715). Absence is
/// carried, not invented.
pub fn harness_credential_dir(agent: &RegisteredAgent, user_home: &Path) -> Option<PathBuf> {
    match agent.name.as_deref()?.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(user_home.join(".claude")),
        "cursor" => Some(user_home.join(".cursor")),
        "codex" => Some(user_home.join(".codex")),
        _ => None,
    }
}

/// Resolved registry plus the per-preset verdicts that produced it. `verdicts` is empty when the
/// node fell back to the single `agent_command` (nothing was listed to verdict).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRegistry {
    pub registry: AgentRegistry,
    pub verdicts: Vec<AgentVerdict>,
}

impl ResolvedRegistry {
    /// The listed presets that failed to resolve. Non-empty alongside a usable registry means the
    /// node is DEGRADED: it serves with the remainder and advertises only those.
    pub fn failures(&self) -> Vec<&AgentVerdict> {
        self.verdicts.iter().filter(|v| !v.passed()).collect()
    }

    /// The loud degrade line, or `None` when every listed preset resolved.
    pub fn degrade_line(&self) -> Option<String> {
        let failures = self.failures();
        if failures.is_empty() {
            return None;
        }
        Some(format!(
            "seller node DEGRADED: {} of {} configured agents unavailable [{}]; serving with {:?}",
            failures.len(),
            self.verdicts.len(),
            failures
                .iter()
                .map(|v| v.line())
                .collect::<Vec<_>>()
                .join("; "),
            self.registry.advertised()
        ))
    }
}

/// Resolve the node's harness registry from its seller config.
///
/// `[seller] agents` is the registry when present: each name resolves through the preset table,
/// every listed name gets a verdict, and the node serves with whatever resolved. When the list is
/// absent the node falls back to the single configured harness — the raw `agent_command`, UNLABELLED
/// (issue #378 removed the singular `agent` label; a wire harness name comes from listing it in
/// `agents`). The stored argv is dispatched verbatim, never re-resolved.
pub fn resolve(
    seller: &SellerConfig,
    presets: &BTreeMap<String, AgentPresetConfig>,
    host: AdapterHost,
) -> Result<ResolvedRegistry, RegistryError> {
    if seller.agents.is_empty() {
        return fallback_registry(seller);
    }

    let mut verdicts = Vec::with_capacity(seller.agents.len());
    let mut entries = Vec::new();
    for name in &seller.agents {
        match resolve_agent_preset_in(name, presets, host) {
            Ok((label, argv)) => {
                verdicts.push(AgentVerdict {
                    name: label.clone(),
                    outcome: Ok(argv.clone()),
                });
                // A duplicate name would shadow itself on dispatch; keep the first (preference
                // order) so the registry and its advertisement stay one-to-one.
                if !entries
                    .iter()
                    .any(|e: &RegisteredAgent| e.name.as_deref() == Some(label.as_str()))
                {
                    entries.push(RegisteredAgent {
                        name: Some(label),
                        argv,
                    });
                }
            }
            Err(reason) => verdicts.push(AgentVerdict {
                name: name.clone(),
                outcome: Err(reason),
            }),
        }
    }

    if entries.is_empty() {
        return Err(RegistryError::AllFailed(verdicts));
    }
    Ok(ResolvedRegistry {
        registry: AgentRegistry::new(entries),
        verdicts,
    })
}

/// The single-harness registry: the stored `agent_command`, UNLABELLED — the raw-argv hatch. Issue
/// #378 removed the singular `agent` label, so a fallback harness has no wire name (a seller that
/// wants one lists the harness in `agents`). The argv is taken from config as-is and never
/// re-resolved, so an existing seller launches the same binary it launched before.
fn fallback_registry(seller: &SellerConfig) -> Result<ResolvedRegistry, RegistryError> {
    if seller.agent_command.is_empty() {
        return Err(RegistryError::Empty);
    }
    Ok(ResolvedRegistry {
        registry: AgentRegistry::new(vec![RegisteredAgent {
            name: None,
            argv: seller.agent_command.clone(),
        }]),
        verdicts: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seller_with(agents: Vec<String>) -> SellerConfig {
        SellerConfig {
            agent_command: vec!["fallback-bin".into()],
            rate_sats: 5,
            git_remote: "https://example.invalid/repo".into(),
            job_timeout_secs: None,
            agents,
            claim_open_pool: false,
            accept_offers_only_from: Vec::new(),
            offer_backfill_secs: 0,
            contribution_enabled: true,
            slots: 1,
            claim_award_timeout_secs: None,
        }
    }

    /// `["a", "b"]` as owned Strings — the shape `[seller] agents` now parses to.
    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn presets(entries: &[(&str, &[&str])]) -> BTreeMap<String, AgentPresetConfig> {
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

    fn registry(names: &[&str]) -> AgentRegistry {
        AgentRegistry::new(
            names
                .iter()
                .map(|name| RegisteredAgent {
                    name: Some((*name).to_owned()),
                    argv: vec![format!("{name}-acp")],
                })
                .collect(),
        )
    }

    #[test]
    fn a_named_request_dispatches_to_that_harness_and_nothing_else() {
        // Invariant 2, the pure core: harness X runs X's argv, and a request the registry cannot
        // serve dispatches to NOTHING rather than falling back to the preferred entry.
        let registry = registry(&["claude", "codex"]);
        assert_eq!(
            registry.dispatch(Some("codex")).map(|a| a.argv.clone()),
            Some(vec!["codex-acp".to_owned()])
        );
        assert_eq!(
            registry.dispatch(Some("claude")).map(|a| a.argv.clone()),
            Some(vec!["claude-acp".to_owned()])
        );
        assert_eq!(registry.dispatch(Some("goose")), None);
        assert!(!registry.serves(Some("goose")));
    }

    #[test]
    fn no_request_takes_the_preferred_entry_and_any_is_the_same_case() {
        let registry = registry(&["codex", "claude"]);
        let preferred = registry.dispatch(None).expect("preferred");
        assert_eq!(preferred.name.as_deref(), Some("codex"));
        for indifferent in ["any", "ANY", "  any  ", ""] {
            assert_eq!(
                registry.dispatch(Some(indifferent)).map(|a| a.name.clone()),
                Some(Some("codex".to_owned())),
                "{indifferent:?} must mean no preference"
            );
        }
        // Case/whitespace on a NAMED request resolves too — the wire value is canonicalised.
        assert_eq!(
            registry.dispatch(Some(" Claude ")).map(|a| a.name.clone()),
            Some(Some("claude".to_owned()))
        );
    }

    #[test]
    fn advertised_is_exactly_what_dispatch_serves() {
        // The wire promise and the dispatch table are one set — a buyer can never read a harness
        // the node would then refuse to run.
        let registry = registry(&["claude", "codex"]);
        let advertised = registry.advertised();
        assert_eq!(advertised, vec!["claude", "codex"]);
        for name in &advertised {
            assert!(registry.serves(Some(name)), "advertised {name} must dispatch");
        }
    }

    #[test]
    fn unlabelled_argv_hatch_advertises_nothing_but_still_runs_untargeted_jobs() {
        let seller = seller_with(Vec::new());
        let resolved = resolve(&seller, &BTreeMap::new(), AdapterHost::Host).expect("hatch resolves");
        assert!(
            resolved.registry.advertised().is_empty(),
            "a raw argv seller has no honest harness name to publish"
        );
        assert_eq!(
            resolved.registry.dispatch(None).map(|a| a.argv.clone()),
            Some(vec!["fallback-bin".to_owned()])
        );
        // …and it cannot serve a named request, because it cannot prove what it is.
        assert!(!resolved.registry.serves(Some("claude")));
    }

    #[test]
    fn a_single_agents_entry_resolves_the_preset_and_advertises_its_name() {
        // Issue #378: the singular `agent` label is gone; a one-harness seller lists it in `agents`,
        // which resolves through the preset table and advertises exactly that name.
        let table = presets(&[("claude", &["claude-acp"])]);
        let seller = seller_with(names(&["claude"]));
        let resolved = resolve(&seller, &table, AdapterHost::Host).expect("single preset resolves");
        assert_eq!(resolved.registry.advertised(), vec!["claude"]);
        assert_eq!(
            resolved.registry.dispatch(None).map(|a| a.argv.clone()),
            Some(vec!["claude-acp".to_owned()]),
            "a listed preset resolves through the preset table"
        );
        assert_eq!(resolved.verdicts.len(), 1);
        assert!(resolved.degrade_line().is_none());
    }

    #[test]
    fn partial_failure_degrades_loud_and_serves_with_the_remainder() {
        let table = presets(&[("good", &["/bin/sh"])]);
        let seller = seller_with(names(&["good", "nope-not-a-preset"]));
        let resolved = resolve(&seller, &table, AdapterHost::Host).expect("partial failure still serves");
        assert_eq!(resolved.registry.advertised(), vec!["good"]);
        assert_eq!(resolved.failures().len(), 1);
        let line = resolved.degrade_line().expect("degrade line");
        assert!(line.contains("DEGRADED"), "{line}");
        assert!(line.contains("nope-not-a-preset"), "{line}");
        // The reduced advertisement is part of the loud line, not just an internal state.
        assert!(line.contains("good"), "{line}");
        // And the failed harness is not dispatchable.
        assert!(!resolved.registry.serves(Some("nope-not-a-preset")));
    }

    #[test]
    fn every_preset_failing_refuses_rather_than_serving_nothing() {
        let seller = seller_with(names(&["nope-one", "nope-two"]));
        match resolve(&seller, &BTreeMap::new(), AdapterHost::Host) {
            Err(RegistryError::AllFailed(verdicts)) => {
                assert_eq!(verdicts.len(), 2);
                assert!(verdicts.iter().all(|v| !v.passed()));
                let message = RegistryError::AllFailed(verdicts).to_string();
                assert!(message.contains("nope-one") && message.contains("nope-two"), "{message}");
            }
            other => panic!("all-fail must refuse the boot, got {other:?}"),
        }
    }

    #[test]
    fn a_duplicate_listing_keeps_one_entry_so_advertisement_matches_dispatch() {
        let table = presets(&[("good", &["/bin/sh"])]);
        let seller = seller_with(names(&["good", "good"]));
        let resolved = resolve(&seller, &table, AdapterHost::Host).expect("duplicates resolve");
        assert_eq!(resolved.registry.advertised(), vec!["good"]);
        assert_eq!(resolved.registry.entries().len(), 1);
    }

    #[test]
    fn no_harness_at_all_refuses() {
        let mut seller = seller_with(Vec::new());
        seller.agent_command = Vec::new();
        assert_eq!(resolve(&seller, &BTreeMap::new(), AdapterHost::Host), Err(RegistryError::Empty));
    }

    // ---- Issue #715: harness credential directory is resolved from the preset label, never guessed ----

    fn labelled(name: &str) -> RegisteredAgent {
        RegisteredAgent {
            name: Some(name.to_owned()),
            argv: vec![format!("{name}-acp")],
        }
    }

    /// RED-PROVE: a raw `--agent-argv` hatch has no preset label. Mapping it to a default
    /// harness's directory (or sniffing argv for "claude") would inspect the wrong path and
    /// pass — the failure #715 names as worse than no check. Drop the `name == None ⇒ None`
    /// arm (or consult argv) and this goes red.
    #[test]
    fn harness_credential_dir_never_guesses_from_an_unlabelled_hatch() {
        use std::path::Path;
        let home = Path::new("/home/seat");
        let hatch = RegisteredAgent {
            name: None,
            argv: vec![
                "claude-agent-acp".into(),
                "/home/seat/.claude/bin/claude".into(),
            ],
        };
        assert_eq!(
            harness_credential_dir(&hatch, home),
            None,
            "an unlabelled hatch must not resolve, even when argv names a known harness"
        );
        assert_eq!(
            harness_credential_dir(&labelled("grok"), home),
            None,
            "an unknown label must not fall back to a built-in's directory"
        );
        assert_eq!(
            harness_credential_dir(&labelled("my-claude"), home),
            None,
            "a substring of a built-in name is not a built-in"
        );
    }

    /// Every built-in preset maps to a directory under the given home. Enumerating
    /// `BUILTIN_PRESETS` (not a hand-listed three) means a preset added later cannot ship
    /// without a known credential directory — guessing nothing for a known harness is the
    /// same class of bug as guessing a path for an unknown one.
    #[test]
    fn every_builtin_preset_maps_to_a_credential_directory_under_user_home() {
        use std::path::Path;
        let home = Path::new("/home/seat");
        for name in crate::agent_presets::BUILTIN_PRESETS {
            let dir = harness_credential_dir(&labelled(name), home)
                .unwrap_or_else(|| panic!("built-in preset {name} has no credential directory"));
            assert_eq!(dir, home.join(format!(".{name}")));
            assert!(
                dir.starts_with(home),
                "{name} credential dir must live under the given user home, not a hardcoded path: {}",
                dir.display()
            );
        }
        // Case/whitespace follow the same normalisation as dispatch, so a label the registry
        // stored in any case still finds its directory.
        assert_eq!(
            harness_credential_dir(&labelled(" Claude "), home),
            harness_credential_dir(&labelled("claude"), home)
        );
    }
}
