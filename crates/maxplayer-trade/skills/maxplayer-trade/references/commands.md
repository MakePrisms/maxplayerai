# Commands and monetary bounds

Every invocation trades from a maxplayer home: `--home <home>` (a directory with `key` /
`config.toml`), else `$MAXPLAYER_HOME`, else `~/.maxplayer`. It signs with the home `key` and
pays from the home's wallet (a seller's too); only the home's mints (default, `extra_mints` and
the seller's `accepted_mints`) that pass its real-mint fence are traded; trade state is in `<home>/trade/`. A legacy standalone home (`wallet.seed`/`trade.key`)
is drain-only: new `list`/`take` are refused (crate README, "Home and wallet").
Money in and out of the home's wallet is `maxplayer wallet` ([below](#money-in-and-out-maxplayer-wallet)).
Every command below is `maxplayer trade <command>`, part of the `maxplayer` CLI. If
`maxplayer trade --help` fails, the installed `maxplayer` predates it: upgrade it (or build it as
in the crate README's "Build and automated tests"). Delete any old standalone `maxplayer-trade`.
Optional repeated global `--relay <url>` goes before the subcommand; preserve the
same relay set during recovery. Use the binary's default relays unless deliberately
configured; the defaults are `wss://relay.maxplayer.ai` first, then `wss://nos.lol`,
`wss://relay.primal.net` and `wss://offchain.pub` as fallbacks. `relay.maxplayer.ai` stores lots
and statuses only from signed-in relay members; a non-member's copy is refused there and the
public relays carry it. To make the home's key a member, link it with `maxplayer account
link` (crate README, "Default market relays"). These source defaults are also listed in root help. ACKed copies are not resent; retries use exponential
backoff with jitter, at most 12 attempts per event/relay and a 24-hour age cutoff.
Rate limiting pauses a relay for at least five minutes.
A separate repeated global `--mint-relay <url>` (max 8; `wss://` only, `ws://` is refused)
carries `nostr://` mint requests only (kinds 23410/23411), never market traffic; it defaults
to `wss://relay.maxplayer.ai`, `wss://relay.ditto.pub` and `wss://nostr-pub.wellorder.net`, the
default credits sidecar's own list. `relay.maxplayer.ai` is in both lists; the market and mint
connections stay separate. Preserve the same mint-relay set during recovery of a `nostr://` trade.
Every `<mint>` below is `https://…` or `nostr://<npub>`. Plain `http://` mints are refused
everywhere, localhost included ("http:// mints are not supported; use https:// or nostr://"), and
lots naming one are never shown or taken; loopback HTTP fixtures exist only in test builds.
Relay ACK is not proof of stored readback or trade success. Respect backoff; do not
restart loops or generate fresh identities to evade rate limits.

Canonical command signatures (placeholders are not literal values):

```text
doctor <mint> --mint-relay <url> [--json] [--timeout-seconds <seconds>]
preflight <mint>
balance <mint>
status [--json] [--check-sends]
send (<mint> --amount <sats> --out <path> | --reclaim <send-id>) [--max-fees <sats>]   # new sends: legacy homes only
list --give-mint <mint> --give <net-sats> --want-mint <mint> --want <net-sats> --max-fees <sats> [--dry-run]
discover [--give-mint <mint>] [--want-mint <mint>]
serve
cancel <lot>
take <lot> --max-give <total-sats> --min-receive <net-sats> --max-fees <sats> [--dry-run]
recover
```

Use `-h` / `--help` at the root or on any application subcommand. The built-in
`help [COMMAND]` also shows root or command help. There is no version option.
`--home <HOME>` defaults to the maxplayer home; `--relay <RELAY>` is
repeatable. All command-specific flags in the signatures above are required except
`--timeout-seconds` (doctor: default 30 seconds), `--give-mint`/`--want-mint` on discover, `--dry-run`, `--json`, `--check-sends`,
and `--max-fees` (default 16 on list/take; on send it is required unless the mint's input fee is 0). `send` takes either `<mint> --amount --out` or `--reclaim <send-id>`. Positional mint arguments are
canonical URLs, or `nostr://<npub>` for a Nostr-reachable mint (hex or uppercase input is
canonicalized to the lowercase npub). A zero `--amount`, `--give`, `--want` or `--max-give` is refused
before anything else (no home, lock, relay or mint) with `amount must be at least 1 sat
(<flag> 0)`; `--min-receive 0` and `--max-fees 0` stay valid bounds. Lot arguments are public lot IDs.
Never use a lab-feature binary for the human's funds.

## Doctor and admission diagnostics

`doctor <mint> --mint-relay <url> --json` makes one read-only info request to that
explicit nostr mint/relay pair. It needs no home and creates no seed or journal. With a
global `--home` it signs as that home's key (a legacy home: its existing `trade.key`; never
created); without one,
or when the home has no key yet, it signs with a throwaway key, which a sign-in-required relay
such as `relay.maxplayer.ai` refuses (`signer` in the JSON says which). No
market connection, keysets, swap, checkstate or restore probe. It re-sends the identical
event every 5 s, like trade admission. `--timeout-seconds`
changes only this read-only budget, never monetary windows.

Doctor exits **0** for usable info (unknown compatibility warns), **1** for proven
incompatibility, **2** for an unsuccessful diagnostic. A missing relay ACK means
acceptance unknown, not unsent. An accepted request without a mint reply cannot
distinguish an offline, incompatible, unsubscribed mint or lost reply. Signed errors
are reported separately; error-body text is withheld. A reply in an unsupported envelope
version is ignored (exit 1 once the budget ends without a usable reply).
A real reply above 5 seconds warns about slowness, never refuses on that basis.

List/take admission, including previews and serve IPC, reuse existing info/keysets
reads. A recognised schema-1 block missing a required capability refuses new effects:
upgrade the mint sidecar and repeat preflight. A missing version, or a capability schema
this tool does not recognise, produces **WARNING: compatibility unknown**, not a refusal
or an invented historical minimum. The new gate
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
- Hard cap: 100,000 sats gross per lock, per trade `send`, and per `maxplayer wallet send` or
  `wallet melt`. None of these limits is permission to spend that amount. Never split
  operations or rotate homes to bypass limits.

Example: a lot gives 32 A for 24 B. If the human approves **at most 27 B total**,
**at least 32 A net**, and **3 B fee sats**, use `take <lot> --max-give 27
--min-receive 32 --max-fees 3`. If they cap total spending at 24, use 24 instead of
27 and accept refusal when fees make it impossible. For a seller giving 32 A with
3 A fee sats, confirm a maximum debit of 35 A and exact wanted net receipt. Also
confirm that the incoming B claim fee fits the cap; do not blindly use 3 across
unequal-fee mints. A larger cap needs the human’s approval, not an automatic retry.
Never round a spend ceiling upward or a minimum receipt downward.

Automatic preflight before listing, taking and sending requires
a reachable mint, NUT-07/09/11/12/14 support, an active sat keyset, and clock skew at most 60 seconds.
The explicit `preflight <mint>` checks the
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

`discover --give-mint <mint> --want-mint <mint>` keeps only lots whose maker gives the
first mint and wants the second (same direction as `list`); either filter alone is allowed.
With both, listings are sorted cheapest first for a taker (fewest wanted sats per given
sat, then the larger lot), so the first entry is the market's best price on that pair.
Before listing A→B, `discover --give-mint A --want-mint B` shows the lots you would
compete with and `discover --give-mint B --want-mint A` shows lots you could take
instead. Mints are canonicalized like positional mints; a malformed or identical pair is
refused before any relay traffic.

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
it never fills in defaults. serve refuses a request whose relays differ from its own
where the command uses them: `list`, `take` and `cancel` need exactly serve's `--relay`
list (same values in the same order); `take`, `cancel`, and a `list` with a
`nostr://` mint need the same `--mint-relay` set (compared as a set: spelling and order do
not matter). A refused hand-off queues and executes nothing. serve is never asked to run a
command on relays you did not give. The client sends only to a serve socket and peer owned by
its own user. A handed-off `take` prints its swap id and that serve is watching it, then
streams states until settlement; if the command is interrupted, serve keeps driving
the swap (check `status`). Without a running serve these commands behave as before.
The buyer daemon and the seller host serve on a home that trades (`[trade] serve`, default on;
its `[trade] relays` / `mint_relays` are also the commands' defaults), so a running daemon is
usually the serve that answers; `serve` itself then refuses ("trade serve is already running").
`recover` and `send` (drain or `--reclaim`) still need the home lock (`home is already in
use` while serve runs, naming the serve); stop serve first (a hosted serve: `[trade] serve =
false` and restart the daemon). serve recovers continuously, so `recover` is not needed meanwhile. `maxplayer wallet` commands never need it. Protocol
version 5: a client of another version is refused with a version mismatch, and an old
standalone `maxplayer-trade` binary is told to delete itself.
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

## Serve lifecycle, stopping and progress lines

serve writes one JSON object per line to stderr; times are UTC (`2026-10-12T20:39:58Z`).

- **Start:** `{"serve":"started","at":…,"version":…,"commit":<sha or unknown>,"pid":…,"home":…,"relays":[…],"mint_relays":[…]}`.
- **Stop:** the first SIGTERM or SIGINT prints `"serve":"stopping"`. serve then admits nothing
  new (no inbox message, hand-off request or recovery tick is started; a queued hand-off is
  answered `serve is stopping; request not started`, exit 1). The step already running may
  finish for up to 30 s. A second signal, or the end of the grace, cancels that step at its
  next wait (`"serve":"stop_forced"`). Every money step is journaled before it takes effect,
  so a cancelled step stays ambiguous in the journal and is replayed on the next start;
  nothing is released or marked refused.
- **End:** exactly one `{"serve":"exited","reason":"signal"|"error"|"inbox_closed","exit":N,"unfinished":{…}}`
  where catchable. `unfinished` lists non-terminal swaps (id, role, state, earliest refund as
  UTC plus time remaining), unserved lots and open funding/withdrawal/receive/send ids. Exit
  codes: **5** stopped by a signal (graceful or forced); 1 error or closed market inbox (every
  relay client stopped; restart serve); 3 and 4 unchanged. The socket file is removed, which
  says nothing about settlement: read `unfinished`.
- **Not catchable:** SIGKILL, OOM kills and power loss leave no final line; the next start then
  logs `"serve_socket":"stale_removed"`. Run serve under a supervisor (systemd, a shell loop)
  that records exit status and signal and restarts it while locks are live, e.g. systemd
  `Restart=on-failure`, `KillSignal=SIGTERM`, `TimeoutStopSec=45` (above the 30 s grace).
  Exit 5 after an intended stop is not a failure.
- **Swap progress** (display only, never fed back into any decision):
  `{"progress":"swap_state",…}` after each journaled state change, and
  `{"progress":"mint_attempt","at":…,"swap_id":…,"mint":…,"op":…,"attempt":N,"outcome":"timeout|no_answer|backing_off|refused|error|answered|started","elapsed":…,"next_retry":…,"claim_deadline":…,"claim_deadline_in":…,"refund_earliest":…,"refund_earliest_in":…}`
  once a swap's mint stops answering (or answers slower than 5 s). `attempt` counts requests
  since that mint last answered. The claim deadline is judged on the mint clock
  (`claim_deadline_clock` says which skew was used); `refund_earliest` is eligibility on this
  host's clock, not a guaranteed refund. Rate-limited: an outcome change prints at once, a
  repeat at most once per 60 s per swap and mint (`suppressed` counts what was held back).
  Lines carry outcome classes, NUT codes and redacted outer messages, never tokens, proofs or
  mint bodies. A `take` handed to serve receives its own swap's lines on stderr; stdout is
  unchanged. Backoff lines (below) are prefixed with the UTC time and show deadlines as UTC.

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
- **Unanswered trade messages are kept:** a counterparty message (quote, lock, claim
  notice) whose check fails only because a mint did not answer is journaled and retried
  on every serve/recover tick (also after a restart) until it is handled, refused by
  the mint or by validation, superseded, or past its deadline. serve logs `trade message
  waits for its mint (no answer); retained`. Validation failures are still rejected.
  An initial request that failed only because a mint did not answer is not queued and
  not recorded as rejected: a later delivery of the same event is handled fresh.
- `fund` polls its quote every 5 s (6 polls), not every second.

## Money in and out: `maxplayer wallet`

Funding, receiving, sending and paying invoices use the home's wallet commands, with the same
`--home`. `maxplayer trade fund`, `withdraw` and `receive` print the command to use and exit 1
without touching anything; a new `trade send` on a maxplayer home does the same.

```text
maxplayer wallet mint <sats> --mint <mint>              # BOLT11 to pay; then wallet mint-complete <quote-id>
maxplayer wallet receive --token-file <path> [--add-mint]
maxplayer wallet send <sats> --mint <mint> --out <new-file>      # max 100,000; file 0600, never overwritten
maxplayer wallet melt <bolt11> --max-debit <sats> --mint <mint>  # max 100,000
maxplayer wallet balance
```

An invoice is a request for payment, not proof of issuance: the human pays it once from their
own wallet. `wallet melt --max-debit` refuses before spending when the invoice plus the mint's
quoted fee reserve exceeds it. A token written by `wallet send --out` that the recipient has not
redeemed comes back with `maxplayer wallet receive --token-file <that file>`. Funding,
withdrawal and receive records already in the trade journal are still finished by `serve` and
`recover` ([recovery](recovery.md)).

## Send (export a token)

A new `send` drains a legacy standalone home; on a maxplayer home use `maxplayer wallet send`.
`send --reclaim <id>` and `status --check-sends` work on both for sends trade made.
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
`recover` rewrites the same token after a crash. On a legacy home, rerunning the same command
with the same `--out` resumes that attempt, or rewrites the same journaled token if the file was
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
token still outstanding; `serve`/`recover` retry it with the same `--max-fees` after
1 min, 10 min and 1 h, then stop; `send --reclaim <id>` retries now and restarts the count) or `redeemed` (also when
the recipient won the race before any POST of our leg); 4 `reclaim_unresolved` (our
leg POSTed, with an ambiguous reply or a refusal while every input is SPENT, and its
outputs absent on two passes; secrets kept) or `reclaim_quarantined`. If they are already
SPENT it reports "redeemed by recipient" and refunds nothing. `status --check-sends`
asks each mint (NUT-07) whether sent tokens were redeemed; plain `status` is offline.

On a `nostr://` mint send, reclaim and `--check-sends` are supported (no NUT-04/05
needed). A swap over 128 proofs or the NIP-44 limit is refused **before anything is
journaled or reserved** (exit 1; send a smaller amount); for a reclaim the token stays
`sent`. The token names `nostr://npub…`: only a nostr-capable wallet (e.g.
`maxplayer wallet receive --add-mint`) can redeem it, not a generic Cashu wallet; `send --reclaim`
takes it back. Keep the same `--mint-relay` set for reclaim, `recover` and
`--check-sends`; with the wrong relays they report `unknown`/retain the attempt.
