# Seller tool onboarding kit — stage 0 contract

> ## ⚠ Read this first: the lifecycle model here is superseded, and code now exists
>
> Governing document: `v2/maxie/runs/seller-tool-scope-correction-20260909.md`
> (sha256 `683da09559bfc12631062c84ecdf4080778c4b31a3e7c16b3a49a7aa89dc64c3`). It overrides
> plan v3 and every file in this directory wherever they conflict.
>
> Petar, 2026-09-09: *"the seller is defined by it's offering, there is no offering per job, it
> is per seller, tool should at all times be active together with the seller daemon"*.
>
> **Withdrawn across this directory:** per-job tool grants, award-eligibility adapters, award
> replay gates, and proposed marketplace job-state changes. Wherever a file below reasons about
> issuing or expiring a tool grant per job, that reasoning no longer applies; the affected
> sections carry their own notes, and [04](04-token-grant-contract.md) Part I is superseded
> outright.
>
> **The model that replaced it:** one enrolment per seller daemon, live for the daemon's
> lifetime. A job gets an endpoint and a directory, never a grant.
>
> **Status has also moved on.** The "paper contract only" line below was true when written and
> is no longer. `crates/maxplayer-tool-kit` implements this model, with 32 tests and a Linux
> container demonstration under `docker/demo.sh`. What survives unchanged is the *epistemic*
> caution: that kit is exercised against a fake vendor and a CLI written for it, so it
> establishes mechanism, not third-party acceptance and not a security guarantee.

Status: **paper contract only.** No runtime implementation, no shipped support, no security
guarantee is established by anything in this directory. Every "the holder does X" sentence
below is a *requirement placed on a future implementation*, never a description of code that
exists today.

Stage: 0 of the build order in plan v3 §6.
Ordering seat: maxie. Human authority: Petar, 2026-09-09 (thread 1546802028091670630).
Author: worker (`w-seller-tool-onboarding-r2`), 2026-09-09.

## Provenance

This contract is derived from, and subordinate to, these two documents. Hashes measured by
the author on 2026-09-09 and matching the values stated in the implementation order:

| Document | SHA256 |
| --- | --- |
| `v2/maxie/runs/seller-tool-onboarding-PLAN-v3-20260909.md` | `1c35ba32686af63dee874943a676461191dd8ea30149a6504c66fe47e1353e01` |
| `v2/advisor/verdicts/20260909-seller-tool-onboarding-plan-v3.md` | `44377fb083c4aca2a029ade51235c7aed12807c965571b0cd2c2653a8245374a` |
| `v2/maxie/runs/seller-tool-implementation-brief-20260909.md` | `d7053b791496d70a0cf5adcb982a488366076acfe0b71aff56c0e275d686a160` |

Where this contract and plan v3 disagree, **plan v3 wins** and the disagreement is a defect in
this contract to be reported, not a silent amendment.

## What stage 0 delivers

| # | Artifact | Plan v3 anchor |
| --- | --- | --- |
| 01 | [Integration survey and proposed kit location](01-integration-survey.md) | §6 stage 0 |
| 02 | [Manifest schema sketch](02-manifest-schema.md) | §3 |
| 03 | [Command-policy profile and source-to-sink mapping](03-command-policy-mapping.md) | §3 |
| 04 | [Token and grant contract](04-token-grant-contract.md) | §4 |
| 05 | [Paper walk A — file-processing CLI](05-walk-a-file-processing-cli.md) | §6 stage 0 |
| 06 | [Paper walk B — tenant-aware HTTP API (contrast)](06-walk-b-tenant-aware-http.md) | §6 stage 0 |
| 07 | [Test entrypoints and evidence layout](07-test-entrypoints-and-evidence.md) | §5, §6 |
| 08 | [Named gaps, unsupported and deferred cases](08-gaps-and-unsupported.md) | §6 stage 0 output |
| 09 | [Production integration plan](09-production-integration.md) | post-review, later stage |

## Scope fence

Stage 0 produces **paper artifacts only**. Specifically it does not:

- add, modify or delete any runtime code, test, build file or dependency;
- create a server, a schema validator, a checker or a template;
- claim that any configuration key, CLI flag, MCP surface or credential store named here is
  already supported by this repository. Where a name is proposed rather than observed, it is
  marked **PROPOSED** and the survey in 01 states what actually exists;
- select the first real tool, or assume live account access. Those remain Petar's under plan
  v3 §7.

## Platform for the stage-1 prototype — already decided, recorded here only

The prototype platform was authorized outside this contract and is **not re-decided here**: a
small custom CLI plus a local fake authenticated service, on Linux Docker, with synthetic
credentials only. It is to demonstrate login persistence, useful artifact output and job
isolation.

The honesty caveat that must travel with every result produced on that platform, and which
this contract restates as a binding condition:

> **The custom CLI proves the mechanism only.** A green run on a CLI written by the same
> people who wrote the kit is evidence that the holder/grant/isolation machinery can work
> against *some* cooperating tool. It is **not** independent real-tool acceptance, and it is
> **not** general onboarding acceptance. A tool built to fit the contract cannot falsify the
> contract.

Accordingly, **contrasting real-tool acceptance is retained in full**: plan v3 §6 stage 2
still requires two services on a real tool one plus one service on a contrasting tool two
selected after freeze, with a human-produced or independent-vendor oracle. Nothing on the
fake-service platform reduces, replaces or pre-satisfies that requirement, and no stage-1
result may be cited as partial credit against it.

## Reading order

01 first (what the repository actually offers), then 02–04 (the contract), then 05–06 (the
two walks that stress it), then 07 (how it would be proven) and 08 (where it fails today).
