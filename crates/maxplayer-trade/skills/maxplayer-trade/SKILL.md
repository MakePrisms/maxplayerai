---
name: maxplayer-trade
description: Trade Cashu tokens for a human with `maxplayer trade` from their maxplayer home, including real money. Use for “sell/list X of mint A for Y of mint B”, “buy that lot”, “find listings”, “fund my trade wallet”, “receive this token”, “send a token”, “withdraw”, “check balance”, or “something got interrupted”.
---

# Trade Cashu for the human

> Evidence and limits: [verification](references/verification.md).

## 1. Establish custody and scope

Use `maxplayer trade`, not Maxplayer jobs. Read
[commands](references/commands.md) before a command and
[recovery](references/recovery.md) for any interruption.

Trade from the human's **maxplayer home** (`--home <home>`, default `$MAXPLAYER_HOME`/`~/.maxplayer`):
its key signs, its wallet pays (sellers too), only its mints trade. A legacy trade home only drains. Preserve that home, keys, databases and
journals across restarts. Never delete it while funds, reservations, unresolved
funding/withdrawals, or trades exist; keep it afterward for recovery.
Never use a fresh home to evade caps. Only one writer owns it: while `serve` runs,
`list`/`take`/`cancel` hand off to it, exact bounds; `status`, `balance`,
`discover` are lock-free. Don't stop serve to look. Stop it with SIGTERM (exit 5;
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
also preflights before list/take/send
([checks](references/commands.md#enforced-monetary-policy)). Never bypass a failed check. Capped at
**100,000 sats** per lock, `wallet send` and `wallet melt`. Money in and out is `maxplayer wallet`
(same home, same wallet). Pass `wallet melt --max-debit` to cap invoice + the mint's fee
reserve. Fees can push a net lock over the cap.

**Before every real-money funding, `list`, `take`, withdrawal or send, present
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

### Fund, receive, send, withdraw (`maxplayer wallet`, same `--home`)

- Fund: `maxplayer wallet mint <sats> --mint <mint>` prints a BOLT11 invoice; never auto-pays.
  Give it privately to the human to pay from **their own wallet**; then
  `maxplayer wallet mint-complete <quote-id>`. Never create a replacement after a timeout.
- Receive (yes needed): token in a private file, `maxplayer wallet receive --token-file <file>`
  (`maxplayer wallet receive --add-mint ...` for a mint not yet in the wallet). Never put tokens
  in argv/chat/logs.
- Send (yes needed): `maxplayer wallet send <sats> --mint <mint> --out <new-file>`; deliver
  the file privately. Unredeemed, `maxplayer wallet receive --token-file <file>` takes it back.
- Withdraw: only the human's invoice, validated first: `maxplayer wallet melt <bolt11>
  --max-debit <approved-sats> --mint <mint>` pays immediately; it is not a preview.
- A legacy standalone home drains with `maxplayer trade --home <legacy> send <mint> --amount
  <sats> --out <file> --max-fees <cap>`, then `wallet receive` on the maxplayer home.
  `trade send --reclaim <id>` still takes back a token trade sent.

**Done:** `maxplayer wallet balance` (or `balance <mint>`) confirms the result.

### Sell/list X of mint A for Y of mint B

After confirmation, `list --give-mint <A> --give <X> --want-mint <B> --want <Y>
--max-fees <cap>`. X and Y are net lot amounts; mints are not interchangeable.
The seller's maximum debit is X plus the approved outgoing fee cap. Record the lot
ID and keep `serve` running until complete or refunded. Listing authorizes serving
that fixed lot, not further listings or price changes.

`cancel <lot>` only with **no active quote/swap**; it never revokes an authorized
HTLC lock. Expired untraded lots release themselves. On rejection, continue recovery; never force-release reservations. **Done:** terminal trade state and balances checked, or a confirmed
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
