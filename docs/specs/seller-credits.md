# Seller-issued credits — a Cashu mint inside the seller node, reachable only over Nostr

**Status:** design spec. No code written. **Anchor commit: `135e4ea0bd5330f7ab0272d501aa83a718edc777`
(= `origin/main` at the time of writing).** Line numbers are written `file:line @135e4ea` and were
re-derived by grep at that commit.

**Scope.** A seller can choose, at setup time, to run a Cashu mint that issues its own credits. Any
seller that opts in to that mint accepts the credits as payment. Buyers and sellers reach the mint
only through Nostr relays, so the issuing seller needs no website, domain, DNS, TLS certificate,
public HTTPS endpoint or inbound port.

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
7. **The issuing mint must be online** for a seller to take a credit-paid job. If the issuer is
   offline, its credits can't be spent or moved. That is accepted.
8. **Issuers advertise their mint** in their heartbeat. A seller must still add a mint by hand
   before it accepts that mint's credits.
9. **The seller carries backup and key-loss risk.** There is no platform-run backup.
10. **No expiry, revocation or retirement** for now.
11. **Firm requirement:** no seller-hosted public endpoint of any kind. The seller only connects
    outward to relays.

Second round (Bob, #credit-feature, 23 Sep 2026 16:02):

12. **The mint is opt-in at build time.** The default release binary contains no mint code. Issuing
    needs a build with the `credits-mint` cargo feature. Accepting, holding and paying with credits
    are wallet-side and ship in the default binary.
13. **No new announcement kind.** Mint info goes in the seller heartbeat (kind 30340) for now.
14. **Rate cap:** about 20 requests per second per mint, tunable by the seller.
15. **Relays:** the mint listens on the seller's relay plus one or two public relays as fallback.
16. **Manual opt-in is enough trust for now:** a seller adds a mint by hand before accepting it.
17. **Credit spend counts against the buyer's sat budget.**
18. **An issuer always accepts its own credits.**
19. **No mint fee** (`set_unit_fee` stays 0).

## 1. The model in one paragraph

The issuing seller's node embeds a CDK mint engine with its own SQLite database and signing keys,
stored in the seller home and never mounted into job sandboxes. The mint has its own Nostr identity.
It listens on the seller's relays for NIP-44-encrypted requests and answers the handful of
**holder** operations: info, keys, keysets, swap, proof-state check and restore. **Issuance is
never on the wire.** The operator issues from the local CLI, so no remote party can mint credits and
there is no auth layer to get wrong. Wallets reach a credit mint through a new CDK `MintConnector`
that speaks this Nostr protocol, chosen by the mint's URL scheme. Everything above the connector is
the money path as it is today: pays-once, the co-signed receipt, amount from the buyer-signed offer
and the budget gate.

## 2. Mint identity

### 2.1 DECISION — `nostr://<npub>` as the mint URL

A credit mint's identity is the URL `nostr://<mint npub>`. It goes wherever a mint URL goes today:
in tokens, `accepted_mints`, NUT-18 payment requests, wallet rows and receipts.

Why this works without changing the token format: `cashu::MintUrl::from_str` (`cashu-0.17.2
src/mint_url.rs`) splits on `://`, lowercases scheme and host, and doesn't require `http(s)`. A
bech32 npub is already lowercase, so `nostr://npub1…` round-trips through `FromStr`, `Display` and
serde unchanged. TokenV4 stores the mint as that string. The handoff's warning, "don't just replace
a URL with an npub", is about pasting an npub where HTTP code expects a host. Here the scheme is
what selects the connector, and no HTTP code ever sees a `nostr://` URL (§4.1).

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

### 2.2 Mint advertisement — seller heartbeat only (settled input 13)

There is no separate announcement event. The issuing seller's heartbeat (kind 30340) carries
`["credit_mint", <npub>, <relay>…]` (§5.3). The binding is checked in both directions without a new
kind:

- The heartbeat, signed by the seller key, claims the mint.
- The mint's `info` reply (NUT-06), signed by the mint key, names its issuer's seller pubkey. A
  heartbeat claim the mint doesn't confirm is shown as unverified.
- Keysets are learned from the mint's `keys` reply as usual. They are not pinned in the heartbeat.
- Keys: the mint key is **separate from the seller key**, generated at `credits enable` and stored
  `0600` at `<home>/mint/nostr.key`. The seller's identity can then rotate without orphaning
  every outstanding credit. Rotating the mint key is out of scope: its npub **is** the mint
  identity, and changing it means issuing a new mint.

Kinds `23410` and `23411` don't collide with `kinds.rs @135e4ea` (3400–3407, 30340). Both are in the
ephemeral range, which relay.maxplayer.ai already passes through (tested, §9).

## 3. The wire protocol — kinds `23410` (request) / `23411` (response)

A Maxplayer-specific extension, **not a NUT**. It's versioned so it can be proposed upstream later.

### 3.1 Envelope

- **Request:** kind `23410` (ephemeral range, so relays don't store it), `["p", <mint hex>]`,
  content = NIP-44 v2 ciphertext to the mint key. Signed by a **per-session ephemeral client key**,
  so relays can't tie requests to a wallet identity. Plaintext:
  `{"v":1,"id":<request id>,"op":<op>,"body":<NUT JSON body>,"exp":<unix>}`.
- **Response:** kind `23411`, `["p", <client ephemeral pubkey>]`, `["e", <request event id>]`,
  NIP-44 to the client and signed by the **mint key**. The connector rejects any response not
  signed by the pinned mint npub. Plaintext: `{"v":1,"id":…,"ok":<NUT JSON>}` or
  `{"v":1,"id":…,"err":{"code":<NUT error code>,"detail":…}}`.
- **`id`** is `sha256(op || canonical body)` for writes (swap and restore), so a retry of the same
  swap is the same request by construction, and random for reads.
- The client subscribes for responses **before** it publishes, on every relay, and takes the first
  valid response. The mint processes a request once no matter how many relays deliver it (§3.3).
- **Limits:** NIP-44 caps plaintext at 65,535 bytes. Measured (§9): a swap costs ~400 B per proof
  each way, so 128 inputs + 128 outputs = 50.9 KB request / 40.0 KB reply; 256 does not fit. The
  mint sets CDK `with_limits(128, 128)` and wallets split larger swaps. Requests past
  `exp` or older than 120 s are dropped unanswered.

### 3.2 Operations — the entire surface

| op | NUT | who | note |
|---|---|---|---|
| `info` | 06 | anyone | also the health check (§5.2) |
| `keys` / `keyset` / `keysets` | 01/02 | anyone | |
| `swap` | 03 | holder | the only way credits move; the double-spend check happens here |
| `checkstate` | 07 | holder | |
| `restore` | 09 | holder | returns signatures only for blinded messages the caller already has; safe to open |

**Not on the wire:** mint quotes and mint (NUT-04), melt (NUT-05), WebSocket subscriptions (NUT-17)
and anything with auth. `info` advertises only what's above. Any other `op` gets `err`
`unsupported`. There's no gateway to misconfigure because the listener has no route to anything
else: it calls `Mint::process_swap_request`, `check_state`, `pubkeys` and friends directly (cdk
`mint` feature, `cdk-0.17.2 src/mint/`).

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

## 4. Changes to existing code

### 4.1 Connector dispatch (`nostr://` → Nostr, `https://` → HTTP)

Every place that constructs a mint connector goes through one factory, `mint_connector_for(url,
home)`:

- `buyer_fund.rs:104 @135e4ea`: `Wallet::new(mint_url, …)` → `WalletBuilder::new()…shared_client(
  mint_connector_for(…))`. This is `open_wallet_at_mint_async`, which the seller receive path uses
  (`seller_node/run.rs:9434`).
- `payment_wallet.rs:1849`: `HttpClient::new(wallet.mint_url…)` for the verifier.
- `doctor.rs:92`: `HttpClient::new(url, None)` probe → `info` over relays for `nostr://`.

**Acceptance:** a `nostr://` mint never produces an HTTP request. This is proven with a test
connector that panics on any HTTP call and a netns run with outbound HTTP to anything except the
relay denied.

### 4.2 Fences that key off the URL

- `home::mint_allowed` (`home.rs:1804`) requires `https://` or the default testnut. Add: a
  `nostr://` mint is allowed **iff** it's in `[credits] accepted` or it's the seller's own mint,
  independent of `allow_real_mints`.
- **Cross-mint hops** (`crossmint.rs:94` `plan_payment`, `:160` `select_source_mint`): a credit mint
  is never a hop source or target, since it can't melt or take Lightning. If a buyer holds only
  credits at a mint the seller doesn't list, the plan is refusal, not a hop.
- **Fee remit** (`fee_remit.rs`) melts to `PLATFORM_FEE_ADDRESS`. It must select only Lightning
  mints (`https://`). That's enforced by type, not by the melt failing. If a seller holds only
  credits, fees accrue unpaid and retry as today (settled input 6).
- **Balances** (`wallet_ops::MintBalance`) are already per mint. Add a `kind: lightning | credit`
  column and show two totals in `wallet balance`. Never sum them into one number.
- **Budget gate:** credit spend counts against the buyer's sat budget like any other spend. Same
  unit, same cap (settled input 17).

### 4.3 Config

```toml
[credits]
issue = true                       # this seller runs a mint (set by `credits enable`)
accepted = ["nostr://npub1…", …]   # other issuers' mints this seller takes
relays = []                        # empty ⇒ [relay_url] + 1–2 public fallback relays
rate_limit = 20                    # requests/second the mint serves; over it ⇒ "rate_limited"
```

The `rate_limited` reply is a definitive error, since the mint did not run the request. The public
fallback relays are picked in stage 3, after checking they accept and deliver kinds 23410/23411
with NIP-42.

The seller's own mint is always accepted when `issue = true`. `accepted_mints` keeps its meaning.
The creq and heartbeat mint lists become `accepted_mints ∪ online credit mints` (§5.2).

## 5. Seller behavior

### 5.1 Enable and issue (local only)

- `maxplayer credits enable` exists only in a `credits-mint` build (settled input 12). In the
  default binary it prints how to get a build with the mint. It generates the mint Nostr key, creates
  `<home>/mint/mint.sqlite` and a seed, and adds the `credit_mint` tag to the next heartbeat. It refuses if `<home>/mint/` already exists: never
  re-initialize a seed under an existing identity. It prints the backup warning (settled input 9):
  losing `<home>/mint/` makes every outstanding credit worthless, and restoring an old copy can let
  spent credits be spent again.
- `maxplayer credits issue <amount> [--to-file]` mints proofs in-process to a local token file. The
  mechanism is a local-only CDK payment processor that auto-settles incoming requests created by
  this process. The processor has no network surface and its quote ops aren't on the Nostr listener
  (§3.2). Each issuance has a unique id with journaled outputs, like the pilot's `issue.mjs`, so an
  interrupted issue is reconciled and never re-issued blind. No quota.
- The listener runs inside `seller_node::run` as an actor next to the ingester and publisher, on
  the same relays and connection pool. Mint keys never leave the node process.

### 5.2 Online gate (settled input 7)

The seller keeps `last_ok` per accepted credit mint, refreshed by an `info` ping every 60 s. At
claim time (`claim_offer`, `seller_node/run.rs:6762`; creq built at `:6853`), a credit mint goes
into the creq **only if** `last_ok` is within 180 s. The seller's own mint is always online. The
buyer can then only pay at a mint that was answering when the seller committed to the work.

If the mint drops after the claim, collect's receive retries on the existing backoff until the mint
answers. The pending-receive breadcrumb (`append_pending_receive`, `run.rs:9458`) already makes that safe.

### 5.3 Advertisement (settled input 8)

- Issuer heartbeat (`heartbeat.rs`, kind 30340): a new `["credit_mint", <npub>, <relay>…]` tag,
  omitted when the seller doesn't issue, so existing beats are byte-identical.
- `maxplayer mints list` reads 30340 beats, pings each advertised mint's `info`, and shows its npub,
  issuer, verified or unverified binding (§2.2), and whether it's online now.
- `maxplayer mints add <npub>` appends to `[credits] accepted`. Nothing is added automatically.

## 6. Buyer behavior

- `maxplayer wallet receive <token>` works for `nostr://` tokens through the connector. The swap at
  the issuer is what makes the credits the buyer's.
- The pay path is unchanged. The buyer pays at a mint in the claim's creq. If it holds credits at a
  listed credit mint, that's a direct pay. The award filter treats a claim whose creq lists no mint
  the buyer can pay directly or hop to as unpayable (credits never hop, §4.2).
- `wallet send` makes transferable credit tokens (settled input 3).

## 7. Stages (each a separate PR, all behind `[credits]`, default off)

1. **Identity and fences.** `nostr://` parsing test, `mint_connector_for` with HTTP only, and the
   §4.2 fences (allow-list, crossmint exclusion, fee-remit Lightning-only, balance kind). No
   behavior change for `https://`.
2. **Embedded mint and local issue.** A `credits-mint` cargo feature, off in the default release,
   enabling cdk `mint` plus the cdk-sqlite mint store, `credits enable` and `credits issue`.
   In-process only, no network. Devshell and CI add protobuf and build both with and without the
   feature. The default release build does not need `protoc`.
3. **Nostr listener and connector.** Kinds 23410/23411, the rate cap, the fallback relay list, and
   tests on the in-process relay (`nostr-relay-builder`, already a dev-dependency).
4. **Seller accept path.** `[credits] accepted`, the online gate, the heartbeat tag,
   `mints list/add`, and seller receive at a foreign credit mint.
5. **Buyer path.** Receive, send, balances by kind, pay and award filter.
6. **End to end.** Two sellers, A issuing and B accepting, and a buyer holding only A's credits. B
   does a job, is paid in A's credits and pays its fee in real sats. Run on a host behind NAT with
   no DNS or inbound ports, with outbound HTTP to anything but relays denied.

### 7.1 Required failure tests

Duplicate requests across relays (one swap executes). Replayed, stale, forged or wrong-key
responses (rejected). Reply lost after commit, on both the receive and the send side
(`recover_incomplete_sagas`, never a blind re-`receive`). Rate cap enforced. A default build contains
no mint code. Mint or wallet killed at
each write boundary. Two holders spending the same proofs at once (one wins). All relays down (clean
error, no fallback). Oversized or malformed requests. Unsupported ops, including NUT-04 and NUT-05
(refused). Fee remit never melts at a credit mint. Credits never selected for a hop. `credits
enable` refuses over an existing mint. **No HTTP call is ever made for a `nostr://` mint.**

## 8. Open questions

None. The earlier questions (budget, self-acceptance, relays, fees) were answered as settled inputs
12–19.

## 9. Pre-code test results (23 Sep 2026, CDK 0.17.2, throwaway crate)

- **`nostr://` identity:** survives parse/serde, creqA/creqB, tokens, full wallet lifecycle and
  restore (§2.1). Wallets need `use_http_subscription()`.
- **Lost swap reply:** mint commits, reply dropped → `recover_incomplete_sagas` recovered all 300.
  Request dropped before the mint → recovery replayed it, 300 recovered. Naive re-`receive` before
  recovery → balance 0 until full seed `restore()`. Sender-side lost reply NOT yet exercised (the
  test send needed no swap).
- **Relay (relay.maxplayer.ai, fresh one-off keys, NIP-42 auto-auth):** kinds 23410/23411 accepted
  and delivered, not stored afterwards. Round trip from this VPS: ~55 ms small, 101 ms at 16 KB,
  200 ms at 51 KB, 219 ms at the 65 KB NIP-44 max. Echo only, one run.
- **Build cost:** cdk `mint` feature hard-enables `cdk-signatory/grpc`, so **`protoc` is required**
  and cannot be switched off without patching CDK. Toy binary: wallet-only 10.4 MB / 367 s cold,
  wallet+mint 17.1 MB / 479 s (+6.3 MB, +30 crates incl. axum, tonic, prost). The mint goes behind the
  opt-in `credits-mint` cargo feature (settled input 12), so only that build needs `protoc`.
