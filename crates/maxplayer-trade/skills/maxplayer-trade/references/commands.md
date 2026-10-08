# Commands and monetary bounds

Every invocation starts `maxplayer-trade --home <private-absolute-home>`.
Optional repeated global `--relay <url>` goes before the subcommand; preserve the
same relay set during recovery. Use the binary's default relays unless deliberately
configured; defaults at verified base 1929972 are `wss://nos.lol`,
`wss://relay.primal.net`, and `wss://offchain.pub`. These source defaults are also listed in root help. ACKed copies are not resent; retries use exponential
backoff with jitter, at most 12 attempts per event/relay and a 24-hour age cutoff.
Rate limiting pauses a relay for at least five minutes.
Relay ACK is not proof of stored readback or trade success. Respect backoff; do not
restart loops or generate fresh identities to evade rate limits.

Canonical command signatures (placeholders are not literal values):

```text
preflight <mint>
balance <mint>
fund <mint> --amount <sats> [--quote <quote-id>]
list --give-mint <mint> --give <net-sats> --want-mint <mint> --want <net-sats> --max-fees <sats>
discover
serve
cancel <lot>
take <lot> --max-give <total-sats> --min-receive <net-sats> --max-fees <sats>
withdraw <mint> --invoice <bolt11>
recover
```

Use `-h` / `--help` at the root or on any application subcommand. The built-in
`help [COMMAND]` also shows root or command help. There is no version option.
`--home <HOME>` is required for operational commands; `--relay <RELAY>` is
repeatable. All command-specific flags in the signatures above are required except
`--quote` and `--max-fees` (default 16 on list/take). Positional mint arguments are
canonical URLs; lot arguments are public lot IDs; `--quote` is a saved funding
quote for the same mint and amount.
Never use a lab-feature binary for the human's funds.

## Enforced monetary policy

- Asset identity is canonical mint URL plus unit `sat`; equal units do not make two
  issuers equivalent. Confirm URLs, not just display names. Use HTTPS for real mints.
- `--give` / `--want`: exact fixed-lot net amounts; no partial-size purchase.
- `--max-give`: buyer's total outgoing debit including mint fees.
- `--min-receive`: buyer's minimum net incoming amount.
- `--max-fees`: outgoing mint preparation + claim fee budget, default **16 sats** on
  list and take. Always pass an explicitly approved cap; this is not a global budget
  for repeated trades or a guarantee against future refund fees. Refund/recovery may
  incur mint fees; disclose this rather than promising a fully refunded balance.
- No platform/trade fee. Do not equate no platform fee with no mint/Lightning fees.
- Hard cap: 100,000 sats gross per lock; 100,000 cumulative funding per mint per home.
  Each withdrawal invoice is also capped at 100,000 sats (millisatoshis rounded up
  to sats). Funding counts all retained intents, including pending ones.
  None of these limits is permission to spend that amount. Never split operations or rotate homes
  to bypass limits. Withdrawing does not reset cumulative funding authority.

Example: a lot gives 32 A for 24 B. If the human approves **at most 27 B total**,
**at least 32 A net**, and **3 B fee sats**, use `take <lot> --max-give 27
--min-receive 32 --max-fees 3`. If they cap total spending at 24, use 24 instead of
27 and accept refusal when fees make it impossible. For a seller giving 32 A with
3 A fee sats, confirm a maximum debit of 35 A and exact wanted net receipt.
Never round a spend ceiling upward or a minimum receipt downward.

Automatic preflight before funding, listing, taking, and withdrawal requires
NUT-07/09/12/14 support, an active sat keyset, and clock skew at most 60 seconds.
The explicit `preflight <mint>` performs the same checks. No opt-in is required;
passing advertisement checks is not independent refund interoperability evidence.
`balance` reads wallet balance without submitting earlier authorizations.
Market commands (including `discover` and `cancel`) first recover existing
authorizations; do not treat them as read-only on a home with pending obligations.
`serve` continuously accepts trades and recovers obligations; keep it running while
locks are live. `recover` is a bounded single pass, not a replacement watcher;
see [recovery](recovery.md) for exit codes and states.

## Funding

The printed invoice is a request for payment, not proof of issuance. Confirm the
mint and amount, then let the human pay it once from their own wallet. Include any
external-wallet routing fee budget in the human's approval; this CLI cannot enforce
fees charged by another wallet. Never assume a real invoice is auto-paid because
a test mint behaved that way. Reuse the retained quote for the same mint/amount.

## Withdrawal: enforcement before execution

The verified interface caps each invoice at **100,000 sats** and the Lightning
fee reserve at **32 sats**. It has no user-selectable withdrawal fee flag and no
Cashu input-fee or total-debit cap. The invoice cap is not a gross debit cap.
Do **not** reuse the trade fee flag on withdrawal. Before invoking withdrawal,
establish the input-fee/total-debit bound and ensure it and the reserve fit the
human's numeric approval. A reserve-only cap does not cap Cashu input fees.
If the approved bounds cannot be established and enforced without executing the
payment, **stop and escalate; do not call withdrawal as a preview**. This is a
specific withdrawal limitation, not a missing real-money opt-in policy.

Require an amount-bearing, unexpired user-supplied BOLT11; do not generate an
invoice or choose a destination for the human. The receipt must distinguish the
invoice amount, input fees, Lightning reserve/actual fee, and returned change.
Only `done` means payment and change accounting have finished. Reconcile using
`recover`, not a new withdrawal. Even an UNPAID status snapshot after timeout is not
permission to release inputs or pay again.
