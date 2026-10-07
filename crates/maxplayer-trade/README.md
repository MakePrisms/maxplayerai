# maxplayer-trade — incomplete, blocked prototype

**No end-to-end trade was completed. This is not a trading-ready implementation.**
Draft work against design #1041 (`555fc08`), based on main `8c3bf74`.
Work stopped at the live funding/recovery blocker below, per the task's instruction to stop
and report blockers. Fixing that blocker alone will **not** complete the missing coordinator.

## Implemented

- Independent Cargo workspace and lock; pinned CDK/Cashu/CDK-SQLite **0.17.2**, Nostr SDK **0.44.1**.
  No dependency on maxplayer-core; no edits to other crates or root manifests/lock.
- CLI `preflight`, `fund`, `balance`; explicit separate `--home`, private seed and SQLite CDK
  wallet, cross-process home exclusion. Asset identity includes canonical mint URL and `sat`.
- Default mint fence: the two named testnuts hosts, loopback IPs and localhost. HTTPS required
  except loopback HTTP. No redirects (both preflight and pinned CDK's native HTTP transport).
  `--allow-real-mint` exists but **was not used**; real money remains separately unauthorized.
- Preflight checks reachable NUT-07/14 advertisements and reported clock within 60 seconds.
- Library primitives for signed 3410 immutable lots and 3411 status chains: signatures, versions,
  tag/content agreement, 24-hour expiry, terminal status, sequence/fork/gap checks; bounded
  binary-denomination fee calculation. These primitives are **not a backed listing CLI**.
- Non-tradable public relay diagnostic, using an independent receiver and NIP-44 for 23412.
  Diagnostic bodies are explicitly `trade_v:0`, not executable listings; no bearer proofs,
  wallet seeds, or trade preimages were published.

## Missing — do not mistake primitive tests for swap coverage

Not implemented: `list`, `discover`, `take`, cancellation CLI, backed proof reservations,
trade SQLite journal/outbox, quote negotiation/holds/caps/idempotency, either HTLC leg,
DLEQ/exact-condition/UNSPENT verification, preimage recovery, settlement/sold publication,
`recover`, lower-level signed HTLC refund adapter, or swap crash recovery.
No automated two-FakeWallet-mint/local-relay e2e, nonzero-fee swap, sell-back, overlisting test,
maker/taker disappearance test, or killed-process swap test has run. No live lots or swaps exist.
No production clock/recovery guarantee is asserted.

## Build and tests

From the repository root (standalone manifest is mandatory):

```sh
nix develop --extra-experimental-features 'nix-command flakes' --command \
  cargo build --manifest-path crates/maxplayer-trade/Cargo.toml --locked --bins --examples
nix develop --extra-experimental-features 'nix-command flakes' --command \
  cargo test --manifest-path crates/maxplayer-trade/Cargo.toml --locked
```

Current crate has wallet-only dependencies, so it does not require `protoc` yet. Adding the
planned in-process CDK mint fixtures requires the mint features and devshell `PROTOC`, as in
`crates/maxplayer-mint`; do not add this workspace to the root build.
For long builds redirect output to a log, background once, and use one long process wait;
do not pipe cargo through `tail` or start duplicate watchers.

| Suite | Passed | Failed | Meaning |
|---|---:|---:|---|
| Protocol primitives (`src/lib.rs`) | 21 | 0 | Asset fence, fee arithmetic, signed lifecycle/tampering/expiry |
| CLI integration (`tests/cli.rs`) | 3 | 0 | Real CLI entry point, home privacy/reopen, fence, process lock |
| Pinned-CDK bug reproducer (`tests/pinned_quote_recovery.rs`) | 1 | 0 | Confirms the bug, **not successful recovery** |
| Required two-mint trade e2e | — | — | Not implemented/run |
| Required swap failure/recovery suite | — | — | Not implemented/run |

An initial reproducer fixture used `u64::MAX` for SQLite expiry and failed conversion; it was
corrected to a valid timestamp and rerun. That fixture error is separate from the live blocker.
The root CI workflow does not test this independent workspace; no CI coverage is claimed here.

## Live run, 2026-10-07: funding blocker

Both mints returned reachable info, advertised NUT-07 and NUT-14, and observed clock skew 0 s:

| Role | Mint | Reported version | Final local balance |
|---|---|---|---:|
| Maker | https://testnut.cashudevkit.org | cdk-mintd/0.18.0 | 0 sat |
| Taker | https://testnut.cashu.space | cdk-mintd/0.17.0-rc.3 | 0 sat |

128-unit quote creation succeeded at both mints. The initial CLI called `mint()` before the
FakeWallet auto-payment had completed. CDK returned **`Amount undefined`** (mintable amount 0).
The CLI now waits for `PAID` before issuance, but the original quotes remain blocked:

- Maker quote: `01a1181c-9e87-7ff1-b9d4-176ec59adfda`
- Taker quote: `01a1181c-acbf-7a40-817a-8c37a9ca04bf`
- Both remote quotes subsequently reported **PAID**, amount 128, not issued.
- Both local `mint_quote` rows have amount_paid=128, amount_issued=0 and non-null
  `used_by_operation`, with **zero wallet_sagas rows**.
- Retrying via CDK's `check_mint_quote()` then `mint()` returned
  **`Quote already in use by another operation`** on both homes.

Pinned source trace:

1. `cdk-0.17.2/src/wallet/issue/saga/mod.rs::prepare_common` reserves before
   `prepare_after_reserve` rejects amount zero. This observed path leaves an orphan reservation.
2. `cdk-0.17.2/src/wallet/issue/mod.rs::inner_check_mint_quote_status` finds the missing saga
   and calls `release_mint_quote(operation_id)`, but keeps the original in-memory
   `mint_quote.used_by_operation` value.
3. Its final `add_mint_quote(mint_quote.clone())` writes that stale reservation back.
   `cdk-sql-common-0.17.2`'s upsert explicitly restores `used_by_operation`.
4. The next issuance cannot reserve the quote. The offline SQLite reproducer proves this
   release/write-back/re-reservation failure using the pinned public database API.

**This is an initial CLI timing bug followed by a pinned-client recovery bug, not proof of a
mint HTLC incompatibility.** No HTLC request was sent. No quote/proof reservation was forcibly
cleared; no dependency was upgraded, mint/NUT semantics changed, fresh wallet substituted,
or unsecured send attempted. The original homes are preserved privately for reconciliation.
A narrow, tested orphan-quote recovery fix/adapter is needed before resuming these quotes;
this draft does not implement or claim that fix.

### Exact commands used and reproducible diagnostic commands

These homes are **not committed**. On this machine:

```sh
TRADE=crates/maxplayer-trade/target/debug/maxplayer-trade
TRADE_RUN=/home/openclaw/.openclaw/workspace/.openclaw/tmp/credit-trade-live

# Initial attempts (using the pre-fix binary): quote creation succeeded, issuance failed.
"$TRADE" --home "$TRADE_RUN/maker" fund https://testnut.cashudevkit.org --amount 128
"$TRADE" --home "$TRADE_RUN/taker" fund https://testnut.cashu.space --amount 128

# Retried existing quotes after adding the PAID wait; both hit the orphan reservation.
"$TRADE" --home "$TRADE_RUN/maker" fund https://testnut.cashudevkit.org --amount 128 \
  --quote 01a1181c-9e87-7ff1-b9d4-176ec59adfda
"$TRADE" --home "$TRADE_RUN/taker" fund https://testnut.cashu.space --amount 128 \
  --quote 01a1181c-acbf-7a40-817a-8c37a9ca04bf

# Read local final balances (no issuance or trading).
"$TRADE" --home "$TRADE_RUN/maker" balance https://testnut.cashudevkit.org
"$TRADE" --home "$TRADE_RUN/taker" balance https://testnut.cashu.space

# Offline deterministic blocker reproduction, without touching preserved homes.
cargo test --manifest-path crates/maxplayer-trade/Cargo.toml --locked \
  --test pinned_quote_recovery

# Public non-tradable relay diagnostic; creates new diagnostic events on each run.
crates/maxplayer-trade/target/debug/examples/relay_probe
```

The corrected funding CLI has **not** been live-proven on fresh quotes after the stop.
There are no rerunnable `list → discover → take` commands yet; inventing them here would
misrepresent this draft.

## Public relay observations

Second run used different publishing and subscribing clients; the first self-subscription
probe was inconclusive because SDK event dedup suppresses the sender's own event notifications.
The table reports observed short-window readback, **not a 24-hour retention guarantee**.

| Relay | Kind | Publish ACK | Peer subscription | Immediate ID fetch |
|---|---:|---|---|---|
| relay.ditto.pub | 3410 | Accepted | Delivered | Absent |
| relay.ditto.pub | 3411 | Accepted | Delivered | Absent |
| relay.ditto.pub | 23412 | Accepted | Delivered | Absent (expected for ephemeral) |
| relay.damus.io | 3410 | Accepted | Delivered | Returned |
| relay.damus.io | 3411 | Accepted | Delivered | Returned |
| relay.damus.io | 23412 | Accepted | Delivered | Returned (unexpected for ephemeral) |

Ditto returned 3411 once in the earlier run, so persistent readback is inconsistent in these
observations. Damus is the candidate fallback for future listing tests; **no trade used either
relay**. NIP-44 confidentiality does not depend on ephemeral events actually being deleted.
No writes went to relay.maxplayer.ai.

Diagnostic event IDs, in the same order as the table:

```text
ditto 3410  b3d5f19219ddc953d83e0b2a14b0af54b0170346fa03ea41330c3e02a5b806e2
ditto 3411  0c651afb69ad0e156b68a46c15bfcbb62f9ce63a20e9d8b67671e102c19f4ee7
ditto 23412 cc395c74c40520549c5cf28d614dbf880442e8dbeb171d6a25d46cff368bf3e9
damus 3410  2a93d6632d236ee505c19dd3e8b2976637670cbd14e8325a9fde7d2f7cdc076d
damus 3411  4d30983a15b08b6f2359179be9b62621f30e360ab2442941d54a1b1d1a89a512
damus 23412 0c6afde437c4d7a95c6b522c368e36cfa2c2d00812e1d1070947fe13d30ce7dd
```

## Spec differences and constraints

- Authorized override: separate crate/home/wallet, no jobs, daemon, budget/ledger integration,
  MCP, or relay.maxplayer.ai. Fork publication instead of the repository skill's usual origin.
- Partial scope only: HTTPS/loopback assets; sat unit; bounded lot amounts ≤1,000,000 and status
  history ≤256. No Nostr mint transport is implemented. The amount/history bounds are local
  prototype limits, not changes to mint semantics.
- Library declares production 3600/900-second locks, 60-second quotes, and 180-second cutoff;
  **no executing swap path exists**, so this is not proof of timing enforcement.
- No trade fees implemented or charged. No hash-lock bypass, no unlocked transfers, no change to
  accepted late-claim semantics. Those money-path rules remain requirements for future work.
