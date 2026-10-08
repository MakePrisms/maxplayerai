# Verification status

Draft against round-4 decided behavior; base 1e0c30af76465c0576bb5d2e9f53ead1b1968eba.
Not yet verified against round 4's final default binary. Do not treat this draft as
release approval. Final command/help checks, relay defaults, cap implementation,
and withdrawal fee enforcement remain pending.

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
The report was still unpublished when read; final-head inclusion is unverified.
The run's recovery harness timed out on an unrelated retained Minibits withdrawal,
so swap refund success must not be described as a clean whole-home recovery exit.

This is evidence for that mint/version/run, not all Nutshell deployments or future
versions. Other Nutshell refund compatibility remains **unverified**. NUT advertisement,
CDK tests, and a successful trade alone do not establish refund interoperability.
