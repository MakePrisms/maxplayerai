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
Git/S3 delivery paths were deliberately not exercised. The coordinator runbook requires a
reconciled existing buyer budget for paid assignments; public app totals are estimates.
