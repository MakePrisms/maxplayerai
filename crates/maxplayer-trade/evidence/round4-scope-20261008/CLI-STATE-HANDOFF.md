# Round-four CLI and state handoff — implementation 1929972

Historical snapshot, not round-five guidance. External review of 1c68714 requested changes; in particular exit code 2 also overlaps Clap usage errors. Round five owns corrections.

Global CLI: `--home <HOME>` required private owned home; repeat `--relay <RELAY>` to override defaults; `-h/--help` prints help. Global options precede subcommands. No version flag, no real-mint allow flag, no real-money opt-in env var.

| Command | Arguments / flags | Meaning |
|---|---|---|
| list | --give-mint, --give, --want-mint, --want, --max-fees (default16), -h/--help | Publish fixed net-amount lot and reserve backing after preflight. |
| discover | -h/--help | Discover verified available lots. |
| cancel | <LOT>, -h/--help | Cancel inactive available listing and release backing; active swaps are refused. |
| serve | -h/--help | Continuously handle messages and recover authorizations. |
| take | <LOT>, --max-give, --min-receive, --max-fees (default16), -h/--help | Trade under total-debit, net-receipt, mint-fee limits. |
| recover | -h/--help | Single bounded pass; 0 terminal, 2 unresolved/deferred, 1 command error. Does not wait for deadlines. |
| preflight | <MINT>, -h/--help | Read NUT07/09/12/14, active sat keyset, <=60sec clock skew. |
| fund | <MINT>, --amount, --quote (optional), -h/--help | Create/resume mint funding; never pays invoice; 100000 cumulative per mint/home. |
| withdraw | <MINT>, --invoice, -h/--help | Pay exact invoice, max100000 sats, reserve max32 sats; reconcile change. |
| balance | <MINT>, -h/--help | Read spendable balance without submitting earlier authorizations. |
| help | [COMMAND] | Show command help (built-in Clap command). |

List `--give` and `--want` are net sats. `--give-mint` and `--want-mint` are canonical mint URLs; `--max-fees` is combined lock and claim mint-fee ceiling. `--max-give` bounds total debit; `--min-receive` bounds net receipt. Funding `--quote` names saved quote; `--amount` is funding sats. No command implies paying a funding invoice externally.

## Swap states

| State | Terminal? | Meaning |
|---|---|---|
| requested | no | Taker request journaled, waiting for maker quote. |
| quoted | no | Maker quote journaled, waiting for taker lock. |
| accepted | no | Taker accepted quote, first lock pending. |
| first_locked | no | Taker outgoing first lock created; waiting for maker second leg. |
| first_validated | no | Maker validated first lock, second lock pending. |
| second_locked | no | Maker outgoing second lock created; waiting for claim evidence or refund deadline. |
| second_validated | no | Taker validated maker lock; claim/refund reconciliation pending. |
| claimed | no | Taker received maker leg; waiting for maker spend evidence or terminal bookkeeping deadline. |
| claiming | no | Maker learned claim preimage; receiving taker leg. |
| settling | no | Maker claim landed; reconcile outgoing proofs/refund outputs. |
| lock_unforwardable | no | Own lock cannot be safely forwarded; retain proofs for timed recovery. |
| lock_reconciling | no | Abandoned/ambiguous lock attempt needs fresh restoration evidence. |
| complete | yes | Successful trade settlement observed. |
| complete_unclaimed | yes | Taker claimed and long deadline passed; own leg remains payable to maker and must never refund. |
| refunded | yes | Local outgoing leg refunded and reconciled. |
| expired | yes | Request/quote expired without a surviving authorized lock. |
| claim_quarantined | yes/manual | Claim response invalid; proofs/journal retained for manual recovery. |
| refund_quarantined | yes/manual | Refund response invalid; proofs/journal retained for manual recovery. |

## Withdrawal states

| State | Terminal? | Meaning |
|---|---|---|
| quote_created | no | Exact invoice authorization retained before payment submission (quote may still need reconciliation). |
| request_sent | no | Payment POST authorized durably; result may be ambiguous. |
| pending | no | Mint reports payment pending; all reservations retained. |
| paid_change_unreconciled | no | Payment paid, exact change still requires verified restore/reconciliation. |
| done | yes | Payment and exact change reconciled; recovery finishes any interrupted reservation release. |
| unpaid_released | yes | Unpaid/no-submit outcome safely established and inputs released; original authorization preserved. |

Terminal quarantine is not spendable money or independent certification. A 120sec item budget bounds each funding/withdrawal/swap attempt; timeout does not undo a delivered RPC or release reservations. Use serve while locks are live, not a single recover invocation.
