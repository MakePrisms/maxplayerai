# Commands and monetary bounds

Every invocation starts `maxplayer-trade --home <private-absolute-home>`.
Optional repeated global `--relay <url>` goes before the subcommand; preserve the
same relay set during recovery. Use the binary's default relays unless deliberately
configured; defaults at verified base 1929972 are `wss://nos.lol`,
`wss://relay.primal.net`, and `wss://offchain.pub`. These source defaults are also listed in root help. ACKed copies are not resent; retries use exponential
backoff with jitter, at most 12 attempts per event/relay and a 24-hour age cutoff.
Rate limiting pauses a relay for at least five minutes.
A separate repeated global `--mint-relay <url>` (max 8; `wss://`, `ws://` only on loopback)
carries `nostr://` mint requests only (kinds 23410/23411), never market traffic; it defaults
to `wss://relay.maxplayer.ai`, `wss://relay.ditto.pub` and `wss://nostr-pub.wellorder.net`, the
default credits sidecar's own list. `relay.maxplayer.ai` is allowed as a `--mint-relay` only; as a
`--relay` (market) it is refused. Preserve the same mint-relay set during recovery of a `nostr://` trade.
Relay ACK is not proof of stored readback or trade success. Respect backoff; do not
restart loops or generate fresh identities to evade rate limits.

Canonical command signatures (placeholders are not literal values):

```text
preflight <mint>
balance <mint>
status
fund <mint> --amount <sats> [--quote <quote-id>]
receive <mint> [--token-file <path>]
list --give-mint <mint> --give <net-sats> --want-mint <mint> --want <net-sats> --max-fees <sats>
discover
serve
cancel <lot>
take <lot> --max-give <total-sats> --min-receive <net-sats> --max-fees <sats>
withdraw <mint> --invoice <bolt11> [--max-debit <sats>]
recover
```

Use `-h` / `--help` at the root or on any application subcommand. The built-in
`help [COMMAND]` also shows root or command help. There is no version option.
`--home <HOME>` is required for operational commands; `--relay <RELAY>` is
repeatable. All command-specific flags in the signatures above are required except
`--quote`, `--max-debit`, `--token-file` (stdin when omitted), and `--max-fees` (default 16 on list/take). Positional mint arguments are
canonical URLs, or `nostr://<npub>` for a Nostr-reachable mint (hex or uppercase input is
canonicalized to the lowercase npub). `fund` and `withdraw` refuse `nostr://` mints before
any journal entry: the Maxplayer credits mint serves no NUT-04/05/20; `receive` is how credits
enter a home. Lot arguments are public lot IDs; `--quote` is a saved funding
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
  for repeated trades or a guarantee against future refund fees. It also limits the
  incoming sender-funded claim fee at admission, in that incoming asset; after
  admission claims use the pinned quote fee, not the receiver’s outgoing budget. Refund/recovery may
  incur mint fees; disclose this rather than promising a fully refunded balance.
- No platform/trade fee. Do not equate no platform fee with no mint/Lightning fees.
- Hard cap: 100,000 sats gross per lock. Funding intents plus charged receives (gross) share one 100,000-sat cap per mint per home; `refused`/`already_spent` receives do not count; `prepared`/`submitted`/`done`/`quarantined` ones do.
  Each withdrawal invoice is also capped at 100,000 sats (millisatoshis rounded up
  to sats). Funding counts all retained intents, including pending ones.
  None of these limits is permission to spend that amount. Never split operations or rotate homes
  to bypass limits. Withdrawing does not reset this cumulative cap.

Example: a lot gives 32 A for 24 B. If the human approves **at most 27 B total**,
**at least 32 A net**, and **3 B fee sats**, use `take <lot> --max-give 27
--min-receive 32 --max-fees 3`. If they cap total spending at 24, use 24 instead of
27 and accept refusal when fees make it impossible. For a seller giving 32 A with
3 A fee sats, confirm a maximum debit of 35 A and exact wanted net receipt. Also
confirm that the incoming B claim fee fits the cap; do not blindly use 3 across
unequal-fee mints. A larger cap needs the human’s approval, not an automatic retry.
Never round a spend ceiling upward or a minimum receipt downward.

Automatic preflight before funding, listing, taking, and withdrawal requires
NUT-07/09/11/12/14 support, an active sat keyset, and clock skew at most 60 seconds.
Funding additionally requires enabled NUT-04 bolt11/sat and NUT-20; withdrawal
requires enabled NUT-05 bolt11/sat. The explicit `preflight <mint>` checks the
common trading capabilities. Diagnostic preflight output goes to stderr. No opt-in is required;
passing advertisement checks is not independent refund interoperability evidence.
`balance` and `status` are lock-free read-only SQLite snapshots, available during
`serve`, without network calls or submission of earlier authorizations.
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

## Receive

`receive <mint> --token-file <path>` (or the token on stdin; never argv) imports one
Cashu token. Refused **before anything is journaled**: a token whose mint is not
exactly `<mint>` after canonicalization, multi-mint tokens, non-sat units, P2PK/HTLC
or other NUT-10 locked proofs, more than 128 or duplicate proofs, an invalid incoming
DLEQ (when present), a token above **100,000 sats**, or one that would push this
mint past the shared cap. Funding intents plus charged receives (gross) share one 100,000-sat cap per mint per home; `refused`/`already_spent` receives do not count; `prepared`/`submitted`/`done`/`quarantined` ones do. A quarantined receive is not spendable but
still occupies the cap. Common preflight runs, then NUT-07 must report every proof UNSPENT.

One swap into fresh home-owned outputs; token, inputs, outputs and secrets are
journaled before the POST. The mint input fee is deducted: `net = amount - fee`.
Only DLEQ-verified result proofs are credited. Output is one JSON line with
`receive` (attempt id), `mint`, `state`, `amount`, `fee`, `net`, `credited`; never
the token or proofs. Repeating the same token (any encoding) resumes the same
attempt and never swaps twice. Exit 0 `done`; 3 unresolved (`prepared`/`submitted`,
or the state could not be journaled); 1 pre-journal refusal, `refused` or
`already_spent`; 4 `quarantined`. Only a parsed mint NUT error (HTTP 400 with JSON
`code` and `detail`) yields `refused`; any other 400 or transport refusal stays
`submitted` (exit 3) and the identical swap is replayed.

## Withdrawal: enforcement before execution

Each invoice is capped at **100,000 sats**. The Lightning reserve ceiling is
**max(32 sats, ceil(invoice sats × 2%))**: a 100,000-sat invoice may reserve 2,000.
Use `withdraw <mint> --invoice <bolt11> --max-debit <approved-sats>` to enforce
**invoice + input fee + quoted reserve** before reserving inputs/POST. Selected
proof face value can be larger; the excess is journaled change, not extra fee
permission. This option is optional in the CLI but required by this skill whenever
the human supplies a total debit bound. The trade fee flag does not apply.
Over-ceiling, invalid, lost, near-expiry, or unfundable quotes become terminal
`refused` without a payment POST. A refusal can be retried explicitly with the
same invoice; it cannot later pay merely because recovery runs or funds arrive.
A first POST requires more than 60 seconds of quote lifetime. After a submitted
request becomes ambiguous, recovery replays exactly the saved inputs/outputs/quote,
never a replacement authorization. Deduplication uses payment hash across mints.

Require an amount-bearing, unexpired user-supplied BOLT11; do not generate an
invoice or choose a destination for the human. The receipt must distinguish the
invoice amount, input fees, Lightning reserve/actual fee, and returned change.
Only `done` means payment and change accounting have finished. Reconcile using
`recover`, not a new withdrawal. Even an UNPAID status snapshot after timeout is not
permission to release inputs or pay again.
