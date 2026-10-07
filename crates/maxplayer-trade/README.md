# maxplayer-trade — standalone fake-money Cashu trades

A fixed-lot CLI, independent of jobs, the maxplayer daemon, core, and `relay.maxplayer.ai`.
**A live 32-for-24 trade completed on 2026-10-07**, including both claims, NUT-07 witness
recovery, fee-inclusive net delivery, and a signed `sold` status. This is a test-money
prototype, not a production deployment or an unconditional atomicity guarantee.

## Implemented

- `list`, `discover`, `cancel`, `serve`, `take`, `recover`, `preflight`, `fund`, `balance`.
- Immutable signed 3410 lots (24 hours), hash-chained 3411 statuses, signature/content/tag
  validation, fork/gap quarantine, no terminal reopening. Every configured relay is queried;
  discovery unions their results. No EOSE is an error, not an empty market; partial relay
  failures are reported separately. Every publication is attempted at every configured relay.
- NIP-44 v2 encrypted 23412 negotiation, NIP-42 authentication, raw relay-message reception
  (retries are not suppressed by SDK event dedup), persisted signed outbox and request digests.
  Conflicting request IDs are rejected; quote rejection outcomes and completed swaps are retained.
- Quote binds immutable price/assets, net/gross/debits/fees/keysets, both identities, unique
  per-swap receive/refund keys, hash, and deadlines. Hold: 60 seconds; one open quote per taker,
  four per maker. `--max-give` caps total outgoing debit; `--min-receive` caps net receipt;
  `--max-fees` defaults to 16 in the relevant asset. No platform/trade fees.
- Taker generates the secret and locks **first**, for 60 minutes. Maker verifies the first
  lock before locking **second**, for 15 minutes. Both parties preflight both mints (reachable
  NUT-07/14, reported clock within 60 seconds), keysets and fees before locking. Each received
  lock must have exact hash/keys/thresholds/SIG_INPUTS/deadline, unique proofs, DLEQ, exact
  net-after-input-fee value, and fresh UNSPENT state. Asset identity is **mint URL + unit**;
  both assets can be `sat`. Taker refuses to initiate a claim with less than three minutes left.
- Maker settlement reads the preimage from NUT-07 witnesses on **its own outgoing proofs**;
  the taker's encrypted notice is only an optimization. Maker publishes sold after claiming
  the incoming leg. Locked proofs are never counted as ordinary wallet balance.
- Private SQLite journal using CDK's KV API; exact inputs, blinded outputs, secrets/blinding
  factors, signed witnesses, results and send deadlines are persisted before mint effects.
  Recovery restores the same outputs and never creates a replacement swap on a timeout alone.
  Concrete proofs are reserved in the CDK wallet, not just subtracted in an order-book counter.
  A home-wide process lock plus durable reservation intents cover interrupted row-by-row CDK
  reservations. Partial/missing restore evidence remains held, not guessed safe.
- Refunds are real signed swaps with an explicit HTLC witness `preimage: ""`, using the refund
  key after strict locktime plus clock margin. Maker recovery polls every three seconds and
  refunds after its short deadline +60 seconds when the mint is reachable. A claim that wins
  the refund race is reconciled through its witness. Refund outputs survive a process exit.
- `recover` resumes existing authorizations until terminal; it does not admit new requests.
  `serve` continuously accepts quotes and recovers swaps. **Keep a watcher running while funds
  are locked.** Stop it before another command uses the same home. Do not delete a trade home.

The only permitted mints are `https://testnut.cashudevkit.org`, `https://testnut.cashu.space`,
and loopback HTTP(S) mints. **There is no real-money override.** HTTP redirects are disabled.
Default relays: `wss://relay.ditto.pub`, `wss://relay.damus.io`; repeat global `--relay URL`
to configure alternatives, including `wss://nostr-pub.wellorder.net`. The production relay is
explicitly forbidden. No production relay writes were made.

## Build and automated tests

This crate is its **own Cargo workspace**, with pinned CDK/Cashu 0.17.2 and Nostr SDK 0.44.1.
Do not add it to the root workspace. From the repository root:

```sh
cargo build --manifest-path crates/maxplayer-trade/Cargo.toml --locked --bins --examples
# Tests build actual CDK mints, so protoc is required (available in the Nix devshell).
export PROTOC="$(find /nix/store -maxdepth 3 -path '*/bin/protoc' -print -quit)"
cargo test --manifest-path crates/maxplayer-trade/Cargo.toml --locked -- --test-threads=1
TRADE_LAB_SECONDS=1 cargo test --manifest-path crates/maxplayer-trade/Cargo.toml \
  --locked --features lab -- --test-threads=1
# Rebuild the production executable after the lab build.
cargo build --manifest-path crates/maxplayer-trade/Cargo.toml --locked --bin maxplayer-trade
```

Long runs should be backgrounded once, redirected to a log and awaited through that process;
do not pipe a build through tail/grep or start duplicate watchers. If cargo is unavailable,
wrap the command with `nix develop --extra-experimental-features 'nix-command flakes' --command`.

`lab` is compile-time-only test instrumentation. `TRADE_LAB_SECONDS=1` uses 24/8-second locks,
2-second claim cutoff and 1-second refund margin, **only for 127.0.0.1 mints**. Production builds
ignore these test settings and retain 3600/900/180/60. Crash injection exits the subprocess at
explicit pre-effect or post-mint/pre-wallet-commit boundaries. No real-money setting exists.

| Suite | Default passed/failed | Lab passed/failed |
|---|---:|---:|
| Protocol primitives | 21 / 0 | 21 / 0 |
| CLI privacy, lock, hard fence | 4 / 0 | 4 / 0 |
| Two-mint integration/recovery | 6 / 0 | 11 / 0 |
| Pinned-CDK orphan-quote reproducer | 1 / 0 | 1 / 0 |
| Relay union/unreachable/production fence | 3 / 0 | 3 / 0 |

Integration tests run two in-process **CDK 0.17.2 FakeWallet mints with distinct mint keys**,
both unit `sat`, SQLite wallets, and a local Nostr relay (same relay-builder used by
`crates/maxplayer-mint/examples/local_relay.rs`). They exercise real mint HTTP operations and
NIP-44 negotiation, not mocked successful swaps:

- list → discover → take → both claims → sold, zero and nonzero `input_fee_ppk`, exact balances;
- sell-back direction with exact fee-inclusive balances;
- maker never locks → taker refund; taker never claims → prompt maker refund;
- maker offline at claim → NUT-07-only preimage recovery;
- subprocess exit after a committed mint swap → journal recovery, no double debit;
- subprocess exit after refund → exact output recovery;
- late claim wins against a prepared refund → maker obtains witness and completes;
- wrong hash/key/deadline/net, forged/missing DLEQ, duplicate proofs rejected;
- interrupted quote-expiry index update recovers without releasing listing backing;
- overlisting and cancelled lots rejected; primitive tests also reject expired/tampered lots;
- lot and status split across two relays still discovered through the union; unreachable is
  distinguished from a real empty EOSE response.

Root CI does not run this independent workspace. These local counts are separate from root CI.

## Rerunnable public-relay trade

Use **fresh homes**. These commands use fake auto-paid testnut quotes; they never pay invoices.
The only public data is signed listing/status metadata and encrypted negotiation events.

```sh
set -eu
umask 077
TRADE="$PWD/crates/maxplayer-trade/target/debug/maxplayer-trade"
RUN="$(mktemp -d /tmp/maxplayer-trade-live.XXXXXX)"
A=https://testnut.cashudevkit.org
B=https://testnut.cashu.space
"$TRADE" --home "$RUN/maker" fund "$A" --amount 128
"$TRADE" --home "$RUN/taker" fund "$B" --amount 128
"$TRADE" --home "$RUN/maker" list --give-mint "$A" --give 32 \
  --want-mint "$B" --want 24 > "$RUN/list.jsonl"
LOT="$(python3 -c 'import json,sys; print([json.loads(l)["lot_id"] for l in sys.stdin if "lot_id" in json.loads(l)][-1])' < "$RUN/list.jsonl")"
"$TRADE" --home "$RUN/maker" serve > "$RUN/maker.log" 2>&1 &
MAKER_PID=$!
"$TRADE" --home "$RUN/taker" discover
"$TRADE" --home "$RUN/taker" take "$LOT" --max-give 40 --min-receive 32
# Only stop the maker after both sides are complete; otherwise keep recovery running.
kill "$MAKER_PID"
wait "$MAKER_PID" || true
for role in maker taker; do
  "$TRADE" --home "$RUN/$role" balance "$A"
  "$TRADE" --home "$RUN/$role" balance "$B"
done
cat "$RUN/maker.log"
```

After interruption, use the same home and relays: `maxplayer-trade --home <home> recover`.
`serve` also resumes journaled work. A refund is not available until its deadline; a watcher may
remain running for the full lock duration. An inconclusive mint result is retained for recovery.
To cancel an unused listing: `maxplayer-trade --home <maker-home> cancel <lot-id>`.
Cancellation does not revoke an already authorized HTLC.

## Live evidence — 2026-10-07

Actual fresh homes on this machine:
`/home/openclaw/.openclaw/workspace/.openclaw/tmp/credit-trade-final-20261007/{maker,taker}`.
The commands above were executed using these homes and a copied production-default binary.
Neither the original `credit-trade-live` homes nor their reservations were changed.

- Relays: **wss://relay.ditto.pub + wss://relay.damus.io** (both queried/published).
- Maker mint: **https://testnut.cashudevkit.org**, `cdk-mintd/0.18.0`.
- Taker mint: **https://testnut.cashu.space**, `cdk-mintd/0.17.0-rc.3`.
- Both advertised NUT-07/14, reported clock skew 0–1 seconds and input fee **100 ppk**.
- Lot: `d659b210246a41cef13c3edfe790ba01d0a60b15ed0ee2d74856eaea52a1c445`.
- Initial available event: `f5e981a34c5fbf4f94853fa1c78e4e008aa77c1acce54b60edfcc95890595dee`.
- Sold event: `a85ca93f54f8c5c922593996c7584945dc7fb3daa8510e86fa8e503facb96d58`.
- Swap: `4f312a95-19aa-4b8a-b871-86bc6731751a`; **maker complete, taker complete**.

| Home | cashudevkit before → after | cashu.space before → after |
|---|---:|---:|
| Maker | 128 → **93** | 0 → **24** |
| Taker | 0 → **32** | 128 → **101** |

Maker: net 32, gross lock 33 (2 proofs), preparation fee 2, claim fee 1, debit 35.
Taker: net 24, gross lock 25 (3 proofs), preparation fee 2, claim fee 1, debit 27.
Each mint's combined final balances are 125, exactly 128 minus its three units of mint fees.
There were **no trade/platform fees, no unsecured sends, and no live protocol blocker**.
An earlier fresh-home run also completed with the same final balances (lot
`85e59585ca6462b7dbf5b459d7e8d5a1077a8742641e3fb541895a2a9c909b3c`).
Private proofs/preimages are not included in this document or logs.

### Relay observations

The earlier independent-reader probe found inconsistent persistent readback at Ditto; hence
publish-to-all and read-the-union. Damus returned stored **23412** events despite their ephemeral
kind. This is acceptable here because their contents are NIP-44 encrypted; confidentiality does
not rely on deletion. Neither observation guarantees 24-hour retention or relay completeness.
The diagnostic remains rerunnable as `target/debug/examples/relay_probe` from this crate.

### Preserved CDK 0.17.2 orphan-quote bug

The old CLI attempted issuance before testnut auto-pay, producing `Amount undefined`. Two old
quotes subsequently remained `PAID` with a reservation but no saga:
`01a1181c-9e87-7ff1-b9d4-176ec59adfda` and `01a1181c-acbf-7a40-817a-8c37a9ca04bf`.
`inner_check_mint_quote_status` releases the orphan reservation, then writes the still-reserved
in-memory quote back through `add_mint_quote`, resurrecting `used_by_operation`. The next mint
returns **`Quote already in use by another operation`**.

`tests/pinned_quote_recovery.rs` retains the exact public-database-API reproducer. It proves the
bug, **not successful recovery**. The corrected `fund` waits for PAID and works on fresh quotes,
as demonstrated by this live run. The original homes are preserved; no reservation was cleared,
CDK was not patched/upgraded, and this old-quote bug is **not a blocker for fresh-home trading**.

## Boundaries, deviations and uncovered cases

- Authorized deviation from the integration spec: standalone crate, home/seed/CDK wallets and
  trade SQLite journal; HTTP(S) test mints and public relays; no jobs, budget/ledger, MCP,
  Nostr-mint transport, sidecar changes, or production-relay deployment. Fork PR stays draft.
- Trade swaps use pinned CDK **public lower-level primitives** with explicitly journaled outputs,
  rather than opaque high-level send/receive sagas. Claims and refunds use the same durable
  adapter; refunds include the empty-preimage witness required by pinned CDK. No dependency fork.
- Negotiation carries bounded proof arrays whose mint/unit are bound by the quote and verified
  against that mint's keys, rather than an additional bearer-token wrapper. Denominations are
  binary; fees use one ceil after summing input ppk. Limits: 1,000,000 net per lot, 128 proofs,
  48 KiB encrypted plaintext, 256 status revisions, 4,096 events per relay query, eight relays.
- Two SQLite domains (trade journal and per-mint wallet) are coordinated by durable intents and
  exclusive home ownership, not a cross-database atomic transaction. Other copies of a wallet
  seed remain outside the single-owner guarantee. No record pruning is implemented.
- A persisted client send deadline prevents **resubmitting** an expired mint request; HTTPS
  mints do not offer an enforceable server-side request expiry. Already-delivered requests may
  race refunds. Ambiguous/partial restoration remains held; no automatic unsafe compensation.
- NUT-14 receiver claims remain possible after locktime. Honest/available mints, intact keys,
  clock bounds, and a running recovery watcher are required. An offline maker can lose its
  recovery window; an issuer can lie or refuse service. This is not trustlessness against mints.
- Not covered: every crash boundary, OS/power-loss durability, long outages/clock skew, hidden or
  malformed live witnesses, live refunds, live reverse pairing/sell-back, mint key rotation,
  adversarial fee changes, exhaustive peer flood/quote-limit concurrency, 24-hour relay retention,
  root workspace suites locally, real money, or production deployment. Tests cover selected
  process-exit boundaries and one claim/refund race, not a full model checker.
