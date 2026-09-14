# What is verified in maxplayer-muse-buyer, and what is not

Read this before you rely on a step. Every claim in the skill that could cost money
carries one of three tiers, and the tier is stated here.

**Pinned base: maxplayer 0.5.8** — the version in this repository's `Cargo.toml` at the
commit this page ships from. Check yours before trusting anything below:

```bash
maxplayer --version
```

## Tier 1 — source-checked in this repository at 0.5.8

Read from the code, not from anyone's report.

| Claim | Where |
|---|---|
| The MCP surface is exactly `post_job`, `get_job`, `collect`, `award_claim` | `crates/maxplayer/src/mcp.rs` |
| `post_job` requires `task`, `output`, `amount_sats` | same |
| `post_job` declares `payment` with enum `["sat", "none"]`, defaulting to `"sat"` when omitted | same |
| `max_sats` is a per-job ceiling for the background auto-award, defaulting to `amount_sats` | same, and `crates/maxplayer-core/src/buyer/mod.rs` |
| `payment` outside `{"sat","none"}` is refused; `payment="none"` with non-zero `amount_sats` is refused | `crates/maxplayer-core/src/buyer/mod.rs` |
| `get_job` requires `job_id`; `wait_for`/`timeout_secs` are optional and the timeout is capped | `crates/maxplayer/src/mcp.rs` |
| `collect` requires `job_id`. **On a paid job** (`payment: "sat"`) its order is accept → verify → **pay** → materialise, and it is idempotent by attempt id | `crates/maxplayer-core/src/collect.rs` |
| **On a free job** (`payment: "none"`) there is no payment leg and no attempt id: the free bind is verified and materialised, and the response reports `state: "none"`, `attempt_id: null`, `amount_sats: 0` and **no** `spent_total_sats` | `crates/maxplayer-core/src/collect.rs`, `crates/maxplayer-core/src/buyer/mod.rs` |
| A retry preserves the pinned award rather than re-transmitting it; an **expired** pending attempt is only probed, so the outcome can stay unresolved or come back refused — retrying is not a convergence guarantee | `crates/maxplayer-core/src/buyer/mod.rs` |
| `award_claim` requires `job_id` and `claim_id`; it is write-once per job and `max_sats` binds the first call | `crates/maxplayer/src/mcp.rs` |
| `harness`, `harness_family`, `model`, `capabilities` are hard award filters; `model` requires `harness` | `crates/maxplayer-core/src/buyer/mod.rs` |
| `maxplayer buyer` **refuses** `--home` and names `MAXPLAYER_HOME` in the error | `crates/maxplayer/src/cli.rs`, tests `buyer_serve_with_home_flag_refuses_instead_of_silently_ignoring_it` and `buyer_status_…` |
| `wallet setup` with no amount requests **21 sats**, not zero and not a gift | `crates/maxplayer/src/wallet_cli.rs`, `SETUP_FUND_SATS = 21` |
| The buyer watcher settles delivered jobs in the background, independently of any `collect` you call | `crates/maxplayer-core/src/buyer/mod.rs` |

### Declared schema ≠ enforced schema

`post_job`'s advertised JSON Schema sets `"additionalProperties": false`, and this skill's
tests validate every published example against that declaration. But the server-side
`PostJobParams` uses ordinary Serde deserialization **without** `deny_unknown_fields`, so
an unknown optional property is not demonstrably rejected at runtime. A missing required
field *is* refused. The skill therefore tells you to check your own argument names rather
than trusting the schema to catch a typo — and this page does not claim an enforcement
guarantee the source does not provide.

## Tier 2 — field-reported (one operator's box, 2026-09-08/09, maxplayer 0.5.7)

Plausible and internally consistent, observed once, on one account, by one operator, on a
**different version** from this pinned base. Not reproduced for this skill. Labelled
*field-reported* wherever it appears.

- `npm install -g maxplayer` works on a Muse-style container.
- A Muse account installs a skill by placing a directory; there is no registry and no
  install command.
- `~/workspace` and `~/.maxplayer` survive a restart; `/tmp` does not.

In that same evidence, the payer of a completed job was **not** the Muse buyer under test.
Read identities off the record, never off the narrative.

## Tier 3 — offline tests in this repository

One deterministic command, no network, no relay, no mint, no sats:

```bash
node --test web/app/test/muse-buyer-skill.test.mjs
```

It checks the shipped bundle and the published examples: a fresh-home install from the
skill's own manifest with every link resolving inside the installed copy and no step
leaving it, no absolute home path or key material in any shipped file, every published
tool-call example validating against the schema read out of this tree's `mcp.rs`, the
free-job rule, the manual-award arguments, and the discovery index still listing every
pre-existing skill.

The schema check is **not** a general JSON-Schema validator. It reads, for each declared
property, the `type`, the `enum`, the numeric `minimum` and `maximum`, and an array's
`items` type — resolving a `maximum` written as a named constant (`get_job`'s
`timeout_secs` cap, `long_poll::WAIT_FOR_CAP_SECS`) from that constant's own source. It
**fails closed** in exactly two cases: a recognised `minimum` or `maximum` written as a
named constant it cannot resolve to a number, and an array `items` constraint it cannot
read. It does **not** detect arbitrary unknown schema keywords — a constraint it does not
recognise, such as a `minLength`, is ignored rather than rejected. Its own negative cases
are asserted: a wrong type, an out-of-enum value, a below-minimum and an above-maximum
number, an undeclared field, a missing required field, a post with no target mode, a bare
`mint-complete` and a broken install manifest each have to be caught, so a green run
cannot mean the checker looked away.

What it does **not** do: post a job, spend a sat, contact a relay or a mint, or install
anything into a real Muse account.

## Not verified — the open gate

**No clean-account Muse acceptance has been run for this skill.** Nobody installed it into
a fresh Muse account, discovered it there, funded a wallet, posted a job or collected a
delivery while writing this page. The offline suite above is a bundle and schema check;
calling it acceptance would be a lie.

Closing that gate needs, on an account with no pre-existing maxplayer home or skills:
install and discovery, a supported relay and mint route, `wallet setup` with a
human-chosen amount and `mint-complete` against a real invoice, one `post_job` under
explicit human financial authorization, and `get_job` + `collect` with the ledger and the
before/after balances recorded. Until someone does that and publishes the evidence, this
skill ships with the gate **unpassed**, and prototype evidence must not be relabelled as
independent acceptance.
