# Cashu mints over Nostr, and seller-issued credits through an opt-in mint sidecar

**Status:** design spec. No code written. **Anchor commit: `135e4ea0bd5330f7ab0272d501aa83a718edc777`
(= `origin/main` at the time of writing).** Line numbers are written `file:line @135e4ea` and were
re-derived by grep at that commit.

**Scope.** Two separable pieces:

- **Core (`maxplayer`, default binary):** a wallet can use any Cashu mint reachable at
  `nostr://<npub>`, talking to it over Nostr relays instead of HTTPS. Core doesn't care whether that
  mint is backed by Lightning or issues credits locally. It reads what a mint can do from the mint's
  signed info.
- **Sidecar (`maxplayer-mint`, separate opt-in binary):** a Cashu mint that answers only over Nostr,
  with a pluggable backend. The first backend is **local issue**: the operator mints credits from
  the CLI. That's how a seller issues its own credits with no website, domain, DNS, TLS
  certificate, public HTTPS endpoint or inbound port. A Lightning backend can be added later
  without core changes.

"Credit mint" is not a type anywhere in code. It's just our word for a mint whose backend is local
issue.

**Background.** The ManySails pilot (22 Sep 2026) proved an HTTPS CDK mint with operator-only
issuance can pay a real Maxplayer job. It was a pilot only; nothing here migrates it or keeps
compatibility with it.

## 0. Settled inputs (Bob, #credit-feature, 23 Sep 2026) — not relitigated here

1. **Enabled at setup.** A seller turns credits on when it sets up; the seller issues them. Selling
   credits on a market comes later and is out of scope.
2. **Accepted by any seller that opts in** to the issuing mint, not only by the issuer.
3. **Transferable** between holders.
4. **Same unit.** Credits use the Cashu `sat` unit. Jobs stay priced in sats and a seller that
   accepts a mint takes its credits one-for-one.
5. **Platform fee is charged to the seller in real sats**: 10% of the job's sat price
   (`PLATFORM_FEE_BPS = 1000`, `platform_fee.rs:48 @135e4ea`), unchanged.
6. **A seller that can't pay the fee keeps working.** Fees accrue and retry as they do today, with
   no cutoff on credit jobs.
7. **The issuing mint must be online** for a seller to take a job paid at that mint. If the issuer
   is offline, its credits can't be spent or moved. That is accepted.
8. **Issuers advertise their mint** in their heartbeat. A seller must still add a mint by hand
   before it accepts that mint's credits.
9. **The seller carries backup and key-loss risk.** There is no platform-run backup.
10. **No expiry, revocation or retirement** for now.
11. **Firm requirement:** no seller-hosted public endpoint of any kind. The seller only connects
    outward to relays.

Second round (16:02):

12. **The mint is opt-in.** The default release binary contains no mint code and building it needs
    no `protoc`. *(Amended 16:20: the opt-in is a separate `maxplayer-mint` binary, not a cargo
    feature of `maxplayer`.)* Accepting, holding and paying at a `nostr://` mint are wallet
    features and ship in the default binary.
13. **No new announcement kind.** Mint info goes in the seller heartbeat (kind 30340) for now.
14. **Rate cap:** about 20 requests per second per mint, tunable by the operator.
15. **Relays:** the mint listens on the seller's relay plus one or two public relays as fallback.
16. **Manual opt-in is enough trust for now:** a seller adds a mint by hand before accepting it.
17. **Spend at any mint counts against the buyer's sat budget.**
18. **An issuer always accepts its own credits.**
19. **No mint fee** (`set_unit_fee` stays 0).

Third round (16:05–16:20):

20. **Accepting a mint is not operating it.** Many sellers can accept one mint. Only the seller that
    operates it claims it in its heartbeat.
21. **Transport is separate from backing.** A real Lightning mint may use `nostr://` later. Core
    decides by advertised capability, never by URL scheme, what a mint can be used for.
22. **The mint is a sidecar**, not part of the seller node.

## 1. The model in one paragraph

An operator who wants to issue installs `maxplayer-mint`, runs `maxplayer-mint init`, and runs it
next to `maxplayer seller`. The sidecar embeds a CDK mint with its own SQLite database, seed and
Nostr key under `<home>/mint/`. It listens on relays for NIP-44-encrypted requests and answers the
Cashu operations its backend supports. The local-issue backend supports only the **holder**
operations (info, keys, keysets, swap, proof-state check and restore). **Issuance is never on the
wire:** the operator issues with `maxplayer-mint issue`, so no remote party can mint and there is no
auth layer to get wrong. In core, wallets reach any `nostr://` mint through a Nostr `MintConnector`
chosen by the URL scheme. Everything above the connector is the money path as it is today:
pays-once, the co-signed receipt, amount from the buyer-signed offer and the budget gate.

## 2. Mint identity

### 2.1 DECISION — `nostr://<npub>` as the mint URL

A Nostr-reachable mint's identity is the URL `nostr://<mint npub>`. It goes wherever a mint URL goes
today: in tokens, `accepted_mints`, NUT-18 payment requests, wallet rows and receipts. The scheme
selects the **transport** only (settled input 21).

Why this works without changing the token format: `cashu::MintUrl::from_str` (`cashu-0.17.2
src/mint_url.rs`) splits on `://`, lowercases scheme and host, and doesn't require `http(s)`. A
bech32 npub is already lowercase, so `nostr://npub1…` round-trips through `FromStr`, `Display` and
serde unchanged. TokenV4 stores the mint as that string. No HTTP code ever sees a `nostr://` URL
(§4.1).

**Verified 23 Sep 2026 (CDK 0.17.2, throwaway test crate, in-process connector, no HTTP):** a
`nostr://npub1…` URL round-trips through `MintUrl` parse/Display/serde and trailing-slash/case
normalisation, NUT-18 `creqA` and NUT-26 `creqB`, a real CDK `Mint` advertising it in `info.urls`,
and a full sqlite-backed wallet lifecycle: issue, P2PK-locked V4 send, receive (wrong key refused),
double-spend refused, NUT-07 state, onward transfer, wallet reopen, NUT-13 restore from seed.
**One hard constraint found:** CDK's WebSocket subscription path (`wallet/subscription.rs:547-553`)
joins `/v1/ws` onto the mint URL and panics (`Could not set scheme`) on `nostr://`, which hangs any
quote/proof stream. Every `nostr://` wallet MUST be built with `WalletBuilder::use_http_subscription()`
(poll mode, which goes through the connector), and a test must assert it.

Relay hints are **not** part of the URL, since hosts can't carry them. They come from §2.2.

### 2.2 Operator binding — seller heartbeat plus the mint's own info (settled inputs 13, 20)

There is no separate announcement event.

- **Operator claim:** the operating seller's heartbeat (kind 30340) carries
  `["runs_mint", <npub>, <relay>…]`, signed by the seller key. Omitted when the seller operates no
  mint, so existing beats are byte-identical. *(Renamed from `credit_mint`: the tag says who
  operates a mint, not what backs it.)*
- **Mint confirmation:** the mint's NUT-06 `info`, returned in a response signed by the mint key
  (§3.1), lists the operator in the standard `contact` field as `{"method":"nostr","info":<seller
  npub>}`. A `runs_mint` claim the mint doesn't confirm is shown as unverified.
- **Accepting is separate.** Every seller that accepts the mint, the operator included, lists it in
  `accepted_mints` exactly as it lists HTTPS mints today. A seller that takes a friend's credits
  lists the friend's mint there and never carries `runs_mint` for it.
- Keysets are learned from the mint's `keys` reply as usual. They are not pinned in the heartbeat.
- **Keys:** the mint key is **separate from the seller key**, generated by `maxplayer-mint init` and
  stored `0600` at `<home>/mint/nostr.key`. The seller's identity can then rotate without orphaning
  every outstanding credit. Rotating the mint key is out of scope: its npub **is** the mint
  identity, and changing it means a new mint. The seller node reads only
  `<home>/mint/public.json` (`{npub, relays}`) and never opens the mint's secret files.

Kinds `23410` and `23411` don't collide with `kinds.rs @135e4ea` (3400–3407, 30340). Both are in the
ephemeral range, which relay.maxplayer.ai already passes through (tested, §9).

## 3. The wire protocol — kinds `23410` (request) / `23411` (response)

A Maxplayer-specific extension, **not a NUT**. It's versioned so it can be proposed upstream later.
It is backend-neutral: it carries any NUT endpoint, and each mint serves only what its `info`
advertises.

### 3.1 Envelope

- **Request:** kind `23410` (ephemeral range, so relays don't store it), `["p", <mint hex>]`,
  content = NIP-44 v2 ciphertext to the mint key. Signed by a **per-session ephemeral client key**,
  so relays can't tie requests to a wallet identity. Plaintext:
  `{"v":1,"id":<request id>,"op":<op>,"body":<NUT JSON body>,"exp":<unix>}`.
- **Response:** kind `23411`, `["p", <client ephemeral pubkey>]`, `["e", <request event id>]`,
  NIP-44 to the client and signed by the **mint key**. The connector rejects any response not
  signed by the npub in the mint URL. Plaintext: `{"v":1,"id":…,"ok":<NUT JSON>}` or
  `{"v":1,"id":…,"err":{"code":<NUT error code>,"detail":…}}`.
- **`id`** is `sha256(op || canonical body)` for writes, so a retry of the same swap is the same
  request by construction, and random for reads.
- The client subscribes for responses **before** it publishes, on every relay, and takes the first
  valid response. The mint processes a request once no matter how many relays deliver it (§3.3).
- **Limits:** NIP-44 caps plaintext at 65,535 bytes. Measured (§9): a swap costs ~400 B per proof
  each way, so 128 inputs + 128 outputs = 50.9 KB request / 40.0 KB reply; 256 does not fit. Mints
  set CDK `with_limits(128, 128)` and wallets split larger swaps. Requests past `exp` or older than
  120 s are dropped unanswered.
- **Rate cap:** over the operator's limit (default 20/s) the mint replies `err` `rate_limited`. That
  is a definitive error: the mint did not run the request.

### 3.2 Operations

| op | NUT | local-issue backend (v1) |
|---|---|---|
| `info` | 06 | served; also the health check (§4.5) |
| `keys` / `keyset` / `keysets` | 01/02 | served |
| `swap` | 03 | served; the only way credits move; the double-spend check happens here |
| `checkstate` | 07 | served |
| `restore` | 09 | served; returns signatures only for blinded messages the caller already has |
| `mint_quote` / `mint_quote_state` / `mint` | 04 | **refused**; reserved for a Lightning backend |
| `melt_quote` / `melt_quote_state` / `melt` | 05 | **refused**; reserved for a Lightning backend |

WebSocket subscriptions (NUT-17) and auth are never on the wire. Any op the backend doesn't serve
gets `err` `unsupported`, and `info.nuts` advertises only what's served. The listener calls
`Mint::process_swap_request`, `check_state`, `pubkeys` and friends directly (cdk `mint` feature,
`cdk-0.17.2 src/mint/`). There's no HTTP router to misconfigure.

### 3.3 Reliable writes

- **No mint-side response cache.** CDK refuses a replayed swap (`TokenAlreadySpent` /
  `DuplicateOutputs`) and never re-executes it; recovery goes through the wallet (below). Tested, §9.
- **Wallet side:** CDK journals swap intent and outputs before sending. After ANY ambiguous error the
  caller runs `recover_incomplete_sagas()`, which replays, checks proof state, and recovers the
  outputs by NUT-09 restore. **Never re-call `receive` after an ambiguous error:** tested, that wipes
  the pending record and only a full seed `restore()` gets the credits back.
- **The connector must report a missing reply as `Error::Timeout` (ambiguous).** A definitive error
  makes CDK compensate, i.e. drop the pending proofs whose inputs the mint already spent.
- A relay `OK` is not a result. Only a signed `23411` is.

## 4. Changes to core

All of this is in the default binary and applies to any mint. `https://` mints keep today's
behavior except where §4.3 and §4.4 fix existing gaps.

### 4.1 Connector dispatch (`nostr://` → Nostr, `https://` → HTTP)

Every place that constructs a mint connector goes through one factory, `mint_connector_for(url,
home)`:

- `buyer_fund.rs:104 @135e4ea`: `Wallet::new(mint_url, …)` → `WalletBuilder::new()…shared_client(
  mint_connector_for(…))`. This is `open_wallet_at_mint_async`, which the seller receive path uses
  (`seller_node/run.rs:9434`).
- `payment_wallet.rs:1849`: `HttpClient::new(wallet.mint_url…)` for the verifier.
- `doctor.rs:92`: `HttpClient::new(url, None)` probe → `info` over relays for `nostr://`.

The Nostr connector uses `nostr-sdk`, already a `wallet` dependency (`maxplayer-core/Cargo.toml`),
and pulls in no cdk `mint` code. Relays for a mint: the `runs_mint` relay hints, else
`[mint_relays]`, else `relay_url` plus the 1–2 public fallbacks (settled input 15; picked in stage 3
after checking they accept and deliver 23410/23411 with NIP-42).

**Acceptance:** a `nostr://` mint never produces an HTTP request. This is proven with a test
connector that panics on any HTTP call and a netns run with outbound HTTP to anything except the
relay denied.

### 4.2 Allow-list

`home::mint_allowed` (`home.rs:1804`) admits `https://` or the default testnut. Add: a well-formed
`nostr://npub1…` is allowed **iff** it's listed in `accepted_mints` or `extra_mints`, or it's the
mint in the seller's own `public.json`. It is independent of `allow_real_mints`: an explicit listing
is the opt-in (settled input 16). `wallet receive` of a `nostr://` token for an unlisted mint
refuses and prints the line to add.

### 4.3 Capability, not scheme (settled input 21)

What a mint can do comes from its `info` (NUT-06 `nuts.4` / `nuts.5` methods), fetched through its
connector and cached per keyset:

- **Cross-mint hops** (`crossmint.rs:94` `plan_payment`, `:160` `select_source_mint`): a mint is a
  hop source only if it advertises NUT-05 bolt11 melt, and a hop target only if it advertises
  NUT-04 bolt11 mint. A local-issue mint advertises neither, so it's never used for a hop, and a
  buyer holding only such credits at a mint the seller doesn't list gets a refusal, not a hop.
- **Fee remit (existing gap, fixed for every mint).** Today `fee_remit` melts only at
  `default_mint()` = `accepted_mints[0]` (`fee_remit.rs:445` → `wallet_ops::melt_quote_blocking(…,
  None)` → `resolve_mint`, `wallet_ops.rs:841`). If that mint can't melt, fees are never paid; if
  its Lightning backend is fake (testnut, or any fake-backed mint), the melt "succeeds" and the fee
  is recorded paid while the platform gets nothing. Fix:
  1. Choose the melt mint by capability and balance: an allowed mint that advertises NUT-05 bolt11
     and holds enough, not whichever is first.
  2. **Verify the payment:** a remit counts as paid only if the melt returns a `payment_preimage`
     with `sha256(preimage) == ` the LNURL invoice's payment hash. Only the platform's Lightning
     node knows that preimage, so a fake backend can't produce it. Missing or mismatched ⇒ the fee
     stays owed and retries (settled input 6), and the event is logged.
  Stage 0 must confirm minibits returns `payment_preimage` on melt before step 2 ships (§8).
- **Balances** (`wallet_ops::MintBalance`) stay per mint. `wallet balance` shows each mint's
  advertised Lightning capability next to its balance, so a holder can see which balances can leave
  over Lightning. No separate credit type or total.
- **Budget gate:** unchanged; spend at any mint counts (settled input 17).

### 4.4 Recovery after ambiguous errors (existing gap)

Only `crossmint_hop.rs:1236` calls `recover_incomplete_sagas()` today; `wallet_ops.rs:661` and
`:1731` already note "a supported recovery path is owed". Every write that can end ambiguous (seller
receive, buyer pay, `wallet receive`, `wallet send`, melt) runs it before any retry, and never
re-issues the operation blind. Over relays, lost replies are routine, so this is a precondition for
`nostr://`; it also hardens `https://`.

### 4.5 Seller accept path

- **Online gate (settled input 7).** The seller keeps `last_ok` per accepted `nostr://` mint,
  refreshed by an `info` ping every 60 s. At claim time (`claim_offer`, `seller_node/run.rs:6762`;
  creq built at `:6853`), such a mint goes into the creq **only if** `last_ok` is within 180 s. The
  seller's own mint is pinged too: it's a separate process now and can be down. `https://` mints are
  unchanged.
- If the mint drops after the claim, collect's receive retries on the existing backoff until the mint
  answers, with §4.4 recovery. The pending-receive breadcrumb (`append_pending_receive`,
  `run.rs:9458`) already records it.
- **Heartbeat:** `runs_mint` from `public.json` when present (§2.2). An issuer accepts its own mint
  (settled input 18): `public.json`'s mint is treated as listed in `accepted_mints`.
- `maxplayer mints list` reads 30340 beats, pings each advertised mint's `info`, and shows its npub,
  operator, verified or unverified binding (§2.2), advertised Lightning capability, how many sellers
  list it in `accepted_mints`, and whether it's online now.
- `maxplayer mints add <url>` appends to `accepted_mints`. Nothing is added automatically.

### 4.6 Config (core)

```toml
accepted_mints = ["https://mint.minibits.cash/Bitcoin", "nostr://npub1…"]   # unchanged meaning
mint_relays = []   # relays for nostr:// mints; empty ⇒ relay_url + 1–2 public fallbacks
```

No `[credits]` section in core.

## 5. The sidecar — `maxplayer-mint`

### 5.1 Packaging (settled inputs 12, 22)

- New crate `crates/maxplayer-mint`, its own binary. It depends on cdk `mint` + cdk-sqlite mint
  store and so needs `protoc` to build. `maxplayer` does not depend on it.
- The shared envelope types (§3.1) live in a small crate both sides use, with no cdk `mint`
  dependency, so core's build is unaffected.
- Not bundled with the default release or installer. It ships as a separate, optional artifact. CI
  builds and tests it with protobuf installed. The default `maxplayer` release build keeps needing
  no `protoc`.
- Its own process, database and relay connections. A seller-node crash, restart or upgrade never
  touches the mint's database, and the mint can upgrade on its own schedule. It doesn't require a
  seller at all: anyone can run a Nostr-only mint.

### 5.2 Commands

- `maxplayer-mint init [--backend local-issue]` generates the mint Nostr key, seed and
  `<home>/mint/mint.sqlite`, writes `public.json` and `mint.toml`. It refuses if `<home>/mint/`
  already exists: never re-initialize a seed under an existing identity. It prints the backup
  warning (settled input 9): losing `<home>/mint/` makes every outstanding credit worthless, and
  restoring an old copy can let spent credits be spent again.
- `maxplayer-mint run` is the relay listener (§3). v1 doesn't supervise it from `maxplayer seller`;
  the operator runs it (a systemd unit is documented), and `maxplayer seller` warns at boot if
  `public.json` exists but the mint doesn't answer `info`.
- `maxplayer-mint issue <amount> [--to-file]` (local-issue backend only) mints proofs in-process to
  a local token file. The mechanism is a local-only CDK payment processor that auto-settles incoming
  requests created by this process; it has no network surface and its quote ops aren't on the
  listener (§3.2). Each issuance has a unique id with journaled outputs, so an interrupted issue is
  reconciled and never re-issued blind. No quota.
- `maxplayer-mint status`: npub, relays, keysets, outstanding issued vs. redeemed totals.

### 5.3 Backend

A `MintBackend` choice maps onto CDK's payment-processor slot:

- **`local-issue` (v1):** the local-only processor above. `info.nuts` advertises no NUT-04/05
  methods, so core treats the mint as Lightning-less by capability (§4.3).
- **Lightning (later, out of scope):** a real processor would serve NUT-04/05 over the reserved ops
  (§3.2) and advertise them. Core needs no change to use it.

### 5.4 Config (`<home>/mint/mint.toml`)

```toml
backend = "local-issue"
relays = []        # empty ⇒ the seller's relay_url + 1–2 public fallbacks
rate_limit = 20    # requests/second (settled input 14)
```

## 6. Buyer behavior

- `maxplayer wallet receive <token>` works for `nostr://` tokens through the connector, once the
  mint is listed (§4.2). The swap at the mint is what makes the credits the buyer's.
- The pay path is unchanged. The buyer pays at a mint in the claim's creq. If it holds credits at a
  listed mint, that's a direct pay. The award filter treats a claim whose creq lists no mint the
  buyer can pay directly or hop to (§4.3) as unpayable.
- `wallet send` makes transferable tokens at any mint (settled input 3).

## 7. Stages (each a separate PR)

0. **Fee remit by capability, with preimage check** (core, §4.3). Independent of credits; fixes an
   existing gap. First confirms minibits returns `payment_preimage`.
1. **Recovery after ambiguous errors** (core, §4.4). Independent; hardens `https://` today.
2. **Connector factory and allow-list** (core). `nostr://` parsing test, `mint_connector_for` with
   HTTP only, §4.2, capability-based hop selection. No behavior change for `https://`.
3. **Nostr connector and shared envelope crate** (core). Kinds 23410/23411 client side, relay
   selection, fallback relays picked. Tests on the in-process relay (`nostr-relay-builder`, already a
   dev-dependency) against a test mint in dev-dependencies (so the test job, not the release build,
   needs protobuf).
4. **`maxplayer-mint` sidecar.** `init`, `run`, `issue`, `status`, local-issue backend, rate cap.
   CI job with protobuf.
5. **Seller accept path** (core). Online gate, `runs_mint` heartbeat tag, `mints list/add`, seller
   receive at a foreign `nostr://` mint.
6. **Buyer path** (core). Receive, send, balances with capability, pay and award filter.
7. **End to end.** Seller A runs `maxplayer-mint`, seller B accepts A's mint, a buyer holds only A's
   credits. B does a job, is paid in A's credits and pays its fee in real sats with a verified
   preimage. Run on a host behind NAT with no DNS or inbound ports, with outbound HTTP to anything
   but relays and B's Lightning mint denied.

### 7.1 Required failure tests

Duplicate requests across relays (one swap executes). Replayed, stale, forged or wrong-key
responses (rejected). Reply lost after commit, on both the receive and the send side
(`recover_incomplete_sagas`, never a blind re-`receive`). Rate cap enforced (`rate_limited`,
definitive). Mint or wallet killed at each write boundary. Two holders spending the same proofs at
once (one wins). All relays down (clean error, no fallback to HTTP). Oversized or malformed
requests. Unsupported ops, including NUT-04 and NUT-05 on local-issue (refused). Fee remit never
picks a mint without NUT-05, and a melt with a missing or wrong preimage leaves the fee owed. A
mint without NUT-04/05 is never a hop source or target. `maxplayer-mint init` refuses over an
existing mint. The default `maxplayer` build contains no cdk `mint` code and builds without
`protoc`. **No HTTP call is ever made for a `nostr://` mint.**

## 8. Open items

No product questions are open. To verify in stage 0: minibits returns `payment_preimage` on melt.
If it doesn't, §4.3 step 2 needs another way to confirm the payment before it can ship, and step 1
ships alone.

## 9. Pre-code test results (23 Sep 2026, CDK 0.17.2, throwaway crate)

- **`nostr://` identity:** survives parse/serde, creqA/creqB, tokens, full wallet lifecycle and
  restore (§2.1). Wallets need `use_http_subscription()`.
- **Lost swap reply:** mint commits, reply dropped → `recover_incomplete_sagas` recovered all 300.
  Request dropped before the mint → recovery replayed it, 300 recovered. Naive re-`receive` before
  recovery → balance 0 until full seed `restore()`. Sender-side lost reply NOT yet exercised (the
  test send needed no swap).
- **Relay (relay.maxplayer.ai, fresh one-off keys, NIP-42 auto-auth):** kinds 23410/23411 accepted
  and delivered, not stored afterwards. Round trip from this VPS, two runs: ~55 ms small, ~100 ms at
  16 KB, 200–214 ms at 51 KB, 219–226 ms at the 65 KB NIP-44 max. Echo only.
- **Build cost:** cdk `mint` feature hard-enables `cdk-signatory/grpc`, so **`protoc` is required**
  and cannot be switched off without patching CDK. Toy binary: wallet-only 10.4 MB / 367 s cold,
  wallet+mint 17.1 MB / 479 s (+6.3 MB, +30 crates incl. axum, tonic, prost). With the mint in a
  separate binary (§5.1), only `maxplayer-mint` pays that.
