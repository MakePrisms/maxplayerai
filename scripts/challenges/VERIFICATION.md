# Advisor r1 fix verification — 2026-10-16

## Fixes for advisor review r1

Author-led fixes to advisor review of `4489e27978fd2ccadaefedb066c87e1d12544afd`
(base `666c87936dbcbd5699e2d6db9a0e72bfa66163f8`). All seven findings confirmed and fixed;
none rejected. No new advisor job or independent re-review was requested.

| Finding | Fix and regression evidence |
| --- | --- |
| 1 — seller-chosen since | Latest membership `created_at` and `since` must both be at/before start in JS and Rust. Late changes wait until the next challenge. `membership replacement preserves since independently of event timestamp and rejects challenge-scoped records`, `membership_persists_across_days_late_join_waits_and_late_leave_wins`, and `late membership with old since and prior acceptance cannot post`; JS and Rust guard-removal mutants failed, restored tests passed. |
| 2 — seller RESULT minutes | Seller RESULT never stops the meter: accrue until buyer ACCEPT or now, capped at deadline; RESULT at/before award charges the full deadline interval. `backdated result consumes offered minutes and swarm posts no further job` and `seller result just after award cannot close the meter before buyer ACCEPT`; old timestamp calculation mutant failed. |
| 3 — partial relay history/ACK | Every configured challenge relay must finish EOSE; every sidecar publish requires all ACKs, with failed URLs reported. Page retains partial data with incomplete status; coordinator/swarm refuse posting. `relay A hiding newer left while down blocks swarm despite relay B old joined` mutant actually posted once; fixed version posts zero. Rust `missing_eose_is_error_and_partial_events_are_discarded` and `union_dedup_latest_and_publish_all_with_partial_rejection` both failed under any-relay mutation and passed restored. |
| 4 — unarmed queued reservation | Confirmed expired, never-awarded, reservation-none jobs may release when auto-award is null/missing, retaining all existing read/award/result/claim guards. `confirmed expired never-awarded job releases reservation, incomplete/possibly payable states do not`: expanded null/missing expiry table failed before fix (5 sats remained), passed after. |
| 5 — AUTH and flood limits | Reissue auth-required CLOSED history after AUTH OK, without clearing hard refusals. Cap discovery at 256 keys, 4096 content characters and 16384 serialized tag characters before expansion; overflow marks incomplete. `auth-required CLOSED before AUTH OK reissues history and clears error`, `membership flood and oversized notes are bounded before subscriptions`, `signed membership flood cannot expand one trade subscription per key`; each relevant mutant failed. `AUTH recovery cannot clear a different hard history refusal` failed before hard-error preservation and passed after. |
| 6 — shared host fence | Canonical JSON-literal hostname denylist in shared relay module, consumed by Rust sidecar, page, swarm parser and build. `challenge relay hostname denylist covers case trailing dot and encoded dot` and `build refuses the production challenge relay hostname`; removed fence failed both. Documented hostname-only, not DNS/IP equivalence. |
| 7 — secret publication | Scan draft content and every tag value before either CLI publish path. `secret_in_content_or_tags_never_writes_relay` observed a relay write before fix, zero writes after for both content and tags. |
| Extra — unrelated buyer jobs | Confirmed ordinary jobs were counted. Runner adds a buyer-signed challenge task prefix; projection counts only that buyer/challenge ID and hides the prefix in display text. No MCP/trade/money-path change. `ordinary buyer jobs do not consume challenge tally or minutes` fails without binding, passes with it. Malformed task tags also have a fail-before/pass-after regression. |

Manual join copy now warns to disable open-pool/open-targeted, matching the join skill.
All changed paths were already part of this PR. No Buzz, trade, mint, buyer money-path,
Vercel configuration, main push, merge, Discord message, or paid job changes/actions.

### Verification

- `web/app npm test`: **383 passed, 0 failed**, including typecheck.
- `scripts/challenges/*.test.mjs`: **52 passed, 0 failed**.
- `maxplayer-challenges --locked`: **11 passed, 0 failed**.
- `maxplayer-core --lib --features acp,gateway,git-delivery,wallet challenge::`: **7 passed, 0 failed** after Rust mutation restoration.
- Loopback challenge E2E: **66 checks passed, 0 failed** (free jobs, real local Buzz plus separate challenge relay). Final-source repeat is being verified before completion report.
- Gateway-only core attempt failed to compile due to existing `private_content/tests.rs:1672` use of feature-gated `repositories`; documented complete feature combination above passed. This failed compile is not counted as a test pass.
- Local mutation/revert evidence covers each finding; deliberate failing mutant runs are excluded from the passing suite counts.

Unverified: paid settlement, real public relay operators, real LLM planner, independent
advisor re-review. CI is monitored separately on the pushed head; no deployment requested.

---

# Historical challenge integration verification — 2026-10-15 (local only)

This record predates the auto-award fix. For the current no-manual-award harness,
root cause and reproduction, see [INTEGRATION.md](INTEGRATION.md).

Run from this checkout:

```sh
bash scripts/challenges/run-e2e.sh
```

Integrated core `ceb29d537420e0a4da567ccd514904d9c187ca43` and app
`1e0cf89aae11be1092323dafbcfc5c16ba14e2e6`. Full real-Buzz E2E passed on implementation
`15b6736f`; the subsequent app merge `50ca3321` adds only the frontend demo module, page
wiring and its test. Frontend tests/build were rerun after that merge; coordinator,
community service, harness, CLI and relay runtime sources were unchanged.

## Final gates

| Gate | Result |
| --- | --- |
| Coordinator policy | 4 passed, 0 failed |
| Community service + public-v2 regression | 26 passed, 0 failed |
| Frontend (including typecheck) | 331 passed, 0 failed |
| Frontend build | PASS |
| `maxplayer-core` wallet+ACP `challenge` filter | 10 passed, 0 failed |
| `maxplayer` wallet+ACP `challenge_cli` binary tests | 7 passed, 0 failed |
| Real local Buzz E2E | 33 assertions passed, 0 failed, 0 history retries |
| CI | No run: configured only for main pushes/PRs; no PR requested or opened |

Exact Rust gates (same private `CARGO_TARGET_DIR=target/challenges-e2e`):

```sh
cargo test --locked -p maxplayer-core --features wallet,acp challenge --lib
cargo test --locked -p maxplayer --no-default-features --features wallet,acp challenge_cli --bin maxplayer
```

## Full final E2E output summary

```text
PASS integration binaries exist
PASS Postgres ready
PASS local Buzz ready
PASS three distinct disposable identities
PASS challenge visible in app
PASS SSE initial snapshot received
PASS both applications visible (60 and 30 minutes)
PASS both initially applied
PASS accepted/denied tally and authenticated history
PASS Rust CLI and app agree on eligibility
PASS coordinator blocks before work window
PASS challenge work window open
PASS coordinator permits only accepted idle seller
PASS coordinator task posted through existing buyer MCP
PASS accepted seller live claim observed
PASS existing manual award override committed
PASS claim and award yield running tally
PASS running seller not assignable
PASS SSE running job received
PASS seller delivered result
PASS only accepted seller claimed and delivered
PASS review fixture answer before collect
PASS collect has no payment and zero sats
PASS collect materialized result
PASS completed tally has positive minutes used
PASS minutes exactly match award/result timestamps
PASS denied seller got no job/minutes
PASS completed seller can receive next task
PASS withdraw visible
PASS withdraw stops further assignment
PASS SSE withdrawal received
PASS withdraw preserves settled job/minutes
PASS Rust CLI confirms withdrawn ineligible seller
PASS ALL community challenge E2E
```

The manual award RPC confirmed an already committed automatic award (`published_now:false`),
so the output's “manual award override committed” means the existing award was confirmed;
it does not establish that this run needed the override to publish a new award.

Final app snapshot: one accepted/finished free job, `running:false`, `accepted:true`,
`amountSats:0`, **0.05 minutes used**. Accepted seller subsequently withdrew with **59.95
minutes remaining**; denied seller used **0** and retained **30**. Both ended ineligible.
Budget total/committed/paid all **0**, unknown amounts **0**. Collect returned
`pay.state=none`, amount **0**, attempt ID **null**. No real relay writes, paid jobs, deployment,
PR, or merge into `feat/community-challenges` occurred.

## Earlier failed attempts (not erased by the final pass)

- Fixture startup: default Buzz S3 conformance probe required MinIO. Inline-only fixture
  now disables that unrelated probe; Git/S3 delivery is explicitly unverified.
- First existing-kind baseline: **10 assertions passed** using auto-award.
- Second baseline: **5 assertions passed**, then a valid claim stalled without auto-award;
  delivery timed out. Subsequent explicit-override and gated baselines each passed **12**.
- First full run: **15 assertions passed**, then running-tally timeout: app ignored actual
  public-v2 trades. Narrow trade-admission fix + two signed regression tests added.
- Merged frontend: **329 passed / 1 failed** on uppercase seller skill path. Canonical lowercase
  artifact/index/test references and missing companion link fixed; final suite is green.
- Second full run: **14 assertions passed**, then buyer `get_job` returned
  `lifecycle wire history incomplete`. Harness now bounds identical retries of only this
  error; final full run needed **zero** retries.

Exact events, logs, causes and the list of small cross-agent fixes: [INTEGRATION.md](INTEGRATION.md).

## Remaining at that historical integration head

The buyer/relay incomplete-history and stalled auto-award behavior remains an implementation
follow-up, not fixed here; the harness uses the existing idempotent award override and bounded
history retries. No required coordinator/E2E work remains. Paid/mint, external reviewer and
Git/S3 delivery paths were deliberately not exercised. The buyer runbook requires a
reconciled existing buyer budget for paid assignments; public app totals are estimates.
