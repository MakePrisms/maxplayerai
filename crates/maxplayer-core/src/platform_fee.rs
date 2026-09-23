//! Seller-side platform fee: a product-set rate, computed and journaled when a payment is collected
//! (stage 1), and a product-set payout destination the accrued balance is remitted to — automatically,
//! by the seller node, as a consequence of collecting (stage 2a).
//!
//! Three pieces live here — the rate ([`PLATFORM_FEE_BPS`]), the arithmetic ([`fee_sats`]) and the
//! destination ([`PLATFORM_FEE_ADDRESS`]) — so the collect seam and the remit path read them instead
//! of reimplementing any. Ungated on purpose: the arithmetic is worth compiling and testing on every
//! build, not only the money-path one.
//!
//! ## What is accrued, and what pays it
//!
//! The fee is **accrued and recorded** at collect time: every collected payment journals the rate in
//! force and the sats it comes to, against the job that earned it. **The seller node then remits it
//! automatically**: once the receipt is journaled new, the node makes a best-effort attempt to pay
//! the whole unremitted balance to [`PLATFORM_FEE_ADDRESS`] from the seller's ecash (`fee_remit`,
//! reached from `seller_node::run`). That attempt cannot affect the collect — the job is already
//! paid — and a failed attempt leaves the balance unremitted for the node's retry tick to try again
//! (base 30 s, doubling to a 30-minute cap, full jitter, for as long as the node runs; addendum 2);
//! balances below the destination's minimum accrue until they clear it. `maxplayer seller fees
//! remit` is the operator's inspection and recovery path (dry run by default, `--confirm` to pay
//! now), not the mechanism. The one operational switch, `[platform_fee] auto_remit`, stops the
//! automatic attempt and nothing else: it cannot touch the rate or the destination, and the fee stays
//! owed and visible.
//!
//! ## Who sets the rate and the destination
//!
//! The product does, in this source file. A seller cannot change either: there is no config key, no
//! env override and no CLI flag, and nothing parses, so nothing can fail at load. See
//! [`PLATFORM_FEE_ADDRESS`] for why the destination in particular must not be seller-editable.

/// The platform fee rate, in **basis points** (`1 bp = 0.01%`, so `250` is 2.5% and `10_000` is the
/// whole payment). This constant is the whole specification of the fee:
///
/// - It is set by the product, here, and not by the seller. No config surface reads or writes it.
/// - It is applied to the offer's **face** amount — the price the buyer paid, which is what the
///   receipt journals as `amount_sats` — not to the smaller sum that lands in the seller's wallet
///   after the mint takes its own swap fee.
/// - The fee on a payment is `floor(face × PLATFORM_FEE_BPS / 10_000)`; it **rounds down**, so a
///   payment too small to earn a whole sat at this rate owes a fee of zero.
/// - The rate in force is journaled beside every receipt (`receipts.fee_bps`), so a store that
///   outlives a change to this number still says what each collection owed.
///
/// **Currently `1000` — ten percent.** Collection accrues and records what this rate comes to on
/// each payment, and the seller node then remits the accrued balance to [`PLATFORM_FEE_ADDRESS`]
/// automatically, best-effort, after the receipt is journaled (`fee_remit`); `maxplayer seller fees
/// remit --confirm` pays it by hand. The receipt rows are the journal every remittance settles
/// against. Nothing about this rate is read from config or environment.
pub const PLATFORM_FEE_BPS: u32 = 1000;

/// The Lightning address (LUD-16, `user@host`) the accrued platform fee is remitted to. Ordered by
/// Josip (real-sats authority), 2026-09-07, as `maxplayer@agi.cash`; moved to `maxplayer@strike.me`
/// on 2026-09-18 (PR #1026). This constant is the whole specification of where the fee goes:
///
/// - It is set by the product, here, and **not by the seller**. There is no config key, no env
///   override and no CLI flag, deliberately: the seller runs this binary, and the seller is the
///   party that owes the fee. A seller-editable `fee_address` would let any seller point the
///   platform's fee at themselves. A compiled-in constant costs a release to change; a seller-editable
///   one costs the whole fee. The release is the cheaper defect.
/// - It is resolved at remit time over LNURL-pay (`https://host/.well-known/lnurlp/user`), fail
///   closed, by [`crate::lnurl_pay`] — by the seller node's automatic attempt after a collect and by
///   `maxplayer seller fees remit`, and by nothing else. Nothing in this module contacts it.
/// - The `[platform_fee] auto_remit` switch cannot change it: that table has no key for an address.
/// - **Every remittance journals the literal it paid** (`fee_remittances.destination`), so a later
///   change to this constant leaves a readable history rather than an ambiguous one.
///
/// The proper long-term fix — an authoritatively signed platform parameter carrying the rate and the
/// address together, so neither needs a release — is a separate, later stage.
pub const PLATFORM_FEE_ADDRESS: &str = "maxplayer@strike.me";

/// Basis points in one hundred percent — the ceiling on any rate.
pub const BPS_PER_WHOLE: u32 = 10_000;

// The rate can never exceed the whole payment. Checked at compile time so a bad edit to the constant
// fails the build, not a seller.
const _: () = assert!(
    PLATFORM_FEE_BPS <= BPS_PER_WHOLE,
    "PLATFORM_FEE_BPS must not exceed 10_000 (100%)"
);

/// The platform fee owed on one collected payment: `floor(face_sats × fee_bps / 10_000)`.
///
/// `face_sats` is the offer's face amount — what the buyer paid, and what the seller node's
/// collect path hands over as `amount_received` (the adapter returns the face once the mint's net
/// credit plus the predicted mint fee reconcile to it). It is NOT the wallet net: the mint's own
/// swap fee is a separate figure, journaled beside this one and never folded into the base.
/// `fee_bps` is the rate in basis points; the seam passes [`PLATFORM_FEE_BPS`], tests pass whatever
/// rate they are proving. The product is taken in `u128` so it cannot overflow for any `u64`
/// amount, and the division rounds **down**: a seller is never charged a sat the arithmetic did not
/// earn, and a payment too small to owe a whole sat owes zero — a fee, not an error.
///
/// A rate above 100% is not a fee anyone can owe; it is clamped to the whole amount so the result
/// always fits a `u64` and never exceeds `face_sats`.
pub fn fee_sats(face_sats: u64, fee_bps: u32) -> u64 {
    let fee_bps = fee_bps.min(BPS_PER_WHOLE);
    let product = u128::from(face_sats) * u128::from(fee_bps);
    let fee = product / u128::from(BPS_PER_WHOLE);
    u64::try_from(fee).expect("a fee of at most 100% of a u64 amount fits a u64")
}

/// What the seller keeps of one collected payment: `face − mint_fee − platform_fee`, saturating at
/// zero. This is a display-time derivation from the three journaled figures — it is deliberately
/// NOT stored, so it can never disagree with the columns it comes from. Both deductions are taken
/// from the face: the mint's swap fee is what the mint kept before the sats reached the wallet, and
/// the platform fee is what collection records as owed — remitted afterwards by the seller node
/// itself (after the collect, and on its retry clock), or by `maxplayer seller fees remit --confirm`.
pub fn kept_sats(face_sats: u64, mint_fee_sats: u64, platform_fee_sats: u64) -> u64 {
    face_sats
        .saturating_sub(mint_fee_sats)
        .saturating_sub(platform_fee_sats)
}

/// Render a basis-points rate as the percentage a seller reads: `1000 → "10%"`, `250 → "2.5%"`,
/// `1 → "0.01%"`. Used by the seller-facing read-out so the label says the rate, not the unit.
pub fn bps_to_percent_label(fee_bps: u32) -> String {
    let whole = fee_bps / 100;
    let fraction = fee_bps % 100;
    if fraction == 0 {
        format!("{whole}%")
    } else if fraction.is_multiple_of(10) {
        format!("{whole}.{}%", fraction / 10)
    } else {
        format!("{whole}.{fraction:02}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the fee arithmetic (§5: 100 sats at 2% → 2; 1 sat at 2% → 0; 100% → all; overflow) ----

    #[test]
    fn two_percent_of_one_hundred_sats_is_two_sats() {
        assert_eq!(fee_sats(100, 200), 2);
    }

    #[test]
    fn a_one_sat_payment_at_two_percent_owes_a_fee_of_zero_and_is_not_an_error() {
        // 1 × 200 / 10_000 = 0.02, rounded DOWN. Zero is a fee, not a refusal.
        assert_eq!(fee_sats(1, 200), 0);
        // The same rounding, one step up: 49 sats at 2% is 0.98 → 0; 50 sats is exactly 1.
        assert_eq!(fee_sats(49, 200), 0);
        assert_eq!(fee_sats(50, 200), 1);
    }

    #[test]
    fn one_hundred_percent_takes_the_whole_amount() {
        assert_eq!(fee_sats(12_345, BPS_PER_WHOLE), 12_345);
        assert_eq!(fee_sats(u64::MAX, BPS_PER_WHOLE), u64::MAX);
    }

    #[test]
    fn zero_basis_points_takes_nothing() {
        assert_eq!(fee_sats(12_345, 0), 0);
        assert_eq!(fee_sats(u64::MAX, 0), 0);
    }

    #[test]
    fn a_large_amount_that_would_overflow_u64_arithmetic_is_computed_exactly() {
        // u64::MAX × 250 overflows a u64 by four orders of magnitude; in u128 it is exact.
        // floor(18_446_744_073_709_551_615 × 250 / 10_000) = 461_168_601_842_738_790.
        assert_eq!(fee_sats(u64::MAX, 250), 461_168_601_842_738_790);
        // And the fee never exceeds the amount, at the bound where an overflowed product would.
        assert!(fee_sats(u64::MAX, 9_999) < u64::MAX);
    }

    #[test]
    fn fractional_rates_round_down_per_payment() {
        // 2.5% (250 bp) of 100 is exactly 2.5 → 2.
        assert_eq!(fee_sats(100, 250), 2);
        // One basis point of 9_999 is 0.9999 → 0; of 10_000 it is exactly 1.
        assert_eq!(fee_sats(9_999, 1), 0);
        assert_eq!(fee_sats(10_000, 1), 1);
    }

    #[test]
    fn a_rate_above_the_whole_is_clamped_to_the_whole_amount() {
        assert_eq!(fee_sats(100, 10_001), 100);
        assert_eq!(fee_sats(u64::MAX, u32::MAX), u64::MAX);
    }

    // ---- what the seller keeps, and the label ----

    #[test]
    fn kept_is_face_minus_mint_fee_minus_platform_fee_and_never_negative() {
        // Face 100, mint fee 1, platform fee 10 (10% of the FACE, not of the 99 net) ⇒ 89 kept.
        assert_eq!(kept_sats(100, 1, fee_sats(100, 1000)), 89);
        // No mint fee: 100 − 0 − 10.
        assert_eq!(kept_sats(100, 0, 10), 90);
        // Deductions that would exceed the face saturate at zero rather than wrapping.
        assert_eq!(kept_sats(5, 3, 3), 0);
        assert_eq!(kept_sats(0, u64::MAX, u64::MAX), 0);
    }

    #[test]
    fn bps_render_as_the_percentage_a_seller_reads() {
        assert_eq!(bps_to_percent_label(0), "0%");
        assert_eq!(bps_to_percent_label(1000), "10%");
        assert_eq!(bps_to_percent_label(250), "2.5%");
        assert_eq!(bps_to_percent_label(1), "0.01%");
        assert_eq!(bps_to_percent_label(10_000), "100%");
        assert_eq!(bps_to_percent_label(PLATFORM_FEE_BPS), "10%");
    }

    // ---- the constants ----

    #[test]
    fn the_shipped_rate_is_ten_percent_and_within_the_whole() {
        // Collection accrues at 10%; the seller node remits it after the collect (`fee_remit`). The
        // `<= 10_000` bound is enforced at compile time by the `const _` assertion in the module body.
        assert_eq!(PLATFORM_FEE_BPS, 1000);
        assert_eq!(fee_sats(100, PLATFORM_FEE_BPS), 10);
        assert_eq!(fee_sats(9, PLATFORM_FEE_BPS), 0);
    }

    // Stage 2a: the destination is the address Josip ordered, in LUD-16 `user@host` shape, and it is
    // a constant — no config surface reads or writes it (see the doc comment for why).
    #[test]
    fn the_shipped_destination_is_the_ordered_lightning_address() {
        assert_eq!(PLATFORM_FEE_ADDRESS, "maxplayer@strike.me");
        let (user, host) = PLATFORM_FEE_ADDRESS
            .split_once('@')
            .expect("a LUD-16 address has exactly one @");
        assert_eq!((user, host), ("maxplayer", "strike.me"));
        assert!(!host.contains('@') && !host.contains('/'));
    }
}
