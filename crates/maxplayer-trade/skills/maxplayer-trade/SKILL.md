---
name: maxplayer-trade
description: Trade Cashu tokens for a human with the standalone maxplayer-trade CLI, including real money. Use for “sell/list X of mint A for Y of mint B”, “buy that lot”, “find listings”, “fund my trade wallet”, “receive this token”, “send a token”, “withdraw”, “check balance”, or “something got interrupted”.
---

# Trade Cashu for the human

> Evidence and limits: [verification](references/verification.md).

## 1. Establish custody and scope

Use the standalone binary, not Maxplayer jobs or wallet/daemon. Read
[commands](references/commands.md) before a command and
[recovery](references/recovery.md) for any interruption.

Choose **one private, persistent absolute home** for this human. Put
`--home <home>` before every command but `doctor`. Preserve that home, keys, databases and
journals across restarts. Never delete it while funds, reservations, unresolved
funding/withdrawals, or trades exist; keep it afterward for recovery.
Never use a fresh home to evade caps. Only one writer owns it: while `serve` runs,
`list`/`take`/`cancel` hand off to it, exact bounds; `status`, `balance`,
`discover` are lock-free. Don't stop serve to look. Stop it with SIGTERM (safe, exit 5;
[lifecycle](references/commands.md#serve-lifecycle-stopping-and-progress-lines)).
**Do not stop serve between `second_locked` and `settling`**; a maker can lose its
claim window. Defer other writing commands until live obligations settle.
Never copy its seed.

Never print or attach secrets, seeds, keys, tokens, proofs, preimages, raw journals,
homes or databases. Report only public IDs, mints, amounts, states, redacted errors. Deliver invoices only in the human's private chat.
**Done:** home and authorized mints (URL or `nostr://<npub>`; sat unit) are known.

## 2. Check readiness and get explicit authorization

Run `balance <mint>` and `preflight <mint>` per mint. Passing preflight
proves neither honesty nor full compatibility. The CLI
also preflights before fund/list/take/withdraw/send
([checks](references/commands.md#enforced-monetary-policy)). Never bypass a failed check. Gross locks and each withdrawal invoice are capped at
**100,000 sats**. Funding intents plus charged receives (gross) share one
100,000-sat cap per mint per home (what counts: [commands](references/commands.md#enforced-monetary-policy)). Pass `--max-debit`
to cap invoice + Cashu input fee + Lightning reserve ([reserve ceiling](references/commands.md#withdrawal-enforcement-before-execution)). Fees can push a net lock over the cap.

**Before every real-money `fund`, `list`, `take`, `withdraw`, or `send`, present
the exact operation and get an explicit yes.** For `list`/`take`, first show the
same command's `--dry-run` fees, gross amounts and max debit (changes nothing).
A broad “trade for me”, earlier approval or published listing approves no new
operation. State:

- Both mint URLs and direction (or the single funding/withdrawal mint and destination).
- Exact net amounts, maximum total debit, minimum receipt, and fee cap in sats.
- Public lot ID for a purchase; invoice amount/destination for a withdrawal.
- Worst-case normal lock budget: **about 75 minutes**, not a guaranteed release
  time. Protocol locks are 60/15 min plus margins; outages or ambiguous evidence
  can hold funds longer, indefinitely with a dishonest mint. Funding and withdrawal
  have their own unresolved-payment risk.
- A mint can steal its tokens. If the counterparty vanishes, funds stay locked until
  the refund deadline and successful recovery; keep `serve` (or the active `take`) running.
  NUT-14 claims remain valid after locktime. Nutshell refund limits: [verification](references/verification.md). NUT-07 advertisement alone does not prove witness emission. The maker relies on
  the chosen mint reporting the HTLC witness if the taker withholds its notice.
  Mint choice is the human's responsibility; there is no rating gate.

Use the human's price and fee budget, never looser. If fees are unspecified, propose a numeric cap and wait for yes; the CLI default is not consent.
If a command cannot enforce the approved bounds, **do not run it**; escalate. Never guess a flag or raise a cap.
**Done:** an explicit yes binds this exact action, amounts, limits, and risks.

## 3. Execute the selected workflow

### Fund my trade wallet

After confirmation, `fund <mint> --amount <sats>`. On a real mint it prints a BOLT11
invoice and never auto-pays. Give it privately to the human to pay from **their own wallet**; never pay it
without separate explicit consent.
An invoice is not funded balance. Keep the quote ID; after
interruption use `recover`, and if needed resume that quote with
`fund <mint> --amount <same-sats> --quote <quote-id>` after confirmation. Never create
or pay a replacement because a reply or issuance timed out.
**Done:** issuance reconciled and `balance <mint>` confirms it, or the retained
quote is reported unresolved without another payment.

### Receive a token

Needs the human's yes (mint, amount); charges the shared cap (§2).
Save it to a private file; run `receive <mint> --token-file <file>` (or
stdin). Never put tokens in argv/chat/logs. Only plain sat proofs of that
mint; fee deducted. Exit 3: `recover`, never re-import; 4 `quarantined`: escalate; other exits:
[commands](references/commands.md#receive). **Done:** `done` + `balance <mint>`.

### Send a token / move funds to another wallet

Needs a yes as for withdraw (mint/amount/fee cap). Run
`send <mint> --amount <sats> --out <new-file> --max-fees <cap>`; deliver the
file privately, not its contents. Exit 3: `recover`; never resend. Until redeemed,
`send --reclaim <id>` takes it back. **Done:** state `sent`.

### Sell/list X of mint A for Y of mint B

After confirmation, `list --give-mint <A> --give <X> --want-mint <B> --want <Y>
--max-fees <cap>`. X and Y are net lot amounts; mints are not interchangeable.
The seller's maximum debit is X plus the approved outgoing fee cap. Record the lot
ID and keep `serve` running until complete or refunded. Listing authorizes serving
that fixed lot, not further listings or price changes.

`cancel <lot>` only with **no active quote/swap**; it never revokes an authorized
HTLC lock. On rejection, continue recovery; never force-release reservations. **Done:** terminal trade state and balances checked, or a confirmed
inactive cancellation is reported; an unresolved lock is never called cancelled.

### Find listings / buy that lot

`discover` (per-lot fees or `unknown`), inspect the mint pair
and lot ID, and match the human's price. Discovery approves nothing. For “X A for at most Y B”,
set `--min-receive X` and `--max-give Y` when Y is the total spending ceiling. If Y
was explicitly a net price plus a separately approved fee F, the total ceiling can
be Y + F; otherwise never silently add fees. Stricter limits are fine, looser
not. Worked example: command reference.

After confirmation, `take <lot> --max-give <total-cap> --min-receive <net-minimum>
--max-fees <cap>`. Keep it (or serve) running through settlement. Never repeat a take
to “fix” a timeout. `counterparty_unresponsive` is only a warning; keep serving. **Done:** verify the terminal result and per-mint balances, or enter recovery.

### Withdraw

Use only the human's invoice; validate amount, destination, expiry and
fee/debit bounds before confirmation. `withdraw <mint> --invoice <bolt11> --max-debit <approved-sats>` may spend
immediately; it is **not** a quote preview. Read the command reference's withdrawal limits first. Non-terminal is unresolved, not failed. Never create a replacement invoice, withdrawal, or payment
while a submitted payment is unresolved. A post-POST timeout is not a failed payment;
pre-POST/`quote_created` rules: [withdrawal states](references/recovery.md#withdrawal-states). **Done:** `done` and reconciled change/balance are confirmed,
or report the retained authorization and recover it without paying again.

### Check balance / something got interrupted

`balance <mint>` is wallet balance, not total wealth or proof every reservation is spendable. After **any interruption**, use `recover` with the same
home and relays before new work. It resumes existing authorizations and does not
admit new trade requests. It makes **one bounded pass**, at most **120 s per
item** (`nostr://` swap: longer, see recovery), not waiting for lock deadlines: exit **0**
all terminal, no deferred work; **3** unresolved/deferred; **4** manual recovery; 1/2 and
withdrawal cases: [recovery](references/recovery.md#bounded-pass-and-exit-status). Exit 1
never proves a submitted withdrawal was refused.
Timeout neither undoes an RPC nor releases reservations. Restore `serve` while locks
are live; one pass is not a watcher. Quarantine and errors: recovery reference.
**Done:** every outcome classified per the recovery reference.

## 4. Verify and report

Check `status` (`--json` raw) and `balance` without stopping serve. A zero exit, elapsed deadline, missing listing or PAID mint is not
reconciled success by itself. Keep the home. Report spent/received amounts and fees
only when known; distinguish complete, refunded, unresolved, and manual recovery.

On `refund_quarantined` or `claim_quarantined`, stop new money actions and escalate
to the human immediately. These are terminal **manual recovery**, not success or
spendable funds. Never credit/import unverifiable outputs, edit journal records,
replace attempts, or treat quarantine as refund permission. Persistent RPC failure,
missing witnesses, odd states or accounting gaps also need a human;
keep safe recovery for other obligations without weakening any check.
**Done:** the human has an accurate outcome and any unresolved custody obligations.
