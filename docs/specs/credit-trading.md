# Credit trading — owner-directed, fixed-lot Cashu swaps

**Status:** design spec; no runtime code changed. **Anchor commit: `8c3bf74f87ca4e5d0f7b0bff64c41a7073ac849e`**
(= freshly fetched `origin/main`, 2026-10-07). All repository citations use this single anchor,
re-derived by grep. The local throwaway probe is not product implementation. Proposed trade kinds,
commands, flags and journal extensions below do not exist yet.

Builds on [seller credits](seller-credits.md), now merged: #1084 spec (`a381dc4`), #1085
Nostr transport (`f31731b`), #1086 sidecar (`1dfc3a0`), #1087 e2e (`7d01bdf`), on 2026-10-02.
Follow-ups #1077 (per-mint award reservations), #1088, #1091 (mints-add hint), #1092
(private jobs accept `nostr://`) and #1100 (credits-first selection) are also on main, released
as v0.6.1-rc7. This replaces the unmerged #1030/#1034/#1036/#1037 dependency framing.
The older seller-credit spec is historical design; current anchored source wins where it differs.
Open integration issues: #1035 (persisted request-exp recovery fence), #1090 (flaky sidecar test),
#1101 (sealed `delivery_mint` at pay). This documentation refresh does not resolve them.

## 0. Settled inputs — not relitigated here

Bob, Discord **#cashu-token-marketplace, 24 Sep 2026**; go at **17:16 UTC**:

1. Any holder can sell: issuer or reseller. Credits must already be issued and in the holder's
   wallet. No print-on-demand and no listing beyond held, unreserved inventory.
2. Two-way: buy credits with sats, or sell credits back for sats. One trade type:
   **give X of asset A, want Y of asset B**. Asset identity is **(mint URL, unit)**, never unit alone.
   Both credit mints and bitcoin-backed mints can use `sat`.
3. A buyer expects to use a seller later and buys its credits at a discount; one credit pays for
   one sat of work at an accepting issuer. This is not a promise of bitcoin redemption.
4. Fixed lots only. No bids, auction, matching engine or partial fill.
5. No trade fee now. **Job platform-fee behavior stays exactly as today**, including credit-paid
   jobs. No fee-rate, accrual, remit, default-mint or failure-policy change.
6. Pilot credit mint is the opt-in `maxplayer-mint` sidecar over `nostr://`.
7. Payment may use any mint the credit seller accepts. The 2026-10-07 Q4 update below settles
   the quote-time capability rule; it replaces the older tested-compatibility requirement.
8. Both buying and selling require an owner's direction through CLI or an MCP call made for that
   direction. No autonomous speculation. Market scanning before a job payment is future work.
9. Integrate in `maxplayer`; no separate order-book service, seller HTTP server or public HTTPS.
   Signed listings live on `relay.maxplayer.ai`; immutable terms, signed status revisions only.
   Negotiation uses encrypted relay messages. Reuse budget, ledger, recovery and doctor.
10. Distinguish relay failure from an empty market. Listing is not endorsement; the buyer's owner
    chooses whom and which mint to trust. Rocky's HTTP book, loopback-only fence, per-buyer policy
    file and ManySails `/v1/restore` change are superseded, not dependencies.

**Settled update — Bob, #cashu-token-marketplace, 2026-10-07 (request 19:37 UTC):**

- **Q1 agreed:** taker generates the secret and locks first; maker locks second.
- **Q4 settled:** any payment mint the credit seller accepts is eligible if reachable info
  advertises NUT-14 and NUT-07 at quote time. A tested-only allowlist is too tight. Advertisement
  is not proof of working claims/refunds/witnesses; the separately authorized canary tests those
  behaviors for its particular mint/configuration, not every eligible mint forever.
- **Q5 settled:** listings live 24 h, then republish with fresh backing/status checks; trade-request
  dedup lasts 30 days. Keep unresolved money records until reconciled, even beyond retention.
- **Q6 settled:** deploy persistent 3410/3411 under buzz `Scope::MessagesWrite` before any client
  uses the feature; 23412 stays ephemeral. At this anchor all three are unallocated in product
  constants; 3410 appears only as an unknown-kind negative test. Global registry review remains.
- **Q7 settled:** canary capped at 100 sats principal + 10 sats mint fees, one trade in flight;
  **Bob's separate go is still required** before running it.
- **Late-claim squeeze accepted (Bob, 2026-10-07 20:29 UTC):** no changes to the sidecar mint or
  to NUT-14 semantics (for example refusing receiver claims after locktime). The residual risk in
  §3.2 stays: a taker can still claim after the maker's deadline while the maker lock is unspent,
  so a maker offline from around the short deadline until near the long deadline can lose its side.
  Mitigation stays client-side: maker recovery refunds its own lock promptly once the short
  deadline plus clock margin passes.

**Settled — Bob, #cashu-token-marketplace, 2026-10-07 19:40 UTC** ("ok great" in reply to
"Are 60 min / 15 min / 60 s OK?"): **Q2** taker lock 60 min / maker lock 15 min (nominal minimum
45 min recovery gap; clock/cutoff qualifications in §3.2); **Q3** quote hold 60 s, one open quote
per taker and four per maker.

Inherited from #1030: acceptance is opt-in and transferable credits may pay **any** seller that
accepts that mint, not exclusively the issuer. Issuers retain mint backup/key-loss risk. Credits
have no expiry or revocation. A **listing** or **HTLC** deadline does not expire the credit asset.

## 1. Asset and lot model

`Asset = {mint_url: canonical Cashu MintUrl, unit: "sat"}` in v1. Retain the explicit unit even
though only `sat` is initially supported. Reject unsupported schemes, malformed `nostr://` identities,
noncanonical/ambiguous aliases, zero/overflow amounts, same-asset swaps and unknown protocol versions.
Use existing configured-mint and real-mint fences, not a new loopback rule. Transport is not backing:
`nostr://` does not itself mean credits, nor does `https://` guarantee bitcoin reserves.

**Maker** posts the lot; **taker** accepts it. Never equate maker with issuer or taker with bitcoin
payer. Example: maker gives 1,000 seller-A credits and wants 800 Minibits sats. Reverse direction:
maker gives 800 Minibits sats and wants 1,000 seller-A credits; the taker is then the credit holder.
An issuer buying back credits is an ordinary taker/maker, not an automatic redemption facility.

One lot binds exact assets and net amounts. To accept several payment mints, an owner may publish
separate fixed lots, but **each needs independent backing**; the same credits cannot back several
listings. No dynamic exchange-rate substitution. Outbound assets, including sats on buyback lots,
are reserved on listing. Incoming credits on a buyback must already be held by the taker before it
can accept; the trade path never calls issuance. A reverse lot advertises backed sats, not credits
owned by the maker; the credit-selling taker reserves its already-issued credits before accepting.

## 2. Listings, signed lifecycle and discovery

### 2.1 Proposed wire format

Use two new ordinary persistent kinds, **3410 CREDIT_LOT** and **3411 CREDIT_LOT_STATUS**, plus
**23412 CREDIT_TRADE** for encrypted, ephemeral negotiation. These numbers are proposals pending
registry collision review; they do not reuse job OFFER, RECEIPT, REVIEW or mint-operation kinds.
Existing constants include 3400–3409 and 30340
(`crates/maxplayer-core/src/kinds.rs:44 @8c3bf74`, `crates/maxplayer-core/src/kinds.rs:49 @8c3bf74`).
All new events carry `t=maxplayer`, `v=1`; credit content also has `trade_v=1`. This is an additive
extension, not a change to job protocol interpretation.

3410 is signed by the maker's configured identity. Event id is the lot id; its bounded JSON content:

```json
{
  "trade_v": 1,
  "give": {"mint_url": "nostr://<credit-mint-npub>", "unit": "sat", "net": 1000},
  "want": {"mint_url": "https://mint.minibits.cash/Bitcoin", "unit": "sat", "net": 800},
  "expires_at": 1791488220,
  "deadline_policy": {"long_seconds": 3600, "short_seconds": 900, "min_gap_seconds": 2700},
  "fee_policy": "sender-funds-net-v1"
}
```

This schematic uses the settled Q2/Q3 timing policy and a placeholder npub, not a valid token.
Listing expiry is publication +24 h; republishing uses a new id after checking backing and retiring
the old lot (never two live lots backed by the same proofs). Single-letter tags `g` (give mint),
`w` (want mint), `u` (give unit), and `x` (want unit)
support standard Nostr `#g/#w/#u/#x` filters; `expiration` carries listing expiry. Content is
authoritative and tags must agree. Expiry stops new quote acceptance, not recovery or completion
of a previously authorized swap. Optional
issuer/seller identity is display-only; verify mint acceptance independently. No proof, wallet
balance, preimage or bearer token is public. Limit listing content to 8 KiB and tag counts/lengths.

3411 tags reference `e=<lot id>`; content is `{trade_v, lot_id, seq, prev, status}`. Only the original
maker may sign it. `seq=1`, `prev=<lot id>` starts `available`; successive revisions hash-chain to
the prior status event. Only `available`, `sold`, `cancelled` are legal. No term/price edits: cancel
and create a newly backed lot. Terminal status cannot revert. Same-sequence forks or broken chains
quarantine the lot instead of choosing a convenient branch. Store all revisions, not replaceable
terms that hide history. Bound chain traversal and page fetches.

A lot without its initial valid `available` revision is not discoverable as tradable. Repetition of
`available` can announce recovery of availability without changing terms. **In-flight reservation is
local/private**, not a fourth public status. `available` is therefore a maker statement, not an
execution guarantee; quoting can return `busy`. Successful swap marks sold; failure can make the
same unchanged lot available only after confirmed refund and rebuilt backing, otherwise cancel.

### 2.2 Inventory and single-fill guarantee

Before signing a listing, take the existing home/wallet cross-process exclusion and atomically
reserve concrete unspent proofs and fee cushion in the wallet's durable reservation domain. Include
all other listings, job payments, sends, melts and pending swaps in available-balance computation.
Recheck NUT-07 and selected keysets. Fail closed if the wallet cannot make a disjoint allocation;
never interpret pending as free. Do not reserve merely an amount in an unrelated order-book file.

A local lot row tracks `listed -> quoted -> taker_locked -> maker_locked -> settling -> terminal`;
these are recovery states, not public event statuses. At most one active swap id per lot, enforced
by a durable uniqueness constraint/CAS, not daemon memory. Publish only after reservation commit;
failed/ambiguous publish retains reservation until cancellation/reconciliation. The signed event
outbox is durable. Cancellation stops new quotes immediately, but cannot revoke an existing HTLC
or release its proofs before successful recovery. A local balance drop cancels/quarantines listings
before new quotes. Other wallets using copies of the same seed remain outside this guarantee.

### 2.3 Discovery contract

Fetch listings and complete valid status chains from `wss://relay.maxplayer.ai`, bounded by mint,
author, age and pagination. Show expiry, freshness, trust warning, assets, net amounts and indicative
mint costs. Never infer available from an absent cancellation. Fetch matching status history before
quoting, then ask the maker for a fresh quote/reservation. Maker availability is not implied by a
recent listing. Discard expired listings even if the relay still stores them. Relay retention must
cover the full 24 h advertised lifetime and status chain; clients fail closed on history gaps.

CLI/MCP result distinguishes `ok` with zero validated lots after EOSE, `relay_unreachable`,
`timeout_or_partial`, `invalid_or_quarantined` and cached/stale results. EOSE is not proof the relay
is honest or complete. Never substitute cache as a live executable quote. No other public listing
relay is assumed to work; losing this single relay can halt discovery and negotiation.

## 3. Swap decision: taker holds secret, taker locks first

**Decision A: settled Q1 (Bob, 2026-10-07), replacing the earlier seller-secret proposal.**
The **taker** samples a fresh random 32-byte secret `s`, persists it privately, and sends only
`H=SHA256(s)` until claiming. The taker locks its `want` asset first with the **long** deadline.
The maker verifies that lock, then locks its `give` asset with the **short** deadline. The taker
claims first, exposing `s`; the maker learns it via NUT-07 on the maker's outgoing mint or the
signed encrypted taker notice, verifies its hash, and claims the first lock.

| Choice | Inventory grief / option | Mint-availability and offline risk |
|---|---|---|
| Earlier: maker/seller secret, credits locked first, long; taker sats second, short | A stranger can solicit timed credit locks without committing funds. Rate limits do not remove Sybil free options. | Taker must learn secret from payment mint/notice and claim credits during the gap; credit mint outage can hurt taker. |
| **Selected: taker secret, taker locks first long; maker second short** | Taker must actually immobilize value before maker creates a timed lock. Short quote holds still admit bounded DoS, and a funded taker retains an option to abort. | **Maker** must observe its outgoing mint's claim witness and redeem at the taker's mint during the gap. Taker/that mint can withhold the notice/witness; maker can lose if outage lasts too long. Taker carries first-lock unavailability/capital-lock risk. |

The selected order is better for publicly listed inventory; it does **not** eliminate grief or turn
independent custodial mints into trustless consensus. Direction is symmetric: in a sell-back,
taker may lock **credits** first and maker locks **sats** second. Never hardcode deadline order to
"credit" versus "bitcoin". The credit buyer still preflights the credit mint before either party
locks, and the maker must accept the taker's exact funding mint under Q4, including the
residual risk that advertised witness support fails.

**Decision B: issuer trust is unavoidable.** An issuer selling its own credits controls their mint.
It can take payment and then refuse a claim or subsequent spending, forge state, selectively reveal
witnesses, roll back a database, or shut down. No HTLC protocol fixes this. Taker-first ordering
makes an honest claim precede payment redemption but cannot make a dishonest issuer's reported
claim or newly issued outputs valuable. The swap protects against counterparty crashes and
third-party resellers **conditional on honest, sufficiently available mints and recovery**; it is
not unconditional protection against either mint. Owners explicitly choose issuer exposure.

### 3.1 Preflight, quote and exact locks

Before **any** lock, both parties preflight both assets: configured URL/identity, reachable info,
active and input keysets, advertised NUT-14 and NUT-07, fee schedule, amount and proof limits,
and clock health. Bitcoin payer specifically verifies the credit mint. Refuse unreachable info or
missing/false NUT-14/07 advertisement at quote time; do not restrict to previously tested mints.
Owner trust and seller acceptance remain mandatory. DLEQ and exact conditions are verified when
proofs arrive. An info flag is not proof of witness retention or correct refund behavior. Known
observed failures pause that mint and surface an error; lack of a prior test alone is not refusal.
The capped canary exercises claim, refund and witness recovery on its selected mints, reducing
implementation risk without certifying other mints or future behavior. Never fall back to an
unsecured send. Doctor exposes the failed mint/op distinctly from relay failure.

1. Taker requests quote with fresh request id and owner caps. Maker signs a quote binding lot id,
   both peers, exact assets/net/gross amounts, keysets/fees, `H`, per-swap receive/refund public keys,
   proof-count limits, absolute deadlines, short claim cutoff, and quote expiry (anchored to quote issuance, never acceptance). Both peers persist
   its hash. No party signs or spends against just a mutable display price.
2. Quote issuance acquires a **60-second soft hold (settled Q3)** on that lot, bounded
   to one open quote per authenticated taker identity and four per maker. Acceptance consumes the
   same window, not a new 60 seconds. It does not create an HTLC. Failed/expired quote releases only
   the soft hold; the lot remains wallet-backed. Process duplicate quote ids before rate limits;
   repeated identical requests do not renew hold or deadlines. New requests face normal admission.
3. Taker commits cap/ledger reservation and recovery intent **before** creating its long lock.
   Taker gives maker the locked proofs privately. They bind `H`, exactly one maker receive key,
   taker refund key, threshold 1, explicit SIG_INPUTS, and long locktime. No arbitrary extra keys,
   duplicate condition tags, mixed conditions or unconditional proof may pass validation.
4. Maker validates token mint/unit, exact net-after-claim-fee amount, DLEQ against the named keyset,
   every condition, proof uniqueness and fresh NUT-07 UNSPENT state. **State alone is not proof of
   issuance.** Per-swap receive keys and stored proof-Y commitments prevent reusing a first lock
   across lots. Maker rejects late first-lock delivery; taker then waits for its refund.
5. Only now maker commits its budget and journal, CASes the lot into in-flight, and locks reserved
   outgoing funds to `H` + taker receive key + maker refund key at the short deadline. Taker checks
   all the same properties independently before redeeming. No locked proof is normal balance.
6. Taker claims short lock with preimage plus receiver signature into its own wallet, redeeming the entire lot in one mint swap (no incremental claims). Persist
   the exact mint request, blinded outputs and unblinding data before sending it. After commit,
   notify maker; do not publish the preimage as listing/status content.
7. Maker polls NUT-07 for the stored Ys of **its own outgoing proofs**, validates witness preimage
   against `H`, and redeems long lock with its receive signature. Notice is a latency optimization;
   mint witness recovery is mandatory when the taker/notice is offline. Treat SPENT without a
   matching preimage as unresolved (it may be a refund), never as successful swap completion.
8. Each side marks terminal only after its own incoming outputs and outgoing mint states are
   reconciled. Maker publishes sold when both legs are verified; taker can already show its own
   received funds if maker completion is pending. Status publication is never settlement authority.

```mermaid
sequenceDiagram
    participant T as Taker (secret s)
    participant TM as Taker outgoing mint
    participant M as Maker (listed lot)
    participant MM as Maker outgoing mint
    T->>M: Quote request, H(s), caps, keys
    M-->>T: Bound quote + short soft hold
    Note over T,MM: Both preflight both mints; durable records before effects
    T->>TM: Lock want asset, maker key + H, taker refund, long
    T->>M: First locked token (encrypted)
    M->>TM: Verify keys, DLEQ, state, conditions, deadlines
    M->>MM: Lock give asset, taker key + H, maker refund, short
    M->>T: Second locked token (encrypted)
    T->>MM: Claim with s + taker signature
    T-->>M: Claim notice (optional delivery)
    M->>MM: NUT-07: spent Ys and witness containing s
    M->>TM: Claim with s + maker signature
    M-->>T: Reconciled completion; signed sold status on relay
    Note over T,MM: On abort, refund keys actively swap after respective deadlines
```

### 3.2 Short windows, not automatic refunds

**Q2/Q3: settled (Bob, 2026-10-07 19:40 UTC).** From the same signed quote timestamp: taker/long lock = **+60 min**,
maker/short lock = **+15 min**, nominal gap **45 min**; quote admission/soft hold **60 s**.
Do not restart either deadline when the second lock is created or a request is retried. Maker
must initiate its lock within the original quote window after validating the first lock; a late
first lock is refunded, never grounds to extend a quote. Both verify the exact absolute deadlines.

The happy path completes in seconds. Deadlines matter only when something fails: shorter windows
reduce how long funds stay stuck on an aborted trade, but reduce how long a seller restart, relay
outage or payment-mint outage the maker can survive after the secret is revealed without losing
its side. They are recovery budgets, not required waiting periods before successful settlement.

**Proposed claim cutoff:** taker refuses to initiate a claim with **less than 3 min remaining on
the maker lock**, and cancels/drops retries once that cutoff is reached. Its prepared claim request
must have persisted `exp` no later than **short − 2 min** (and no later than its actual caller
wait); use an appropriately shortened call window, never publish an already-expired request.
This leaves 1 min for request delivery/processing before that expiry, plus 2 min for clock margin.
Maker also refuses a second lock unless at least 3 min remain and the full deadline gap holds.
The bounds are conservative proposed margins, to be validated by the stage-3 two-mint fault lab,
not measured production latency guarantees.

With accurate clocks a claim before short leaves at least 45 min until long. To preserve that
minimum under the proposed clock bound (each mint within 60 s of the agreed reference, hence up
to 120 s relative skew), stop executable claims by short − 2 min. This gives at least 45 min
from an on-time claim to the earliest long refund under those bounds. Mint request admission,
queueing and execution must respect that fence; if execution can continue after `exp`, account
for that bounded processing time too or refuse the timing policy. The local probe does not prove
this cross-mint operational bound. **“Always 45 min” is conditional on these bounds and compliant
clients**, not an HTLC-enforced guarantee: pinned CDK permits late receiver claims after expiry.
A malicious/late taker or unbounded clock/processing error can defeat that margin. Maker must
monitor/refund promptly and accept this residual risk; unconditional atomicity is not claimed.

Synchronize clocks, refuse new locks on detected skew above 60 s, record observed mint info time
when supplied, and monitor well before expiry. If clock uncertainty cannot be bounded, refuse this timing policy rather than claiming its
45 min margin. Clock health is not a Byzantine-mint guarantee.
Late taker claims can still happen: HTLC receiver claims remain valid **after locktime** and race
refunds. A deadline enables a refund; it does not revoke the claim or transfer funds automatically.
Refund uses the sender's refund-key signature in an explicit swap back to fresh wallet outputs.
Pinned CDK uses strict `locktime < now`; attempt after deadline plus clock safety margin, never
assume equality suffices. Resume deadline work on startup; a sleeping agent past both deadlines
has no safety guarantee. A recovery watcher is a continuation of the owner's authorized trade,
not permission to enter new trades.

## 4. Encrypted messaging, idempotency and crash recovery

23412 uses NIP-44 v2 to the peer's bound identity, signed by the sender; `p` is routing metadata,
not confidentiality. No mint RPC is overloaded with negotiation: 23410/23411 remain mint request /
response (`crates/maxplayer-core/src/mint_wire.rs:41 @8c3bf74`,
`crates/maxplayer-core/src/mint_wire.rs:44 @8c3bf74`). Subscribe before publishing and authenticate
NIP-42 with the same signing identity. Verify signature, peer, lot/quote/swap ids, message type,
version, body hash and expiry before any state transition.

Envelope: `{trade_v, request_id, swap_id, lot_id, quote_hash, step, body, exp}`. Request ids are
random 128-bit minimum; journal key is `(peer pubkey, request_id)` with immutable body digest.
Identical retry returns recorded result; conflicting reuse is rejected and alarmed. Per-swap
state transition CAS prevents a fresh request id from repeating a spend. Persist exact outgoing
signed events and replies. SDK event-level dedup must not suppress legitimate identical retries:
use the transport's raw-message delivery approach. Expired first-seen messages never initiate a
spend; recorded outcomes remain queryable with a fresh status request. Retain dedup/outcomes at
least 30 days and unresolved money records indefinitely; terminal swaps can never be reopened.

Negotiation metadata ≤8 KiB; encrypted token payloads ≤48 KiB plaintext and ≤128 proofs, also
bounded by the tested encrypted event size and both mints' output limits. No multipart v1: refuse
oversized lots before first lock, using estimated worst-case response size, not just input bytes.
Rate-limit decrypted work, peers, outstanding quotes and global queue depth. Ephemeral messages
are not stored: persist locally, retry with bounded backoff and expose offline status. Trade
negotiation stays on the listing relay; inherited mint transport can use its configured fallback
relays. Fallback discovery/trade delivery is not assumed.

**Reuse, extend, do not duplicate:** add trade records/reservation ownership to existing wallet
recovery storage and doctor reporting. Durable record contains role, immutable quote, caps,
assets, state, proof Ys, locked proofs, exact prepared mint requests/outputs, key derivation refs,
secret (taker only, protected), deadlines, request outcomes and ledger attempt ids. Restrict files
like wallet keys; redact proofs, preimages and signatures from ordinary logs/MCP. Outbox and
wallet reservation cannot diverge across crash boundaries; reconciliation checks mint before
releasing either. No second service, budget database or seed store.

| Failure / boundary | Taker action | Maker action / invariant |
|---|---|---|
| No quote, unsupported mint, cap refusal | No lock or outgoing spend | Reject; no fee; release only soft hold |
| Two takers or duplicate request | Same id resumes; loser gets busy | Unique active-swap CAS; same proofs never back two locks |
| First lock created, maker never responds | Hold and refund after long deadline with refund signature | Never lock after quote acceptance window; no credit debit |
| Maker lock committed, reply lost | Fetch swap status, recover original token; do not pay again | Replay recorded second token; retain its reservation |
| Taker disappears after second lock, no claim | Refund own long lock when eligible | Refund own short lock when eligible; not merely mark expired |
| Taker claims, notice lost/offline | Persist incoming outputs; retry notice | Read maker-outgoing-mint NUT-07 witness, validate H, claim long lock |
| Outgoing mint hides/unavailable witness after claim | Show pending counterparty settlement | Retry urgently; gap is finite. Maker carries selected-order loss risk |
| Claim/refund request times out after possible commit | Keep inputs/outputs ambiguous; query state/restore outputs | Same on either side; never generate a replacement spend from timeout alone |
| Crash before ledger/reservation commit | No external effect was allowed | Same on either side |
| Crash after journal but before/after mint swap | Resume exact attempt, reconcile CDK saga + input states + restored outputs | Same on either side; no repeated issuance or implicit unreserve |
| Deadline reached while mint/relay offline | Refund pending, not refunded; retry when reachable | Do not relist or credit ledger until recovery proves funds returned |
| Concurrent late claim/refund | Mint accepts at most one; reconcile loser, including witness | Receiver path survives locktime; do not assume timeout wins |
| Mint SPENT but witness absent/malformed | Preserve evidence, doctor alert | No unsafe assumption about peer claim; capability violation, manual intervention |
| Maker cancellation while in flight | Existing authorized swap continues/reconciles | Stop new takers; cancellation cannot claw back issued locks |
| Keys/seed lost, issuer rollback/fraud | Recovery may be impossible | No protocol guarantee; explicit owner/issuer risk |

For the proposed trade path, mint request expiry must not outlive its caller's actual wait.
This requires threading the remaining deadline into the connector; do not assume current
`request_exp` or dropping a future already enforces the proposed trade-specific cutoff. Transport clock bound is
`crates/maxplayer-core/src/mint_wire.rs:132 @8c3bf74`; its expiry calculation is
`crates/maxplayer-core/src/nostr_mint.rs:392 @8c3bf74`. Main's HTTPS/read mint-call timeout is
`crates/maxplayer-core/src/payment_wallet.rs:33 @8c3bf74`. Do not put a timed HTLC wait inside
that call or let dropping a future mark proofs free. Nostr mutations use the longer
`mint_mutation_timeout` (`crates/maxplayer-core/src/payment_wallet.rs:41 @8c3bf74`), not a universal
5 s bound. Existing saga retirement
(`crates/maxplayer-core/src/payment_wallet.rs:946 @8c3bf74`) needs trade-reservation awareness;
this spec does not claim it already understands HTLCs. Restore retrieves signatures for persisted
outputs, not the counterparty's secret; NUT-07 supplies the claim witness.

**Required #1035 fence (not yet implemented on main):** before any publication, persist exact
request event id, `exp` and saga/trade binding, including all retry requests. Recovery waits until
all persisted expiries plus allowed mint clock skew have passed, then reconciles NUT-07 and
restore before compensation or releasing reservations. Missing request evidence remains held for
manual reconciliation, never guessed safe. Main still derives a hold from `updated_at + 300s`
(`crates/maxplayer-core/src/payment_wallet.rs:61 @8c3bf74`); that estimate is **not** the trade
recovery contract. Test future persisted `exp` despite old `updated_at`, interrupted publication,
late delivery, and empty restore; never drop a live request's saga. #1035 is a prerequisite to
safe trade recovery, not a fix claimed by this spec.

## 5. Fee-inclusive quote and owner caps

**No platform fee on trades.** Mint swap fees still exist. On each mint, each actual swap costs
`ceil(sum(input proof keyset input_fee_ppk) / 1000)` in that mint's unit. No cross-mint summing into
an exchange rate. Pinned fee aggregation is in `cdk-0.17.2/src/fees.rs`; fee-aware splitting is in
`cashu-0.17.2/src/amount.rs` (see §9).
Inspect fee schedules and exact proof selection; an advertised zero fee is not a permanent promise.

Listed X/Y are **net delivered**. Sender funds its preparation/locking fee and enough gross locked
value for the receiver's claim fee, so the receiver obtains the stated net. Quote fixes actual
input/output denominations and counts; solve fee-aware splitting including the claim-input count.
Show, separately for each asset: net amount, gross lock, preparation/lock fee, claim fee, worst-case
refund fee, total wallet debit, change, and maximum possible nonrefundable abort cost. Sidecar's
current default is zero fee, but do not bake zero into validation. No lightning mint/melt or
cross-mint hop is part of a swap. Fees for failed/aborted swaps may remain spent.

Taker supplies `max_debit` **per asset**, `min_receive`, `max_mint_fees`, accepted assets and deadline
bounds. Maker's listing authorization includes fee cushion and maximum debit too. Quote may not
rewrite immutable net terms. Revalidate keysets/fees immediately before each lock; reject changes
outside the accepted caps. After one leg is locked, changed fees mean abort/refund if the other
party cannot honor net terms within caps, never silently deduct from the recipient. Reserve enough
for the displayed recovery bound; if later mint fee changes make refund uneconomic or over cap,
leave `refund_blocked_fee`, alert owner and request a new explicit cap, not automatic overspend.

## 6. Wallet, ledger, budget and unchanged jobs

The credit mint must be configured **before** accepting/receiving:
`maxplayer wallet mints add nostr://<mint-npub>`. This does not change default mint or seller
acceptance. Config distinguishes wallet `extra_mints` from seller `accepted_mints`
(`crates/maxplayer-core/src/home.rs:1746 @8c3bf74`). Seller operators accept their own mint and append
it after the bitcoin fee mint when retaining bitcoin fee remittance; a credits-only seller
can operate but still owes platform fees in real sats. Trade UI does not silently add or trust a mint.

Successful claim swaps locked proofs into fresh ordinary wallet proofs at the acquired asset's
mint. Only confirmed unspent outputs become spendable balance. Existing job payment then uses them
at an issuer/other seller advertising that mint, one credit per sat of job price; an offline mint
still blocks spending. No special credit ledger, denomination, redemption promise or job discount
is added. A discount applies at purchase, not by rewriting a later job's amount.

**Current #1100 behavior:** acquired covering, configured `nostr://` credits are selected before
HTTPS sats for jobs at the issuer or any accepting seller, preserving seller order within each
group (`crates/maxplayer-core/src/crossmint.rs:173 @8c3bf74`). Net availability excludes existing
per-mint award holds; coverage and fee checks still apply. Credit mints are never cross-mint hop
targets (`crates/maxplayer-core/src/crossmint.rs:140 @8c3bf74`), and never melt sources
(`crates/maxplayer-core/src/wallet_ops.rs:785 @8c3bf74`). A sats-only buyer therefore cannot award
a credits-only seller through a sats-to-credit hop: it must acquire accepted credits first.
Buying credits is the proposed paid route to do that (existing transfers/operator issuance are
not a sats-funded job-payment conversion). Trading remains owner-directed, never automatic at award.
Private claims/receipts now validate well-formed mint identities rather than requiring every
listed mint to be buyer-accepted (`crates/buzz/crates/maxplayer-private-protocol/src/wire.rs:112 @8c3bf74`);
planning must still select a permitted mint. Do not restore the obsolete all-listed-mints rule.
Issue #1101 tracks enforcing the sealed delivery mint at pay; this spec neither fixes nor assumes
that open issue resolved. Stage 4 must verify source/reservation/sealed-mint consistency.

Reuse `BudgetGate::check` (`crates/maxplayer-core/src/budget.rs:191 @8c3bf74`), append-before-effect
`authorize_then_attempt` (`crates/maxplayer-core/src/budget.rs:239 @8c3bf74`) and idempotent
`credit_reserve` (`crates/maxplayer-core/src/budget.rs:266 @8c3bf74`). Current check enforces the
configured **per-job cap only**; older `docs/protocol-v1.md` §11 mentions a total cap, but no rolling
total cap is implemented here. Do not invent one or change job behavior in this feature. Apply the
same cap conservatively to each trade's maximum outbound `sat` face-value debit, plus owner quote
caps; explicit asset fields prevent treating credit face value as liquid bitcoin wealth.

Extend the additive spend record (`crates/maxplayer-core/src/budget.rs:87 @8c3bf74`) with defaulted
`purpose=job|credit_trade`, `trade_id`, `asset`, `phase` and reconciliation reference. Existing records
remain job spends. Namespace attempt ids `credit-trade/<swap>/<role>/<phase>` and reconciliation ids
separately; validate immutable amounts/assets on retry, since dedup by id alone is not authorization.
One logical maximum outlay must not be counted again for lock, claim and refund retries.
Incoming gross proofs swapped into net receipts are not another owner-funded debit; their claim
fee was funded by the peer and remains separately disclosed. Reconcile
unused fee reserves and **confirmed returned funds** at most once; do not subtract the value of the
acquired asset or refund on mere expiry. Record acquired assets as trade metadata, not budget credit.
Credit purchases and later job spending both remain visible as separate outlays under existing
face-value accounting; no P&L/net-worth interpretation is promised.

Trade settlement bypasses job `collect`, job receipt creation and seller platform-fee accrual.
Later credit-paid jobs use those paths **unchanged**: platform rate is still 1,000 bps
(`crates/maxplayer-core/src/platform_fee.rs:48 @8c3bf74`), seller fee remains payable in real sats,
and fee accrual/remittance failures behave as today. Tests must prove a trade creates no job fee
and a subsequent credit-funded job still does. This is not a free-job lane.

Doctor extends existing mint probing (`crates/maxplayer-core/src/doctor.rs:88 @8c3bf74`) with
trade capability, reserved/pending/claimable/refundable states and deadlines. Recovery extends the
existing wallet machinery with the HTLC journal; it must not simply call generic saga recovery
and assume balances are safe. Pinned wallet gaps and required refund adapter are in §9.

## 7. Proposed CLI and MCP surface — owner-directed only

CLI namespace `maxplayer credits trade`:

- `list --give-mint ... --give-unit sat --give ... --want-mint ... --want-unit sat --want ...
  --expires ... --max-mint-fees ...` reserves already-held inventory and publishes one fixed lot.
- `discover [--give-mint ...] [--want-mint ...] [--issuer ...]` is read-only, with explicit health.
- `quote <lot-id> --max-debit ... --min-receive ... --max-mint-fees ...` preflights without locking.
- `take <quote-id> --request-id ...` binds the owner's approved quote hash/caps and starts execution.
- `cancel <lot-id>`, `status <swap-id>`, and `recover [<swap-id>]` use the same durable records.

MCP names: `credit_trade_list`, `credit_trade_discover`, `credit_trade_quote`, `credit_trade_take`,
`credit_trade_cancel`, `credit_trade_status`, `credit_trade_recover`. Same schemas, gates and
`MAXPLAYER_HOME` as CLI; no alternative money implementation. Mutating tool descriptions require
explicit owner direction, exact assets, caps and request id. A tool call is an execution surface,
not cryptographic proof of human intent: the invoking harness must have owner approval; no timer,
market notification or job-payment path calls list/take automatically. Listing approval authorizes
its one fixed fill within caps without another prompt per protocol message. Recovery may finish or
refund that existing authorization, never create a replacement trade.

Return quote expiry, asset-qualified debits/net receipts, mint fees, issuer risk acknowledgement,
deadlines and structured refusal/recovery state. Never return bearer proofs or preimages through
normal tool results. Disabling new trading does not disable recovery of outstanding authorized swaps.

## 8. Required relay changes and security boundary

At the anchor, persistent allowlisting lives in
`crates/buzz/crates/buzz-relay/src/handlers/ingest.rs:244 @8c3bf74`; unknown kinds are rejected at
`crates/buzz/crates/buzz-relay/src/handlers/ingest.rs:375 @8c3bf74`. **Stage 2 must add named constants
for 3410 and 3411 in core `kinds.rs` and buzz ingest, and add both to the `Scope::MessagesWrite` arm
of `required_scope_for_kind`, beside the existing review kinds**
(`crates/buzz/crates/buzz-relay/src/handlers/ingest.rs:363 @8c3bf74`). Update the test currently
asserting that 3410 is unknown and retain an unknown-kind negative control. Deploy that explicit
allowlist change before any client uses these trade kinds; do not enable broad open ingest.

23412 is ephemeral: no persistent allowlist addition, WS-only and unstored. WebSocket handling
requires authenticated NIP-42 identity (`crates/buzz/crates/buzz-relay/src/handlers/event.rs:614 @8c3bf74`),
author/auth key equality except gift-wrap (`crates/buzz/crates/buzz-relay/src/handlers/event.rs:637 @8c3bf74`),
and routes ephemeral kinds before persistent ingest
(`crates/buzz/crates/buzz-relay/src/handlers/event.rs:675 @8c3bf74`). Channel-less ephemeral events
fan out globally (`crates/buzz/crates/buzz-relay/src/handlers/event.rs:844 @8c3bf74`). Encryption does
not hide peer tags, timing or traffic volume; application-level size/rate/expiry bounds are required.

Security limits: signatures prove authorship, not solvency or mint honesty; a malicious wallet can
lie about backing. Local reservations constrain Maxplayer, not outside software or cloned seeds.
Validate DLEQ and exact HTLC syntax, not just state; no missing refund key, empty key list, duplicate
tag, extra receiver, reused secret or mixed-proof lock is accepted. Preserve refund keys until
terminal reconciliation and backups; never log secrets. Crash safety needs durable intent before
mint calls and recoverable output blinding data. Use unique per-swap keys derived/journaled through
the existing key system, not public listing keys as universal redemption authority.

Funded takers can still lock inventory, issuers can cheaply issue their own fake-value first legs,
and short quote holds permit Sybil spam. Bound queue/holds, rate-limit and support operator pause;
NIP-42 is authentication, not Sybil resistance. Only accept mints the owner trusts. A relay can
censor, partition, replay or suppress cancellation; quotes and local uniqueness remain authoritative,
while cached discovery can be stale. Single-relay failure can strand negotiation; mint-direct
recovery may continue if its transport remains reachable. No fallback relay durability is claimed.

## 9. Pinned dependency evidence and local probe

Both main's wallet (`crates/maxplayer-core/Cargo.toml:83 @8c3bf74`) and the sidecar
(`crates/maxplayer-mint/Cargo.toml:29 @8c3bf74`) pin **CDK 0.17.2**, not Minibits' deployed version.
Checked the unpacked `cdk`, `cashu`, `cdk-common` and `cdk-sqlite` 0.17.2 sources, not remembered
APIs. CDK's package records upstream source commit `6132607495ae0741e412a63f2acc34e4ccddfc55`.
Source inspection is version-specific, not a claim that newer upstream has no fix.
The refresh audit re-grepped **39 unique repository citations** at the single anchor and
byte-compared the three copied modules (one import adaptation) plus the shared protocol crate.
`git diff --check` passed. Mermaid source was reviewed; rendered GitHub visualization is unverified.

| Claim | Pinned source checked |
|---|---|
| Mint defaults advertise NUT-07 and NUT-14 | `cdk-0.17.2/src/mint/builder.rs`, `.nut07(true)` / `.nut14(true)` |
| Native HTLC condition representation | `cashu-0.17.2/src/nuts/nut10/spending_conditions.rs`, `HTLCConditions`, `Conditions` |
| Native wallet prepares HTLC and receives with signing keys/preimages | `cdk-0.17.2/src/wallet/send/`, `src/wallet/receive/saga/mod.rs` |
| Receive requires matching preimage even for refund | `cdk-0.17.2/src/wallet/receive/saga/mod.rs`, HTLC branch, `PreimageNotProvided` |
| Mint refund branch, witness-type requirement, receiver path after expiry | `cashu-0.17.2/src/nuts/nut14/mod.rs`, `verify_htlc`; `src/nuts/nut10/mod.rs`, `get_pubkeys_and_required_sigs` |
| Spent-state witness retrieved from stored proofs | `cdk-0.17.2/src/mint/check_spendable.rs`, `ys_needing_witness`, `witness_map` |
| Input fee aggregation, single ceil after sum | `cdk-0.17.2/src/fees.rs`, `calculate_fee`; splitting in `cashu-0.17.2/src/amount.rs` |

Sidecar construction is `crates/maxplayer-mint/src/backend.rs:28 @8c3bf74`, configuring the `sat`
unit with defaults at `crates/maxplayer-mint/src/backend.rs:36 @8c3bf74`. Its dispatcher forces NUT-04/05 disabled with empty method lists in `info`, while retaining
NUT-07/14; it passes `swap`, `checkstate`, `restore` to CDK without stripping witness fields:
`crates/maxplayer-mint/src/dispatch.rs:27 @8c3bf74`,
`crates/maxplayer-mint/src/dispatch.rs:53 @8c3bf74`,
`crates/maxplayer-mint/src/dispatch.rs:57 @8c3bf74`,
`crates/maxplayer-mint/src/dispatch.rs:61 @8c3bf74`. Keys/keysets are also served. The matching
connector passes these operations (`crates/maxplayer-core/src/nostr_mint.rs:531 @8c3bf74`,
`crates/maxplayer-core/src/nostr_mint.rs:542 @8c3bf74`,
`crates/maxplayer-core/src/nostr_mint.rs:546 @8c3bf74`). No new sidecar operation, mint/melt endpoint,
HTTP endpoint or ManySails restore change is necessary for the protocol.

### 9.1 Probe setup and result boundary

Throwaway crate: `~/.openclaw/workspace/.openclaw/tmp/credit-htlc-probe/` (not committed).
`cdk`, `cashu`, `cdk-common`, `cdk-sqlite`, `cdk-fake-wallet` pinned to `=0.17.2`, CDK features
`mint,wallet,bip353`. Tests copy sidecar `backend.rs` and `dispatch.rs` plus core `mint_wire.rs`
**from `git show 8c3bf74`**, changing only the dispatch import to the copied module.
The merged wire module also needs `maxplayer-private-protocol`; its whole source crate is copied
unchanged from the same anchor into the tmp probe and used as a path dependency. The first refresh
build exposed that missing harness dependency; adding it changes no production source. Mint uses the
sidecar's actual `MintBuilder` configuration, SQLite and zero-fee unit, not a fake info response.
Test funds are blind-signed locally with disposable keys. CDK's direct in-process connector from
the earlier matching-version probe connects real wallets; no HTTP or relay transport for the swap.

Run with the available Rust toolchain (cargo 1.98.1 for this refresh) and Nix `PROTOC`,
with target/log under the tmp dir; use the repository devshell if cargo is unavailable:

```sh
PROTOC=<nix-protobuf>/bin/protoc CARGO_TARGET_DIR=<probe>/target \
  cargo test --manifest-path <probe>/Cargo.toml --test htlc sidecar_htlc_probe -- --nocapture
```

**Observed result, 2026-10-07 (refresh):** `sidecar_htlc_probe` **1 passed, 0 failed**,
5 copied mint-wire helper tests filtered out; test runtime **5.04 s** after compilation.
Log: `<probe>/refresh-20261007.log`. The copied wire module emits one harmless unexpected
`cfg(feature="gateway")` warning in the standalone harness. Assertions:

| Required item | Result |
|---|---|
| **1. Sidecar advertises NUT-14 + NUT-07** | **PASS** via copied real sidecar `info` dispatcher; both `supported=true`. NUT-04/05 both assert `disabled=true`; no capability-advertisement change needed. |
| **2. Hash + receiver key + locktime + refund key claim** | **PASS**. A native wallet produced the 64-sat HTLC token; raw mint swaps with neither witness component, preimage only, or receiver signature only were all refused. Native wallet receive with the matching preimage and receiver signature returned 64 sats. |
| **3. Refund by refund key only after locktime** | **PASS at mint API**, with a compatibility constraint. The same refund token with refund-key signature and explicit HTLC witness `preimage:""` was refused before its deadline and accepted after it; unsigned refund after expiry was refused. Test deadline was +4 seconds with a bounded real wait past equality, not a production timing recommendation. A signatures-only witness without the `preimage` field was also refused by pinned CDK. |
| **4. NUT-07 witness contains claim preimage** | **PASS** via sidecar `checkstate` dispatch: all claim-input Ys were SPENT and every serialized witness contained the exact preimage. No peer claim notification was used to retrieve it. |
| **5. Native wallet HTLC construction/redemption** | **PASS** for `prepare_send` with `HTLCConditions` and `receive` with preimage + signing key. **FAIL / integration gap** for high-level refund without the secret: `receive` returned `Preimage not provided` even after locktime with the correct refund key. Lower-level signed swap succeeded. |
| **6. Seconds-scale deadlines** | **PASS**: claim uses +10 s and must complete before expiry; refund uses +4 s, refusing early and accepting only after strict expiry. This demonstrates no hours-scale minimum in pinned CDK, not a two-mint timing guarantee. |

**Required integration, not a mint capability failure:** use native CDK HTLC creation and normal
claims. Maxplayer still needs its trade coordinator, exact-condition validation, witness polling,
durable recovery and a narrow **refund adapter** (or separately verified upstream wallet fix).
On the pinned version, the adapter must make an HTLC witness with an **empty preimage field** plus
refund-key signatures, not a signatures-only P2PK witness. This reveals no secret and is accepted
only after locktime. Persist blinded outputs before the swap, unblind/store returned signatures,
and reconcile ambiguous outcomes through existing wallet storage; never fabricate an unrelated
preimage or release inputs locally in place of a refund. The probe demonstrates mint acceptance,
not an implemented/restart-safe Maxplayer refund adapter. A future dependency upgrade must rerun
this matrix rather than assume identical witness semantics.

**Coverage limits:** one local honest zero-fee sidecar mint, direct wallet connector, actual
sidecar dispatch for info and raw swap/checkstate. Not a two-mint atomic-swap implementation;
not killed-process recovery, relay encryption/delivery, production clock skew, real-money,
fee-bearing proof splitting, key rotation or live Minibits HTLC compatibility. Stage 3 must prove
those local recovery paths; stage 5 alone may exercise explicitly authorized real money. No
production relay writes were performed. This doc PR ran no repository runtime suite because it
changes no runtime code.

**Minibits info only, 24 Sep 2026:** GET `https://mint.minibits.cash/Bitcoin/v1/info` returned
`cdk-mintd/0.17.6` and supported NUTs 7/10/11/12/14. Saved response locally with the probe.
This did **not** prove claims, refunds or witness retention at Minibits, and is not approval for a
live swap. Historical info is not current quote preflight. Lack of prior compatibility testing does not
refuse an otherwise eligible mint under Q4; advertising still does not prove behavior.

Related read-only issue search found [cashubtc/cdk#2252](https://github.com/cashubtc/cdk/issues/2252),
which discusses signatures-only HTLC refund witnesses and other implementation divergences.
Do not treat that discussion as a patch in the pinned version. This task did not audit latest CDK
or other mint implementation source; the concrete claims above are pinned-source/probe claims.

## 10. Staged implementation PRs and required failure tests

Every stage has a **default-off** gate. Proposed `[credit_trading] enabled=false` is the umbrella;
subgates below also default false. Existing job/wallet defaults are unchanged. Closing an admission
gate stops new trades but leaves safety recovery of already-authorized trades available. Each stage
must rebase/re-grep anchors against its then-current main; do not merge this spec as code.

| Stage / default-off gate | Scope | Required failure tests before advancing |
|---|---|---|
| **1. Probe-backed mint capabilities** / `capability_checks` | Port the pinned local tests to maintained fixtures; doctor + quote-time checks; select/journal compatible refund handling | Missing NUT-14/07; advertised-but-missing witness; unreachable mint vs relay; false/unsupported info; wrong keyset/URL/unit; missing signature/preimage; early/late refund, strict boundary, malformed/duplicate conditions, wrong refund key; native refund gap; no spend during preflight |
| **2. Listings + discovery** / `listings` (relay admission separately default-off) | 3410/3411 allowlist, 23412 registry, signed parser, durable backing and outbox | Unknown kind rejected before relay gate and admitted only when enabled; auth/author mismatch; immutable-term edits; status forks/out-of-order history; terminal resurrection; stale/partial/no EOSE vs empty; overlisting across processes and competing job/send; quote-hold Sybil bounds; cancel/publish crash; expired/fake/oversized listing |
| **3. Swap + recovery fake-money lab** / `lab_swaps` | Two local sidecar/CDK mints, encrypted local relay, both directions, no external networking; wallet journal/refund adapter | Kill each process before/after every journal/ledger/mint boundary; persisted-exp fence from #1035 (old updated_at with future exp, missing record); lost request vs lost reply after commit; same-id conflict; concurrent takers; replay after expiry; frozen/dropped state witness; partial restore; claim/refund race with expired deadlines; mint/relay outage past gap; clock skew; wrong keys/hash/mint; fee changes and fee rounding/caps; reservations not released by generic saga retirement; seed restore plus spent-input reconciliation; balances and no platform fee; offline taker notice recovered from NUT-07 |
| **4. CLI + MCP** / `owner_tools` | Thin wrappers around the same tested state machine, exact approval/caps, shared home | No owner direction means no list/take; stale quote/cap mismatch; request-id retries across CLI/MCP; restart with flag off still offers recovery; hidden/unconfigured mint refusal; redacted secrets; recovery blocked fee; no job code path auto-trades; acquired credits selected first for a job, sats-only buyer refused for credits-only seller, per-mint holds and sealed delivery mint (#1101), unchanged fee accrual/remit behavior |
| **5. Capped real-money canary** / `real_money_canary` | Separate explicit Bob authorization, trusted issuer and seller-accepted advertising bitcoin mint, fresh small funded wallets | Settled max 100 bitcoin sats principal per trade, one in flight, max 10 sats mint fees, manual issuer exposure cap in credit units; prove small claim and refund under observed witness behavior; pause on any accounting/capability mismatch. Reconcile every proof/ledger entry before expanding. No production relay or mint writes are authorized by this documentation task |

Stage 2's relay flag guards only the two new kinds, never relaxes unrelated admission. Stage 3
must use honest-money invariants even on fake money; do not green it with protocol-invalid tokens.
Short lab deadlines need a deterministic clock/harness or bounded real wait; production values are the settled 60/15 min, not lab seconds. Attach exact test counts and untested boundaries to each implementation PR.

## 11. Self-review: flaws fixed vs risks accepted

**Fixed in this refresh (since `9f17266`):**

- Unmerged-stack and old money-path framing → single current-main anchor, credits-first selection,
  no credit hop targets, current private mint checks and explicit #1101 boundary.
- Tested-only mint gate contradicted Q4 → reachable advertised NUT-14/07, with behavioral risk
  and canary scope explicit; no prior-test allowlist hidden in preflight or open questions.
- Stale 24h/12h and five-minute values → Q2/Q3 settled by Bob at 60/15 min and 60 s,
  3 min claim cutoff plus expiry/clock qualifications; no unconditional 45 min guarantee.
- Generic recovery reuse could release live requests → persisted `exp` + skew prerequisite #1035,
  distinct from main's `updated_at + 300s` estimate.
- Old-sidecar probe missed NUT-04/05 override → recopy merged backend/dispatch and assert its info.

**Other flaws addressed by this design:**

- Unit-only pricing confused two `sat` assets → explicit URL+unit everywhere, including caps/ledger.
- Seller-first secret proposal let unpaid strangers lock inventory → taker-funded first lock;
  reversal and maker availability risk called out, not silently changed.
- A timer was liable to be read as refund → explicit signed mint operation, post-expiry claim race,
  durable outputs, witness recovery and wallet-adapter gap made normative.
- Persistent listings would be rejected by deployed buzz → exact 3410/3411 scope change and gate.
- Nonstandard multi-character discovery tag filters would not be portable → single-letter
  Nostr index tags plus content/tag consistency checks.
- Public `available` was liable to imply spendable stock → wallet reservation + active-swap CAS,
  soft holds and fresh maker quote; ambiguous money never released.
- Info flags/state alone were liable to imply spendability → fresh advertised-capability checks plus DLEQ and
  exact condition checks; advertising risk is accepted under Q4 and the canary tests behavior.
- Zero trade fees were liable to hide swap costs/change job fees → fee-inclusive net quote and
  explicit separation from existing job settlement/platform accrual.
- Reuse of budget/recovery was liable to imply existing HTLC support → named additive extensions,
  current per-operation cap limitation and required failure tests.

**Accepted/remaining risks:** advertised-but-broken NUT support without a tested-only allowlist; issuer/mint fraud; offline beyond the safety gap; maker dependence on
witness availability under the selected order; funded free options and Sybil quote DoS; single-relay
censorship; traffic analysis; external cloned-wallet double spending; backup/key loss; changing mint
fees blocking capped recovery. Mitigation narrows exposure, not a promise of trustlessness. Owner
approval governs trust/caps. Default-off stages and explicit canary gate prevent treating this
spec or the limited probe as deployment readiness.

## 12. Future work and open questions (proposed defaults)

Future only: owner-authorized scanning for discounted credits before paying another agent; trade
fees with a separate decision and explicit quote disclosure. Neither introduces automatic trading
or changes job platform fees in v1. Multi-relay listing replication, divisible lots, matching,
market making, oracle pricing and issuer guarantees are not part of this design.

| Open question | Proposed default |
|---|---|
| Final kind registry allocation / relay rollout owner | Reserve 3410/3411/23412 after collision review, deploy guarded buzz support before clients; no reuse of job kinds |
| Claim cutoff (derived from settled Q2) | 3 min minimum remaining on the maker lock to initiate a claim; persisted claim exp ≤ short − 2 min; clock/late-claim caveats in §3.2 |
| Pinned CDK refund integration | Narrow journaled lower-level refund adapter using CDK primitives; evaluate an upstream fix separately, no blanket dependency upgrade or forked money stack |
| Owner-directed tool boundary | Harness enforces owner instruction; expose no autonomous entry hook. List authorizes one fixed fill within its caps; recovery only completes/refunds that authorization |
| Canary execution (Q7 cap settled) | Bob must give a separate go; choose mints and explicit credit-face exposure cap, ≤100 sats principal + ≤10 sats fees, one in flight |
