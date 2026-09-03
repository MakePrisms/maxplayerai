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

    /// The buyer's own funded mint — where the proofs being spent live, on either path.
    ///
    /// This is the SELECTION the accept-bind freezes. Freezing the selection rather than the
    /// realized mint is what keeps the hop reachable: the realized mint of a hop is already in the
    /// seller's accepted set, so a bind that sealed it would re-plan at pay time as a direct payment
    /// from a mint the buyer holds nothing at.
    pub fn source_mint(&self) -> &MintUrl {
        match self {
            Self::Direct { mint } => mint,
            Self::Hop { source, .. } => source,
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
/// Both the source and the target must be usable mint URLs. There is no real-mint/test-mint policy:
/// which mints may be paid at is decided by the seller's `accepted_mints` (that list is the creq's
/// contents) and by the buyer's own configured list — this function only refuses a string that is
/// not a mint URL at all.
///
/// Target selection is the FIRST admissible entry of `accepted_mints` — the seller's list order is
/// their preference. It must stay deterministic: the attempt id is derived from the realized mint, so
/// a retry that re-derived a different target would compute a different attempt id and defeat
/// pays-once.
pub fn plan_payment(
    buyer_selected_mint: &str,
    accepted_mints: &[String],
) -> Result<PayPlan, AuthorizePayError> {
    if !home::mint_url_supported(buyer_selected_mint) {
        return Err(AuthorizePayError::Input(format!(
            "buyer mint {buyer_selected_mint} is not a usable mint URL (expected http:// or \
             https:// with a host)"
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

    // No overlap. Hop to the first accepted mint that is a usable mint URL; refuse fail-closed if
    // none is, rather than hopping to something we cannot hold ecash at.
    let target = accepted_mints
        .iter()
        .zip(listed)
        .find(|(_, parsed)| home::mint_url_supported(&parsed.to_string()))
        .map(|(_, parsed)| parsed);

    match target {
        Some(target) => Ok(PayPlan::Hop {
            source: buyer_mint,
            target,
        }),
        None => Err(AuthorizePayError::Input(format!(
            "buyer mint {buyer_mint} is not in the creq mint list {accepted_mints:?} and no \
             accepted mint is a usable mint URL, so the cross-mint hop has nowhere to land"
        ))),
    }
}

/// Choose the buyer's SOURCE mint for a payment, preferring a mint the buyer already holds a
/// covering balance at so a pre-funded cross-mint balance is spent directly instead of hopping from
/// the default (which would drain the default and pay a melt fee). Falls back to the configured
/// default — today's behavior — when no held, accepted, usable mint covers the amount.
///
/// Deterministic preference: the FIRST entry of the seller's `accepted_mints`, in the seller's list
/// order, that (1) is a usable mint URL and (2) shows a balance `>= amount_sats`. Returning an
/// accepted mint makes [`plan_payment`] plan a DIRECT payment from it (no hop); the seller's order is
/// their stated preference and keeps the choice stable across retries.
///
/// Balance-awareness is ADVISORY and applied ONCE, here at accept. The result is sealed into the
/// accept-bind and re-derived (not re-decided) at pay, so a later balance or config-default change
/// cannot shift the sealed mint — the pays-once attempt-id invariant is unchanged. Exact coverage
/// (including fees) is enforced at pay: if the chosen mint's balance is spent before pay, the pay
/// refuses fail-closed at that sealed mint rather than silently re-selecting a different one.
pub(crate) fn select_source_mint(
    config_default: &str,
    accepted_mints: &[String],
    balances: &[crate::wallet_ops::MintBalance],
    amount_sats: u64,
) -> String {
    accepted_mints
        .iter()
        .find(|accepted| {
            let mint = accepted.as_str();
            home::mint_url_supported(mint) && holds_at_least(balances, mint, amount_sats)
        })
        .cloned()
        .unwrap_or_else(|| config_default.to_owned())
}

/// Whether configured `balances` shows at least `amount_sats` at `mint`, comparing normalized mint
/// URLs. Balance display was widened in #266 to include DB-discovered, unconfigured mints; source
/// selection deliberately excludes those rows because the selected mint is sealed into the
/// pays-once attempt id. A parse failure on either side is treated as "no match" (never a panic);
/// the caller's fallback to the configured default keeps a malformed accepted entry from becoming
/// a selected source.
fn holds_at_least(balances: &[crate::wallet_ops::MintBalance], mint: &str, amount_sats: u64) -> bool {
    let Ok(target) = MintUrl::from_str(mint) else {
        return false;
    };
    balances.iter().any(|entry| {
        entry.configured
            && entry.balance_sats >= amount_sats
            && MintUrl::from_str(&entry.mint_url).map(|url| url == target).unwrap_or(false)
    })
}

/// What the buyer spends to deliver `offer.amount` across a hop.
///
/// Three components, all of which the buyer pays and none of which reduce what the seller receives:
/// the melt amount (which equals the invoice raised at the target), the Lightning fee reserve the
/// source mint holds back, and the source mint's input fee for spending the proofs. All three must be
/// covered by the budget cap BEFORE the melt fires — a fee that reaches the wire without passing the
/// cap is the #185 class of defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopCost {
    /// Amount the source mint melts, per its melt quote.
    pub melt_amount: u64,
    /// Lightning fee reserve held back by the source mint (`MeltQuote::fee_reserve`).
    pub fee_reserve: u64,
    /// Source-mint input fee for spending the proofs that fund the melt.
    pub input_fee: u64,
}

impl HopCost {
    /// Total the cap must cover before any melt.
    ///
    /// Checked, and refuses on overflow rather than saturating: a saturated total would still refuse
    /// at the cap today, but it would do so by arriving at a number nobody computed. A cost we cannot
    /// add up is a cost we decline to spend.
    pub fn planned_cost(&self) -> Result<u64, AuthorizePayError> {
        self.melt_amount
            .checked_add(self.fee_reserve)
            .and_then(|subtotal| subtotal.checked_add(self.input_fee))
            .ok_or_else(|| {
                AuthorizePayError::Input(format!(
                    "cross-mint hop cost overflows u64: melt_amount={} fee_reserve={} input_fee={}",
                    self.melt_amount, self.fee_reserve, self.input_fee
                ))
            })
    }
}

/// The pairing record written before the melt fires.
///
/// cdk already journals each half of the hop on its own (a `WalletSaga` carries the quote id, and
/// `check_melt_quote_status` resumes off the persisted store from a cold process). The one thing
/// nothing in cdk knows is that these two quotes are **one logical hop** — so that, and nothing more,
/// is what we persist. Written before the melt (write-before-effect), which is the same discipline the
/// budget ledger already uses, and stronger than marking the melt "initiated" reactively once the mint
/// reports it pending — that ordering leaves a window where money is in flight and the flag says
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HopJournal {
    /// The pay attempt this hop funds; ties the hop to the budget ledger's idempotence key.
    pub attempt_id: String,
    /// Mint whose proofs are melted.
    pub source_mint: String,
    /// Melt quote id at the source mint — the handle recovery uses to ask "did this melt land?".
    pub melt_quote_id: String,
    /// Mint where the fresh ecash lands.
    pub target_mint: String,
    /// Mint quote id at the target mint — the handle recovery uses to detect a paid-but-unissued strand.
    pub mint_quote_id: String,
    /// What the seller receives: the amount pinned by the buyer-signed offer. Journalled so a
    /// recovering process knows what to expect at the target without re-reading it from anywhere
    /// that could have changed since the offer was signed. Equals the source melt amount (the melt
    /// quote is raised against the target invoice for exactly this many sats).
    pub delivered_sats: u64,
    /// Cost charged against the cap before the melt.
    pub planned_cost: u64,
    /// Source mint's raw Lightning fee reserve for this melt quote (`MeltQuote::fee_reserve`).
    /// Journalled so the post-melt reconciliation (MakePrisms/maxplayerai#186) can credit the unused
    /// reserve back to the budget without re-reading the quote. A pre-#186 pairing has no field and
    /// defaults to 0, which makes the reconciliation a no-op (spend stays at the safe over-count).
    #[serde(default)]
    pub fee_reserve: u64,
    /// Source mint's input fee for spending the proofs that fund the melt (the fixed, actually-spent
    /// component). Journalled alongside `fee_reserve` so the reconciliation can isolate the LN-fee
    /// reserve portion of `planned_cost` — the only part that reconciles against the actual fee.
    #[serde(default)]
    pub input_fee: u64,
}

impl HopJournal {
    /// The portion of `planned_cost` reserved for the Lightning fee — everything above the fixed,
    /// actually-spent costs (melt amount + input fee), i.e. the raw fee reserve plus any plan-time
    /// buffer. This is the ceiling the post-melt reconciliation nets the ACTUAL fee against; the
    /// remainder is credited back to the budget (MakePrisms/maxplayerai#186). Saturating so a
    /// legacy pairing (zero components) yields 0 — a safe no-op credit.
    pub fn reserved_ln_fee_sats(&self) -> u64 {
        self.planned_cost
            .saturating_sub(self.delivered_sats)
            .saturating_sub(self.input_fee)
    }
}

#[cfg(test)]
mod tests {

    /// Test fixture mint host — NOT a default. A mint is just a mint: what makes one usable is membership of
    /// the home's configured list, which each test sets up explicitly.
    const FIXTURE_MINT_URL: &str = "https://mint.example/Bitcoin";
    use super::*;

    fn mint(url: &str) -> MintUrl {
        MintUrl::from_str(url).expect("test mint url parses")
    }

    // Invariant 2 — the decision tooth. Overlap must NOT hop: the existing direct path stays exactly
    // as it was. Asserted on the decision, not on a downstream outcome, because a hop that happened
    // and then coincidentally produced the right amount would still be a bug.
    #[test]
    fn buyer_mint_in_accepted_set_pays_direct_without_hopping() {
        let plan = plan_payment(FIXTURE_MINT_URL, &[FIXTURE_MINT_URL.to_owned()])
            .expect("an accepted, fenced buyer mint plans");
        assert_eq!(
            plan,
            PayPlan::Direct {
                mint: mint(FIXTURE_MINT_URL)
            }
        );
        assert!(!plan.is_hop(), "overlap must not hop");
        assert_eq!(plan.hop_source(), None);
        assert_eq!(plan.realized_mint(), &mint(FIXTURE_MINT_URL));
    }

    // Overlap anywhere in the list is still overlap, even when the buyer's mint is not first: the
    // seller's preference order decides hop TARGETS, never whether to hop at all.
    #[test]
    fn buyer_mint_listed_but_not_first_still_pays_direct() {
        let accepted = vec![
            "https://b.example".to_owned(),
            "https://a.example".to_owned(),
        ];
        let plan = plan_payment("https://a.example", &accepted).expect("listed mint plans");
        assert!(!plan.is_hop(), "a listed buyer mint never hops");
        assert_eq!(plan.realized_mint(), &mint("https://a.example"));
    }

    #[test]
    fn no_overlap_plans_a_hop_from_the_buyer_mint_to_an_accepted_mint() {
        let accepted = vec!["https://seller.example".to_owned()];
        let plan =
            plan_payment("https://buyer.example", &accepted).expect("a hop is plannable");
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
        // The source is the buyer's own mint on either path — that is what the accept-bind seals.
        assert_eq!(plan.source_mint(), &mint("https://buyer.example"));
    }

    // What the accept-bind seals must re-plan into the SAME payment. Sealing the realized mint
    // instead would make a hop re-plan as a direct payment from a mint the buyer holds nothing at,
    // which is how a hop ships dead.
    #[test]
    fn the_sealed_source_replans_into_the_same_plan() {
        let accepted = vec![
            "https://first.example".to_owned(),
            "https://second.example".to_owned(),
        ];
        let planned = plan_payment("https://buyer.example", &accepted).expect("plans");
        let sealed = planned.source_mint().to_string();

        let replanned = plan_payment(&sealed, &accepted).expect("the sealed source re-plans");
        assert_eq!(replanned, planned, "the seal must re-derive the same plan");
        assert!(replanned.is_hop(), "re-planning must not collapse the hop");
        assert_eq!(replanned.realized_mint(), planned.realized_mint());
    }

    // The direct path's seal is unchanged: the buyer's own mint, which is also what it realizes at.
    #[test]
    fn the_direct_path_seals_the_mint_it_realizes_at() {
        let plan = plan_payment(FIXTURE_MINT_URL, &[FIXTURE_MINT_URL.to_owned()])
            .expect("direct plans");
        assert_eq!(plan.source_mint(), plan.realized_mint());
        assert_eq!(plan.source_mint(), &mint(FIXTURE_MINT_URL));
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
        let first = plan_payment("https://buyer.example", &accepted).expect("plans");
        for _ in 0..5 {
            let again = plan_payment("https://buyer.example", &accepted).expect("plans");
            assert_eq!(
                again, first,
                "target selection must not vary between attempts"
            );
        }
        assert_eq!(first.realized_mint(), &mint("https://first.example"));
    }

    #[test]
    fn empty_accepted_set_pays_direct_at_the_buyer_mint() {
        let plan = plan_payment(FIXTURE_MINT_URL, &[]).expect("legacy bind plans");
        assert_eq!(
            plan,
            PayPlan::Direct {
                mint: mint(FIXTURE_MINT_URL)
            }
        );
    }

    // A buyer "mint" that is not a mint URL refuses first, before any hop is considered.
    #[test]
    fn a_buyer_mint_that_is_not_a_mint_url_is_refused_before_planning_a_hop() {
        let accepted = vec![FIXTURE_MINT_URL.to_owned()];
        let error = plan_payment("not-a-url", &accepted)
            .expect_err("a buyer mint that is not a URL must refuse");
        let rendered = error.to_string();
        assert!(
            rendered.contains("not a usable mint URL"),
            "expected a URL-shape refusal, got: {rendered}"
        );
    }

    // NEGATIVE: a hop still refuses fail-closed when NO accepted entry is a mint URL at all — the
    // only refusal left, now that a mint is just a mint.
    #[test]
    fn hop_refuses_when_no_accepted_mint_is_a_usable_url() {
        // `ftp://` PARSES as a mint URL but is not one this wallet can speak to, so it is the
        // shape rule — not the parse — that refuses here.
        let accepted = vec!["ftp://nope.example".to_owned(), "ftp://also-nope.example".to_owned()];
        let error = plan_payment("https://buyer.example", &accepted)
            .expect_err("an unusable hop target must refuse");
        let rendered = error.to_string();
        assert!(
            rendered.contains("nowhere to land"),
            "the refusal should say the hop had no target, got: {rendered}"
        );
    }

    // Any mint the seller lists is a permitted hop target — there is no second switch to flip.
    #[test]
    fn hop_lands_on_any_mint_the_seller_accepts() {
        let accepted = vec!["https://real-mint.example".to_owned()];
        let plan = plan_payment(FIXTURE_MINT_URL, &accepted)
            .expect("any listed mint is a permitted hop target");
        assert_eq!(
            plan,
            PayPlan::Hop {
                source: mint(FIXTURE_MINT_URL),
                target: mint("https://real-mint.example"),
            }
        );
    }

    // Invariant 3 — the cap must see the WHOLE hop. The failure this guards is #185's shape: a fee
    // that reaches the wire without passing the cap. Asserted as "all three components are in the
    // total", so dropping any one of them bites.
    #[test]
    fn planned_cost_counts_melt_amount_fee_reserve_and_input_fee() {
        let cost = HopCost {
            melt_amount: 100,
            fee_reserve: 7,
            input_fee: 2,
        };
        assert_eq!(cost.planned_cost().expect("sums"), 109);

        // Each component individually moves the total — none is silently dropped.
        let no_reserve = HopCost {
            fee_reserve: 0,
            ..cost
        };
        let no_input_fee = HopCost {
            input_fee: 0,
            ..cost
        };
        assert_eq!(no_reserve.planned_cost().expect("sums"), 102);
        assert_eq!(no_input_fee.planned_cost().expect("sums"), 107);
    }

    // A cost we cannot add up is a cost we decline to spend: overflow refuses rather than saturating
    // into a number nobody computed.
    #[test]
    fn planned_cost_refuses_on_overflow_instead_of_saturating() {
        let cost = HopCost {
            melt_amount: u64::MAX,
            fee_reserve: 1,
            input_fee: 0,
        };
        let error = cost
            .planned_cost()
            .expect_err("an unrepresentable total must refuse");
        assert!(
            error.to_string().contains("overflows"),
            "expected an overflow refusal, got: {error}"
        );
    }

    // The journal's whole job is to record that two quotes are ONE hop, so both ids must survive a
    // round trip — recovery reads this to ask each mint about its half.
    #[test]
    fn hop_journal_round_trips_both_quote_ids() {
        let journal = HopJournal {
            attempt_id: "attempt-1".to_owned(),
            source_mint: "https://a.example".to_owned(),
            melt_quote_id: "melt-quote-1".to_owned(),
            target_mint: "https://b.example".to_owned(),
            mint_quote_id: "mint-quote-1".to_owned(),
            delivered_sats: 100,
            planned_cost: 109,
            fee_reserve: 7,
            input_fee: 2,
        };
        let encoded = serde_json::to_string(&journal).expect("journal serializes");
        let decoded: HopJournal = serde_json::from_str(&encoded).expect("journal deserializes");
        assert_eq!(decoded, journal);
        assert_eq!(decoded.melt_quote_id, "melt-quote-1");
        assert_eq!(decoded.mint_quote_id, "mint-quote-1");
        // The delivered amount survives the round trip separately from the cost: recovery must be
        // able to issue exactly the offer amount without consulting anything mutable.
        assert_eq!(decoded.delivered_sats, 100);
        assert_ne!(decoded.delivered_sats, decoded.planned_cost);
    }

    // Selection skips an entry that is not a mint URL rather than refusing outright when a later
    // entry is fine. `http://` is NOT such an entry — it is an ordinary mint URL and wins when it
    // comes first (a seat's own sidecar mint runs on loopback).
    #[test]
    fn hop_skips_unusable_entries_and_lands_on_the_first_usable_one() {
        let accepted = vec!["ftp://not-a-mint.example".to_owned(), FIXTURE_MINT_URL.to_owned()];
        let plan = plan_payment("https://buyer.example", &accepted)
            .expect("a later usable entry is reachable");
        assert_eq!(plan.realized_mint(), &mint(FIXTURE_MINT_URL));

        let loopback = "http://127.0.0.1:3338";
        let accepted = vec![loopback.to_owned(), FIXTURE_MINT_URL.to_owned()];
        let plan = plan_payment("https://buyer.example", &accepted)
            .expect("an http mint is an ordinary hop target");
        assert_eq!(plan.realized_mint(), &mint(loopback));
    }

    // ---- select_source_mint: balance-aware source selection at accept (#497 behavior B) ----

    fn balance(mint_url: &str, sats: u64) -> crate::wallet_ops::MintBalance {
        crate::wallet_ops::MintBalance {
            mint_url: mint_url.to_owned(),
            balance_sats: sats,
            is_default: false,
            configured: true,
        }
    }

    // #497 / probe ⑤: the buyer's default (minibits) is not what the seller accepts (cuba), but the
    // buyer holds a covering balance at cuba. The old default-seeded plan HOPS minibits->cuba (melt
    // fee, drains the default); balance-aware selection seeds cuba and pays DIRECT from the
    // already-held balance — no hop, no melt fee.
    #[test]
    fn select_prefers_a_covering_accepted_mint_so_the_prefunded_balance_pays_direct() {
        let minibits = "https://minibits.example";
        let cuba = "https://cuba.example";
        let accepted = vec![cuba.to_owned()];
        let balances = vec![balance(minibits, 5_000), balance(cuba, 5_000)];

        // Control — the default seed hops to reach cuba (the ⑤ waste).
        let control = plan_payment(minibits, &accepted).expect("control plans");
        assert!(control.is_hop(), "control: the default-seeded plan hops minibits->cuba");

        // Balance-aware — cuba is selected and plan_payment pays direct from it.
        let seed = select_source_mint(minibits, &accepted, &balances, 100);
        assert_eq!(seed, cuba, "the held, accepted mint is chosen as the source");
        let plan = plan_payment(&seed, &accepted).expect("balance-aware plan");
        assert!(!plan.is_hop(), "direct from the held mint — no hop, no melt fee");
        assert_eq!(plan.realized_mint(), &mint(cuba));
    }

    // No accepted mint holds enough -> fall back to the configured default (today's behavior). A
    // balance below the amount, or no balance row at all, does not qualify.
    #[test]
    fn select_falls_back_to_the_default_when_no_accepted_mint_covers() {
        let minibits = "https://minibits.example";
        let cuba = "https://cuba.example";
        let accepted = vec![cuba.to_owned()];

        let thin = vec![balance(cuba, 99)];
        assert_eq!(select_source_mint(minibits, &accepted, &thin, 100), minibits);

        let elsewhere = vec![balance(minibits, 10_000)];
        assert_eq!(select_source_mint(minibits, &accepted, &elsewhere, 100), minibits);

        // Legacy: an empty accepted set has nothing to prefer; the default stands.
        assert_eq!(select_source_mint(minibits, &[], &elsewhere, 100), minibits);
    }

    // #266 guard: the widened balance read surfaces DB-discovered, unconfigured mints for DISPLAY,
    // but accept-time selection must ignore them — the selected mint is sealed into the pays-once
    // attempt id, so this row must never move it. Red if holds_at_least drops the configured pin.
    #[test]
    fn select_ignores_a_covering_unconfigured_discovered_mint() {
        let default_mint = "https://default.example";
        let stray = "https://stray.example";
        let accepted = vec![stray.to_owned()];
        let mut discovered = balance(stray, 5_000);
        discovered.configured = false;

        assert_eq!(
            select_source_mint(default_mint, &accepted, &[discovered], 100),
            default_mint,
            "DB discovery widens display only; it must not change the sealed funding mint"
        );
    }

    // SELECTION applies the same shape rule as plan_payment and nothing else: a covered mint the
    // seller lists is chosen whatever it is, and an entry that is not a mint URL is skipped even
    // when a balance is somehow recorded against it.
    #[test]
    fn select_takes_any_covered_listed_mint_and_skips_an_unusable_entry() {
        let real = "https://real-mint.example";
        let accepted = vec![real.to_owned()];
        let balances = vec![balance(real, 10_000)];
        assert_eq!(
            select_source_mint(FIXTURE_MINT_URL, &accepted, &balances, 100),
            real,
            "a covered mint the seller lists is selectable — there is no second gate"
        );

        let unusable = "ftp://not-a-mint.example";
        assert_eq!(
            select_source_mint(
                FIXTURE_MINT_URL,
                &[unusable.to_owned()],
                &[balance(unusable, 10_000)],
                100
            ),
            FIXTURE_MINT_URL,
            "an entry that is not a mint URL is skipped, falling back to the default"
        );
    }

    // Two covered, admissible accepted mints -> the FIRST in the seller's order wins, matching
    // plan_payment's deterministic first-admissible rule. Order is the only thing that moves it, so
    // a retry cannot re-order into a different sealed mint / attempt id.
    #[test]
    fn select_is_deterministic_first_covering_in_seller_order() {
        let default_mint = "https://default.example";
        let a = "https://a.example";
        let b = "https://b.example";
        let balances = vec![balance(a, 5_000), balance(b, 5_000)];
        assert_eq!(
            select_source_mint(default_mint, &[a.to_owned(), b.to_owned()], &balances, 100),
            a
        );
        assert_eq!(
            select_source_mint(default_mint, &[b.to_owned(), a.to_owned()], &balances, 100),
            b
        );
    }

    // Stale-source safety: selection happens ONCE at accept. If the chosen source is later drained,
    // the PAY path re-derives from the sealed source (plan_payment over the seal) and never
    // re-selects to a different held mint — so a drained sealed source lands on the wallet's existing
    // fail-closed insufficient-balance refusal, never a silent swap.
    #[test]
    fn a_sealed_source_is_honored_at_pay_and_never_re_selected() {
        let minibits = "https://minibits.example";
        let cuba = "https://cuba.example";
        let accepted = vec![cuba.to_owned()];
        // cuba is covered at accept and gets sealed.
        let sealed = select_source_mint(minibits, &accepted, &[balance(cuba, 5_000)], 100);
        assert_eq!(sealed, cuba);
        // At pay, re-derivation uses ONLY the sealed mint + accepted set — no balance input — so the
        // plan is sourced at the sealed cuba regardless of where funds now sit. A drained cuba then
        // fails the wallet's coverage check (the existing fail-closed guard), never a re-select.
        let pay_plan = plan_payment(&sealed, &accepted).expect("re-derives from the seal");
        assert_eq!(pay_plan.source_mint(), &mint(cuba), "pay honors the sealed source");
        assert!(!pay_plan.is_hop());
    }
}
