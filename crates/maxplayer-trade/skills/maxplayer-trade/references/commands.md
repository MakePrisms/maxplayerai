# Commands and monetary bounds

Every invocation except wallet-free `doctor` starts `maxplayer-trade --home <private-absolute-home>`.
Build the production binary as in the crate README's "Build and automated tests" section:
the crate is its own Cargo workspace, so `cargo build -p maxplayer-trade` from the repository
root fails; build it by manifest path, release profile, locked (no protoc needed for the binary).
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
doctor <mint> --mint-relay <url> [--json] [--timeout-seconds <seconds>]
preflight <mint>
balance <mint>
status [--json] [--check-sends]
fund <mint> --amount <sats> [--quote <quote-id>]
receive <mint> [--token-file <path>]
send (<mint> --amount <sats> --out <path> | --reclaim <send-id>) [--max-fees <sats>]
list --give-mint <mint> --give <net-sats> --want-mint <mint> --want <net-sats> --max-fees <sats> [--dry-run]
discover
serve
cancel <lot>
take <lot> --max-give <total-sats> --min-receive <net-sats> --max-fees <sats> [--dry-run]
withdraw <mint> --invoice <bolt11> [--max-debit <sats>]
recover
```

Use `-h` / `--help` at the root or on any application subcommand. The built-in
`help [COMMAND]` also shows root or command help. There is no version option.
`--home <HOME>` is required for operational commands; `--relay <RELAY>` is
repeatable. All command-specific flags in the signatures above are required except
`--timeout-seconds` (doctor: default 30 seconds), `--quote`, `--max-debit`, `--token-file` (stdin when omitted), `--dry-run`, `--json`, `--check-sends`,
and `--max-fees` (default 16 on list/take; on send it is required unless the mint's input fee is 0). `send` takes either `<mint> --amount --out` or `--reclaim <send-id>`. Positional mint arguments are
canonical URLs, or `nostr://<npub>` for a Nostr-reachable mint (hex or uppercase input is
canonicalized to the lowercase npub). `fund` and `withdraw` refuse `nostr://` mints before
any journal entry: the Maxplayer credits mint serves no NUT-04/05/20; `receive` is how credits
enter a home. A zero `--amount`, `--give`, `--want`, `--max-give` or `--max-debit` is refused
before anything else (no home, lock, relay or mint) with `amount must be at least 1 sat
(<flag> 0)`; `--min-receive 0` and `--max-fees 0` stay valid bounds. Lot arguments are public lot IDs; `--quote` is a saved funding
quote for the same mint and amount.
Never use a lab-feature binary for the human's funds.

## Doctor and admission diagnostics

`doctor <mint> --mint-relay <url> --json` makes one read-only info request to that
explicit nostr mint/relay pair. It needs no home and creates no seed or journal; no
market connection, keysets, swap, checkstate or restore probe. There are no scheduled
retransmissions (SDK authentication can resend the same event). `--timeout-seconds`
changes only this read-only budget, never monetary windows.

Doctor exits **0** for usable info (unknown compatibility warns), **1** for proven
incompatibility, **2** for an unsuccessful diagnostic. A missing relay ACK means
acceptance unknown, not unsent. An accepted request without a mint reply cannot
distinguish an offline, incompatible, unsubscribed mint or lost reply. Signed errors
and unsupported envelope versions are reported separately; error-body text is withheld.
A real reply above 5 seconds warns about slowness, never refuses on that basis.

List/take admission, including previews and serve IPC, reuse existing info/keysets
reads. Explicit missing capabilities refuse new effects: upgrade the mint sidecar
and repeat preflight. Missing version with otherwise passing checks produces
**WARNING: compatibility unknown**, not an invented historical minimum. The new gate
never blocks existing-trade recovery. Both mints must remain reachable throughout a
trade; info is no guarantee of later claims, refunds or spent-witness emission.

## Enforced monetary policy

- Asset identity is canonical mint URL plus unit `sat`; equal units do not make two
  issuers equivalent. Confirm URLs, not just display names. Real mints are HTTPS, or `nostr://<lowercase npub>`
  for the Maxplayer credits mint.
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
`serve`, without submission of earlier authorizations; neither makes network calls
except `status --check-sends` (read-only NUT-07 per sent token). `status`
prints one readable line per lot/swap/funding/withdrawal/receive/send, unresolved items
first (non-terminal and manual-recovery sends included, so reserved send inputs are visible), any `counterparty_unresponsive` warning, and the exit-code meanings;
`status --json` prints the unchanged raw public records for scripts.

`discover` is lock-free and read-only too: it reads relays only, with a throwaway
relay identity, never runs recovery, never takes the home lock, never creates or
touches the home or journal, and works while `serve` or `take` runs. Each listing
carries `expected_fees` per leg (`net`, `ppk`, `gross`, `claim_fee`) from the
mints' published keysets, or `"unknown"` when a mint did not answer within ~5 s
(discover never blocks on a mint). The taker's own lock fee depends on its inputs:
use `take --dry-run`. Relay union, EOSE and `timeout_or_partial` reporting are unchanged.

## Fee preview: `--dry-run`

`list ... --dry-run` and `take ... --dry-run` run the normal preflight, then print one
JSON object: `you_give` (keyset, ppk, net, gross, `lock_fee`, `claim_fee`, `debit`,
inputs that would be selected), `you_receive` (incoming leg's gross/claim fee, funded
by the counterparty), `max_total_debit` and `fees_total`. The numbers use the same
arithmetic and input selection as the real command against the current wallet and
the mints' published keysets. Nothing is journaled, reserved, locked, created or
published, and no quote request reaches the maker. Exit 0 when the real command
would pass the same bounds; exit 1 with the reason (e.g. `maximum give cap`,
`fee cap exceeded`, insufficient balance, or `cannot solve bounded fee-inclusive
split` for an amount with no exact fee-inclusive gross) otherwise. Run it before
asking for approval, then run the identical command without `--dry-run`. Balances
can change in between; the real command enforces your bounds again.

## Hand-off to a running serve

While `serve` owns the home, `list`, `take` and `cancel` hand their request to it
over `<home>/serve.sock` (mode 0600, same user only) and print serve's result with
the usual output and exit codes. serve executes it with exactly the bounds given;
it never fills in defaults. Use the same `--relay` set as serve or the request is
refused. A handed-off `take` prints its swap id and that serve is watching it, then
streams states until settlement; if the command is interrupted, serve keeps driving
the swap (check `status`). Without a running serve these commands behave as before.
`recover` and the money commands still need the home lock; stop serve first.
`cancel` first recovers existing authorizations when it runs without serve.
`serve` continuously accepts trades and recovers obligations; keep it running while
locks are live. `recover` is a bounded single pass, not a replacement watcher;
see [recovery](recovery.md) for exit codes and states.

**One process per home, no recover loops.** Run exactly one `serve` per home. Never run
parallel `recover` loops, a `recover` loop beside a `take`, or several serve/take processes
against the same mints: each process paces and backs off only its own requests, so parallel
processes multiply the load a rate-limited or blocking mint sees (a live run kept a 429 and
a TCP block alive for 9+ hours this way). If a mint is backing off, wait for serve; rerun
`recover` at most every few minutes, not in a tight loop.

## Mint backoff and pacing

Every request to a mint (HTTP or `nostr://`) passes a per-mint gate shared by all items
in one process:

- **Backoff:** on HTTP 429, 502, 503 or 504, a refused/reset connection or a timeout, that mint is
  not contacted again for 5 s, doubling per consecutive failure with 0-25 % jitter, up to
  10 min. A `Retry-After` header (seconds or an HTTP date) is honoured up to 15 min. Any
  other answer clears it; a failure long after a backoff ended starts again at 5 s.
  serve/recover log `mint <url> backing off <N>s (429|503|502/504|refused|timeout)` once per state
  change and `mint <url> answered; backoff cleared` when it ends.
- **Pacing (no backoff):** each request adds 6 s of per-mint debt; a recovery item starts
  only while that debt is at most 30 s ahead. This bounds only the recovery loop (serve
  tick, `recover`): there, at most ~1 request / 6 s (10/min) per mint per process, after a
  short burst. Advances driven by inbound trade messages, CDK wallet calls and other
  processes are not paced; only backoff bounds them. serve skips a paced item until a
  later tick; `recover` waits for it. Items take turns, oldest-served first.
- **What it never does:** a backed-off request is simply not sent and counts as "no
  answer" (exit 3 / deferred): journals, reservations and money-step order are unchanged,
  nothing is marked refused or failed.
- **Claims are never idle:** once a claim is possible (maker: preimage known, until the
  taker's refund locktime; taker: second lock validated, until its claim cutoff), the
  claim mint is retried once per 30 s round however long its backoff or `Retry-After`
  (one failed request per round at most), and every 10 s in the last 10 min before the
  deadline. Deadlines are judged on the mint's clock (its `info.time`; an unseen mint is
  assumed 2 min ahead). Only the claim mint: refund and own-mint requests keep normal
  backoff. Logs `a claim to it is possible: retrying it every 30s` / `final stretch`. The
  taker's claim is still never sent past its cutoff. The same rounds (`nostr://` mints
  only) cover the checks a claim depends on: the taker's check of the maker's lock (until
  its claim cutoff) and the maker's NUT-07 watch for the claim witness (until `long`).
- **Unanswered trade messages are kept:** a counterparty message (initial request,
  quote, lock, claim notice) whose check fails only because a mint did not answer is journaled and retried
  on every serve/recover tick (also after a restart) until it is handled, refused by
  the mint or by validation, superseded, or past its deadline. serve logs `trade message
  waits for its mint (no answer); retained`. Initial requests are bounded (64 globally,
  4 per peer), persisted before retry and rechecked for digest, terms, availability
  and expiry. Matching legacy timeout rejections are reopened without discarding
  their digest protection. Validation failures are still rejected.
- `fund` polls its quote every 5 s (6 polls), not every second.

## Funding

`fund <mint> --amount 0` is refused with `amount must be at least 1 sat (--amount 0)`;
above 100,000 sats it is `funding cap exceeds 100,000 sats`.

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
mint past the shared cap, or (`nostr://` mint) whose swap would not fit one NIP-44
request (error names NIP-44; exit 1, nothing journaled, cap untouched). Funding intents plus charged receives (gross) share one 100,000-sat cap per mint per home; `refused`/`already_spent` receives do not count; `prepared`/`submitted`/`done`/`quarantined` ones do. A quarantined receive is not spendable but
still occupies the cap. Common preflight runs, then NUT-07 must report every proof UNSPENT.

One swap into fresh home-owned outputs; input proofs, blinded outputs and their
secrets are journaled before the POST (not the token text). The mint input fee is deducted: `net = amount - fee`.
Only DLEQ-verified result proofs are credited. Output is one JSON line with
`receive` (attempt id), `mint`, `state`, `amount`, `fee`, `net`, `credited`; never
the token or proofs; also `settled` and `refusal_code`. Repeating the same token
(any encoding) resumes the same attempt and only replays its identical swap; a
second output set is never created. Exit 0 `done` and released; 3 unresolved
(`prepared`/`submitted`, `done` with release outstanding, or the state could not
be journaled); 1 pre-journal refusal, `refused` or
`already_spent`; 4 `quarantined`. Only a definitive mint NUT error (allowlisted code
such as 11001/11005/12001-12003; HTTPS: a 400 with JSON `code` + `detail`; `nostr://`:
a numeric NUT `code` in the mint's reply) yields `refused`; `50000`, 11002, 11003
(outputs already signed: evidence of a landing), 11004, unknown codes, any other 400
and every nostr transport error (`expired`, `bad_request`, `rate_limited`, timeout,
oversized request) stay `submitted` (exit 3) and the identical swap is replayed.
`refused` is FINAL: no command retries it, the token was not imported and can be
redeemed elsewhere, and it does not occupy the cap.

## Send (export a token)

`send <mint> --amount <sats> --out <path> --max-fees <sats>` writes one single-mint V4
token for exactly `<amount>` sats to a NEW file (mode 0600, created exclusively; an
existing path, including a symlink, is refused). The token is never printed and never
accepted through argv. Only ordinary unreserved balance is spent: locked HTLC/P2PK
proofs and proofs reserved by a swap, withdrawal, receive or send are excluded. The
mint input fee is paid by the sender; `--max-fees` is required unless it is 0. Refused
**before anything is journaled**: amount 0 or above **100,000 sats**, insufficient balance,
fee above `--max-fees`, existing `--out`, failed preflight or non-UNSPENT inputs.

One swap of the selected inputs into the exact send outputs plus change; inputs,
outputs and secrets are journaled before the POST. Ambiguity → NUT-09 restore of the
same outputs and identical replay only. The file is written only after the swap is
definitive and every output verifies DLEQ; the token is kept in the private journal so
`recover` rewrites the same token after a crash. Rerunning the same command with the
same `--out` resumes that attempt, or rewrites the same journaled token if the file was
moved or deleted (only if NUT-07 shows every sent proof UNSPENT; SPENT, PENDING or an
unreachable mint exit 1, "already (partially) redeemed or pending; choose another --out"; a rewrite prints a stderr notice
and `"rewritten": true`); it never sends twice. A `reclaiming` send's `--out` is
refused: run `send --reclaim <id>`. A used `--out` (other terms, reclaimed/redeemed
or manual-recovery send) is refused; only a `refused` send frees its path. Coin
selection skips inputs of any unfinished journaled send, withdrawal, listing or swap.
A send the recipient's input fee would consume is refused before the journal. Output: one JSON line with
`send` (attempt id), `mint`, `state`, `amount`, `fee`. Exit 0 `sent`; 3 unresolved;
1 refusal, `refused` or `redeemed`; 4 manual recovery.

Until the recipient redeems it, `send --reclaim <send-id>` swaps the still-UNSPENT
proofs back (same journaling; fee needs `--max-fees` unless 0). It also works on a
`swapped` send whose token file could not be placed. Reclaim exits 0 `reclaimed`; 3
`reclaiming`; 1 when refused or partially redeemed meanwhile (state stays `sent`,
token still outstanding: run `send --reclaim <id>` again) or `redeemed` (also when
the recipient won the race before any POST of our leg); 4 `reclaim_unresolved` (our
leg POSTed, with an ambiguous reply or a refusal while every input is SPENT, and its
outputs absent on two passes; secrets kept) or `reclaim_quarantined`. If they are already
SPENT it reports "redeemed by recipient" and refunds nothing. `status --check-sends`
asks each mint (NUT-07) whether sent tokens were redeemed; plain `status` is offline.

On a `nostr://` mint send, reclaim and `--check-sends` are supported (no NUT-04/05
needed). A swap over 128 proofs or the NIP-44 limit is refused **before anything is
journaled or reserved** (exit 1; send a smaller amount); for a reclaim the token stays
`sent`. The token names `nostr://npub…`: only a nostr-capable wallet (e.g.
`maxplayer-trade receive`) can redeem it, not a generic Cashu wallet; `send --reclaim`
takes it back. Keep the same `--mint-relay` set for reclaim, `recover` and
`--check-sends`; with the wrong relays they report `unknown`/retain the attempt.

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

Refused before any mint request, reservation or journal entry (exit 1): a
non-mainnet invoice (`invoice is for testnet; this mint pays Bitcoin mainnet invoices;
nothing was journaled or sent`), an invoice already expired (`invoice expired at
<UTC>; nothing was journaled or sent`) or expiring within 30 seconds (`invoice expires
at <UTC>, within 30 s; …`), an amountless, zero-amount or over-cap invoice. Resuming an
invoice that is already journaled skips these checks and continues the saved authorization.

Require an amount-bearing, unexpired user-supplied BOLT11; do not generate an
invoice or choose a destination for the human. The receipt must distinguish the
invoice amount, input fees, Lightning reserve/actual fee, and returned change.
Only `done` means payment and change accounting have finished. Reconcile using
`recover`, not a new withdrawal. Even an UNPAID status snapshot after timeout is not
permission to release inputs or pay again.
