//! Cross-mint payment planning: which mint the buyer pays at, and whether reaching it needs a hop.
//!
//! A buyer funded only at mint A can pay a seller who accepts only mint B by hopping through
//! Lightning *between the mints*: request a NUT-04 mint quote at B (yields a bolt11), NUT-05 melt from
//! A to pay that invoice, receive fresh ecash at B, then hand it to the ordinary send path. Lightning
//! is connective tissue between mints only — the wire keeps exactly one settlement shape, so
//! pays-once, the co-signed receipt, and amount-from-the-buyer-signed-offer are untouched by it.
//!
//! This module holds only the DECISION (a pure function of the buyer's selected mint and the seller's
//! accepted set). It moves no money and touches no network, so the decision is testable on its own —
//! which matters, because "did we hop when we shouldn't have" is a question about the decision, not
//! about the outcome.

use std::str::FromStr;

use cdk::mint_url::MintUrl;

use crate::authorize_pay::AuthorizePayError;
use crate::home;

/// How the buyer reaches a mint the seller accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayPlan {
    /// The buyer's own mint is already in the seller's accepted set — pay from it directly. No hop,
    /// no Lightning, nothing added to the settlement path.
    Direct {
        /// The mint the buyer pays at.
        mint: MintUrl,
    },
    /// No overlap: melt at the buyer's funded mint to pay a mint quote raised at the seller's mint,
    /// leaving the buyer holding fresh ecash at `target`.
    Hop {
        /// The buyer's funded mint, whose proofs are melted.
        source: MintUrl,
        /// A mint from the seller's accepted set, where the fresh ecash lands and the send happens.
        target: MintUrl,
    },
}

impl PayPlan {
    /// The mint the payment is realized at — where the ecash the seller receives lives, and therefore
    /// the mint sealed into the payment terms, the attempt id, and the co-signed receipt.
    ///
    /// For a hop this is the TARGET, not the buyer's funded mint: after the hop the buyer holds ecash
    /// at the target, and the send, the attempt id, and the receipt must all agree on that one mint.
    pub fn realized_mint(&self) -> &MintUrl {
        match self {
            Self::Direct { mint } => mint,
            Self::Hop { target, .. } => target,
        }
    }

    /// The hop's source mint, or `None` on the direct path.
    pub fn hop_source(&self) -> Option<&MintUrl> {
        match self {
            Self::Direct { .. } => None,
            Self::Hop { source, .. } => Some(source),
        }
    }

    /// Whether reaching the realized mint requires a hop.
    pub fn is_hop(&self) -> bool {
        matches!(self, Self::Hop { .. })
    }
}

/// Decide how to pay: directly from the buyer's selected mint, or via a hop to one the seller accepts.
///
/// `buyer_selected_mint` is the mint frozen into the accept-bind (see `authorize_pay`), never the live
/// config default — a config change between attempts must not shift the mint and mint a second attempt
/// id.
///
/// Both the source and the target pass the real-mint fence. Fencing the target matters as much as the
/// source: a hop ends with the buyer holding ecash at the target, so an unfenced target would let a
/// real-sats mint in through the back door while `allow_real_mints` is off.
///
/// Target selection is the FIRST admissible entry of `accepted_mints` — the seller's list order is
/// their preference. It must stay deterministic: the attempt id is derived from the realized mint, so
/// a retry that re-derived a different target would compute a different attempt id and defeat
/// pays-once.
pub fn plan_payment(
    buyer_selected_mint: &str,
    accepted_mints: &[String],
    allow_real_mints: bool,
) -> Result<PayPlan, AuthorizePayError> {
    if !home::mint_allowed(buyer_selected_mint, allow_real_mints) {
        return Err(AuthorizePayError::Input(format!(
            "real-mint fence: buyer mint {buyer_selected_mint} is not an allow-listed testnut/dev \
             mint; set allow_real_mints=true to pay at a real mint"
        )));
    }
    let buyer_mint = MintUrl::from_str(buyer_selected_mint)
        .map_err(|error| AuthorizePayError::Input(format!("buyer mint url: {error}")))?;

    // No accepted set to satisfy (legacy binds carry none) — the buyer's own mint stands.
    if accepted_mints.is_empty() {
        return Ok(PayPlan::Direct { mint: buyer_mint });
    }

    let listed = accepted_mints
        .iter()
        .map(|entry| MintUrl::from_str(entry))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AuthorizePayError::Input(format!("creq accepted mint url: {error}")))?;

    if listed.contains(&buyer_mint) {
        return Ok(PayPlan::Direct { mint: buyer_mint });
    }

    // No overlap. Hop to the first accepted mint that the fence admits; refuse fail-closed if none
    // does, rather than hopping to a mint we are not permitted to hold ecash at.
    let target = accepted_mints
        .iter()
        .zip(listed)
        .find(|(raw, _)| home::mint_allowed(raw, allow_real_mints))
        .map(|(_, parsed)| parsed);

    match target {
        Some(target) => Ok(PayPlan::Hop {
            source: buyer_mint,
            target,
        }),
        None => Err(AuthorizePayError::Input(format!(
            "real-mint fence: buyer mint {buyer_mint} is not in the creq mint list \
             {accepted_mints:?} and no accepted mint is an allow-listed testnut/dev mint, so the \
             cross-mint hop has nowhere permitted to land; set allow_real_mints=true to pay at a \
             real mint"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::DEFAULT_MINT_URL;

    fn mint(url: &str) -> MintUrl {
        MintUrl::from_str(url).expect("test mint url parses")
    }

    // Invariant 2 — the decision tooth. Overlap must NOT hop: the existing direct path stays exactly
    // as it was. Asserted on the decision, not on a downstream outcome, because a hop that happened
    // and then coincidentally produced the right amount would still be a bug.
    #[test]
    fn buyer_mint_in_accepted_set_pays_direct_without_hopping() {
        let plan = plan_payment(DEFAULT_MINT_URL, &[DEFAULT_MINT_URL.to_owned()], false)
            .expect("an accepted, fenced buyer mint plans");
        assert_eq!(
            plan,
            PayPlan::Direct {
                mint: mint(DEFAULT_MINT_URL)
            }
        );
        assert!(!plan.is_hop(), "overlap must not hop");
        assert_eq!(plan.hop_source(), None);
        assert_eq!(plan.realized_mint(), &mint(DEFAULT_MINT_URL));
    }

    // Overlap anywhere in the list is still overlap, even when the buyer's mint is not first: the
    // seller's preference order decides hop TARGETS, never whether to hop at all.
    #[test]
    fn buyer_mint_listed_but_not_first_still_pays_direct() {
        let accepted = vec![
            "https://b.example".to_owned(),
            "https://a.example".to_owned(),
        ];
        let plan = plan_payment("https://a.example", &accepted, true).expect("listed mint plans");
        assert!(!plan.is_hop(), "a listed buyer mint never hops");
        assert_eq!(plan.realized_mint(), &mint("https://a.example"));
    }

    #[test]
    fn no_overlap_plans_a_hop_from_the_buyer_mint_to_an_accepted_mint() {
        let accepted = vec!["https://seller.example".to_owned()];
        let plan =
            plan_payment("https://buyer.example", &accepted, true).expect("a hop is plannable");
        assert_eq!(
            plan,
            PayPlan::Hop {
                source: mint("https://buyer.example"),
                target: mint("https://seller.example"),
            }
        );
        // The realized mint is the TARGET — that is where the seller's ecash ends up, so the terms,
        // the attempt id and the receipt must bind it rather than the buyer's funded mint.
        assert_eq!(plan.realized_mint(), &mint("https://seller.example"));
        assert_eq!(plan.hop_source(), Some(&mint("https://buyer.example")));
    }

    // The attempt id is derived from the realized mint, so a retry must re-derive the SAME target or
    // pays-once breaks. Pin determinism against a multi-entry list.
    #[test]
    fn hop_target_selection_is_deterministic_first_admissible() {
        let accepted = vec![
            "https://first.example".to_owned(),
            "https://second.example".to_owned(),
            "https://third.example".to_owned(),
        ];
        let first = plan_payment("https://buyer.example", &accepted, true).expect("plans");
        for _ in 0..5 {
            let again = plan_payment("https://buyer.example", &accepted, true).expect("plans");
            assert_eq!(
                again, first,
                "target selection must not vary between attempts"
            );
        }
        assert_eq!(first.realized_mint(), &mint("https://first.example"));
    }

    #[test]
    fn empty_accepted_set_pays_direct_at_the_buyer_mint() {
        let plan = plan_payment(DEFAULT_MINT_URL, &[], false).expect("legacy bind plans");
        assert_eq!(
            plan,
            PayPlan::Direct {
                mint: mint(DEFAULT_MINT_URL)
            }
        );
    }

    // The fence still refuses the buyer's own mint first, before any hop is considered.
    #[test]
    fn unfenced_buyer_mint_is_refused_before_planning_a_hop() {
        let accepted = vec![DEFAULT_MINT_URL.to_owned()];
        let error = plan_payment("https://real-mint.example", &accepted, false)
            .expect_err("an unfenced buyer mint must refuse");
        let rendered = error.to_string();
        assert!(
            rendered.contains("real-mint fence"),
            "expected a fence refusal, got: {rendered}"
        );
    }

    // A hop must not become a back door around the fence: with the switch off, an accepted mint that
    // is not allow-listed is not a permitted place to land, so the plan refuses fail-closed.
    #[test]
    fn hop_refuses_when_no_accepted_mint_passes_the_fence() {
        // Buyer sits on the one fenced mint; the seller accepts only an unfenced real mint.
        let accepted = vec!["https://real-mint.example".to_owned()];
        let error = plan_payment(DEFAULT_MINT_URL, &accepted, false)
            .expect_err("an unfenced hop target must refuse");
        let rendered = error.to_string();
        assert!(
            rendered.contains("real-mint fence"),
            "expected a fence refusal, got: {rendered}"
        );
        assert!(
            rendered.contains("nowhere permitted to land"),
            "the refusal should say the hop had no permitted target, got: {rendered}"
        );
    }

    // With the switch on, the same shape plans a hop — proving the previous refusal came from the
    // fence and not from some unrelated rejection of the list.
    #[test]
    fn hop_to_a_real_mint_plans_once_the_operator_opts_in() {
        let accepted = vec!["https://real-mint.example".to_owned()];
        let plan = plan_payment(DEFAULT_MINT_URL, &accepted, true)
            .expect("opted-in real mint is a permitted hop target");
        assert_eq!(
            plan,
            PayPlan::Hop {
                source: mint(DEFAULT_MINT_URL),
                target: mint("https://real-mint.example"),
            }
        );
    }

    // Selection skips an unfenced entry rather than refusing outright when a later entry is fine.
    #[test]
    fn hop_skips_unfenced_entries_and_lands_on_the_first_admissible_one() {
        let accepted = vec![
            "http://not-https.example".to_owned(),
            DEFAULT_MINT_URL.to_owned(),
        ];
        let plan = plan_payment("https://buyer.example", &accepted, true)
            .expect("a later admissible entry is usable");
        assert_eq!(plan.realized_mint(), &mint(DEFAULT_MINT_URL));
    }
}
