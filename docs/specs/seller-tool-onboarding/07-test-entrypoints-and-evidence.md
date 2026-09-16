# 07 — Intended test entrypoints and evidence layout

Paper artifact. **These are planned tests, not existing commands, and not measured security
guarantees.** Nothing here has been run. Anchored in plan v3 §5 and §6.

## Fixture defaults (plan v3 §5)

No internet. Fake vendor only. Synthetic credentials only. Parties `P`, `Q`; jobs `J`, `K`;
holders `H`, `G`; resource `R` allowed, `S` forbidden. Maximum 20 simulated invocations and
64 KiB generated data per case, reset between cases. No real spend, no external writes.

Tests apply to the stage-2 MCP-container CLI template unless noted. Deferred transport tests
are **SKIPPED with their deferred reason** — never quietly omitted, never counted as passes.

## Result vocabulary

| Result | Meaning |
| --- | --- |
| PASS | oracle satisfied, with evidence |
| FAIL | oracle not satisfied |
| SKIPPED | a dependency failed, or the case is deferred; **never a fake PASS** |

A manifest fault and a defective runtime are different fixtures and must not share one case.
Dependency failure propagates as SKIPPED to dependents, so a broken fixture reads as a broken
fixture rather than a clean sweep.

## Proposed entrypoints

**PROPOSED** — no such crate, binary or test target exists today (gaps G-1, G-6).

Location follows the kit crate proposed in [01](01-integration-survey.md):

| Entrypoint | Form | Purpose |
| --- | --- | --- |
| `cargo test -p maxplayer-tool-kit --test fixture_suite` | Rust integration test | checks 1–12 against the fake vendor |
| `cargo test -p maxplayer-tool-kit --test negative_controls` | Rust integration test | runtime mutants; each has a **required FAIL** |
| `maxplayer-tool-kit check --manifest <path> --fixtures` | CLI subcommand | seller-facing fixture checker |
| `maxplayer-tool-kit check --manifest <path> --live` | CLI subcommand | live checker, bounded per plan v3 §5 |
| `crates/maxplayer-tool-kit/fixtures/fake-vendor/` | fixture service | records every call; owns the authoritative call counter |
| `crates/maxplayer-tool-kit/profiles/` | reviewed profiles | content-addressed; drift resets acceptance |

### Three independent observers (F4)

An earlier version of this contract asserted "zero child calls" from the fake vendor's request
counter alone. **That oracle was broken**, and the break was success-shaped: a malformed
manifest could launch a child that reads a local file, writes output, errors or exits *before
any network activity*, leaving the vendor counter at zero and the check passing. The child ran;
the oracle said it hadn't.

The harness therefore owns **three separate observers**, and no one of them may stand in for
another:

| Observer | Owned by | Answers |
| --- | --- | --- |
| **child-start counter** | the harness's process-launch instrumentation, wrapping the exec boundary itself | did any child process **start**? |
| **vendor request counter** | the fake vendor | did any **remote effect** occur? |
| **forbidden-effect markers** | the fixture filesystem//egress probes | did a forbidden local read, write or egress occur? |

**Every reject-before-invocation check requires all three: a validation error, AND zero child
starts, AND zero vendor effects.** A check that can only report the vendor counter is not
implemented.

Two further properties:

- **The clock is a fixture clock.** Expiry tests set time explicitly; none sleep.
- **The holder never grades itself.** No oracle reads the holder's own report of what it did.

## Check map

Each row: the plan v3 §5 check, its oracle, and its declared budget.

| # | Check | Oracle | Budget |
| --- | --- | --- | --- |
| 1 | Schema/profile: valid manifest loads; embedded hole, unknown profile, unsafe constant, text-to-URL mapping each reject | validation error **and zero child starts and** zero vendor effects | 0 starts, 0 calls for negatives |
| 2 | Discovery/result: list equals approved verbs; `render(R)` returns known bytes | independent expected file + digest; **not** exit code, **not** server self-report | 1 call |
| 3 | Operand grammar: `-x`, `@file`, `x;touch y` reject; `hello-world` arrives as exactly one literal operand | **zero child starts** and zero vendor calls for negatives; expected bytes and no forbidden-effect marker for the positive | 1 positive call |
| 4 | Artifact consumption: forged handles; driver swaps symlinks/renames **during** staging and **after** validation | no outside-file-read marker; consumed bytes match staged digest; valid staged object may succeed only with its original digest | ≤2 calls, 8 KiB |
| 5 | Authentication: missing/garbage signature, wrong holder/party/service/job, natural expiry, post-expiry, each close outcome | reject with zero calls, against fixture clock and authoritative records; valid `J/H/P` succeeds once; includes same token after restart and after renewal against a closed record | 1 positive call |
| 6 | Grant authority: unauthorized opener, excessive grant request, forged claim grant version, verb/resource outside `J`, **`K`'s valid token against `J`**, and **`K`'s genuine handle/slot presented with `J`'s genuine token** (F5) | zero unauthorized calls **and zero child starts**; seller-authorized `J/R` succeeds once | 1 positive call |
| 7 | Leakage: synthetic auth secret may appear **only** in the declared fake-vendor auth channel; a separate non-auth canary appears nowhere outside its forbidden store | every captured channel inspected, success **and** failure paths; failing to authenticate at the permitted sink also fails the positive oracle | 2 calls |
| 8 | Isolation: concurrent `J`/`K` cannot read each other's HOME/input/output markers; sequential `K` cannot see `J`'s config/cache mutations | instrumented child observes mounts and identities; forbidden-read marker zero; credential lookup still succeeds at its permitted store | 4 calls, 8 KiB |
| 9 | Cleanup: fixture child **with a descendant**; close/cancel/timeout `J` | no owned process or job filesystem within 5 s; repeated calls refuse; unrelated `K` still usable; repeat with natural expiry during an active child | ≤4 calls |
| 10 | Budgets: call=3, item=3, byte=8 set independently; exact boundary, one over, concurrent reservations exceeding the limit | only admitted effects occur; renewal/restart cannot reset; new authorized `K` has separate counters | ≤10 requests, 16 B/subcase |
| 11 | Lifecycle: fake vendor expires auth mid-operation | credential-expired returned, holder unhealthy, exactly one seller notice, registration paused, new work refused; after simulated protected re-enrollment + health, a **newly authorized** job runs; failed job stays closed; **no write auto-replays** | 4 calls |
| 12 | Registration: fail each mandatory check; pinned version/profile drift | rejection/pause; drift invalidates prior acceptance | 1 registration/variant, 0 vendor effects |

Check 7's last clause is the one most often lost in implementation: a run where the secret
leaked nowhere *because authentication never happened* is a FAIL, not a PASS.

## Negative controls — runtime mutants

Schema-invalid manifests are not sufficient. Each mutant below is a deliberately defective
**runtime**, with a named required FAIL and named dependent SKIPPEDs.

| Mutant | Required FAIL | Dependent SKIPPED |
| --- | --- | --- |
| bypass signature validation | 5 | — |
| omit audience/party checks | 5, 6 | — |
| leak auth to output | 7 | — |
| expose another job's mount | 8 | — |
| leave descendants alive | 9 | — |
| skip quota reservation | 10 | — |
| retain closed tokens | 5, 6 | — |
| **launch a child, then reject without contacting the vendor** | **1, 3** — must fail the pre-invocation oracle on the child-start observer alone | — |
| open a grant on `NoClaim`, `Duplicate`, suppression or ACCEPT-only | 13 | — |
| trust the marketplace record instead of the holder record | 14 | — |
| reopen a grant on marketplace resume | 14 | — |

**Do not demand exactly one failing line.** A mutant may legitimately trip several checks; the
requirement is that the named check fails, not that nothing else does.

**Dependent skips must name a genuine dependency.** The previous version made cleanup (check 9)
a dependent skip of the retain-closed-tokens mutant. That was wrong: cleanup is independently
observable — processes, descendants and job filesystem either remain or do not — regardless of
whether that mutant also fails authentication. It now runs. A skip is justified only when the
dependency genuinely prevents observation, and the reason is recorded in `skipped.json`.

## Integration checks 13 and 14 (F1, F2)

These are new, and exist because the grant boundary meets real marketplace code whose semantics
([01](01-integration-survey.md) §2) do not match holder needs.

### 13 · Owned-award admission

> **SUPERSEDED — this check is withdrawn, not merely unimplemented.** It tests that a tool grant
> is refused unless the award is genuinely ours. With no per-job grant there is nothing to
> refuse, and the tool's availability does not depend on any award. Deleting the check loses
> nothing, because the property it protected no longer exists.
>
> The replacement checks are the ones that survived the model change: parameter validation,
> job-directory confinement, cross-job refusal, credential containment, and availability
> tracking the daemon. Those are implemented and run — 32 tests in `crates/maxplayer-tool-kit`
> plus 27 container checks in `docker/demo.sh`. See [00](00-README.md).

Each case asserts **zero child starts and no grant minted** unless stated:

| Case | Required outcome |
| --- | --- |
| award for a claim we do not hold (`NoClaim`) | no grant |
| award we lost — local claim id differs from the award's | no grant |
| award author is not the buyer on our own recorded offer | no grant |
| suppression path records another party's win | no grant |
| ACCEPT binds an award without executing | no grant |
| `Duplicate` for an already-admitted job | **same** authorization re-presented after revalidation; counters unchanged, deadline unextended, scope unwidened |
| `Duplicate` for a tombstoned job | refused; tombstone not reopened |
| crash between award and grant persistence, then restart | at most one authorization exists; recovery commits the same one idempotently or abandons it |
| replay of a closed job's award | refused |
| unreadable authority fact | refused (fail closed) |
| genuine `New` + `AwardMatch::Execute` | exactly one grant, correct scope — the positive control |

### 14 · Durable close and reconciliation

| Case | Required outcome |
| --- | --- |
| crash **before** the `closing` commit | boot reconciliation closes or denies; no call served first |
| crash **after** the `closing` commit, before cleanup | cleanup re-runs idempotently; new calls already denied |
| repeated cleanup invocation | idempotent; no error, no double refund |
| marketplace fail-write fails (`Ok(0)` or `Err`) | holder still closes on its own authority |
| holder record lost or unreadable | `reconciled-unknown`; deny; authority is **not** reconstructed from the marketplace record |
| restart with active descendants | descendants stopped; no owned process survives |
| marketplace process disappears entirely | holder-enforced expiry still closes the grant |
| missing or unreadable deadline | treated as **expired**, not live — opposite of the marketplace resume rule |
| marketplace resume of a non-terminal row | agent may re-run; **no grant reopens**, no vendor write replays |
| outstanding reservation at close | forfeited absent positive proof of non-occurrence; `uncertain-effect` recorded |
| tombstone + counters after close | preserved; renewal and restart cannot reset them |

Actual crash execution is a later-stage gate. What is owed **now** is this coherent protocol and
its proof obligation.

## Live checks

Only seller-approved test resources. **One health request and one read per run**, with an
independent expected result.

A write-producing acceptance example requires a **separate explicit disposable-resource/effect
allowance**. Stop at that allowance: lack of permission is a blocker, not permission to
improvise.

Remote black-box checks cannot prove internal isolation. Pinned artifacts and local isolation
evidence are recorded **separately**, and registration must not label an arbitrary mutable
endpoint secure on the strength of live checks alone.

## Evidence layout

One directory per acceptance run, content-addressed and append-only:

```
evidence/<run-id>/
  manifest.json          run id, UTC start/end, operator, git commit of the kit
  pins.json              kit commit, server digest, schema version,
                         each profile name+version+digest, fixture image digest,
                         fake-vendor digest, tool version(s)
  oracles/               expected results RECORDED BEFORE THE RUN
    <case>.expected      bytes or digest, plus provenance of the oracle
  cases/
    <case>/
      result.json        PASS | FAIL | SKIPPED, reason, dependency if skipped
      command.txt        exact argv, redacted only where a redaction rule names the field
      observations/      captured channels: stdout, stderr, vendor call log,
                         egress log, output slot listing, marker counters
      budget.json        reserved vs consumed calls/items/bytes
  interventions.json     every human intervention, or an explicit empty list
  skipped.json           every SKIPPED case with its reason
  summary.md             counts by result; NOT a pass/fail verdict on its own
```

Rules that make the bundle evidence rather than decoration:

- **Oracles are written before the run** and hashed into `pins.json`. An oracle produced after
  seeing the output is not an oracle.
- **Redaction is by rule, not by judgement.** A redaction rule names the field; ad-hoc removal
  of an inconvenient log line is a defect.
- **`interventions.json` is mandatory and may be empty**, because the stage-2 agent criterion
  is *zero* human interventions and an absent file cannot demonstrate zero.
- **`summary.md` never overrides `cases/`.** The per-case results are authoritative.

## Acceptance run wiring (plan v3 §6 stage 2)

- Two services on tool one, plus one on a contrasting tool two selected **after** freeze, with
  **no edits to the pinned kit**. Petar or Josip selects tool two; this is also the unseen-tool
  test.
- Advisor independently assesses eligibility against frozen capabilities **before** execution,
  and records required outputs, forbidden effects and the safe effect allowance.
- A haiku-class agent receives only the pinned kit, task inputs and an already-enrolled test
  holder; it may edit only the seller manifest and job input data. Within 40 turns and two
  hours, zero human interventions, it must pass applicable checks **and** produce the real
  tool's expected result. A human-produced artifact or independent vendor read is the oracle.
  **No fixture-only success.**
- The agent cannot decide its own unsupported escape; the advisor adjudicates against the
  predeclared policy. Correct `unsupported` classification is a routing success but **does not
  satisfy the required successful second-tool onboarding** — another eligible sample must be
  selected, or acceptance remains incomplete.
- Across all three services: verify out-of-grant and closed-token rejection; run all negative
  controls, concurrency, cleanup, budget and expiry/re-enrollment cases; independently verify
  real outputs; test registration rejection and subsequent pause.

## Relationship to the authorized stage-1 platform

The stage-1 platform — custom CLI plus local fake authenticated service on Linux Docker,
synthetic credentials — exercises the fixture entrypoints above and demonstrates login
persistence, useful artifact output and job isolation.

Its evidence bundle is written to the same layout, and is labelled in `manifest.json` with a
`mechanism_only: true` marker, because: **a CLI written to fit the contract cannot falsify the
contract.** A stage-1 bundle is not independent real-tool acceptance, is not general onboarding
acceptance, and contributes **zero** credit toward the stage-2 acceptance run described above,
which is retained in full.
