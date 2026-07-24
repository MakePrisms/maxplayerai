//! Deterministic roster selection: route one offer to one agent under the single seller identity.
//!
//! The roster ([`crate::home::SellerRosterConfig`]) is private execution capacity — many agents,
//! one seller pubkey. Selection replaces the single hardcoded agent command: given an offer's
//! required capabilities and rate, it filters the roster to the agents that can and will serve, then
//! picks one by a stable, deterministic rank. When no roster is configured the node falls back to
//! the single [`crate::home::SellerConfig::agent_command`], so existing single-agent sellers behave
//! exactly as before.
//!
//! Selection is deliberately deterministic (no history, no scoring model yet): a specialist whose
//! declared capabilities cover the offer is preferred over a generalist, ties break toward the
//! higher rate floor (the more valuable agent) and then the name, so the same inputs always pick the
//! same agent. Selection uses no execution history and no scoring model — only the offer's needs
//! and the agents' declared terms.

use crate::home::{RosterAgent, SellerRosterConfig};

/// What an offer needs from an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferNeeds {
    /// Task capability tags the offer requires (empty ⇒ no capability constraint).
    pub capabilities: Vec<String>,
    /// The offer's rate (sats) — an agent whose `min_rate_sats` exceeds this is not dispatched.
    pub rate_sats: u64,
}

/// The agent selected to run a job. `name` is operator-facing routing metadata only — it is never
/// published to buyers (public claims expose terms, not worker identity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedAgent {
    pub name: String,
    pub argv: Vec<String>,
    pub timeout_secs: Option<u64>,
}

/// Select the best-fit agent for `needs`. When the roster is empty, fall back to the single
/// `fallback_argv` (labelled `fallback_name`) if it is non-empty. Returns `None` when a roster is
/// configured but no agent can serve the offer (the caller declines), or when there is neither a
/// matching roster agent nor a fallback command.
pub fn select_agent(
    roster: &SellerRosterConfig,
    fallback_name: &str,
    fallback_argv: &[String],
    needs: &OfferNeeds,
) -> Option<SelectedAgent> {
    if roster.agents.is_empty() {
        if fallback_argv.is_empty() {
            return None;
        }
        return Some(SelectedAgent {
            name: fallback_name.to_owned(),
            argv: fallback_argv.to_vec(),
            timeout_secs: None,
        });
    }

    roster
        .agents
        .iter()
        .filter(|agent| serves(agent, needs))
        .max_by(|left, right| rank(left, needs).cmp(&rank(right, needs)))
        .map(|agent| SelectedAgent {
            name: agent.name.clone(),
            argv: agent.argv.clone(),
            timeout_secs: agent.timeout_secs,
        })
}

/// Whether `agent` can and will serve `needs`: it clears the rate floor and covers every required
/// capability (a generalist with no declared capabilities covers any task).
fn serves(agent: &RosterAgent, needs: &OfferNeeds) -> bool {
    let rate_ok = needs.rate_sats >= agent.min_rate_sats.unwrap_or(0);
    let capability_ok = agent.capabilities.is_empty()
        || needs
            .capabilities
            .iter()
            .all(|required| agent.capabilities.iter().any(|have| have == required));
    rate_ok && capability_ok
}

/// A deterministic rank key for a serving agent, larger is preferred:
/// 1. a specialist that declares capabilities outranks a generalist (empty capabilities),
/// 2. then the higher rate floor (the more valuable agent),
/// 3. then the name, reversed, so ties resolve to the lexicographically-smaller name.
fn rank(agent: &RosterAgent, _needs: &OfferNeeds) -> (u8, u64, std::cmp::Reverse<String>) {
    let specialist = u8::from(!agent.capabilities.is_empty());
    (
        specialist,
        agent.min_rate_sats.unwrap_or(0),
        std::cmp::Reverse(agent.name.clone()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, caps: &[&str], min_rate: Option<u64>) -> RosterAgent {
        RosterAgent {
            name: name.to_owned(),
            argv: vec![name.to_owned()],
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            min_rate_sats: min_rate,
            timeout_secs: None,
            slots: 1,
        }
    }

    fn roster(agents: Vec<RosterAgent>) -> SellerRosterConfig {
        SellerRosterConfig { agents }
    }

    #[test]
    fn empty_roster_falls_back_to_single_command() {
        let selected = select_agent(
            &roster(vec![]),
            "claude",
            &["claude".to_owned(), "code".to_owned()],
            &OfferNeeds { capabilities: vec![], rate_sats: 100 },
        )
        .expect("fallback");
        assert_eq!(selected.name, "claude");
        assert_eq!(selected.argv, vec!["claude".to_owned(), "code".to_owned()]);
    }

    #[test]
    fn empty_roster_and_no_fallback_selects_nothing() {
        assert!(select_agent(
            &roster(vec![]),
            "custom",
            &[],
            &OfferNeeds { capabilities: vec![], rate_sats: 100 },
        )
        .is_none());
    }

    #[test]
    fn specialist_matching_capability_beats_generalist() {
        let r = roster(vec![
            agent("generalist", &[], None),
            agent("rustacean", &["rust"], None),
        ]);
        let selected = select_agent(
            &r,
            "x",
            &[],
            &OfferNeeds { capabilities: vec!["rust".to_owned()], rate_sats: 5000 },
        )
        .expect("select");
        assert_eq!(selected.name, "rustacean");
    }

    #[test]
    fn agent_below_its_rate_floor_is_not_dispatched() {
        let r = roster(vec![agent("pricey", &["rust"], Some(5000))]);
        // Offer pays under the floor ⇒ no candidate, no selection.
        assert!(select_agent(
            &r,
            "x",
            &[],
            &OfferNeeds { capabilities: vec!["rust".to_owned()], rate_sats: 100 },
        )
        .is_none());
        // Offer clears the floor ⇒ selected.
        let selected = select_agent(
            &r,
            "x",
            &[],
            &OfferNeeds { capabilities: vec!["rust".to_owned()], rate_sats: 5000 },
        )
        .expect("select");
        assert_eq!(selected.name, "pricey");
    }

    #[test]
    fn a_required_capability_no_agent_covers_selects_nothing() {
        let r = roster(vec![agent("rustacean", &["rust"], None)]);
        assert!(select_agent(
            &r,
            "x",
            &[],
            &OfferNeeds { capabilities: vec!["frontend".to_owned()], rate_sats: 5000 },
        )
        .is_none());
    }

    #[test]
    fn selection_is_deterministic_and_carries_timeout() {
        let mut specialist = agent("a-rust", &["rust"], Some(1000));
        specialist.timeout_secs = Some(1200);
        let r = roster(vec![
            specialist,
            agent("b-rust", &["rust"], Some(1000)),
        ]);
        let needs = OfferNeeds { capabilities: vec!["rust".to_owned()], rate_sats: 5000 };
        // Same rank class + same rate floor ⇒ the lexicographically-smaller name wins, stably.
        let first = select_agent(&r, "x", &[], &needs).expect("select");
        let again = select_agent(&r, "x", &[], &needs).expect("select");
        assert_eq!(first, again);
        assert_eq!(first.name, "a-rust");
        assert_eq!(first.timeout_secs, Some(1200));
    }
}
