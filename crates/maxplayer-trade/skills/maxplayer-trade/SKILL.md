---
name: maxplayer-trade
description: Sell or buy fixed lots of fake-money Cashu assets with the standalone maxplayer-trade CLI; fund, inspect balances, preflight mints, and recover interrupted trades.
---

# Trade test Cashu assets

Use the standalone `maxplayer-trade` binary, not Maxplayer jobs, wallets, or daemon.

## Shared preparation

1. Choose a private persistent home. Put `--home <home>` before every command and use
   that same home throughout the role's lifetime. Never delete it during a trade.
   Never print secrets, wallet seeds, proofs, preimages, or tokens; do not dump journals.
2. Stay on fenced test mints: `https://testnut.cashudevkit.org`,
   `https://testnut.cashu.space`, or a deliberately configured loopback test mint.
   No real money or fence override. Run `preflight <mint>` for both mints.
   Preflight advertisement is not proof of NUT-07 witness emission; missing evidence
   at refund time deliberately blocks a taker refund. Escalate persistent ambiguity.
3. Run `balance <mint>`. For fake auto-paid funding only, use
   `fund <mint> --amount <amount>`; resume a retained quote with the same amount and
   `--quote <quote-id>`. Never pay a real invoice. Check the resulting balance.

## Sell

1. Match the user's exact assets, net price, and fee budget:
   `list --give-mint <mint> --give <net> --want-mint <mint> --want <net> [--max-fees <cap>]`.
   Record the public lot ID, not any private journal data.
2. Keep `serve` running while funds are locked, until complete or refunded. The home
   has an exclusive process lock: do not run another command concurrently on it.
   After an interruption, run `recover` with the same home (and relay configuration).
3. `cancel <lot>` is only for a listing with no active quote/swap; it cannot revoke
   an authorized HTLC. Check cancellation output, never assume it unlocks an active trade.

## Buy

1. Run `discover` and select a public lot whose mint pair and price the user authorized.
2. Before `take`, verify that `--max-give` matches the user's maximum total debit
   including mint fees and `--min-receive` matches the user's minimum net receipt.
   Never silently raise either spending or fee authority to make a trade work.
3. Run `take <lot> --max-give <cap> --min-receive <minimum> [--max-fees <cap>]`.
   Keep it running through settlement. After interruption run `recover`; use `serve`
   if a continuous watcher is needed, keeping it running until complete or refunded.

## Recovery and terminal outcomes

`recover` resumes existing authorizations and reports terminal state; it does not admit
new requests. Preserve the home and continue recovery for nonterminal states, including
`lock_reconciling`, `lock_unforwardable`, `claiming`, and `settling`. Refunds are signed
swaps after deadlines, not automatic expiry. Production locks are 60/15 minutes.

- `complete`: both legs settled.
- `complete_unclaimed`: taker received payment; maker remains entitled to the taker's
  locked leg. Never attempt to refund that leg.
- `refunded`: this role's refund completed; this is not a successful sale.
- `expired`: authorization expired without a confirmed lock; no successful trade.
- `refund_quarantined` / `claim_quarantined`: terminal **manual recovery**, not spendable
  balance or success. Mint-reported commit evidence exists but owned outputs failed
  DLEQ verification. Exact attempts/outputs remain private in the journal. Automatic
  retries stop. Preserve the entire home and escalate to a human; never import or
  credit these outputs, retry with fresh outputs, delete records, or print secrets.

Verify public completion and balances only after stopping the home-owning watcher safely.
If a mint stays unavailable, witness evidence stays ambiguous, or recovery quarantines
funds, report the public swap ID/state to the human and retain the home for investigation.
