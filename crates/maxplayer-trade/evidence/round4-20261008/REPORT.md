# Round 4 — author-led verification, 2026-10-08

This is evidence for a final external review, **not independent reviewer approval or a
production safety certification**. PR #1107 remains draft. Only `crates/maxplayer-trade`
changes. No Maxplayer job, merge, CLI/core modification, new real funding, or production
relay use occurred. Starting PR head: `1e0c30af76465c0576bb5d2e9f53ead1b1968eba`.

## Changes and safety evidence

| Change | Tests/evidence | Mutation result |
|---|---|---|
| Real-money commits integrated with round-three quarantine handling | `money` 12 default / 13 lab; cap and fence tests; existing N3/probe suite retained | Removing dual opt-in fence fails `SAFETY: real mint requires BOTH exact URL and environment opt-in`; restored control passes |
| Fail-closed withdrawal and exact change reconciliation | Lost reply, pending/unpaid/paid reconciliation, restore failure, reopen and reservation tests | Swallowing restore failure and marking Done fails `SAFETY: restore failure must not mark done`; restored control passes |
| Durable publication receipts, retry budgets, relay-wide cooldown/block | Seven relay tests; restart dedup, negative OK, blocked relay and ACK-without-storage coverage | Missing ACK treated as success fails `SAFETY: missing ACK must not advance maker money state`; restored control passes |
| Money admission requires prior publication receipt | `r4_missing_quote_ack_cannot_authorize_maker_lock` | Same publish-rule mutation above |
| Mint-only timed refunds remain reachable during relay outage | `r4_all_relays_down_after_both_locks_still_refunds`; both roles restore exact balances | Not separately mutated |
| Reduce unchanged-event traffic | Full zero/fee-bearing trade and sell-back assert ten unique events and one send/event/healthy relay | Not separately mutated |
| Retain journal errors; replace money-path unwraps; audit redaction | Full suites, full contention suite, default/lab clippy; actual live audit | Not separately mutated |

The three mutations compiled and failed at the intended safety assertion, not setup or build.
Each restored control passed. See [mutations.json](mutations.json).

## Automated verification

| Suite | Default passed | Lab passed |
|---|---:|---:|
| Library | 22 | 22 |
| CLI | 5 | 5 |
| Full trade/recovery | 6 | 11 |
| Funding/withdrawal | 12 | 13 |
| Pinned quote reproducer | 1 | 1 |
| Relays | 7 | 7 |
| Review regressions | 13 | 41 |
| **Total, zero failures** | **66** | **100** |

All used `--test-threads=1`. The full lab suite also passed **100/100 under deliberate CPU
contention**: two busy-loop processes and tests sharing CPUs 0–3. See [stress.json](stress.json).
No timing failure required an isolated rerun in these suites. Fmt passed. Default and lab
clippy `--all-targets --no-deps` exited 0 with non-blocking style warnings; not warning-free.
Root CI is separate: it does not test this independent Cargo workspace.

**20/20 complete rounds passed: 860 test executions across 80 distinct suite invocations,
zero failures and no isolated reruns.** Each round: two full trade + sell-back tests
(zero-fee and fee-bearing), 21 N-series/probe/R4 checks, 13 money tests and seven relay tests.
Totals: 40 roundtrip tests, 220 N-series, 160 probe, 40 R4, 260 money and 140 relay executions.
See [loops.json](loops.json), including original-log hashes and exact counts.

The first five rounds were serialized. During round six, only the Python scheduling
parent was paused; its active suite ran uninterrupted to natural exit 0 and 21/0. Its
exact elapsed time was not captured. The stopped scheduler was then replaced, without
restarting any completed test or overwriting logs. Remaining suites ran in two isolated
CPU lanes (0–3 and 4–7), still `--test-threads=1` within every suite. Same frozen binaries,
assertions and timings throughout. This scheduler termination is not a test failure.

Repeated compiled-test filters: e2e `trade_and_sell_back`; review regressions
`n1_ n2_ n3_ n4_ probe_ r4_`; all money tests; all relay tests. Every invocation used
`TRADE_LAB_SECONDS=1`, `--test-threads=1 --nocapture`, with the real-money opt-in unset.

## Relay probes and traffic

Each cell is **publish ACK / live delivery / stored fetch-by-ID at ≥60 seconds**.
The stored probe used a new reader that had not seen the live events. Fresh ephemeral
keys only; no funded-home identity was used. Two probe rounds corroborated the result.

| Relay | 3410 | 3411 | 23412 | Default |
|---|---|---|---|---|
| nos.lol | yes/yes/yes | yes/yes/yes | yes/yes/yes | yes |
| relay.primal.net | yes/yes/yes | yes/yes/yes | yes/yes/yes | yes |
| offchain.pub | yes/yes/yes | yes/yes/yes | yes/yes/yes | yes |
| relay.ditto.pub | yes/yes/yes | yes/yes/yes | yes/yes/no | no |
| nostr-pub.wellorder.net | yes/yes/yes | yes/yes/yes | yes/yes/no | no |
| relay.nostr.band | no/no/no | no/no/no | no/no/no | no |

Raw public probe IDs and measurements: [relay-results.json](relay-results.json).
The old-head reproduction sent **38 EVENT attempts for ten unique events** in one
7.60-second trade across two relays. By kind: 3410=2, 3411=6, 23412=30; some unchanged
events were sent ten times. The old three-second serve tick resent unchanged messages,
plus duplicate deliveries triggered replies. Damus explicitly reported a rate-limit ban;
its actual historical threshold and send total remain unknown.

The fixed controlled path sends ten unique events, once per healthy relay (20 attempts
for two, 30 for the three defaults). Concurrent live completion can omit the claimed
notice when mint evidence already proves completion, giving nine unique events.
At least one positive recorded ACK is required. Other copies receive bounded exponential
backoff and jitter; ACKed copies are not resent. Rate-limited responses pause the whole
relay for ≥5 minutes; blocked/banned responses disable that home's writes to it. Attempts
are capped at 12 per event/relay, with a 24-hour age cutoff. Exhaustion requires another
explicitly configured relay; restarts do not reset budgets.

Offchain missed some actual live ACKs, including an event subsequently independently
read back there. Nos and Primal ACKed every inspected live event. The cause of Offchain's
missed ACKs is unverified. ACK is never a storage guarantee, and 60-second retention is
not 24-hour retention. Encrypted ephemeral-kind events may be stored.

## Frozen default binary and live fake-money trade/sell-back

Production CLI SHA-256:
`9f03aa0d687d52a4eba281290d71ee927397f198c59a799c2288a9e51b23d7be`.
[Source fingerprints](source-sha256.json) cover the exact code/tests used. Default features,
no lab environment: actual quotes asserted **3600/900/180/60 seconds** for
long/short/cutoff/margin. Both refund scenarios ran concurrently, sleeping to deadlines.

Fresh fake homes received 128 units each, 256 total. Both directions completed on the new
default relays; lot, available and sold events were independently fetched from **all three**.
See [forward](fake-forward.json) and [sell-back](fake-sellback.json).

| Stage | Maker devkit | Maker space | Taker devkit | Taker space |
|---|---:|---:|---:|---:|
| Funded | 128 | 0 | 0 | 128 |
| Forward 32 devkit / 24 space | 93 | 24 | 32 | 101 |
| Sell-back 16 space / 24 devkit | 117 | 6 | 6 | 117 |
| Timed refund scenario complete | 115 | 6 | 6 | 115 |

Final fake balances sum to **242 = 256 − 14 mint fees**; these are not real sats.
Devkit version cdk-mintd/0.18.0; space cdk-mintd/0.17.0-rc.3.

## Both production-timing refunds

Both sides locked. Each taker was stopped at durable `second_validated`, with nonempty
incoming/outgoing proof sets and **no claim authorization**, then killed. Makers refunded
after short +60 seconds; takers were recovered after long +60 seconds (+5 runner margin).
This tests the harder fresh-evidence branch with stored incoming proofs, not the empty
incoming shortcut. No mint timing or production timeout was shortened.

| Scenario | Swap | Maker eligible | Taker eligible | Result |
|---|---|---|---|---|
| Fake devkit ↔ space | `95123d9c-1998-4dc2-9ec2-60db193a8013` | 15:40:14 UTC | 16:25:14 UTC | both refunded |
| Real Macadamia ↔ cashu.cz | `e242bfeb-28ee-459e-b700-42624330114c` | 15:40:24 UTC | 16:25:24 UTC | both refunded |

Real maker was existing `pair1-taker`, giving 24 Macadamia net; real taker was existing
`pair2-taker`, giving 24 cashu.cz net. **No new real funding.** Macadamia: 116→114;
cashu.cz: 104→104. Real scenario fees: **2 sats**, no unexplained loss.

The real `recover` command remained running because the unrelated old Minibits withdrawal
was still pending and fenced. The harness timed out after 180 seconds and recorded STOP,
although both swap journals already said `refunded`. No replacement recovery/payment was
made. Independent read-only typed and raw-wire audits confirmed both refunds. This harness
completion limitation is retained honestly in [real-refund-verified.json](real-refund-verified.json).

### Real Nutshell NUT-07 evidence (redacted)

cashu.cz reports Nutshell/0.21.0. Both outgoing proofs are **SPENT, 2/2**; missing/invalid,
PENDING and UNSPENT counts are all zero. Each witness is a **JSON string** encoding an
object with keys `preimage` and `signatures`: preimage is empty; signature count is one.
Signatures and secret material are redacted. The pinned Cashu parser accepts it as an
HTLCWitness. Macadamia and both fake mints each report 3/3 corresponding refund witnesses.
See [wire-witnesses.json](wire-witnesses.json); the independent Y derivation first passed
all three pinned CDK hash-to-curve vectors. No Ys, proof secrets or actual signatures are published.

## Final real sat accounting, sweep and preserved authorizations

Minibits returned at **18:21 UTC** (cdk-mintd/0.17.7). The exact old melt quote was inspected:
UNPAID, expired, zero inputs/outputs, no POST. Resuming its **original invoice and authorization**
`3c1baf7b-c5cd-4577-b499-7f7ca1a636ef` through the tool produced `unpaid_released`, zero debit.
The original journal row remains; it was not replaced or deleted.

Five subsequent withdrawals completed, each followed by `maxplayer wallet mint-complete`
on the **explicit Minibits receiving mint**, with exact home debit and receiver credit checks:

| Home / mint | Invoice credited | Wallet before → after | Home before → after | Mint fee | Lightning fee |
|---|---:|---:|---:|---:|---:|
| pair2-taker / Minibits | 29 | 8868 → 8897 | 32 → 3 | 0 | 0 |
| pair1-maker / Macadamia | 2 | 8897 → 8899 | 6 → 2 | 1 | 1 |
| pair1-taker / Macadamia | 110 | 8899 → 9009 | 114 → 2 | 1 | 1 |
| pair2-maker / cashu.cz | 18 | 9009 → 9027 | 24 → 3 | 0 | 3 |
| pair2-taker / cashu.cz | 98 | 9027 → 9125 | 104 → 3 | 0 | 3 |
| **Total** | **257** | | | **2** | **8** |

**Preserved pre-send refusal, not hidden:** the first cashu.cz sweep asked for 21 from 24,
but that mint quoted a **5-sat reserve**, unlike the 2 observed on Minibits/Macadamia.
The selector refused before recording inputs or posting payment. Authorization
`5a9928b3-c20a-45bf-9e81-70a96a6305aa` stayed `quote_created`; the entire sweep queue paused.
After natural expiry at 19:26:21 UTC, a fresh read still said UNPAID, and the exact original
invoice resumed to `unpaid_released` with zero inputs/debit. Only then were the 18/98-sat
payment plans authorized. No journal edits, reserve relaxation, extra funding or replacement
of an unresolved authorization occurred. There is no withdrawal-cancel command: this wait
is a current operational limitation. Quote/invoice material is retained privately.

Final snapshot **19:27 UTC**:

| Home / wallet | Minibits | Macadamia | cashu.cz |
|---|---:|---:|---:|
| Designated maxplayer wallet | **9125** | — | — |
| pair1-maker | 3 | 2 | 0 |
| pair1-taker | 3 | 2 | 0 |
| pair2-maker | 3 | 0 | 3 |
| pair2-taker | 3 | 0 | 3 |
| **Homes total** | **12** | **4** | **6** |

**9169 = 9125 wallet + 22 retained + 22 confirmed fees.** Fees comprise 10 historical,
2 for the round-four real refund, and 10 for these sweeps. The source-wallet decrease 44
is **not 44 lost**: 22 remains owned in preserved homes. No unexplained sats.

The remaining 22 is **tool-constrained dust under the observed reserves and positive-change
rule**, not burned value: Minibits 3/home, Macadamia 2/home, cashu.cz 3/home. No remaining home
can fund even a 1-sat invoice with the observed mint input fee, reserve and guaranteed positive
change. Future fee changes or separately authorized consolidation are outside this run.
All four homes and both naturally expired authorizations remain intact.

All real-home swaps/funding/withdrawals are terminal. A final read-only NUT-07 audit matched
**every retained home proof and all 9125 designated-wallet sats as UNSPENT**, with zero locally
held proofs. Proof identifiers, witnesses and secret material were not emitted. See
[final snapshot](final-checks.json), [sweep receipts](sweeps-redacted.json),
[terminal journal states](journal-final.json), and [remote proof audit](proof-accounting.json).
Accounting is limited to this designated wallet and four run homes; unrelated wallet mints
were not used and are excluded from public evidence.

## Per-mint verdicts and review focus

- **Devkit / space:** default-binary trade, sell-back, signed stored status readback and
  both production-timing refunds verified for this run; fake money only.
- **Macadamia:** real maker refund, fresh empty-preimage witness and exact fee accounting
  verified here; two real withdrawals also completed with exact change. Prior trade/sell-back/claim evidence remains historical.
- **cashu.cz / Nutshell:** real taker fresh-evidence refund and actual refund-spend witness
  now verified; two real withdrawals and exact change also verified. Prior claim evidence exists.
- **Minibits:** prior funding/trading/claims worked; the endpoint returned, the retained authorization
  reconciled, a new withdrawal completed, and remaining spendable proofs were remotely audited.
  A live Minibits HTLC refund was not exercised.

Final reviewer should scrutinise receipt-gated admission versus already-committed mint
state; retry exhaustion/block persistence; exact withdrawal outputs/change and reserved
credit ordering; quarantine/`settle_unforwardable` integration; and fresh refund evidence
including missing/ambiguous witness behavior. Honest/available issuer assumptions, late
server processing beyond client grace, every crash boundary, pre-fix journal migration,
key rotation, adversarial fee changes and 24-hour public-relay retention remain unproven.
The tests are not a model checker, and no production readiness claim is made.
