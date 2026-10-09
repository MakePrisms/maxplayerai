---
name: maxplayer-trade
description: Trade Cashu tokens for a human with the standalone maxplayer-trade CLI, including real money. Use for “sell/list X of mint A for Y of mint B”, “buy that lot”, “find listings”, “fund my trade wallet”, “receive this token”, “send a token”, “withdraw”, “check balance”, or “something got interrupted”.
---

# Trade Cashu for the human

> Round-five operating policy. Read the scoped evidence and
> verification limits in [verification](references/verification.md).

## 1. Establish custody and scope

Use the standalone binary, not Maxplayer jobs or the Maxplayer wallet/daemon. Read
[commands](references/commands.md) before any command and
[recovery](references/recovery.md) before handling an interruption.

Choose **one private, persistent absolute home** for this human's trade wallet. Put
`--home <home>` before **every** command. Preserve that home, keys, databases and
journals across restarts. Never delete it while funds, reservations, unresolved
funding/withdrawals, or trades exist; retain it for historical recovery afterward.
Never use a fresh home to evade caps. One writer owns it. Read-only `status` and `balance` work while `serve` runs.
**Do not stop serve between `second_locked` and `settling`**; a maker can lose its
claim window. Defer other writing commands until live obligations settle.
Never copy its seed.

Never print secrets, seeds, keys, tokens, proofs, preimages, or raw journals, or
attach a home/database to chat. Report only public IDs, mint URLs, amounts, states
and redacted errors. Invoice delivery belongs in the human's private conversation.
**Done:** the correct home and authorized mint identities (URL + sat unit) are known.

## 2. Check readiness and get explicit authorization

Use `balance <mint>` and `preflight <mint>` for each relevant mint. Passing preflight
does not establish honesty or full compatibility. The CLI
also preflights before fund/list/take/withdraw: NUT-07/09/11/12/14, an active sat keyset, reachable
mint, and clock skew at most 60 seconds. Never bypass a failed check. Gross locks and cumulative funding per mint per home are
capped at **100,000 sats**, as is each withdrawal invoice. The withdrawal Lightning
fee reserve ceiling is **max(32 sats, 2% of invoice sats rounded up)**. Pass
`--max-debit` to cap invoice amount + Cashu input fee + Lightning reserve. Fees can make an otherwise
eligible net lock amount too large.

**Before every real-money `fund`, `list`, `take`, `withdraw`, or `send`, present the
exact operation and get an explicit yes.** A broad “trade for me”, previous trade approval,
or a published listing is not approval for a new operation. State:

- Both mint URLs and direction (or the single funding/withdrawal mint and destination).
- Exact net amounts, maximum total debit, minimum receipt, and fee cap in sats.
- Public lot ID for a purchase; invoice amount/destination for a withdrawal.
- Worst-case normal trade lock budget: **about 75 minutes**, not a guaranteed release
  time. Protocol locks are 60/15 minutes plus margins; outages or ambiguous evidence
  can hold funds longer, indefinitely with a dishonest mint. Funding and withdrawal
  have their own unresolved-payment risk, not a guaranteed 75-minute timeout.
- A mint can steal its tokens. If the counterparty vanishes, funds stay locked until
  the refund deadline and successful recovery. `serve` must stay running while locks are live (the active `take` also watches its trade).
  NUT-14 claims remain valid after locktime. Nutshell refund status and limits: [verification](references/verification.md). NUT-07 advertisement alone does not prove witness emission. The maker relies on
  the chosen mint reporting the HTLC witness if the taker withholds its notice.
  Mint choice is the human's responsibility; there is no rating gate.

Use the human's price and fee budget, never looser. If fees are unspecified,
propose a numeric cap and wait for yes; the CLI default is not consent.
If a command cannot enforce the approved bounds, **do not run it**; escalate.
Never substitute a guessed flag or a larger cap.
**Done:** an explicit yes binds this exact action, amounts, limits, and risks.

## 3. Execute the selected workflow

### Fund my trade wallet

After confirmation, `fund <mint> --amount <sats>`. On a real mint it prints a BOLT11
invoice and never auto-pays. Give that invoice privately to the human to pay from
**their own wallet**; the agent must never pay without separate explicit consent.
A printed invoice is not funded balance. Preserve the quote ID; after
interruption use `recover`, and when needed resume that same funding quote with
`fund <mint> --amount <same-sats> --quote <quote-id>` after confirmation. Never create
or pay a replacement because a reply or issuance timed out.
**Done:** issuance is reconciled and `balance <mint>` confirms the result, or the
retained quote is reported as unresolved without another payment.

### Receive a token

Needs the human's yes (mint URL, token amount); it charges the per-mint cap.
Save the token to a private file; run `receive <mint> --token-file <file>` (or
stdin). Never put a token in argv, chat or logs. Only plain sat proofs from that
exact mint, at most 100,000 sats; the input fee is deducted. Exit 3 is not failure:
`recover`, never re-import elsewhere. **Done:** state `done` and `balance <mint>`.

### Send a token / move funds to another wallet

Needs the human's explicit yes, like withdraw (mint, amount, fee cap). Run
`send <mint> --amount <sats> --out <new-file> --max-fees <cap>`; deliver the file
privately, never its contents. Exit 3: `recover`; never resend. Until redeemed,
`send --reclaim <id>` takes it back. **Done:** state `sent`.

### Sell/list X of mint A for Y of mint B

After confirmation, `list --give-mint <A> --give <X> --want-mint <B> --want <Y>
--max-fees <cap>`. X and Y are net lot amounts of distinct mint assets.
The seller's maximum debit is X plus the approved outgoing fee cap. Record the lot
ID; keep `serve` running until complete or refunded. Listing authorizes serving
that fixed lot; it does not authorize further listings or price changes.

`cancel <lot>` only with **no active quote/swap**; it never revokes an authorized
HTLC lock. On rejection, continue recovery; never force-release reservations. **Done:** terminal trade state and balances are checked, or a confirmed
inactive cancellation is reported; an unresolved lock is never called cancelled.

### Find listings / buy that lot

`discover`, inspect the exact mint pair and public lot ID, and match the human's
price. Discovery is not purchase consent. For “receive X A for at most Y B”,
set `--min-receive X` and `--max-give Y` when Y is the total spending ceiling. If Y
was explicitly a net price plus a separately approved fee F, the total ceiling can
be Y + F; otherwise never silently add fees. A stricter limit is fine, a looser one
is not. See the worked example in the command reference.

After confirmation, `take <lot> --max-give <total-cap> --min-receive <net-minimum>
--max-fees <cap>`. Keep it running through settlement; never repeat a take after
a timeout. **Done:** verify the terminal result and per-mint balances, or enter recovery.

### Withdraw

Use only the human's invoice; validate amount, destination, expiry and
fee/debit bounds before confirmation. `withdraw <mint> --invoice <bolt11> --max-debit <approved-sats>` may spend
immediately; it is **not** a quote preview. Read the withdrawal limitations in the
command reference first. Non-terminal: payment/change unresolved, not failed. Never create a replacement invoice, withdrawal, or payment
while a submitted payment is unresolved. A pre-POST refusal cannot pay later via
recovery; only explicit same-invoice withdrawal can authorize a new attempt.
`quote_created` is never auto-submitted. A timeout after POST is not a failed payment. **Done:** `done` and reconciled change/balance are confirmed,
or report the retained authorization and recover it without paying again.

### Check balance / something got interrupted

`balance <mint>` is wallet balance, not total wealth or proof reservations are spendable. After **any interruption**, use `recover` with the same
home and relays before new work. It resumes existing authorizations and does not
admit new trade requests. It makes **one bounded pass**, at most **120 seconds per
funding/withdrawal/receive/swap item**, without waiting for lock deadlines: exit **0** means
all terminal with no deferred work, **3** unresolved/deferred, **4** terminal manual recovery, **1** command error, pre-submission refusal, or definitive terminal `unpaid_released` (never proof a submitted withdrawal was refused; a submitted non-terminal withdrawal exits **3**),
**2** CLI usage error.
Timeout neither undoes an RPC nor releases reservations. Restore `serve` while locks
are live; one recovery pass is not a watcher. See the recovery reference for
quarantine and errors.
**Done:** classify every reported outcome using the recovery reference.

## 4. Verify and report

Check public states with `status` and balances with `balance` without stopping
the owner process. A zero exit, elapsed deadline, missing listing, or PAID mint is not alone
reconciled success. Keep the home. Report spent/received amounts and fees
only when known; distinguish complete, refunded, unresolved, and manual recovery.

On `refund_quarantined` or `claim_quarantined`, stop new money actions and escalate
to the human immediately. These are terminal **manual recovery**, not success or
spendable funds. Never credit/import unverifiable outputs, edit journal records,
replace attempts, or treat quarantine as refund permission. Persistent RPC failure,
missing witnesses, unexpected states, or accounting gaps also need a human; keep safe
recovery for other obligations without weakening any check.
**Done:** the human has an accurate outcome and any unresolved custody obligations.
