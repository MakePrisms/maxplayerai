# known-drift — where upstream docs are wrong at our pin

Each of these cost real time on 2026-09-09 and each is settled by a source line, not by memory.
When a doc and the pinned source disagree, **the source wins**.

## 1. `cdk-mintd` config: the section is `[ln]`, not `[payment_backend]`

Notes circulating internally describe `[payment_backend] backend = "fakewallet"` and
`CDK_MINTD_LN_BACKEND=fakewallet`. Neither is the 0.17.2 schema.

Real shape (`cdk-mintd-0.17.2/src/config.rs:1002` `struct Settings`;
`example.config.toml:116-119`, `:283`):

```toml
[ln]
ln_backend = "fakewallet"
unit = "sat"

[fake_wallet]
fee_percent = 0.02
reserve_fee_min = 1
```

`ln` deserializes as `Vec<Ln>` via an untagged `LnOneOrMany`, so `[ln]` (one) and `[[ln]]`
(per-unit) are both valid. Duplicate `(unit, method)` pairs are rejected at startup.

## 2. `[ln]` min/max are REQUIRED, though the example comments them out

`example.config.toml` shows `min_mint`, `max_mint`, `min_melt`, `max_melt` commented out. But
`struct Ln` (`src/config.rs:170-179`) gives them **no** `#[serde(default)]`. Omit any one and the
whole config fails with:

```
data did not match any variant of untagged enum LnOneOrMany for key `ln`
```

— an error that names the enum and not the missing field, and points nowhere near the cause. All
four must be present. `impl Default for Ln` uses 1 / 500000 / 1 / 500000.

## 3. `cdk-mintd` needs `protoc` at build time even with `--no-default-features`

`cdk-mintd` depends on `cdk-signatory` unconditionally, and that crate's `build.rs:14` compiles
`src/proto/signatory.proto` regardless of the `grpc` feature. Without `protobuf-compiler` the build
panics with "Could not find `protoc`". Nothing in the feature flags avoids it.

## 4. `pay_err` alone does NOT simulate a failed payment

`cdk-fake-wallet-0.17.2/src/lib.rs:706-714` inserts `check_payment_state` into its payment-states
map **before** it honours `pay_err`. So with `FakeInvoiceDescription::default()`'s `Paid` and only
`pay_err: true`, `make_payment` errors, the mint re-checks the payment status, sees `PAID`, and the
melt finalises `state=Paid`. Measured exactly that: `state=Paid, amount=5, fee_paid=0` for an
invoice the backend refused.

A genuine failure needs **both**:

```rust
FakeInvoiceDescription {
    pay_invoice_state: MeltQuoteState::Unpaid,
    check_payment_state: MeltQuoteState::Unpaid,
    pay_err: true,
    check_err: false,
}
```

Then the wallet call errors with `Payment failed` and the balance is unchanged.

The general lesson, worth more than the four items: a fake backend that reports success when asked
the wrong way will make a test suite green without testing anything. Assert on amounts and states,
never on "the call returned".
