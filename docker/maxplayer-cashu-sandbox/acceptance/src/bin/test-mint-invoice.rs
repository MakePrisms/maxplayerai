//! test-mint-invoice — print a BOLT11 string the sandbox fakewallet mint will settle (or refuse).
//!
//! Usage: test-mint-invoice <msat> [fail]
//!
//! `fail` injects a genuine payment failure. Both `pay_err` AND the two states must say so: the
//! fake backend records `check_payment_state` into its payment-states map BEFORE it honours
//! `pay_err` (cdk-fake-wallet 0.17.2 src/lib.rs:706-714), so leaving the states at their `Paid`
//! default produces a melt that finalises `Paid` for an invoice the backend refused.

use cdk::nuts::MeltQuoteState;
use cdk_fake_wallet::{create_fake_invoice, FakeInvoiceDescription};

fn main() {
    let mut args = std::env::args().skip(1);
    let msat: u64 = match args.next().map(|a| a.parse()) {
        Some(Ok(v)) => v,
        _ => {
            eprintln!("usage: test-mint-invoice <msat> [fail]");
            std::process::exit(64);
        }
    };
    let fail = args.next().is_some_and(|a| a == "fail");

    let description = if fail {
        FakeInvoiceDescription {
            pay_invoice_state: MeltQuoteState::Unpaid,
            check_payment_state: MeltQuoteState::Unpaid,
            pay_err: true,
            check_err: false,
        }
    } else {
        FakeInvoiceDescription::default()
    };

    let json = serde_json::to_string(&description).expect("serialize fake invoice description");
    println!("{}", create_fake_invoice(msat, json));
}
