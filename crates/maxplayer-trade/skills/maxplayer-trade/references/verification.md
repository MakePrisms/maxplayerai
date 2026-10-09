# Verification status

## Implementation base and scope

Round five updates the policy from reviewed base `1c68714`. This is author-led
implementation verification, not independent reviewer sign-off or a mint rating.
The PR remains draft. No real wallets/mints or Maxplayer jobs are used in this pass.

Source checks and named regressions are in `tests/round5.rs`, `tests/money.rs`,
`tests/cli.rs`, and `tests/relays.rs`:

- Pinned sender-funded claim fees; incoming caps checked before lock admission.
- Bounded message handling and CDK calls; own-mint refund before incoming claim.
- Terminal pre-POST withdrawal refusals; no passive QuoteCreated execution.
- Reserve ceiling max(32, ceil(amount × 2%)); total-debit bound enforced by the
  withdrawal debit option, and payment-hash dedupe across mints.
- Exact journaled melt replay after no reply; definitive NUT errors still require
  fresh UNPAID/UNSPENT evidence before release; first POST expiry margin 60 seconds.
- Read-only balance/status without stopping serve; recovery exits 0/1/2/3/4 are
  distinguished in the recovery reference. Funding/withdrawal preflight includes
  NUT-04/20 and NUT-05 respectively; trading also requires NUT-11.

The final round-five test counts, mutation outcomes, detached fake-mint refund,
and exact-head CI receipts are recorded in PR #1107’s round-five section. Earlier
round-four evidence below remains scoped to its original run. The CLI help check
covers all application commands and all four Markdown files; it establishes syntax,
not monetary safety. Fake mints do not establish arbitrary real-mint interoperability.

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
