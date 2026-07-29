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
//! (see [`crate::home::SellerConfig::slots`]) — every slot runs whichever harness the job asked
//! for. The PER-ENTRY pool count (`agents = [{ name, slots }]`) is a different, heterogeneous knob
//! and is still parsed and REFUSED above 1 (see [`RegistryError::ParallelismUnsupported`]):
//! per-harness slot pools are out of scope for V1, whose slots are homogeneous.

use std::collections::BTreeMap;

use crate::agent_presets::resolve_agent_preset;
use crate::home::{AgentPresetConfig, SellerConfig};

/// Wire tag naming the harnesses an event's author can run — `["mobee_agent", "claude", "codex"]`,
/// ordered by the seller's preference. Multi-value (the `protocol_versions` convention), so issue
/// #170's single-harness `["mobee_agent", "claude"]` is the one-entry case of the same tag.
pub const AGENT_TAG: &str = "mobee_agent";

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
    /// `PASS <name> argv0=<bin>` / `FAIL <name>: <reason>` — one line per listed preset.
    pub fn line(&self) -> String {
        match &self.outcome {
            Ok(argv) => format!(
                "PASS {} argv0={}",
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
    /// The config declares a pool larger than one. Parallel execution is not implemented; running
    /// such a config SERIALLY would quietly deliver a fraction of the declared capacity, so it is
    /// refused rather than downgraded.
    ParallelismUnsupported { name: String, slots: u32 },
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
            Self::ParallelismUnsupported { name, slots } => write!(
                formatter,
                "agent {name:?} declares slots={slots}: per-harness slot pools are not supported \
                 (V1 slots are homogeneous — use node-level `[seller] slots` instead). Set slots = 1."
            ),
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
/// absent the node falls back to the single configured harness — the stored `agent_command` under
/// its `agent` preset label — so a seller written before this existed resolves to the identical
/// one-entry registry and dispatches the identical argv.
pub fn resolve(
    seller: &SellerConfig,
    presets: &BTreeMap<String, AgentPresetConfig>,
) -> Result<ResolvedRegistry, RegistryError> {
    if seller.agents.is_empty() {
        return fallback_registry(seller);
    }

    let mut verdicts = Vec::with_capacity(seller.agents.len());
    let mut entries = Vec::new();
    for slot in &seller.agents {
        if slot.slots != 1 {
            return Err(RegistryError::ParallelismUnsupported {
                name: slot.name.clone(),
                slots: slot.slots,
            });
        }
        match resolve_agent_preset(&slot.name, presets) {
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
                name: slot.name.clone(),
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

/// The single-harness registry: the stored `agent_command` labelled by the configured preset (or
/// unlabelled for the raw-argv hatch). The argv is taken from config as-is and never re-resolved,
/// so an existing seller launches the same binary it launched before.
fn fallback_registry(seller: &SellerConfig) -> Result<ResolvedRegistry, RegistryError> {
    if seller.agent_command.is_empty() {
        return Err(RegistryError::Empty);
    }
    let name = seller
        .agent
        .as_ref()
        .map(|label| label.trim().to_ascii_lowercase())
        .filter(|label| !label.is_empty());
    Ok(ResolvedRegistry {
        registry: AgentRegistry::new(vec![RegisteredAgent {
            name,
            argv: seller.agent_command.clone(),
        }]),
        verdicts: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::AgentSlotConfig;

    fn seller_with(agents: Vec<AgentSlotConfig>, label: Option<&str>) -> SellerConfig {
        SellerConfig {
            agent_command: vec!["fallback-bin".into()],
            rate_sats: 5,
            git_remote: "https://example.invalid/repo".into(),
            job_timeout_secs: None,
            agent: label.map(str::to_owned),
            agents,
            claim_open_pool: false,
            offer_backfill_secs: 0,
            contribution_enabled: true,
            slots: 1,
            claim_award_timeout_secs: None,
        }
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
        let seller = seller_with(Vec::new(), None);
        let resolved = resolve(&seller, &BTreeMap::new()).expect("hatch resolves");
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
    fn single_preset_config_resolves_to_the_same_one_entry_registry_and_argv() {
        // Compat: a seller written before `agents` existed keeps its stored argv verbatim (never
        // re-resolved off PATH) and advertises exactly its one configured harness.
        let seller = seller_with(Vec::new(), Some("claude"));
        let resolved = resolve(&seller, &BTreeMap::new()).expect("single preset resolves");
        assert_eq!(resolved.registry.advertised(), vec!["claude"]);
        assert_eq!(
            resolved.registry.dispatch(None).map(|a| a.argv.clone()),
            Some(vec!["fallback-bin".to_owned()]),
            "the stored agent_command is the truth for an existing seller"
        );
        assert!(resolved.verdicts.is_empty());
        assert!(resolved.degrade_line().is_none());
    }

    #[test]
    fn partial_failure_degrades_loud_and_serves_with_the_remainder() {
        let table = presets(&[("good", &["/bin/sh"])]);
        let seller = seller_with(
            vec![
                AgentSlotConfig::named("good"),
                AgentSlotConfig::named("nope-not-a-preset"),
            ],
            None,
        );
        let resolved = resolve(&seller, &table).expect("partial failure still serves");
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
        let seller = seller_with(
            vec![
                AgentSlotConfig::named("nope-one"),
                AgentSlotConfig::named("nope-two"),
            ],
            None,
        );
        match resolve(&seller, &BTreeMap::new()) {
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
    fn a_pool_larger_than_one_is_refused_never_silently_serialized() {
        let table = presets(&[("good", &["/bin/sh"])]);
        let seller = seller_with(vec![AgentSlotConfig { name: "good".into(), slots: 2 }], None);
        assert_eq!(
            resolve(&seller, &table),
            Err(RegistryError::ParallelismUnsupported { name: "good".into(), slots: 2 })
        );
    }

    #[test]
    fn a_duplicate_listing_keeps_one_entry_so_advertisement_matches_dispatch() {
        let table = presets(&[("good", &["/bin/sh"])]);
        let seller = seller_with(
            vec![AgentSlotConfig::named("good"), AgentSlotConfig::named("good")],
            None,
        );
        let resolved = resolve(&seller, &table).expect("duplicates resolve");
        assert_eq!(resolved.registry.advertised(), vec!["good"]);
        assert_eq!(resolved.registry.entries().len(), 1);
    }

    #[test]
    fn no_harness_at_all_refuses() {
        let mut seller = seller_with(Vec::new(), None);
        seller.agent_command = Vec::new();
        assert_eq!(resolve(&seller, &BTreeMap::new()), Err(RegistryError::Empty));
    }
}
