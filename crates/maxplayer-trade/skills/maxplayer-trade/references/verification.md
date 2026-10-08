# Verification status

## Base and release gate

Rebased onto **9dad8e62669468492f997b4cb44fabe2b26ba8e1**, the published round-4
head whose eight GitHub CI jobs and Vercel checks passed, checked at 20:00 UTC on
2026-10-08. This is a **blocked draft**, not permission to run real-money workflows.

The user's decided behavior is **not implemented at this head**:

- `src/real_money.rs` still sets the cap to **500**, not 100,000 sats.
- The binary still exposes `--real-mint-allow` and requires the old environment
  opt-in. This skill deliberately does not instruct agents to enable either.
- `preflight` checks NUT-07/09/12/14 and clock skew, but its standalone implementation
  does not check the required sat keyset. Keyset checks elsewhere do not establish
  the requested uniform automatic preflight for every operation.
- Withdrawal exposes only the invoice argument; its source caps Lightning reserve
  at 32 sats but does not enforce a user-selected total fee/debit ceiling. Never
  execute it to discover the quote or assume the trade fee flag applies.

A successful help test proves command/flag spelling and the 16-sat list/take fee
default, **not** the missing cap, no-opt-in behavior, preflight guarantee, or monetary
safety. Those defaults/policies are not advertised by help; source was checked too.
The intended workflow must be reverified on the later implementation before use.

## Checks run on this base

Default (no lab feature) binary build passed, using the crate’s standalone Cargo
manifest and locked dependencies, with only the maxplayer-trade binary selected.
Read root help and all ten subcommand help pages from that binary. Extended the
existing `skill_commands_and_flags_match_binary_help` test to derive command coverage
from root help, validate canonical signatures against each command, scan flags in
all skill Markdown (including references), and assert the documented fee defaults.
The test invokes only help, with no home, mint requests, payments, or jobs.

Focused test output:

```text
PASS preflight <mint>
PASS balance <mint>
PASS fund <mint> --amount <sats> [--quote <quote-id>]
PASS list --give-mint <mint> --give <net-sats> --want-mint <mint> --want <net-sats> --max-fees <sats>
PASS discover
PASS serve
PASS cancel <lot>
PASS take <lot> --max-give <total-sats> --min-receive <net-sats> --max-fees <sats>
PASS withdraw <mint> --invoice <bolt11>
PASS recover
PASS 10 commands; 14 distinct flags; list/take fee default 16; 4 skill documents
test result: ok. 1 passed; 0 failed; 4 filtered out
```

Skill frontmatter validation, resource-link resolution, and whitespace checks passed.
Other CLI tests and money-path suites were not run by this documentation task. No
real or fake-money operations were executed. No push was made.

This base qualified through completed CI just before the five-hour wait limit.
The queued no-opt-in/100,000-sat implementation did not land within that window;
verification of that later behavior remains outstanding. Keep this draft blocked
until that implementation is available and rebase/rebuild/reverify it then.

## Nutshell evidence available during drafting

Round 4's author-led report, read at 19:30 UTC on 2026-10-08, reports a successful
production-timing refund on **https://cashu.cz, Nutshell/0.21.0**, paired with
Macadamia. Both roles reached `refunded`; the cashu.cz taker balance was 104 before
and after, and the Macadamia maker spent 2 sats in mint fees. The taker had stored
incoming proofs and no claim authorization when interrupted. Its two outgoing
proofs were reported SPENT with JSON-string HTLC refund witnesses (empty preimage,
one signature), accepted by the pinned parser. No witness or secret is reproduced
here. This agent did not execute or independently reproduce that money-moving run.

Evidence source: round-4 worktree, `crates/maxplayer-trade/evidence/round4-20261008/REPORT.md`,
sections “Both production-timing refunds” and “Real Nutshell NUT-07 evidence”.
The report is included at the verified base head. Its evidence is author-led,
not a new independent refund test by this skill author.
The run's recovery harness timed out on an unrelated retained Minibits withdrawal,
so swap refund success must not be described as a clean whole-home recovery exit.

This is evidence for that mint/version/run, not all Nutshell deployments or future
versions. Other Nutshell refund compatibility remains **unverified**. NUT advertisement,
CDK tests, and a successful trade alone do not establish refund interoperability.
