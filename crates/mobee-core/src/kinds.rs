//! The single registry of every mobee nostr event kind.
//!
//! This module is the ONE place a kind *number* may appear. Every other site refers to a named
//! constant from here (re-exported through [`crate::gateway`] for the trade-path kinds). The
//! trade kinds form a contiguous mobee-owned block — `3400`–`3405` — plus the addressable
//! seller heartbeat `30340`.
//!
//! | Kind | Object | Author |
//! |---|---|---|
//! | `3400` | RECEIPT | buyer + seller (co-signed) |
//! | `3401` | OFFER | buyer |
//! | `3402` | CLAIM (bid + `creq` invoice) | seller |
//! | `3403` | RESULT | seller |
//! | `3404` | FEEDBACK (progress / error / refusal) | seller |
//! | `3405` | AWARD (claim selection) | buyer |
//! | `30340` | SELLER HEARTBEAT (addressable, `d="mobee-seller"`) | seller |

/// Co-signed settlement receipt (buyer + seller).
pub const JOB_RECEIPT_KIND: u16 = 3400;
/// Buyer-authored work offer.
pub const JOB_OFFER_KIND: u16 = 3401;
/// Seller-authored claim carrying the NUT-18 `creq` invoice.
pub const JOB_CLAIM_KIND: u16 = 3402;
/// Seller-authored typed delivery.
pub const JOB_RESULT_KIND: u16 = 3403;
/// Seller-authored progress / error / refusal feedback.
pub const JOB_FEEDBACK_KIND: u16 = 3404;
/// Buyer-authored claim award / acceptance — e-tags the offer + winning claim.
/// A buyer-authored selection must not ride the seller's feedback kind.
pub const JOB_AWARD_KIND: u16 = 3405;
/// Addressable seller liveness heartbeat, `d="mobee-seller"`. Must stay in the NIP-01
/// parameterized-replaceable range `30000`–`39999`, hence `30340` (not a `34xx` value).
pub const SELLER_HEARTBEAT_KIND: u16 = 30340;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trade_path_kinds_are_the_contiguous_mobee_block() {
        // The trade path lives in a contiguous mobee-owned block; none reuse the generic DVM range.
        assert_eq!(
            [
                JOB_RECEIPT_KIND,
                JOB_OFFER_KIND,
                JOB_CLAIM_KIND,
                JOB_RESULT_KIND,
                JOB_FEEDBACK_KIND,
                JOB_AWARD_KIND,
            ],
            [3400, 3401, 3402, 3403, 3404, 3405]
        );
        for kind in [5109u16, 6109, 7000] {
            assert!(
                ![
                    JOB_RECEIPT_KIND,
                    JOB_OFFER_KIND,
                    JOB_CLAIM_KIND,
                    JOB_RESULT_KIND,
                    JOB_FEEDBACK_KIND,
                    JOB_AWARD_KIND
                ]
                .contains(&kind),
                "generic DVM kind {kind} must not appear in the mobee block"
            );
        }
    }

    #[test]
    fn heartbeat_is_addressable_replaceable() {
        assert!((30000..=39999).contains(&SELLER_HEARTBEAT_KIND));
    }
}
