# Recovery and escalation

After any interruption, run `recover` using the original private home and relay
configuration. It resumes existing authorizations, not new trade requests. Do not
run it concurrently with the home-owning watcher. Recovery can complete already
authorized money effects; it is not a read-only status command. Do not enlarge the
authorization or construct replacement outputs. Keep `serve` up when authorized
listings should continue **and while any locks are live**; recovery alone does not
accept new quotes. An active `take` watches its own trade; after interruption restore
`serve` instead of starting another take.

Refunds require a signed mint operation **after** the deadline plus margin, not
merely waiting for expiry. Production taker/maker locks are 60/15 minutes, with
60-second refund margin and a 3-minute taker claim cutoff. Budget about 75 minutes
for normal worst-case lock/recovery handling, but never promise a release deadline.
A vanished peer means waiting for refund; a vanished or dishonest mint can block
recovery indefinitely. An offline seller can miss its recovery opportunity; NUT-14
receiver claims remain valid after locktime. Keep the watcher alive.

**Do not stop serve between `second_locked` and `settling`.** Use lock-free
`status` / `balance` / `discover` for observation while the writer runs; hand new
`list`/`take`/`cancel` to it instead of stopping it.

### `counterparty_unresponsive`

When a swap has waited more than 180 seconds for its counterparty (taker in
`requested` awaiting a quote or in `first_locked` awaiting the maker's lock; maker in
`quoted` awaiting the taker's lock or in `second_locked` awaiting the claim), `take`'s
live output, serve's log and `status` show `counterparty_unresponsive` with the
elapsed time and, once our own lock exists, when its refund becomes available. It is
display only: no state, deadline, lock time (60/15 min) or refund rule changes. Keep
serve running; settlement or the timed refund proceeds as usual. Do not start another
take or stop serve because of it.

## Bounded pass and exit status

`recover` makes one pass without waiting for future lock deadlines. Each funding,
withdrawal, receive, and swap attempt has a **120-second** budget; this is not a 120-second
whole-command deadline. Items are handled sequentially and relay/setup/publication
work is additional. A timeout does not undo a delivered RPC or release reservations.

- **0:** successful command; recovery has all items terminal, no deferred work or quarantine.
- **1:** command error, or a withdrawal refused/unpaid with no unresolved submission;
  inspect the state. Exit 1 is never proof that a submitted withdrawal was refused.
- **2:** CLI syntax/usage error only.
- **3:** non-final withdrawal (including a submitted one whose mint failed after the
  POST), unresolved/deferred recovery, or item error/timeout.
  Preserve authorization; keep `serve` running for live locks and retry with backoff.
- **4:** terminal manual-recovery quarantine (unresolved work takes precedence as 3).

Exhausted/blocked relay publication is reported as `publication_abandoned`, not
unresolved money work. Money records and signed receipts remain; the retry scan
contains only pending publications, with the 24-hour cutoff and 12-attempt budget.

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
| `expired` | Terminal authorization expiry without a surviving authorized lock, not a successful trade. Do not infer safety from time alone; use the reported state and accounting. |
| `refund_quarantined` | Terminal manual recovery: refund commit evidence exists, owned outputs failed verification. Not refunded spendable balance. |
| `claim_quarantined` | Terminal manual recovery: claim commit evidence exists, owned outputs failed verification. Not a successful credited claim, and not permission to refund. |

All rows above `complete` are non-terminal. Keep `serve` running for live locks. A terminal result
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
| `quote_created` | No POST authorized yet. Recovery only expires/releases it; only explicit `withdraw` with the same invoice may advance it. |
| `request_sent` | Payment POST may have reached the mint; timeout does not mean failure. Reconcile only. |
| `pending` | Mint reports payment pending; preserve reservation and wait/recover. |
| `paid_change_unreconciled` | Invoice reported paid, but change/accounting not proven complete. Never pay again; preserve outputs and recover. |
| `refused` | Terminal pre-POST refusal; no inputs/payment reserved. Same invoice may be explicitly retried. |
| `done` | Terminal: paid with reconciled change and wallet accounting. Verify receipt and balance. |
| `unpaid_released` | Terminal: safe unpaid outcome established and reservation released. Not payment success. Any new payment needs a new invoice and fresh confirmation. |

The last three are terminal. Missing/partial restore, ambiguous UNPAID, unknown
status, contradictory state, or lost zero-change reply may keep a withdrawal held.
Never replace a submitted unresolved payment. For an unsent `quote_created` record,
let recovery release it when its quote expires; if its reply was lost, recovery
releases it immediately. An explicit same-invoice command can retry an unexpired
unsent quote. `refused` permits a same-invoice retry; `unpaid_released` requires a
new invoice for a new payment. A failed pre-POST withdrawal cannot pay later via
serve/recover/market commands; a post-POST timeout is still unresolved, not failure. Escalate persistent uncertainty without forcing release.

## Receive states

| State | Meaning and action |
| --- | --- |
| `prepared` | Journaled, swap not yet sent. Recovery sends exactly the journaled swap. |
| `submitted` | Swap may have reached the mint. Recovery restores (NUT-09) the same outputs and replays only the identical swap; never import the token again elsewhere. |
| `done` | Terminal: DLEQ-verified proofs credited; check `balance`. |
| `refused` | Terminal: the mint returned a NUT error and the inputs stayed UNSPENT. Nothing credited, not charged to the cap. |
| `already_spent` | Terminal: inputs SPENT and none of our outputs restorable. Nothing credited, not charged. |
| `quarantined` | Terminal manual recovery (exit 4): our outputs were signed but lack valid DLEQ; never credited. Preserve the home and escalate. |

Inputs reported PENDING keep the attempt unresolved. A lost swap reply is never
answered with replacement outputs.

## Funding and market status are separate

Fresh UNPAID after funding expiry +60 seconds becomes terminal `expired_unpaid`,
still charged to the lifetime cap. A late-paid invoice may be resumed explicitly
with its original funding quote. Never pay the old and a replacement invoice.
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
