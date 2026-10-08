# Recovery and escalation

After any interruption, run `recover` using the original private home and relay
configuration. It resumes existing authorizations, not new trade requests. Do not
run it concurrently with the home-owning watcher. Recovery can complete already
authorized money effects; it is not a read-only status command. Do not enlarge the
authorization or construct replacement outputs. Keep `serve` up when authorized
listings should continue; recovery alone does not accept new quotes.

Refunds require a signed mint operation **after** the deadline plus margin, not
merely waiting for expiry. Production taker/maker locks are 60/15 minutes, with
60-second refund margin and a 3-minute taker claim cutoff. Budget about 75 minutes
for normal worst-case lock/recovery handling, but never promise a release deadline.
A vanished peer means waiting for refund; a vanished or dishonest mint can block
recovery indefinitely. An offline seller can miss its recovery opportunity; NUT-14
receiver claims remain valid after locktime. Keep the watcher alive.

## Trade states

| State | Meaning and agent action |
| --- | --- |
| `requested` | Taker request sent; awaiting quote. Continue recovery; do not start a duplicate take. |
| `quoted` | Maker reserved an active quote. Continue watcher; cancellation is refused. |
| `accepted` | Taker accepted the quote; lock creation/reconciliation pending. |
| `first_locked` | Taker's outgoing long lock exists; await maker lock or timed recovery. |
| `first_validated` | Maker validated taker lock; its own lock is pending. |
| `second_locked` | Maker's outgoing short lock exists; awaiting claim/preimage or timed refund. |
| `second_validated` | Taker validated maker lock; claim is pending or being reconciled. |
| `claimed` | Taker obtained maker tokens; maker's claim of taker payment is not yet confirmed. Never refund that payment. |
| `claiming` | Maker knows the preimage; its incoming claim still needs reconciliation. |
| `settling` | Maker incoming claim succeeded; outgoing claim/refund reconciliation remains. |
| `lock_reconciling` | Lock result or reservation release is ambiguous. Preserve exact attempt; do not assume expiry released funds. |
| `lock_unforwardable` | Own lock has missing/invalid DLEQ evidence; never forward it. Continue safe timed refund/reconciliation. |
| `complete` | Terminal: trade legs settled according to this role's evidence. Check balances; retain home. |
| `complete_unclaimed` | Terminal taker outcome: received maker tokens, maker still entitled to original taker lock. Never refund it; retain home. |
| `refunded` | Terminal: this role's outgoing refund completed, not a successful trade. Fees may remain spent; invalid change can still require manual recovery. |
| `expired` | Terminal authorization expiry without a confirmed lock, not a successful trade. Do not infer safety from time alone; use the reported state and accounting. |
| `refund_quarantined` | Terminal manual recovery: refund commit evidence exists, owned outputs failed verification. Not refunded spendable balance. |
| `claim_quarantined` | Terminal manual recovery: claim commit evidence exists, owned outputs failed verification. Not a successful credited claim, and not permission to refund. |

All rows above `complete` are non-terminal. Keep recovery active. A terminal result
is not proof that every piece of private journaled change is spendable. A mint
processing an already-delivered request arbitrarily late is a residual risk.

Quarantine means exact attempts/outputs remain privately journaled and automatic
execution for that attempt stops. Stop new money actions and notify the human with
public ID, role, mint URL, state, known amounts, and redacted error only. Preserve
the **whole** home. Never dump journals, import unverified proofs, edit/release
reservations by hand, delete attempts, or retry with fresh outputs. A quarantined
maker listing stays held. A human must arrange verified recovery; no automatic
“repair” or manual token export belongs in this skill.

## Withdrawal states

| State | Meaning and action |
| --- | --- |
| `quote_created` | Authorization saved; quote/setup may be incomplete. Recover the same authorization; no replacement. |
| `request_sent` | Payment POST may have reached the mint; timeout does not mean failure. Reconcile only. |
| `pending` | Mint reports payment pending; preserve reservation and wait/recover. |
| `paid_change_unreconciled` | Invoice reported paid, but change/accounting not proven complete. Never pay again; preserve outputs and recover. |
| `done` | Terminal: paid with reconciled change and wallet accounting. Verify receipt and balance. |
| `unpaid_released` | Terminal: safe unpaid outcome established and reservation released. Not payment success. Any new payment needs fresh confirmation. |

Only the last two are terminal. Missing/partial restore, ambiguous UNPAID, unknown
status, contradictory state, or lost zero-change reply may keep a withdrawal held.
Never create a replacement while unresolved, even if the invoice expired or the
recipient requests another. Escalate persistent uncertainty without forcing release.

## Funding and market status are separate

Funding has a retained quote/issuance authorization; invoice paid is not proof of
wallet credit. Recover or resume that exact quote, never pay a second invoice to
resolve missing credit. A historical CDK orphan-quote reservation problem needs a
human; never clear it from the database yourself.

Published listing statuses (`available`, `sold`, `cancelled`) are not wallet
settlement states. Reservations/active quotes and lot expiry are separate conditions. Relay absence, a publication ACK, or a stale listing is
not permission to duplicate a trade. A listing quarantined by discovery validation
must not be purchased or manually reconstructed to bypass checks.

If recovery remains blocked past expected deadlines, a mint is persistently offline,
witnesses are missing/ambiguous, or actual accounting differs from approved limits,
escalate promptly. Safe retries for existing obligations may continue with backoff;
no new money action and no loosened checks. Unknown states fail closed until the
installed version's behavior is understood.
