# Free job lane — `payment = none`

**Status:** design spec. No code written. **Anchor commit: `8c3bc9b834fefaf1fc8985ef5eee172a10f4684c`
(= `upstream/main` at the time of writing).** Every line number below was re-derived by grep at that
commit and is written `file:line @8c3bc9b`. Line numbers in files touched by the open PR #964
(`8c3bc9b..14e149b`) are **provisional** and marked so in §7.

**Scope.** One lane in which a job completes with no payment at all, so a buyer holding zero bitcoin
can use a seller that takes nothing.

**Settled inputs (Bob, thread "optional payment — seller advertises 0 fees", 3 Sep 2026), not
relitigated here:**

1. The seller declares free through the existing seat minimum `[seller] rate_sats = 0`; no new price
   dimension. `rate 0` alone means only "any amount ≥ 0", so an explicit "takes no payment"
   advertisement is added.
2. The buyer needs no bitcoin and pays nothing. **The 1-sat convention is rejected** and appears
   nowhere in this spec, in any disguise.
3. A free job still writes the delivery record and the announce event, with payment recorded as
   `"none"`.
4. No abuse bounds for now — the seller manages manually. No caps, no rate limits, no quotas are
   invented here; the exposure is named in §6.

---

## 1. Wire representation, and the version question

### 1.1 DECISION — a param/tag, **no `PROTOCOL_VERSION` bump**

The free lane ships as three additive tags in one vocabulary. **`PROTOCOL_VERSION` does not change.**

| Event | Kind | Tag | Card. | Absent means |
|---|---|---|---:|---|
| OFFER | 3401 | `["param","payment","none"]` | 0..1 | `sat` (a priced job) |
| CLAIM | 3402 | `["payment","none"]` | 0..1 | `sat` |
| SEAT ANNOUNCEMENT | 30340 | `["takes_payment","none"]` | 0..1 | unstated — **never "no"** |

One enum, two spellings of the same word, three surfaces. The offer states it as a `param` because
that is where the offer's request family already lives (`docs/protocol-v1.md` §6.1 table; built at
`gateway.rs:199-230 @8c3bc9b`). The claim states it as a bare filterable tag because the buyer's
award filter reads the CLAIM and §6.2 admits only filterable tags there. The seat states it on the
beat next to `rate`, `accepting` and the admission pair (§4.2 table).

**Absent ⇒ `sat` is the fail-closed direction.** Every event on the wire today carries no such tag,
so a stripped, dropped or pre-upgrade tag reads as *paid* — the status quo, which the money gates in
§2 already refuse to run for free. The opposite default would let a tag-dropping relay or an older
signer silently produce a free job out of a paid one.

The seat tag reads `unstated`, not `no`, matching the rule the admission pair already states for its
own absence (`heartbeat.rs:749-758 @8c3bc9b`: "⛔ ABSENT IS UNKNOWN. IT IS NEVER 'NO'"). A reader
that resolved absence to "takes payment: yes" would be right today but would be asserting a fact no
seat published.

### 1.2 Why no bump — cited, not asserted

**Which constant.** The wire version is `crates/maxplayer-core/src/gateway.rs:10 @8c3bc9b`,
`pub const PROTOCOL_VERSION: &str = "1"`. It is **not**
`crates/maxplayer-core/src/driver/acp.rs:17 @8c3bc9b`, `pub const PROTOCOL_VERSION: u32 = 2`, which
is the ACP driver's own handshake number and is unrelated to any maxplayer event. `git grep -n
"const PROTOCOL_VERSION" -- crates` at `8c3bc9b` returns exactly those two lines; a bare grep for the
bare name returns both, so every citation below names the file.

**The spec says additive facts ship as tags.** `docs/protocol-v1.md` §2.3 @8c3bc9b: *"A new fact MUST
ship as a new tag, or as a new optional field on an understood artifact. A change that cannot take
that form is a new major."* A payment-mode enum takes exactly that form, so by the protocol's own
rule it is not a new major. §2.1 supplies the other half: *"A reader MUST ignore tags it does not
recognize"* — a v1 reader that never learns the tag keeps parsing free events, and (per §2 below)
refuses to act on them, which is the behaviour we want from an un-upgraded peer.

**What a bump would actually cost.** The wire version has **no negotiated range**. Both readers
compare it by strict equality and refuse on any mismatch:

- `gateway.rs:401 @8c3bc9b` — `if version != PROTOCOL_VERSION { return Err(OfferParseError::UnsupportedVersion(..)) }`, inside `pub fn parse_offer` (`:393-465 @8c3bc9b`).
- `heartbeat.rs:1027 @8c3bc9b` — `if version != Some(PROTOCOL_VERSION) { return Err(HeartbeatParseError::WrongVersion(..)) }`, inside `pub fn parse_heartbeat` (`:1015-1065 @8c3bc9b`).

Contrast a component in this same tree that *does* negotiate:
`crates/buzz/crates/buzz-relay/src/audio/handler.rs:417 @8c3bc9b` accepts
`requested_version` in `1..=CURRENT_PROTOCOL_VERSION` (`:124 @8c3bc9b`). maxplayer's wire has no such
window. **Therefore setting `PROTOCOL_VERSION = "2"` would make every deployed v1 peer refuse every
offer and every seat announcement from a v2 peer — a total market partition — to buy one optional
field.** That is the derived consequence, and it is decisive.

**The repo has already made this exact call twice, and documented why.** The #897 capability request
shipped as `param` tags with the emitter noting that an offer requesting nothing "emits no tag and is
byte-identical to one posted before this existed" (`gateway.rs:208-211 @8c3bc9b`). The #784 seat
capability shipped as beat tags with the reader noting that every field defaults to unstated, "that
is what lets emitters and readers ship without a `v` bump" (`heartbeat.rs:928-931 @8c3bc9b`). The
free-mode tag is the third instance of the same shape, and inherits the same argument.

### 1.3 The amount tag is unchanged and stays required

A free OFFER still carries `["amount","0","sat"]`. §6.1's table marks `amount` cardinality 1,
required, and `parse_offer` requires the tag (`gateway.rs:405-410 @8c3bc9b`), requires
`unit == "sat"` (`:415`), and `.parse()`s the value with **no lower bound** (`:418`) — so `0` parses
today, on an unmodified reader. **We change nothing about that tag.** `payment=none` is what makes
that `0` mean "no payment leg exists", rather than "a payment of zero", which §11.6 forbids
(`docs/protocol-v1.md` §11 rule 6 @8c3bc9b: *"Fee floor. An amount at or below the mint fee is dust,
and a buyer MUST refuse it."*). Reading `amount 0` as a payment would require weakening a normative
rule for **every** job; reading it as "not a payment at all" leaves that rule untouched.

---

## 2. The gates: what changes, and the proof a paid job cannot reach the free path

### 2.0 The one enum, and the both-ends rule

```
enum PaymentMode { Sat, None }   // absent on the wire ⇒ Sat
```

**The free path is entered only when the buyer's signed OFFER states `none` AND the seller's
published CLAIM states `none`.** Every other combination — including both "offer none / claim
absent" and "offer absent / claim none" — is a MISMATCH and refuses. Mode is never inferred from
`amount == 0`, never from `rate_sats == 0`, and never from the absence of a `creq`. It is read from
one tag on each side, and the two must agree.

Each gate below states its **failure direction**. Every one of them refuses.

### 2.1 Seller — post/claim admission (`seller.rs`, `seller_node/run.rs`)

**Today.** `rate_gate_allows` (`crates/maxplayer-core/src/seller.rs:81-108 @8c3bc9b`) refuses an
untargeted offer without `claim_open_pool` (`:89-93`), refuses a foreign `p` tag (`:94-98`), and
refuses `offer.amount < rate_sats` (`:101-106`). With `rate_sats = 0` an `amount = 0` offer clears
that floor already — the floor is `<`, not `<=`. `require_seller_config`
(`seller.rs:54-75 @8c3bc9b`) already permits `rate_sats: 0` explicitly (`:73`).

**Change.** Add `[seller] takes_no_payment: bool` (serde default `false`) to `SellerConfig`
(`home.rs:192 @8c3bc9b`). Add a mode argument to `rate_gate_allows` and one refusal:

> A `PaymentMode::None` offer is refused unless `takes_no_payment == true`.
> A `PaymentMode::Sat` offer is refused if the seat's price floor is not met (today's `:101`,
> unchanged).

Failure direction: **refuse** (`SellerError::Policy`, a new `SkipReason::FreeNotOffered` at the
`classify_offer` call site, `seller_node/run.rs:2213 @8c3bc9b`, inside
`fn classify_offer` at `:2165`).

**Config coupling, deliberate.** `takes_no_payment = true` is only valid with `rate_sats = 0`;
`require_seller_config` refuses the pair otherwise. That prevents the state "advertises free, floors
at 21 sat", which is a seat no buyer can satisfy in either mode.

### 2.2 Seller — the claim (`seller_node/run.rs`, `gateway.rs`)

**Today.** `claim_offer` (`run.rs:4885 @8c3bc9b`) unconditionally builds a NUT-18 request at
`:4969-4979` (`gateway::creq::build_seller_creq`, defined `gateway.rs:1112-1147 @8c3bc9b`) over
`config.accepted_mints`, then passes it to `claim_draft` at `:4991-5000`. `claim_draft`'s signature
takes `creq: &str` — not an `Option` (`gateway.rs:591-610 @8c3bc9b`), and §6.2 marks `creq`
cardinality 1, required.

**Change.** In free mode, **no `creq` is built and no `creq` tag is emitted**; the claim carries
`["payment","none"]` instead. `claim_draft` takes `creq: Option<&str>` plus the mode, and emits
exactly one of the two tags — never both, never neither.

**Why not a zero-amount creq.** `build_seller_creq` would happily encode `amount = 0`, but every
consumer of that request is a payment gate that refuses zero (§2.4, §2.5). A zero creq would put an
unpayable invoice on the wire that reads as an invoice to every un-upgraded buyer, which is the exact
ambiguity the mode tag exists to remove.

Failure direction: a free claim carrying a `creq`, or a paid claim carrying `payment=none`, is
**refused by the buyer** at §2.3 — the seller cannot make it acceptable by emitting both.

### 2.3 Buyer — AWARD (`buyer/lifecycle.rs`)

**Today.** `claim_is_payable` (`buyer/lifecycle.rs:587-612 @8c3bc9b`) returns `false` on: no creq
(`:588`), unparseable creq (`:589-591`), `payment_id != job_id` (`:592-594`), `unit != sat`
(`:595-597`), `amount != offer_amount_sats` (`:600-602`), over `max_sats` (`:603-605`), and finally
`plan_payment(buyer_mint, listed, allow_real_mints).is_err()` (`:611`). It has exactly two callers,
both production: `select_awardable_claim` at `:115 @8c3bc9b` (the auto path) and
`named_claim_awardable` at `:579 @8c3bc9b` (the manual path, `:549-583`), the latter surfacing
`NamedAwardRefused::Unpayable` (variant `:503`, message `:523-526`).
Filter for that claim: `git grep -n "claim_is_payable" -- crates` at `8c3bc9b`, 4 hits — the
definition, those two call sites, and nothing else.

**Change.** `claim_is_payable` becomes `claim_is_settleable(mode, job_id, creq, filters)`:

- `mode == Sat` → today's body, byte-for-byte unchanged, **plus** a refusal if the claim carries
  `payment=none`.
- `mode == None` → returns `true` only if **all** of: the claim carries `payment=none`; the claim
  carries **no** `creq` tag; and `filters.offer_amount_sats == 0`. Any miss returns `false`.

Both call sites are updated; neither gains or loses a filter relative to the other, which is the
property `named_claim_awardable`'s doc already binds (`:544-548 @8c3bc9b`: *"a filter skipped here is
a filter that never runs at all"*).

Failure direction: **refuse the award** (`false` → `NamedAwardRefused::Unpayable` on the manual path;
claim skipped on the auto path).

**This is the both-ends check.** `plan_payment` — the only thing on this path that requires a mint —
is reached solely through the `Sat` arm. Nothing in the `None` arm reads `buyer_mint`,
`allow_real_mints`, or any wallet state.

> **Structural warning for the implementer.** If `AwardFilters` (`buyer/lifecycle.rs:33 @8c3bc9b`)
> gains a mode field, the test `display_only_fields_never_reach_the_award_filter`
> (`#[test]` at `:4441`, fn at `:4442 @8c3bc9b`) **parses that struct's literal source text**:
> `split_once("pub struct AwardFilters<'a> {")` at `:4449`, terminated on `"\n}"` at `:4452`, then
> `strip_prefix("pub ")` per line. A change to the struct's brace style, its generic parameter, or a
> field's `pub ` prefix changes what that test inspects. It carries its own positive control against
> a planted declaration at `:4468-4472`; read that before judging the test.

### 2.4 Buyer — ACCEPT (`job_lifecycle.rs`)

**Today.** `accept_claim_async` (`:1218 @8c3bc9b`) calls
`verify_accepted_claim_creq(claim.creq.as_deref(), &request.job_id, offer.amount_sats)` at
`:1324 @8c3bc9b`. That function is `:1601-1640 @8c3bc9b` — the closing brace is `:1640`, immediately
before `fn resolve_accepted_contribution` at `:1642`. It has **six** refusals, not five:

| Refusal | Line @8c3bc9b |
|---|---|
| creq present | `:1606` |
| creq parses | `:1611` |
| `payment_id == job_id` | `:1616` |
| `amount == offer.amount_sats` | `:1622` |
| `unit == sat` | `:1628` |
| accepted-mint list `m` non-empty | `:1634` |

(Its own doc comment lists the same six as bullets at `:1593-1598 @8c3bc9b`.)

Immediately after, `accept_claim_async` calls `crossmint::plan_payment` at `:1363-1368 @8c3bc9b` to
freeze the funding mint, and refuses fail-closed if no route exists. **This is a second mint
requirement on the accept path, distinct from the award-path one in §2.3.**

**Change.** Both are made mode-conditional:

- `mode == Sat` → both run exactly as today.
- `mode == None` → `verify_accepted_claim_creq` is **not** called and `plan_payment` is **not**
  called. In their place, a new `verify_free_accept` refuses unless **all** of: the claim carries
  `payment=none`; the claim carries **no** creq; `offer.amount_sats == 0`. The resulting
  `AcceptedBind` (`:1387-1420 @8c3bc9b`) is written with `amount_sats = 0`, `creq_hash = None`,
  `accepted_mints = vec![]`, `funding_mint = None`, `delivery_mint = None`, and a new
  `payment_mode: PaymentMode` field.

Failure direction: **refuse the accept, write no bind.** Note the ordering that must be preserved:
the bind is written before the ACCEPT is published (`write_accepted_bind` at `:1421`, publish at
`:1423`), so a refusal before `:1421` leaves nothing public and nothing local.

### 2.5 Buyer — PAY (`authorize_pay.rs`, `payment_wallet.rs`) — the free path never arrives

**Today.** `derive_payment` (`:905-955 @8c3bc9b`) calls `plan_payment` at `:929-933`, and the spend
runs `require_fee_safe_amount` (`payment_wallet.rs:730-743 @8c3bc9b`) which calls
`require_amount_covers_fee` (`:774-784 @8c3bc9b`), refusing at `:778` on `amount <= fee`. **That
refuses `amount = 0` even when the fee is `0`** — `0 <= 0`. The realized-token gate
`require_realized_locked_token` (`:787-806 @8c3bc9b`) independently refuses a zero-value token at
`:793-797`.

**Change.** None to any of those checks. Instead, one **new** guard at the entry of
`authorize_pay_async`:

> A bind whose `payment_mode == None` is refused. There is nothing to pay.

Failure direction: **refuse.** This is defence in depth, not the primary control — §2.4 never writes
a payable bind for a free job — and it is what makes the claim "a paid job cannot ride the free
path" symmetric: the free path cannot ride the *paid* path either.

**These three zero-refusals stay untouched deliberately.** They are §11.6 in code. Relaxing any of
them to admit `amount = 0` would weaken the dust rule for every priced job in the market — which is
the failure mode the rejected 1-sat convention was itself an attempt to route around.

### 2.6 Buyer — POST (`job_lifecycle.rs`) — the gate nobody has named

**Today.** `post_job_async` (`:546-622 @8c3bc9b`), under `#[cfg(feature = "wallet")]` at
`:592-603 @8c3bc9b`, **opens a wallet** (`buyer_fund::open_wallet_async`, `:594`) and then dust-guards
the posted amount (`payment_wallet::require_fee_safe_amount_for_post`, `:597-602`). The shipped CLI
turns that feature on by default (`crates/maxplayer/Cargo.toml:71 @8c3bc9b`, `default = ["wallet"]`).

Consequence, measured, not inferred: **at `8c3bc9b` a `payment=none` offer at `amount = 0` cannot be
posted at all.** `require_fee_safe_amount_for_post` reaches `require_amount_covers_fee(0, fee)` and
`0 <= fee` for every fee including zero — so the post is refused before `build_offer_draft` at `:605`
is ever reached. Separately, `open_wallet_async` requires a mint URL that clears the real-mint fence
(`buyer_fund.rs:93-97 @8c3bc9b`) and a wallet sqlite store (`:100-104`).

**Change.** Make the whole `:592-603` block mode-conditional: skipped entirely when
`mode == PaymentMode::None`, run unchanged when `mode == PaymentMode::Sat`. Add one refusal in its
place: a `None` post whose `amount_sats != 0` is refused.

Failure direction: **refuse.** A free offer with a non-zero price is a contradiction and must not
reach the relay.

The budget-cap check at `:586` (`assert_amount_within_budget_cap`) is left in place for both modes:
`0` is within every cap, so it passes for free jobs without a code change.

### 2.7 Summary — the fail-closed argument in one paragraph

A paid job cannot reach the free path because *entering* the free path requires two independent
signed statements — the buyer's `["param","payment","none"]` on a kind-3401 the buyer signed, and the
seller's `["payment","none"]` on a kind-3402 the seller signed — checked together at three places:
the seller's admission (§2.1), the buyer's award (§2.3), and the buyer's accept (§2.4). Absence
defaults to `Sat` at every reader, mismatch refuses at every reader, and the money gates that
enforce the price (`verify_accepted_claim_creq`'s six refusals, `plan_payment`, the three
zero-refusals of §2.5) are **not modified** — they are only *not reached*, and only when both
statements say `none` and the offer's amount is `0`. The free path cannot reach the paid path either,
by the §2.5 guard.

---

## 3. Recording the delivery, and the announce event (ruling 3)

### 3.1 The premise correction: these are two different records, in two different files

The brief's premise that "the spend record is written by the pay path" is right about the **spend**
and wrong about the **delivery**. Re-measured at `8c3bc9b`:

- The **delivery record** is written at DELIVERY time, by the seller, in
  `crates/maxplayer-core/src/seller_node/store.rs`: `pub fn deliver_and_enqueue` at `:911-952`, whose
  `INSERT INTO deliveries` is at `:934-937`, against the DDL at `:311-315`. Production call sites:
  `seller_node/run.rs:6278` (in `async fn execute_job`, `:5749`) and `run.rs:6419` (in
  `async fn finalize_pushed_delivery`, `:6327`).
- The **spend record** is the buyer's ledger, `crates/maxplayer-core/src/budget.rs`:
  `struct LedgerRecord` `:87` (doc `:84`), `fn append_spend` `:335` (doc `:334`),
  `fn append_record` `:430` (doc `:427`). Its `#[cfg(test)]` markers are `:461` and `:467`.

They are separate. Ruling 3 names the delivery record; a free job writes no spend record at all,
because no spend occurs.

### 3.2 DECISION — the delivery record carries the mode as an additive column

```sql
ALTER TABLE deliveries ADD COLUMN payment TEXT;   -- NULL ⇒ 'sat' (legacy rows)
```

`deliver_and_enqueue` writes `'none'` for a free job and `'sat'` otherwise; every reader resolves
`NULL` to `'sat'`. `SCHEMA_VERSION` (`store.rs:27 @8c3bc9b`, currently `6`) goes to `7`, and one
idempotent step is appended to `fn migrate` (`store.rs:392-416 @8c3bc9b`), in the same shape as its
four existing steps (`:393`, `:400`, `:406`, `:412`).

**This shape is forced, not chosen.** `migrate`'s own contract (`:386-391 @8c3bc9b`) is that "Every
step is ADDITIVE and idempotent — a nullable column whose absence reads the same as its default.
Nothing here rewrites or drops a row: this store holds live trade state." A nullable `TEXT` column is
the only change that satisfies it.

**The rejected alternative, and why.** Adding a `'settled_free'` value to `jobs.state` would need the
CHECK constraint at `store.rs:291-292 @8c3bc9b`
(`CHECK (state IN ('awarded','executing','delivered','paid','failed'))`) widened, and SQLite cannot
widen a CHECK by `ALTER TABLE ADD COLUMN` — it requires a table rebuild, which `migrate`'s contract
forbids on a live money store. **So a free job's terminal `jobs.state` stays `'delivered'`**, and
`deliveries.payment = 'none'` is the fact that says it will never advance further.
`collect_receipt` (`store.rs:973-996 @8c3bc9b`) is the only writer of `state = 'paid'` (`:991-994`)
and is unreachable for a free job, since no kind-1059 wrap ever arrives.

Filter behind "the only writer": `git grep -n "state = 'paid'" -- crates` at `8c3bc9b` returns 1
production hit, `store.rs:992`, within `collect_receipt`.

### 3.3 The seller journal is NOT the place — it is dead code at this head

`crates/maxplayer-core/src/seller.rs` carries a full `SellerJournal` with `JournalEntry::Delivery`
(`:160-175 @8c3bc9b`), `append_delivery` (`:544-562`), `deliveries_awaiting_receipt` (`:567-612`) and
`oldest_unsettled_delivery_ts` (`:381-404`) — all of which look like exactly the right place to
record a free delivery, and none of which run.

Measured: `git grep -n "SellerJournal\|JournalEntry\|plan_orphaned_claims" -- crates` at `8c3bc9b`
returns hits **only inside `seller.rs` itself** (plus two prose mentions of the filename in
`episode.rs:4` and `:222`). Denominator: all of `crates/**`. Cross-check —
`git grep -no "seller::[a-z_]*" -- crates`, excluding `seller.rs`, returns five distinct symbols in
use: `rate_gate_allows`, `job_deadline_unix`, `sign_receipt_hash`,
`unwrap_own_payment_gift_wrap`, `cashu_secret_from_nostr_hex`. The journal is superseded by the
sqlite `SellerStore` and has no live caller.

**DECISION: the free lane specifies nothing against `SellerJournal`.** Had it been live, note the
hazard for whoever revives it: `JournalEntry` is a serde internally-tagged enum
(`seller.rs:121-123 @8c3bc9b`) with no `deny_unknown_fields`, so adding a *field* is safe in both
directions, but adding a *variant* is not — an older binary reading a newer line hits the fail-closed
`entries()` path (`:330-332`) and every journal read for that home turns into an error.

### 3.4 The announce event — identification, and one question handed back

**Candidate enumeration.** Filter: `git grep -ln "announce\|Announce" -- crates` at `8c3bc9b`,
then discarding the `crates/buzz/**` relay tree (a different product surface) and prose-only hits.
Denominator: all of `crates/**`. Three named surfaces survive, plus a fourth structural reading the
brief did not list:

| # | Surface | Verdict |
|---|---|---|
| A | `announce.rs` — `pub struct AnnounceEvent :36`, `pub fn dispatch :229`, configured by `[seller_announce]` (`home.rs:797`, `:1354-1356`) | **The only per-job surface whose vocabulary can carry ruling 3** — and it has no production caller |
| B | kind-30340 seat announcement — `publish_seat_announcement` (`run.rs:4579`), fed by `publish_heartbeat` (`:4448-4478`) | **Excluded**: per-seat, not per-job. §4.2's tag table carries no job id, so it cannot record a payment for a job |
| C | kind-30617 git-repo announce (`home.rs:179-182`) | **Excluded**: NIP-34, informational, not maxplayer-owned. §2.1 lists it among borrowed kinds; §4.3 says a reader "MUST NOT use kind `30617` to resolve the remote for a delivery" |
| D | the kind-3403 RESULT enqueued inside `deliver_and_enqueue` (`store.rs:942-949`) | Not called "announce" anywhere in the source, but it is the *only* event a delivery actually publishes, and it is written in the same transaction as the delivery row |

**Surface A is the right vocabulary and it is dead.** `AnnounceEvent`'s `event` label is documented as
`online · claimed · delivered · collected · refused · reconcile_released · job_failed`
(`announce.rs:39-41 @8c3bc9b`) and it carries `amount`, `amount_received` and `expected`
(`:53-58`) — a per-job payment record in all but name. But:

- `git grep -l "AnnounceEvent" -- crates` at `8c3bc9b` returns **1** path: `announce.rs` itself.
- `git grep -n "announce::" -- crates` at `8c3bc9b` returns **3** hits, all naming
  `announce::run_sink`, all from `telemetry.rs` (`:20`, `:103`, `:112`). **`dispatch` has no
  caller.**
- Its sibling stream is dead the same way: `git grep -n "crate::telemetry" -- crates/maxplayer-core
  crates/maxplayer` returns **2** hits, both doc-comment references (`home.rs:856`, `oplog.rs:12`),
  and `crate::episode` is referenced only from `telemetry.rs` and `home.rs`.

So at `8c3bc9b` the whole `[seller_announce]` / `[telemetry]` observability tier is configured,
schema-versioned, and **never emitted**.

**DECISION on identification:** ruling 3's "announce event" can only mean surface A — B and C are
excluded by citation above, and D is not called an announce anywhere in the tree.

**HANDED BACK to maxie (a design question, per the brief's §6 — not answered here):**

> Ruling 3 says a free job "still writes … the announce event". Surface A has no production caller
> at `8c3bc9b`, so honouring that literally means **wiring the announce sink** — work that is not a
> payment-mode change and would touch every lifecycle transition, paid and free alike. Which is
> intended?
> **(a)** the free lane wires `announce::dispatch` for the `delivered` transition (and, being the
> first caller, effectively ships the announce tier); or
> **(b)** ruling 3 is satisfied by the `deliveries.payment = 'none'` row of §3.2 plus the kind-3403
> RESULT (surface D), and the announce sink stays dark until it is wired on its own ticket.
>
> The spec is written to be correct under either answer: §3.2 stands unchanged in both, and only the
> §7 change list gains `announce.rs` under (a). **If (a):** the field to carry ruling 3 is a new
> `payment: Option<&'static str>` on `AnnounceEvent`, added with `skip_serializing_if` per the
> schema's own additive-only rule (`announce.rs:24-27 @8c3bc9b`) — `ANNOUNCE_SCHEMA_VERSION` stays
> `1`, for the same reason `PROTOCOL_VERSION` stays `"1"` in §1.

**No RECEIPT is published for a free job**, in either answer. §6.8's table marks `["mint", mint_url]`
cardinality 1, required, and requires both co-signatures; a free trade settles at no mint, so a
conformant receipt cannot be constructed. The free lifecycle terminates at `offer → claim → award →
result → verify → accept` and stops — the `pay → receipt` tail of §5 does not run. (Corrected
2026-09-04: this sentence originally ended the free lifecycle at `verify`, which contradicts §2.4
above — §2.4 keeps the ACCEPT publish on the free path, and §2.4 is what shipped.)

---

## 4. The seller's "takes no payment" advertisement, and how a buyer finds it

### 4.1 DECISION — one tag on the kind-30340 beat, derived from config, never operator-set

The seat announcement gains `["takes_payment","none"]`, emitted only when the seat's effective
`[seller]` config has `takes_no_payment = true`. Absent means unstated (§1.1), never "no".

Built in `HeartbeatDraft::to_event_draft` (`heartbeat.rs:520-543 @8c3bc9b`), beside the existing
`rate` tag (`:523`, `:529`) and the admission pair (`:537-539`). Read back in `parse_heartbeat`
(`:1015-1065 @8c3bc9b`) into a new `ParsedHeartbeat` field, alongside `rate_sats` (`:1044-1048`,
struct field `:916`) and `admission` (`:1057`).

**Why a separate tag rather than reading `rate == 0`.** That is ruling 1's own reasoning, and the
wire bears it out: `rate` is parsed as a `u64` floor with no distinguished zero
(`heartbeat.rs:1044-1048 @8c3bc9b`), and §4.2 defines it as "Lowest price the seat accepts". A seat
at `rate 0` is saying "I will take any amount, including nothing" — which is not the same statement
as "I take nothing", and a buyer holding zero sats cannot act on the first.

**Why derived, not operator-set.** `AdmissionPolicy::from_seller_config`
(`home.rs:499 @8c3bc9b`) is the precedent, and its call site states the rule outright
(`run.rs:4471-4473 @8c3bc9b`): *"An operator-set field would be a second place to state one fact, and
the ad would drift from the gate that enforces it."* The advertisement is derived from the same
`SellerConfig` that §2.1's admission gate reads, in the same call, so the ad cannot lie about the
gate.

### 4.2 Discovery — how a buyer finds a free seat

The beat is the buyer's only seat reader: `parse_heartbeat`'s own doc calls it "the buyer-side seat
reader" and "the ONLY source of a seat's capability" (`heartbeat.rs:1008-1014 @8c3bc9b`). A buyer
resolves seats by `(author pubkey, kind, d)` taking the newest `created_at` (§4.4), filters for
`takes_payment == none` and `accepting == y`, and posts a targeted `payment=none` offer at
`amount 0` to that seat's pubkey.

**Freshness is not weakened for free seats.** §4.4 is normative: a reader MUST weigh an announcement
by its age, and a recent beat proves only that the seat published. Nothing about a zero price changes
that, and the free lane adds no exemption.

### 4.3 A hard constraint the free lane cannot remove

**A free seat must still publish at least one mint.** `parse_heartbeat` rejects a beat whose
`accepted_mints` tag is absent or empty — `HeartbeatParseError::MissingAcceptedMints`,
raised at `heartbeat.rs:1051-1052 @8c3bc9b`, spelled out in §4.2 ("A buyer can pay a seat only on a
mint in this list") and in the field's own doc (`:462-464`: "a seat stating none is unpayable").

A seat that published no mints would be **invisible to every buyer, free or paid** — the beat would
not parse at all. So a seller that takes nothing still names a mint it will never be paid at. That is
cosmetic for the free lane and it is **not** changed here: relaxing `MissingAcceptedMints` would make
a genuinely unpayable *paid* seat parseable, which is a market-visible regression to buy a config
tidiness. It is named in §6 and in §5 as the one place ruling 2's spirit is dented — on the seller's
side, not the buyer's.

---

## 5. What a wallet-less buyer still needs — plainly

**As the code stands at `8c3bc9b`, ruling 2 is NOT reachable. A buyer with no wallet cannot post any
offer at all, free or priced.**

The reason is §2.6: `post_job_async` opens a wallet and dust-guards the amount at
`job_lifecycle.rs:592-603 @8c3bc9b`, before the offer draft is built at `:605`, and the shipped
binary compiles that block in by default (`crates/maxplayer/Cargo.toml:71 @8c3bc9b`). Two
independent refusals fire:

1. `require_amount_covers_fee(0, fee)` refuses, because `0 <= fee` for **every** fee value including
   `0` (`payment_wallet.rs:774-784 @8c3bc9b`, refusal at `:778`).
2. `open_wallet_async` → `open_wallet_at_mint_async` requires a mint URL clearing the real-mint fence
   and a wallet sqlite store (`buyer_fund.rs:74-106 @8c3bc9b`, fence at `:93-97`, store at
   `:100-104`).

**So ruling 2 is not reachable without the §2.6 change.** That change — making the post-time
wallet-open and dust guard mode-conditional — is the single thing standing between a zero-bitcoin
buyer and a free job. It is stated here rather than smoothed over, per the brief.

**With §2.6 (and §2.3, §2.4) in place, here is what such a buyer still needs, honestly:**

| Still required | Why | Anchor @8c3bc9b |
|---|---|---|
| A nostr secret key | Signs OFFER and AWARD; every publish path reads it | `buyer_keys` in `job_lifecycle.rs`, `home::read_secret_key_hex` |
| A relay URL | Publish and read the job view | `home.rs:150` `relay_url` |
| Git read access to the seller's delivery remote | §11 rule 3 and §5 step 5: the buyer MUST verify the delivery itself, from its own read of the remote | `docs/protocol-v1.md` §5.5, §11.3 |
| Disk for the buyer store | Award attempt rows and the job view | `buyer/store.rs` |

**No longer required on the free path:** a mint URL, a wallet sqlite database, a funded balance, a
keyset read, any network call to a mint, `allow_real_mints`, or a per-job budget with room in it.
`plan_payment` — the only mint-touching call on the buyer's award and accept paths
(`buyer/lifecycle.rs:611`, `job_lifecycle.rs:1363` @8c3bc9b) — sits entirely inside the `Sat` arms of
§2.3 and §2.4 and is never reached.

**One dent in ruling 2, and it is on the seller's side.** Per §4.3, the free *seller* must still put
at least one mint URL in its config or its beat will not parse for any buyer. The buyer needs
nothing from that mint and never contacts it. Ruling 2 says the buyer needs no bitcoin — that holds
after §2.6. It does not extend to the seller's config file, and this spec does not make it.

**Note on `default_mint()`.** A buyer with no configured mint still resolves a mint *string* —
`MaxplayerConfig::default_mint` falls back to `DEFAULT_MINT_URL` (`home.rs:1493-1498`, const at
`:72` @8c3bc9b). That is why the §2.6 failure above is a dust refusal rather than a missing-config
error, and it is why "the buyer has a mint configured" must never be used as the free-mode
discriminator: every buyer has one, whether they meant to or not. The mode tag is the discriminator.

---

## 6. Risks (ruling 4 — exposure named, no bounds invented)

No caps, rate limits, or quotas are specified. These are the exposures the operator carries.

- **Unbounded free work.** A seat with `takes_no_payment = true` and `claim_open_pool = true` will
  claim every well-formed free offer in the pool until its slots fill (`try_reserve`,
  `run.rs:5005 @8c3bc9b`). The price floor, which is the market's only natural throttle, is `0` by
  construction. Slot count (`[seller] slots`) is the only remaining limit and it bounds concurrency,
  not volume. Cost lands as compute, egress, and harness API spend the operator pays.
- **Free work is compute a stranger can spend.** §11 rule 1 keeps work behind the award, so a free
  job still costs nothing until the buyer awards it — but awarding is free too, so the whole cost of
  a free job is the seller's.
- **The mitigation that already exists is admission, not price.** `accept_offers_only_from`,
  `accept_open_targeted` and `claim_open_pool` (`home.rs`, gate at `run.rs:2199-2214 @8c3bc9b`) are
  how an operator scopes who may reach the seat. An operator turning on `takes_no_payment` should be
  told, in the config doc, that admission is now the only control they have left.
- **`accepting=n` is the manual brake, and it is not instant.** A seat can stop taking free work by
  flipping `accepting`, but the beat is addressable and consumers weigh it by age (§4.4), so there is
  a cadence-sized window in which stale `accepting=y` still attracts offers.
- **A free seat's ad is a permanently attractive target.** The kind-30340 beat is public and
  indexable, and a free seat is the most attractive entry in it. Discovery cannot be scoped; only
  admission can.
- **No reputational or accounting trail.** No receipt is published for a free job (§3.4), so nothing
  third-party-verifiable records that the seat did the work. A free job leaves the seat's public
  settlement history unchanged, which is a real cost to a seat building a track record.
- **Free jobs sit at `state='delivered'` forever.** By §3.2 a free job never advances to `'paid'`.
  Any operator tooling that reads "delivered but not paid" as *owed money* will mis-report free jobs
  as arrears until it learns to read `deliveries.payment`.
- **Mode confusion at the seam.** The single largest correctness risk is a reader that infers mode
  from `amount == 0` or from a missing `creq` instead of the tag. §2.0 forbids it; the implementation
  should carry a test that a `payment`-tag-less offer at `amount 0` is refused, not run free.

---

## 7. File-by-file change list

**Every line number is anchored at `8c3bc9b834fefaf1fc8985ef5eee172a10f4684c`.** Files marked
**PROVISIONAL** are in the change-set of the open PR #964 (`8c3bc9b..14e149b`, 22 files, verified by
`git diff --name-only 8c3bc9b 14e149b` in this worktree); their anchors move when #964 merges.

| # | File | Anchor @8c3bc9b | Change | PR #964 |
|---|---|---|---|---|
| 1 | `docs/protocol-v1.md` | §6.1 table (offer), §6.2 table (claim), §4.2 table (seat), §5 lifecycle, §11 rules | Add the three tags of §1.1 as `0..1 / no` rows; state absent ⇒ `sat`; state that a `payment=none` trade ends after `verify` and publishes no `RECEIPT` | — |
| 2 | `crates/maxplayer-core/src/gateway.rs` | `PROTOCOL_VERSION :10` (**unchanged**); `OfferDraft :73-93`; `to_event_draft :192-238`; `parse_offer :393-465`; `claim_draft :591-610` | Add `payment_mode` to `OfferDraft` + a builder; emit `["param","payment","none"]` conditionally in `to_event_draft`; read it in `parse_offer` into `ParsedOffer :242`; `claim_draft` takes `creq: Option<&str>` + mode and emits `creq` **xor** `["payment","none"]` | — |
| 3 | `crates/maxplayer-core/src/heartbeat.rs` | `HeartbeatDraft :455-476`; `to_event_draft :520-543`; `ParsedHeartbeat :912-933`; `parse_heartbeat :1015-1065` | Add `takes_no_payment: bool` to draft + parsed; emit/read `["takes_payment","none"]`; **do not** touch the `v` check at `:1027` or `MissingAcceptedMints` at `:1052` | — |
| 4 | `crates/maxplayer-core/src/home.rs` | `SellerConfig :192-…`, `rate_sats :195` | Add `[seller] takes_no_payment: bool` (serde default `false`). ⚠ `rate_sats` at `:159` is **`BuzzConfig`**, a deprecated runtime-ignored struct (`:143-148`) — the seat minimum is `:195` only | **PROVISIONAL** |
| 5 | `crates/maxplayer-core/src/seller.rs` | `require_seller_config :54-75`; `rate_gate_allows :81-108`, floor `:101` | Take the mode; refuse a `None` offer unless `takes_no_payment`; refuse `takes_no_payment` paired with `rate_sats != 0` | — |
| 6 | `crates/maxplayer-core/src/seller_node/run.rs` | `classify_offer :2165`, rate gate `:2213`; `claim_offer :4885`, creq build `:4969-4979`, `claim_draft` call `:4991-5000`; `publish_heartbeat :4448-4478` | Thread the offer's mode into `classify_offer`; add `SkipReason::FreeNotOffered`; skip the creq build in free mode; pass `takes_no_payment` into the beat draft at `:4464-4475` | **PROVISIONAL** |
| 7 | `crates/maxplayer-core/src/seller_node/store.rs` | `SCHEMA_VERSION :27`; `deliveries` DDL `:311-315`; `migrate :392-416`; `deliver_and_enqueue :911-952`, INSERT `:934-937` | Bump `SCHEMA_VERSION` to `7`; add nullable `payment TEXT` to the DDL and one idempotent `migrate` step; write `'none'`/`'sat'` in the INSERT. **Do not touch the `jobs.state` CHECK at `:291-292`** (§3.2) | — |
| 8 | `crates/maxplayer-core/src/buyer/lifecycle.rs` | `AwardFilters :33`; `select_awardable_claim :105`, call `:115`; `named_claim_awardable :549-583`, call `:579`; `claim_is_payable :587-612`; `NamedAwardRefused::Unpayable :503`, message `:523-526` | Add mode to `AwardFilters`; `claim_is_payable` → `claim_is_settleable` with the two arms of §2.3; update both call sites. ⚠ read the source-parsing test at `:4441-4452` + its control at `:4468-4472` **before** editing the struct | **PROVISIONAL** |
| 9 | `crates/maxplayer-core/src/job_lifecycle.rs` | `post_job_async :546-622`, wallet+dust block `:592-603`, draft `:605`; `accept_claim_async :1218`, creq verify call `:1324`, `plan_payment` `:1363-1368`, bind `:1387-1420`, write `:1421`, publish `:1423`; `verify_accepted_claim_creq :1601-1640` | §2.6: make `:592-603` mode-conditional, refuse `None` with `amount != 0`. §2.4: gate `:1324` and `:1363` behind the `Sat` arm, add `verify_free_accept`, add `payment_mode` to `AcceptedBind`. **Leave `:1601-1640` itself unmodified** | **PROVISIONAL** |
| 10 | `crates/maxplayer-core/src/authorize_pay.rs` | `derive_payment :905-955`, `plan_payment` `:929-933` | Refuse at entry any bind with `payment_mode == None` (§2.5). No change to `derive_payment` itself | **PROVISIONAL** |
| 11 | `crates/maxplayer-core/src/payment_wallet.rs` | `require_fee_safe_amount :730-743`; `require_fee_safe_amount_for_post :751-771`; `require_amount_covers_fee :774-784`, refusal `:778`; `require_realized_locked_token :787-806` | **NO CHANGE.** Listed so the reviewer can see the zero-refusals were considered and deliberately left intact (§2.5) | — |
| 12 | `crates/maxplayer-core/src/announce.rs` | `AnnounceEvent :36-75`, `delivered :130`, `collected :147`, `dispatch :229`, `ANNOUNCE_SCHEMA_VERSION :27` | **Only under answer (a) of §3.4.** Add `payment: Option<&'static str>` with `skip_serializing_if`; schema version stays `1` | — |
| 13 | `docs/SELLER-QUICKSTART.md`, `docs/BUYER-QUICKSTART.md` | — | Document `takes_no_payment`, the admission-only mitigation of §6, and the §4.3 mint-still-required constraint | `BUYER-QUICKSTART.md` **PROVISIONAL** |

**Not touched, and deliberately:** `crossmint.rs` (`plan_payment :94`) — the free path never calls
it; `budget.rs` — a free job writes no spend record (§3.1); `seller.rs`'s `SellerJournal` — dead code
at this head (§3.3); `driver/acp.rs:17` — a different `PROTOCOL_VERSION` (§1.2).

---

## Appendix — corrections to the brief's ground, measured here at `8c3bc9b`

Reported because the brief asked to be corrected rather than believed. The five numbered hazards in
the brief's §4 all reproduce; these are additions and two errors.

1. **Reproduces.** `verify_accepted_claim_creq` is `:1601-1640`; the closing brace is `:1640`,
   proven by `fn resolve_accepted_contribution` beginning at `:1642`. **But it has six refusals, not
   five** — `:1606`, `:1611`, `:1616`, `:1622`, `:1628`, `:1634`. Its own doc lists the same six as
   bullets at `:1593-1598`.
2. **Reproduces exactly.** Two `PROTOCOL_VERSION` constants, `gateway.rs:10` (wire, `&str`) and
   `driver/acp.rs:17` (ACP, `u32`); `git grep -n "const PROTOCOL_VERSION" -- crates` returns those
   two lines and no others.
3. **Reproduces.** `gateway.rs:401` compares by equality with no range;
   `buzz-relay/src/audio/handler.rs:417` accepts `1..=CURRENT_PROTOCOL_VERSION` (`:124`). Adding to
   it: `heartbeat.rs:1027` is a **second** strict-equality wire check, on the seat announcement — so
   a bump would partition seat discovery as well as offers.
4. **Reproduces.** `deliveries` is written by `store.rs deliver_and_enqueue :911`, INSERT `:934`,
   DDL `:311`; `budget.rs` writes the spend. Adding to it: the `jobs.state` CHECK at `:291-292` and
   `migrate`'s additive-only contract at `:386-391` together forbid a new terminal state (§3.2).
5. **Reproduces and strengthens.** `git grep -l AnnounceEvent -- crates` returns exactly 1 path.
   Adding to it: `dispatch` has no caller — the only `announce::` uses repo-wide are three
   references to `run_sink` from `telemetry.rs`, and `telemetry.rs`/`episode.rs` are themselves
   unreferenced outside doc comments. The whole observability tier is dead at this head.
   A fourth structural candidate the brief did not list — the kind-3403 RESULT enqueued *inside*
   `deliver_and_enqueue` (`store.rs:942-949`) — is why §3.4 hands the choice back rather than
   picking.

**Two corrections to the brief's §3 ground:**

- **`[seller] rate_sats` is `home.rs:195` only.** The brief cites "`:159` (`Option<u64>`) and `:195`
  (`u64`)" as one field. They are two fields in two different structs: `:159` belongs to
  **`BuzzConfig`** (`pub struct BuzzConfig` at `:148`), documented at `:143-147` as *"Deprecated,
  runtime-ignored schema compatibility … The buzz persona was never wired into production and has
  been removed"* — it is the kind-0 rate-card display value, not the claim floor. `:195` belongs to
  `SellerConfig` (`:192`) and is the seat minimum the rate gate reads. A second name collision of
  exactly the `PROTOCOL_VERSION` kind.
- **A gate the brief's list does not contain, and it is the first one a free job hits.**
  `post_job_async` opens a wallet and dust-guards the amount at `job_lifecycle.rs:592-603`, so at
  `8c3bc9b` an `amount = 0` offer **cannot be posted at all**, by any buyer, wallet or not. See §2.6
  and §5.

**Production/test boundaries re-measured** (`grep -n '#\[cfg(test)\]'` per file, at `8c3bc9b`): every
line cited above as production sits before its file's test module — `buyer/lifecycle.rs :1522`;
`job_lifecycle.rs :2950` (its `:2175` is a single-fn decoration); `payment_wallet.rs :624` decorates
one fn closing at `:637`, the module is `:2251`; `gateway.rs :1157`; `heartbeat.rs :1123`;
`store.rs :1435`; `budget.rs :461`/`:467`; `home.rs :2203`; `announce.rs :287`;
`authorize_pay.rs :1188`; `seller.rs :717`. `seller_node/run.rs` has several
(`:1520`, `:1703`, `:6731`, `:7230`, `:14631`); the sites cited here — `:2213`, `:4448-4478`,
`:4579`, `:4885-5000`, `:5090`, `:6278`, `:6419`, `:6648` — each fall outside them.
