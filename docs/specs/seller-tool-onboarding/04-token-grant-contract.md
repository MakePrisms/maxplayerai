# 04 — Token, grant and custody contract

> ## ⚠ Part I is SUPERSEDED. Parts II–VII are superseded in part — see the list below before relying on any of them.
>
> Governing document: `v2/maxie/runs/seller-tool-scope-correction-20260909.md`
> (sha256 `683da09559bfc12631062c84ecdf4080778c4b31a3e7c16b3a49a7aa89dc64c3`), which overrides
> plan v3 and this contract wherever they conflict.
>
> Petar, 2026-09-09: *"the seller is defined by it's offering, there is no offering per job, it
> is per seller, tool should at all times be active together with the seller daemon"*.
>
> **Withdrawn:** the per-job grant model in Part I below — award-eligibility adapters, grant
> issuance at the owned-award boundary, per-job grant expiry, award replay gates, and every
> proposed marketplace job-state change. The award boundary is not the tool-lifecycle boundary,
> and the careful reasoning in Part I about *which* award edge to hook is answering a question
> that should not have been asked.
>
> **What replaced it:** the tool enrols once when the seller daemon starts and stays enrolled
> until that daemon stops. A job is an addressing and isolation concern only — `attach_job`
> creates a per-job endpoint and directory, `detach_job` removes them and explicitly reports
> `tool_still_enrolled: true`. Neither issues, meters nor expires anything.
>
> **Still live in this document — exactly these, and nothing more:** the trust boundary
> immediately below; enrollment and credential custody (Part IV); job-directory confinement;
> session persistence across restart; and re-enrolment after vendor-side revocation.
>
> **Also withdrawn, beyond Part I.** An earlier version of this banner said "Parts II onward
> stand", which was wrong: those parts carry grant and marketplace machinery that contradicts
> per-seller enrolment. Specifically withdrawn, and marked in place below — holder admission
> rows and marketplace reconciliation as authority (Part II); holder-enforced grant expiry, and
> "a missing deadline is expired" (Part II); per-call token verification against an `admitted`
> row (Part III); per-job durable budgets and reservations (Part VI); and the rule that only a
> **newly authorized** job may run after re-enrolment (Part VII), which contradicts the
> governing same-job recovery requirement.
>
> Per the fix order, these are **not to be implemented**. They are withdrawn requirements, not
> a backlog.
>
> **Implementation status, stated precisely.** `crates/maxplayer-tool-kit` implements the
> per-seller enrolment lifecycle, and its persistence and re-enrolment behaviour is exercised
> by tests. The advisor findings against the executable prototype are now addressed:
> - **F2 (file confinement) is fixed.** The holder no longer re-opens a checked path string. It
>   resolves each job file itself, following no symlink on any component, copies an input into a
>   private staging directory, and publishes an output with a no-follow create. See
>   `src/safeio.rs` and `src/bin/tool_holderd.rs`. Deterministic and live replacement controls in
>   `tests/negative_controls.rs` assert that a symlink planted after validation is refused and the
>   outside file is neither read nor written.
> - **F1 (credential absence) and F3 (stop/restore lifecycle)** are repaired in
>   `docker/demo.sh`: the credential scan runs host-side with a functioning negative control, and
>   the lifecycle proof re-attaches a job after each restart and proves call → loss → restore on a
>   live endpoint. The pre-repair evidence bundles remain superseded and must not be cited.

Paper artifact. **PROPOSED** throughout. Anchored in plan v3 §4, and revised against the
stage-0 verdict findings F1, F2 and F6.

Citation convention as in [01](01-integration-survey.md): unqualified `store.rs` and `run.rs`
are under `crates/maxplayer-core/src/seller_node/`.

## Trust boundary

Trusted: the holder supervisor, and the genuine pinned CLI running inside the holder.
Untrusted: buyer prompts, buyer-supplied inputs, and the job container — always.

Plan v3 §4 chooses **trusted credential-reading children** over supervisor-only custody. That
choice imports an obligation:

> A container does not prove a CLI will not disclose its credential. The profile's reviewed
> input semantics, egress policy, output handling and isolation are all load-bearing, and a
> profile that cannot establish all four is unsupported in the initial release.

## Part I — Grant open (F1)

### The boundary is owned award, not a store write

[01](01-integration-survey.md) §2.1 establishes from source that an award row means only "an
award for this job exists": `Awarded::NoClaim` records someone else's win, `Awarded::Duplicate`
returns before the claim is even read, and suppression (`run.rs:6291-6294`) and
ACCEPT-without-execution (`run.rs:6172-6182`) reach the same method.

**The authorization adapter is therefore called at the authenticated owned-award / eligible
execution boundary** — inside the `Awarded::New` arm (`run.rs:6428-6459`), after `match_award`
has returned `AwardMatch::Execute` — and never from the store.

### Five positive preconditions

A grant opens only when **all five** hold. Any one unproven is a refusal, not a narrower grant.

1. **Owned-win proof.** A local accepted-claim row exists whose claim id equals the award's
   claim id. Presence of an award row is not proof (`store.rs:1584-1589`).
2. **Authorized awarder.** The award author equals the buyer recorded on *our own* offer
   (`run.rs:6382-6386`).
3. **Eligible durable job state.** The job occupies an execution slot and is not terminal, not
   lapsed, not delivered, not settled elsewhere.
4. **Seller-approved binding.** The service id, party, resource set and ceilings come from the
   seller's holder policy, keyed by a durable binding (below) — **never derived from buyer
   prose, offer text or job content.**
5. **Readable authority.** Every fact above was read successfully. An unreadable or errored
   read is a refusal.

### No grant on

`NoClaim` · `Duplicate` · any store or read error · the suppression path · ACCEPT-only binding ·
any award that failed `match_award` · resume of a lapsed or terminal row.

### Arm-by-arm treatment

| Arm / path | Grant behaviour |
| --- | --- |
| `New` + `AwardMatch::Execute` | Open a fresh grant. The only minting path. |
| `Duplicate` | **Never mint.** May only re-present the *same* committed authorization, after revalidating preconditions 1–5. Must not reset counters, extend deadlines, widen scope, or reopen a tombstone. |
| `NoClaim` | Refuse. Nothing to authorize. |
| ACCEPT-only | Refuse. The path explicitly does not execute. |
| suppression | Refuse. Someone else's win. |
| resume after restart | Treated as recovery, not as a new open — see Part II. |

### Idempotent open

Opening is a two-phase commit against the holder's own durable record, keyed by
`(job_id, award_id, grant_version)`:

1. **reserve** the authorization row as `opening`;
2. **commit** it as `admitted` once the token is minted.

A crash between award and grant persistence leaves an `opening` row. Recovery **revalidates
preconditions 1–5 and then either commits the same authorization idempotently or abandons it**.
It never mints a second, differently scoped authorization for the same award, and a replayed
award for an already-tombstoned job is refused.

### The durable binding

Because the `jobs` table has no service or grant column ([01](01-integration-survey.md) §2.2),
the holder owns its own record. Per maxie's ruling: **use a durable holder admission/close
record; do not add gratuitous marketplace state.**

```
holder_admission
  job_id            the marketplace job id (bare String; treated as untrusted until matched)
  award_id          the award event id that authorized this admission
  holder_id         which holder
  party             seller-approved, from holder policy
  service_id        seller-approved, from holder policy
  grant_version     monotonic; frozen at admission
  resources         the enumerated granted set
  ceilings          calls / items / bytes
  expiry            absolute
  state             opening | admitted | closing | closed     (monotonic, never regresses)
  close_reason      success | failure | cancel | timeout | expiry | reconciled-unknown
  reservations      durable counters
```

The seller-controlled source of `party`, `service_id`, `resources` and `ceilings` is the
manifest's `grant_policy` ([02](02-manifest-schema.md)), reviewed by a human. The mapping from
a marketplace job to a holder/service is seller configuration, not inference.

## Part II — Close, restart and reconciliation (F2)

### Why the holder cannot trust the marketplace record

From source ([01](01-integration-survey.md) §2.3): timeout **does** reach `Failed`, but
`fail_job` is best-effort — "a fail-mark that itself errors is logged, never propagated — the
loop keeps serving" — and it logs `Ok(0)` "no job row moved" and `Err` "write error
(continuing)". Restart re-drives non-terminal rows (`run.rs:4597-4634`), graceful shutdown
leaves rows for replay (`run.rs:4721-4727`), and resume treats a **missing deadline as live**
(`run.rs:~1548-1578`).

Those are correct marketplace choices — never lose a genuine award. They are the **opposite**
of what credential authority needs. Per maxie's ruling: **fail closed independently rather than
trusting the record.**

Three consequences, binding:

1. The holder's own admission record is authoritative for authority decisions; the marketplace
   record is corroborating evidence.
2. The holder enforces its **own** absolute deadline. A missing or unreadable deadline is
   **expired**, not live.
3. A marketplace resume never reopens a grant. Only a fresh authorized open does.

> **WITHDRAWN (all three).** There is no grant to expire and no admission row to be
> authoritative over. The tool is enrolled while the seller daemon runs. Retained as a record of
> the withdrawn model only — do not implement.

### Commit point and ordering

Close has one commit point: the monotonic transition of `state` to `closing` with a
`close_reason`, in the holder's durable store.

Ordering is fixed, because "deny new calls" and "clean up" cannot share one transaction —
process termination and filesystem removal are not transactional with SQLite:

1. **commit** `closing` + `close_reason` durably;
2. from that instant **deny all new calls and renewals** for this job;
3. **release or forfeit** outstanding reservations (below);
4. revoke mediated tokens; stop supervisor-owned processes; remove job data;
5. **commit** `closed`.

Steps 3–5 are **idempotent and repeatable**. A crash anywhere re-runs them from the durable
`closing` row. Because step 1 precedes every effect, a crash after step 1 still denies calls.

### Adapter call sites

| Event | Core site | Reason |
| --- | --- | --- |
| success | delivery/enqueue path | `success` |
| failure and timeout | every path through `fail_job` (`run.rs:8377-8393`) | `failure` / `timeout` |
| cancellation, shutdown | graceful shutdown (`run.rs:4721-4727`) | `cancel` |
| boot reconciliation | restart sweep (`run.rs:4597-4634`) | see below |
| holder-local expiry | holder's own timer, independent of core | `expiry` |

The last row is essential: **the holder supervises its own deadlines**, so a marketplace process
that disappears entirely still results in closure. Holder-side expiry does not depend on any
core call arriving.

### Boot reconciliation

On start, every `opening`, `admitted` or `closing` row is reconciled **before any call is
served**:

- `closing` → re-run idempotent steps 3–5.
- `admitted` past its holder-enforced expiry → close, reason `expiry`.
- `admitted` within expiry → serve **only** if preconditions 1–5 re-verify against a currently
  eligible job; otherwise close with `reconciled-unknown`.
- `opening` → the idempotent-open rule in Part I.

**Fail closed until reconciliation completes.** An unreadable or lost holder record is
`reconciled-unknown`: deny, do not reconstruct authority from the marketplace record.

### Reservations and uncertain vendor effects

Outstanding reservations at close are **forfeited, not refunded**, unless the holder holds
positive proof the effect did not occur. "The process died before we saw a response" is not
proof of non-occurrence.

**No write auto-replays, ever** — not on restart, not on reconciliation, not on resume. Where a
call's outcome is unknown, the holder records an `uncertain-effect` marker against the closed
job and surfaces it to the seller. Marketplace resume may legitimately re-run an agent
(`run.rs:~1548-1578`); that must never re-drive a vendor write through a reopened grant, which
is exactly why resume cannot reopen a grant.

### Residual, recorded rather than solved

**Vendor operations already accepted can outlive local cancellation.** Closing a job stops our
calls; it does not undo a send, a charge or a publish the vendor accepted. Counters bound
*admission*, not consequence. Disclosed to the seller; never described as mitigated.

## Part III — Token verification

> **WITHDRAWN in full.** No per-job token exists, so there is nothing to verify per call. What
> survives of this section's intent is enforced differently and is implemented: a call is
> validated against the seller's declared operation grammar, and confined to the job directory
> belonging to the endpoint the call arrived on — job identity comes from the listener, never
> from the request body. Do not implement the checks below.

The token binds **holder, party, service, job ID, grant version, expiry**.

Every call verifies, before any child process exists:

1. signature; 2. audience; 3. time against the holder's clock; 4. an `admitted` **holder
admission row** (not merely a marketplace job row); 5. party and service equality; 6. verb and
resource membership; 7. remaining budget.

**Claims never override the record.** Because the seller-path job id is a bare `String`
([01](01-integration-survey.md) §2.2), this record re-check is the only barrier between job
`K`'s token and job `J`'s resources.

## Part IV — Custody

### Enrollment

The seller enrolls **interactively, inside the persistent holder**, via a protected local
terminal or vendor browser flow. Secrets never enter chat, arguments, manifests, logs or
notices. A host login is not assumed portable (plan v3 §7).

The existing per-job credential proxy ([01](01-integration-survey.md) §4.1) is prior art for
mediation and for fail-closed containment — "there is no fallback to putting the real
credential in the container" (`seller_exec.rs:2575-2620`) — and the kit should adopt both that
posture and the no-`Debug` secret type from `codex_subscription.rs:14-19`. It is **not** an
enrolled persistent tool holder, and must not be described as one.

### Per-job environment

| Element | Binding |
| --- | --- |
| `HOME` | private per-job directory, controlled configuration base |
| cache | private, per-job, discarded at close |
| credential store | only the profile-selected store, to the trusted child |
| input / output | holder-private staging and slot ([03](03-command-policy-mapping.md)) |
| egress | default deny; reviewed allowlist, ideally one pinned upstream |

Never available: the credential mount from any buyer container, unrelated host files, other
jobs' directories, the Docker socket, host process namespaces.

The child environment **starts empty** except reviewed tool/runtime variables. This is
deliberately *not* the existing `FORWARDED_AGENT_ENV` + `forward_env` behaviour
(`seller_exec.rs:301,908`), which forwards a built-in credential-bearing allowlist plus
operator additions. Reuse the mechanism; do not inherit the defaults.

### Credential maintenance

Refresh/profile writes use a **serialized credential-maintenance operation outside job
control**. Only its designated auth-store writes persist; job cache/config never merges back.
A tool that cannot separate those writes is **unsupported in the initial release**. Browser
profile mutation and isolation are deferred; a lock file does not solve them.

## Part V — Holder sharing (F6)

Separate holders for different parties and vendors, without exception.

**`party_scope` selects between exactly two shapes, and neither permits cross-party sharing:**

- `per-party` — one holder instance per party. The default.
- `shared-holder` — **one holder serving concurrent jobs of the same party and the same
  vendor.** It exists only so several simultaneous jobs from one party can reuse one enrolled
  login.

**No cross-party authority may be inferred from `shared-holder`.** A manifest listing parties
`P` and `Q` declares which openers may open jobs; it does **not** authorize one holder to serve
both. Distinct parties are represented as distinct holders.

Seller hosting is the initial scope; platform-hosted credential custody is deferred.

## Part VI — Budgets

> **WITHDRAWN.** Per-job durable budgets, reservations and refunds all presuppose a per-job
> grant. The one limit that survives is a per-call output ceiling, which is enforced and tested.
> Do not implement durable per-job counters.

Reserve calls, items and bytes **atomically before execution**, from profile-declared **maxima**.
Refuse unbounded operations. Counters survive restart, do not reset on renewal, and reset only
for a separately authorized new job. Refund only reservations **proved** unused.

Concurrency: two calls whose combined declared maxima exceed a ceiling must not both admit —
reservation precedes execution rather than accounting following it.

## Part VII — Lifecycle failure

On vendor auth expiry mid-operation the holder returns credential-expired, marks itself
unhealthy, queues **one** secret-free seller notice, pauses registration and refuses new work.
Recovery is protected re-enrollment plus a successful health check, after which a **newly
authorized** job may run. The failed job stays closed. **No write auto-replays.**

> **PARTLY WITHDRAWN.** Live and implemented: on vendor auth failure the holder marks itself
> unhealthy, fails closed, and recovers through re-enrolment plus a health check. Withdrawn: the
> restriction to a **newly authorized** job afterwards — it contradicts the governing same-job
> recovery requirement, since the same job continues against the same seller-level enrolment.
> "No write auto-replays" stands.
