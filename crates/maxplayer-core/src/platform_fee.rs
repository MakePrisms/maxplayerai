//! Seller-side platform fee, stage 1: a product-set rate, computed and journaled when a payment is
//! collected.
//!
//! Two pieces live here — the rate ([`PLATFORM_FEE_BPS`]) and the arithmetic ([`fee_sats`]) — so the
//! collect seam reads one and calls the other instead of reimplementing either. Ungated on purpose:
//! the arithmetic is worth compiling and testing on every build, not only the money-path one.
//!
//! ## What this stage does and does not do
//!
//! The fee is **accrued and recorded**. Every collected payment journals the rate in force and the
//! sats it comes to, against the job that earned it. **Nothing is remitted**: there is no fee
//! recipient in the product yet, so no call here or in any caller moves a sat. Paying the accrued
//! balance out is a later stage that sits on top of this journal.
//!
//! ## Who sets the rate
//!
//! The product does, in this source file. A seller cannot change it: there is no config key, no env
//! override and no CLI flag, and nothing parses, so nothing can fail at load.

/// The platform fee rate, in **basis points** (`1 bp = 0.01%`, so `250` is 2.5% and `10_000` is the
/// whole payment). This constant is the whole specification of the fee:
///
/// - It is set by the product, here, and not by the seller. No config surface reads or writes it.
/// - It is applied to the **net** amount the seller actually received at the mint — the sats left
///   after the mint's own swap fee — never to the offer's face amount.
/// - The fee on a payment is `floor(amount_received × PLATFORM_FEE_BPS / 10_000)`; it **rounds
///   down**, so a payment too small to earn a whole sat at this rate owes a fee of zero.
/// - The rate in force is journaled beside every receipt (`receipts.fee_bps`), so a store that
///   outlives a change to this number still says what each collection owed.
///
/// **Currently `1000` — ten percent.** Stage 1 accrues and records what this rate comes to on each
/// collection; it pays nobody, because no payout destination exists in the product yet. The rows
/// this stage writes are a journal, not a bill.
pub const PLATFORM_FEE_BPS: u32 = 1000;

/// Basis points in one hundred percent — the ceiling on any rate.
pub const BPS_PER_WHOLE: u32 = 10_000;

// The rate can never exceed the whole payment. Checked at compile time so a bad edit to the constant
// fails the build, not a seller.
const _: () = assert!(
    PLATFORM_FEE_BPS <= BPS_PER_WHOLE,
    "PLATFORM_FEE_BPS must not exceed 10_000 (100%)"
);

/// The platform fee owed on one collected payment: `floor(amount_received × fee_bps / 10_000)`.
///
/// `amount_received` is the NET the seller actually got at the mint (after the mint's own swap
/// fee), which is the base the rate applies to. `fee_bps` is the rate in basis points; the seam
/// passes [`PLATFORM_FEE_BPS`], tests pass whatever rate they are proving. The product is taken in
/// `u128` so it cannot overflow for any `u64` amount, and the division rounds **down**: a seller is
/// never charged a sat the arithmetic did not earn, and a payment too small to owe a whole sat owes
/// zero — a fee, not an error.
///
/// A rate above 100% is not a fee anyone can owe; it is clamped to the whole amount so the result
/// always fits a `u64` and never exceeds `amount_received`.
pub fn fee_sats(amount_received: u64, fee_bps: u32) -> u64 {
    let fee_bps = fee_bps.min(BPS_PER_WHOLE);
    let product = u128::from(amount_received) * u128::from(fee_bps);
    let fee = product / u128::from(BPS_PER_WHOLE);
    u64::try_from(fee).expect("a fee of at most 100% of a u64 amount fits a u64")
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

    // ---- the constant ----

    #[test]
    fn the_shipped_rate_is_ten_percent_and_within_the_whole() {
        // Stage 1 accrues at 10% and pays nobody. The `<= 10_000` bound is enforced at compile time
        // by the `const _` assertion in the module body.
        assert_eq!(PLATFORM_FEE_BPS, 1000);
        assert_eq!(fee_sats(100, PLATFORM_FEE_BPS), 10);
        assert_eq!(fee_sats(9, PLATFORM_FEE_BPS), 0);
    }
}
