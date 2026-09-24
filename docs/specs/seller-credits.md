# Seller-issued credits: Cashu mints over Nostr, and an opt-in mint sidecar

**Status:** design spec. No code written. **Anchor commit: `135e4ea0bd5330f7ab0272d501aa83a718edc777`
(= `origin/main` at the time of writing).** Line numbers are written `file:line @135e4ea`.

**Goal: the smallest change to how Maxplayer works today.** Two pieces:

- **Core (`maxplayer`, default binary):** a wallet can use a Cashu mint at `nostr://<npub>` and talks
  to it over Nostr relays instead of HTTPS. That's the only new behavior in core. Everything above
  the transport (pay path, receipts, budget, fees, hops, balances, heartbeat) is unchanged.
- **Sidecar (`maxplayer-mint`, separate opt-in binary):** a Cashu mint that answers only over Nostr.
  Its v1 backend is **local issue**: the operator mints credits from the CLI. The seller needs no
  website, domain, DNS, TLS certificate, public endpoint or inbound port.

"Credit mint" is not a type anywhere in code. It's our word for a mint whose backend is local
issue. A Lightning-backed mint could use `nostr://` later with no core change.

**Background.** The ManySails pilot (22 Sep 2026) proved an HTTPS CDK mint with operator-only
issuance can pay a real Maxplayer job. Nothing here migrates it or keeps compatibility with it.

## 0. Decisions (Bob, #credit-feature, 23 Sep 2026)

1. A seller turns credits on at setup and issues them itself. Selling credits on a market is later.
2. Any seller that opts in to a mint accepts its credits, not only the issuer.
3. Credits are transferable between holders.
4. Credits use the Cashu `sat` unit, one-for-one with sats. Jobs stay priced in sats.
5. The platform fee is charged to the seller in real sats (10%, `platform_fee.rs:48`), unchanged.
6. A seller that can't pay the fee keeps working; fees accrue and retry as today.
7. If the issuing mint is offline, its credits can't be spent or moved. That is accepted.
8. A seller adds a mint by hand before accepting it. That's enough trust for now.
9. The issuer carries backup and key-loss risk. No platform backup.
10. No expiry, revocation or retirement for now.
11. **Firm requirement:** no seller-hosted public endpoint. The seller only connects out to relays.
12. The mint is opt-in: a separate `maxplayer-mint` binary. The default release has no mint code
    and needs no `protoc`. Accepting, holding and paying credits ship in the default binary.
13. Mint info goes in the existing heartbeat. No new event kind for it.
14. The mint serves about 20 requests/second, tunable.
15. The mint listens on the seller's relay plus one or two public relays as fallback.
16. Credit spend counts against the buyer's budget. An issuer accepts its own credits. No mint fee.
17. Accepting a mint is not operating it; many sellers can accept one mint.
18. Transport is separate from backing: `nostr://` says nothing about Lightning.
19. Keep changes to current Maxplayer behavior minimal (17:13).
20. A listed `nostr://` mint is allowed under the same `allow_real_mints` rule as `https://` (17:28).

## 1. What changes in core, and what doesn't

**Changes (one feature: a second transport):**

1. **Connector factory.** The three places that build an HTTP mint client call one function,
   `mint_connector_for(url, home)`: `https://` → today's `HttpClient`, `nostr://` → the Nostr
   connector (§3). Sites: `buyer_fund.rs:104` (`open_wallet_at_mint_async`, also the seller receive
   path), `payment_wallet.rs:1849` (verifier), `doctor.rs:92` (probe). A `nostr://` wallet is built
   with `WalletBuilder::use_http_subscription()` (§6: CDK's WebSocket path panics on `nostr://`).
2. **Allow-list.** `home::mint_allowed` (`home.rs:1804`) admits `https://` only. Add: a well-formed
   `nostr://npub1…` is allowed when `allow_real_mints` is on, exactly like any `https://` mint. The
   manual listing in `accepted_mints` / `extra_mints` stays the opt-in, as it is for HTTPS mints.

**Unchanged, and why that's fine:**

- **Advertising (decision 13).** The issuer accepts its own mint, so its `nostr://` URL is in the
  `accepted_mints` tag of its heartbeat already. No new tag. Anyone who wants to know who runs a
  mint can read the operator's contact in the mint's NUT-06 `info`.
- **Fee remit.** Pays from `accepted_mints[0]`, as today. `wallet mints add` never changes the
  default, and a seller appends a credit mint after its Lightning mint, so the fee mint stays
  Lightning. If someone puts a credit mint first, melt quotes fail before anything is spent and fees
  accrue (decision 6). Documented, not coded.
- **Cross-mint hops.** A hop raises the mint quote and the melt quote before spending anything
  (`crossmint_hop.rs:898` `plan_quotes`). A local-issue mint refuses both, so a hop through it fails
  cleanly with nothing spent.
- **Offline mint (decision 7).** No claim-time gate. A `nostr://` mint that doesn't answer behaves
  like an `https://` mint that is down today: the pay or receive fails or retries on the existing
  backoff.
- **Lost replies.** Handled inside the connector and the sidecar (§3.3), so core never sees them.
  Core's recovery behavior is unchanged.
- Balances, budget gate, receipts, pays-once, award filter, config: unchanged.

## 2. Mint identity

The mint URL is `nostr://<mint npub>`. It goes wherever a mint URL goes today: tokens,
`accepted_mints`, NUT-18 payment requests, wallet rows, receipts. `cashu::MintUrl` doesn't require
`http(s)` and a bech32 npub is lowercase, so it round-trips unchanged (tested, §6).

The mint key is separate from the seller key, generated by `maxplayer-mint init`, stored `0600` at
`<home>/mint/nostr.key`. Its npub **is** the mint; changing it means a new mint.

## 3. Wire protocol (kinds `23410` request / `23411` response)

Maxplayer-specific, versioned, **not a NUT**. Both kinds are ephemeral, so relays don't store them;
relay.maxplayer.ai already passes them (tested, §6). No collision with `kinds.rs` (3400–3407, 30340).

### 3.1 Envelope

- **Request:** kind `23410`, `["p", <mint hex>]`, NIP-44 v2 content to the mint key, signed by a
  per-session throwaway client key. Plaintext `{"v":1,"id":…,"op":…,"body":<NUT JSON>,"exp":<unix>}`.
- **Expiry:** `exp` is the **first unix second in which the request is invalid**. The mint refuses
  with `expired`, without executing, once `now >= exp`. The wallet computes `exp` from the same
  start as its wait deadline, so `exp` is never later than the moment it stops waiting (it may wait
  up to 1 s longer, which is safe). Wallet and mint clocks must agree within **60 s**
  (`MAX_CLOCK_SKEW_SECS`); a mint clock behind the wallet's lets a request run after the wallet
  gave up, and the wallet's 300 s recovery hold is sized to cover that bound.
- **Definitive errors** (`unsupported`, `rate_limited`, `bad_request`, `expired`) promise the
  request was not executed. The mint sends them only before the op reaches anything that can
  commit it; after that it answers `internal` or the original reply.
- **Response:** kind `23411`, `["p", <client key>]`, `["e", <request id>]`, NIP-44 to the client,
  signed by the mint key. The connector drops anything not signed by the npub in the URL. Plaintext
  `{"v":1,"id":…,"ok":<NUT JSON>}` or `{"v":1,"id":…,"err":{"code":…,"detail":…}}`.
- The client subscribes before it publishes and takes the first valid response from any relay.
- **Relays:** the wallet's `relay_url` plus the same one or two public fallbacks the mint uses
  (decision 15; picked in stage 1 after checking they carry these kinds with NIP-42).
- **Size:** NIP-44 caps plaintext at 65,535 bytes. That is the cryptographic ceiling, not a
  transport guarantee: relay event-size limits after encryption and event JSON are often lower, so
  the mint uses a tested lower limit and bounds **response** size too (checked before executing,
  from the output count); a swap costs ~400 B per proof each way (§6). The
  mint sets `with_limits(128, 128)`. A bigger swap is refused with a clear error; no splitting in v1.
- **Rate cap:** over the limit the mint replies `err` `rate_limited`.

### 3.2 Operations

The connector implements the whole CDK `MintConnector` trait, so any NUT op a mint serves works.
Swap is the only operation that moves credits. The local-issue backend serves `info` (06), `keys`/`keyset`/`keysets` (01/02), `swap` (03),
`checkstate` (07) and `restore` (09). It refuses mint and melt (04/05) with `unsupported`, and
`info` doesn't advertise them. Issuance is never on the wire.

### 3.3 Lost replies

- **Connector:** on no reply it re-sends the identical request (same event content) for up to 30 s,
  then returns `Error::Timeout`.
- **Sidecar:** a swap CDK refuses as a replay (`TokenAlreadySpent` / `DuplicateOutputs`) whose
  outputs are all already signed gets the original reply rebuilt from the mint's own stored
  signatures (`Mint::restore`), only when **every** requested output matches, in order, with the
  same DLEQ. Stage 2 adds the durable idempotency record in §3.4, so this rebuild is a fallback.
- Tested (§6): with a lost reply after the mint committed, both a receive and a sender-side swap
  complete normally on the re-send, with correct balances and no recovery call in the wallet.

### 3.4 Mint obligations (stage 2; from the #1034 Cashu review)

1. Verify signature, `p` tag, NIP-44 decryption, `v`, op schema, size and `exp` **before** dispatch.
2. Deduplicate on (client pubkey, request `id`), bound to the request event id and a digest of
   `v`/`op`/`body`/`exp`. Same key with different content never executes: answer `internal` and alarm.
3. Write an "executing → completed" record plus the exact response in the same transaction as the
   mint state change. Duplicates (across relays, after restart, concurrent) wait for or read the
   first execution; they never race it.
4. Replay the original reply, successes **and** definitive failures, for at least the relay/retry/
   recovery horizon, and even after `exp`: expiry never overwrites recorded history with `expired`.
   A request first seen after `exp` is refused without executing.
5. Restore-based rebuild (§3.3) only when all outputs match; a partial restore is never success.
6. Keep old keysets for redemption and restore after rotation; sign only under the requested
   keyset, never a substituted active one; produce NUT-12 DLEQ consistently.
7. Look up completed requests **before** rate limiting, so a replay never becomes a new
   `rate_limited`.

**Trust model.** The npub authenticates the transport; it doesn't make relay delivery reliable or
the mint honest, and more relays add availability, not consensus. Throwaway client keys hide the
Nostr identity, but connection metadata, timing and request sizes can still link requests.

## 4. The sidecar: `maxplayer-mint`

- New crate `crates/maxplayer-mint`, its own binary, depending on cdk `mint` + the cdk-sqlite mint
  store, so it needs `protoc`. `maxplayer` doesn't depend on it; the shared envelope types live in
  `maxplayer-core` (no mint deps). CI builds it in its own job with protobuf. Not bundled with the
  default release.
- `maxplayer-mint init` creates `<home>/mint/` (key, seed, `mint.sqlite`, `mint.toml`) and prints the
  `nostr://` URL plus the line to append to `accepted_mints`. It refuses if `<home>/mint/` exists
  and prints the backup warning (decision 9): losing `<home>/mint/` makes every credit worthless,
  and restoring an old copy can let spent credits be spent again.
- `maxplayer-mint run` is the relay listener. The operator runs it (a systemd unit is documented).
- `maxplayer-mint issue <amount>` mints proofs in-process to a local token file, through a
  local-only CDK payment processor with no network surface. Each issue has an id and journaled
  outputs, so an interrupted issue is reconciled, never re-issued blind.
- `mint.toml`: `relays = []` (empty ⇒ relay.maxplayer.ai + fallbacks), `rate_limit = 20`.

## 5. Stages

1. **Core: `nostr://` transport.** Connector factory, Nostr connector with re-send, allow-list,
   fallback relays picked. Tested against an in-process relay (`nostr-relay-builder`, already a
   dev-dependency) and a scripted mint responder, so core gains no cdk `mint` dependency and its CI
   needs no `protoc`. The real CDK mint over relays is tested in stage 2. No behavior change for
   `https://`.
2. **`maxplayer-mint`.** `init`, `run`, `issue`, idempotent swap replay, rate cap, CI job.
3. **End to end.** Seller A runs `maxplayer-mint`, seller B accepts A's mint, a buyer holds only A's
   credits. B does a job, is paid in A's credits and pays its fee in real sats from its Lightning
   mint. Run behind NAT with no inbound ports and outbound HTTP denied except to B's Lightning mint.

**Required tests:** a `nostr://` mint never makes an HTTP call; duplicate requests across relays
execute once; replayed, stale, forged or wrong-key responses rejected; lost reply after commit on
receive and on a sender swap (re-send completes, balances right); mint killed mid-swap; two holders
spending the same proofs (one wins); all relays down (clean error, no HTTP fallback); oversized and
malformed requests; mint/melt refused on local-issue; `init` refuses over an existing mint; the
default `maxplayer` build has no cdk `mint` code and builds without `protoc`.

**Out of scope (noted, not planned):** a Lightning backend for the sidecar; a heartbeat tag naming
who operates a mint; picking the fee mint by capability; wiring `recover_incomplete_sagas` into
core's `https://` paths (an existing gap, `wallet_ops.rs:661`); reporting the CDK sender-side
recovery bug (§6) upstream.

## 6. Pre-code test results (23 Sep 2026, CDK 0.17.2, throwaway crate)

- **`nostr://` identity:** survives `MintUrl` parse/serde, creqA/creqB, tokens, a full wallet
  lifecycle and NUT-13 restore. CDK's WebSocket subscription path panics on `nostr://`, so wallets
  need `use_http_subscription()`.
- **Connector re-send + idempotent sidecar swap:** reply lost after commit, then the identical
  request re-sent. Receive: completes with 300, 300 unspent at the mint, total 1000 conserved.
  Sender swap (one 512 proof, send 300): completes, sender 212, receiver 300, total 512. No wallet
  recovery call in either.
- **Without re-send (wallet-side recovery only):** receive recovers with `recover_incomplete_sagas`,
  but a naive re-`receive` first leaves balance 0 until a seed restore. Sender swap: recovery
  restores the new proofs **and** un-spends the spent input, so the balance reads 1024 for 512 until
  a NUT-07 check. That's why §3.3 handles lost replies below core.
- **Relay (relay.maxplayer.ai, fresh keys, NIP-42):** 23410/23411 accepted, delivered, not stored.
  Round trip, two runs: ~55 ms small, ~100 ms at 16 KB, 200–214 ms at 51 KB, 219–226 ms at 65 KB.
- **Size:** 128 inputs + 128 outputs = 50.9 KB request / 40.0 KB reply; 256 doesn't fit.
- **Build cost:** cdk `mint` forces `cdk-signatory/grpc`, so `protoc` is required. Toy binary:
  wallet-only 10.4 MB / 367 s, with mint 17.1 MB / 479 s. Only `maxplayer-mint` pays that.
- **Minibits melt preimage (live):** a 1-sat melt returned `PAID` and a preimage matching the
  invoice hash. Not used by this design; recorded for reference.
