# Round-four scope follow-up evidence — 2026-10-08

## Status and provenance

Implementation: `19299727e5e25479977cd95dee86a45a16bb3821`. Integration baseline: `1c68714208b310c2f6e12bdccf35bee212985af8` (subsequent skill/CLI-test commits; production source unchanged). This docs-only evidence does not certify round-five changes.

**At Bob's request, 1929972 was pushed around 22:09 UTC before the 20-round repetitions, stress run, and automatic refund rerun finished.**

**cashu-rust-dev's final review of 1c68714 returned REQUEST-CHANGES (H1, H2, M1–M4 and additional findings). A separate round-five session owns fixes. Passing author tests do not supersede that verdict. PR #1107 remains draft; this is not approval to merge.**

## Scope delivered

Removed real-money opt-in; automatic preflight includes NUT-07/09/12/14, an active sat keyset and clock-skew admission. New/prepared lock submission rechecks admission. Gross locks, cumulative per-mint/home funding, and withdrawal invoices have 100,000-sat caps. Withdrawal reserve ceiling remains 32 sats. Recovery makes one item-isolated pass and reports unresolved work with status 2; unchanged records remain recoverable. Review findings qualify these claims; see round five before relying on them operationally.

## Verification

| Suite | Default | Lab |
|---|---:|---:|
| Library | 22 | 22 |
| CLI | 5 | 5 |
| Trade/recovery | 6 | 11 |
| Money | 16 | 17 |
| Pinned quote | 1 | 1 |
| Relays | 7 | 7 |
| Review regressions | 17 | 46 |
| Total | **74** | **109** |

Both clippy invocations and fmt exited 0. These frozen binaries predate the added CLI tests from the skill integration: do not attribute those incoming tests to these counts.

20 rounds completed: **80/80 unique suite invocations, 1,040 passing tests, zero failures**. Each round: 2 trade/sell-back, 26 selected review regressions, 17 money, 7 relay tests. Every suite used `--test-threads=1`; two CPU-isolated lanes, unchanged production timings/assertions.

**Stress limitation at publication:** seven executable entries exited 0, but the eighth (`review_regressions`, 46 tests) was still running when evidence was captured. The runner and two CPU burners were observed alive. `final-stress.json` is a partial receipt, not proof of full stress completion. No new runner was started or existing runner stopped.

Five mutations (publish missing-ACK, withdrawal change restore, funding cap, withdrawal invoice cap, gross-lock cap) each compiled and failed its safety assertion with exit 101, then passed restored with exit 0. A sixth mutation restoring the global recovery wait failed the bounded-exit assertion, restored passed. Exact-cap and one-over tests, restart funding accounting, and unreachable-mint isolation are included.

## Final-binary live fake-money results

Fresh homes, default-feature frozen binary, testnut.cashudevkit.org ↔ testnut.cashu.space. Trade and sell-back completed on both sides. Lot/available/sold chains independently read back from nos.lol, relay.primal.net and offchain.pub. Balances after sell-back: maker 118 devkit/6 space; taker 6 devkit/118 space.

The first timed scenario lost its maker process before deadlines. Most likely harness process-group cleanup after its scheduler was killed; no exact maker exit receipt survives, so the signal/cause is unproven. Delayed explicit recovery refunded both sides: maker 116 devkit/6 space; taker 6 devkit/116 space. This first run is not automatic-serve evidence.

Detached rerun: swap `16802081-bba2-4de0-ac6d-3e3eb6a37dcc`, production lock intervals 3600/900 seconds and 60-second refund margin. Taker killed before claim. **Maker serve refunded automatically with no maker recover call**, then was intentionally stopped only after the refunded journal and audit. Taker explicitly recovered after long+margin. Both journals ended `refunded`; each 128 → 126 fake sats (4 fake sats fees total). Each audit found 3/3 spent outgoing proofs carrying empty-preimage HTLC refund witnesses with one signature; zero missing/invalid witnesses. Signatures and secrets redacted.

## Limits and handoff

No real wallets/homes were accessed in this follow-up. Real Nutshell refund and real-sat accounting remain historical evidence at 9dad8e6, not rerun on this binary; see ../round4-20261008/REPORT.md. Public relay availability and long-term retention are not guaranteed. Exact old maker termination remains unproven. Full stress completion and this docs head's CI/Vercel are not asserted here. No new CI watcher or tests were started under the final instruction to finish and stop.

See [CLI-STATE-HANDOFF.md](CLI-STATE-HANDOFF.md) for every command/flag and swap/withdrawal state. It is pinned to round four and must not override round-five fixes. JSON receipts and suite log hashes accompany this report. No invoices, tokens, private keys, mnemonics or claim preimages are included.
