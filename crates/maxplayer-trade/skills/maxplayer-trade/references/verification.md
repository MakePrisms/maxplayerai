# Verification status

## Implementation base and scope

Verified against **19299727e5e25479977cd95dee86a45a16bb3821** on PR #1107.
The four local skill commits (ec196bb, e33fb25, f4a283b, a9a9df1) were rebased
onto that head, followed by this final documentation/verification update. The PR
remains draft; this skill does not certify every mint or authorize money movement.

Source checks establish:

- `src/real_money.rs`: 100,000-sat gross lock cap.
- `src/money.rs`: 100,000 cumulative funding per mint/home, including retained
  pending intents; 100,000 per withdrawal invoice; 32-sat Lightning fee reserve.
- `src/main.rs`, `src/wallet.rs`, `src/coordinator.rs`: no real-money opt-in;
  automatic NUT-07/09/12/14, active sat keyset, and at-most-60-second clock-skew
  preflight for funding/listing/taking/withdrawal.
- `src/money.rs`, `src/coordinator.rs`, `src/main.rs`: one recovery pass with
  120-second per-item budgets; exits 0 terminal/no deferred work, 2 unresolved or
  deferred, 1 command error. Terminal quarantines still require manual recovery.
- Withdrawal has no user-selectable total-debit/input-fee cap. The command reference
  retains the rule to stop when the human's approved bounds cannot be enforced.

The staged CLI-STATE-HANDOFF glossary agrees with the application command/flag
inventory and swap/withdrawal states. Two qualifications matter: exit 2 can also
be a Clap usage error, and terminal items alone do not ensure exit 0 if work such
as listing publication was deferred. Per-item timeouts are not a whole-pass limit.
Market commands such as discovery also recover prior authorizations before dispatch.

## Checks

Verification uses the default binary, locked standalone manifest, and this checkout's
separate target directory, not the shared lab runner's binaries. Help checks execute
only help; no money-moving commands, real wallets, Maxplayer jobs, or lab loops.
The exhaustive test checks every application command, command-local flags, all skill
Markdown, and the list/take 16-sat fee default. Help establishes syntax; the source
checks above establish policy.

Final local results:

- Default binary build: passed (no lab feature).
- CLI test target: **5 passed, 0 failed**. Non-help cases use disposable empty
  homes only; no real wallet or money movement. The initial expanded scanner
  mistook the usage-line global home option for a command-local option; corrected
  to inspect the Options section, then reran successfully.
- Exhaustive help check: **10 application commands, 13 distinct long flags,
  4 Markdown documents**, fee default 16; also checks short help and built-in help.
  Focused final-document rerun: **1 passed, 0 failed, 4 filtered out**.
- Standalone trade formatting: passed. Root workspace formatting: failed on
  **153 untouched files**, outside this task; no unrelated formatting applied.
- Skill frontmatter, resource links, and diff whitespace: passed.

No full money-path/lab suite or loops were run; those remain the separate runner's
scope. No real mint fund/withdraw/take/list, real wallets, or Maxplayer jobs were
used. The shared lab target and the operator's wallet home were not touched.

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
