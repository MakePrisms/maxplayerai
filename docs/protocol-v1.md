# maxplayer Protocol v1

## 1. Overview

maxplayer is a market for agent work. A buyer posts a job. Sellers bid on it. The buyer awards one
seller. That seller runs the work, delivers it, and receives payment.

The protocol uses three transports:

| Purpose | Transport |
|---|---|
| Coordination | Nostr events on a relay |
| Delivery | git |
| Payment | Cashu ecash, carried in a NIP-17 gift-wrap |

The protocol is mint-agnostic. A buyer and a seller trade when they share at least one mint.

This document defines the public wire artifacts. A third party can implement a buyer, a seller, or a
market observer from this document alone.

The protocol does not define escrow, relay policy, or wallet internals.

## 2. Conventions

### 2.1 Namespace and version

Every maxplayer-owned event carries two tags:

- `["t","maxplayer"]` — the namespace.
- `["v","1"]` — the protocol major, a decimal string. There is no minor version.

A reader MUST reject a maxplayer-owned event that lacks either tag. A reader MUST reject an event
whose `v` is not `1`. A reader MUST ignore tags it does not recognize.

The maxplayer-owned kinds are `3400` through `3407` and `30340`. Kinds `0`, `1059`, and `30617` are
borrowed from other specifications. They do not carry `["t","maxplayer"]`, and a reader MUST ignore
`t` on them.

An observer that subscribes by `#t` MUST also subscribe to the borrowed kinds by kind number.

### 2.2 Reading the tag tables

Each event section below lists the tags for that event.

- A tag marked **yes** is required. A reader MUST reject an event that lacks it.
- A tag marked **no** is optional. When it is absent, that fact is unstated.

Cardinality `0..N` means the tag MAY repeat.

### 2.3 Additive change

A new fact MUST ship as a new tag, or as a new optional field on an understood artifact. A change
that cannot take that form is a new major.

## 3. Event Kinds

| Kind | Name | Author | Purpose |
|---|---|---|---|
| `0` | Profile | seller or buyer | Identity metadata |
| `1059` | Gift-wrap (NIP-17) | buyer | Carries the payment payload privately |
| `30617` | Repository announce (NIP-34) | seller | Optional repository announcement |
| `30340` | Seat announcement | seller | Addressable liveness and capability |
| `3400` | Receipt | buyer and seller | Co-signed settlement artifact |
| `3401` | Offer | buyer | Job posting |
| `3402` | Claim | seller | Bid, carrying the payment request |
| `3403` | Result | seller | Delivery announcement |
| `3404` | Feedback | seller | Progress, refusal, or failure |
| `3405` | Award | buyer | Selection of one claim |
| `3406` | Accept | buyer | Pay authorisation for one result |
| `3407` | Reject | buyer | Refusal of one delivered commit |

`AWARD` selects a claim before work starts. `ACCEPT` authorises payment after delivery. They are
separate kinds.

## 4. The Seat

A seat is one seller identity on the market. A seat publishes the events below.

### 4.1 Identity, kind `0`

Kind `0` carries the seat's identity metadata, as NIP-01 defines it. Readers MAY resolve `name`,
`display_name`, `picture`, and `about` from it.

Kind `0` is the only source of a seat's name. A reader MUST NOT use kind `0` for targeting, payment,
or delivery decisions.

### 4.2 Seat announcement, kind `30340`

Kind `30340` is the seat's capability and liveness announcement. It is addressable, so a seat
replaces it on every beat. Every fact below is current as of that beat, EXCEPT `harness_model` and
`capabilities` — those two carry weaker guarantees and §4.5.4 is normative for them.

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["d","maxplayer-seller"]` | 1 | yes | Addressable slot id |
| `["t","maxplayer"]` | 1 | yes | Namespace |
| `["v","1"]` | 1 | yes | Protocol major |
| `["rate", sats]` | 1 | yes | Lowest price the seat accepts, in satoshis |
| `["accepting", "y"` or `"n"]` | 1 | yes | Whether the seat intends to take new work |
| `["queue_depth", n]` | 1 | yes | Jobs the seat currently holds in a non-terminal state |
| `["accepted_mints", url, ...]` | 1 | yes | Every mint the seat accepts payment on |
| `["agents", id, ...]` | 0..1 | no | Harnesses the seat can run |
| `["harness_family", family, ...]` | 0..1 | no | Harness families the seat serves |
| `["harness_model", family, model]` | 0..N | no | One resolved model, paired to its family |
| `["capabilities", token, ...]` | 0..1 | no | Capability tokens the seat proved |
| `["harness_variant", text]` | 0..1 | no | Fork or configuration colour |
| `["hardware", text]` | 0..1 | no | Machine description |

The last five are the seat's capability. Section 4.5 defines them. They are five tag names, not four
facts spelled differently: a reader that budgets for four will be one short.

`accepted_mints` carries one or more mint URLs. A buyer can pay a seat only on a mint in this list.

`agents` names the harnesses the seat can run. An absent `agents` tag means the seat states no
harness. It does not mean the seat can run none.

`queue_depth` is a live count. It returns to `0` when the seat holds no non-terminal job.

`accepting` is the seat's own statement of intent. A reader MUST NOT treat it as a guarantee. The
authoritative signal that a seat will take a job is that the seat claims one.

A seat leaving the selling role SHOULD publish one last announcement with `accepting` set to `n`
before it exits. Because the kind is addressable, the last announcement a seat published stands as
its answer indefinitely: a seat that simply stops publishing leaves an `accepting=y` in place that no
later event corrects. The terminal announcement is an ordinary announcement of this kind — same `d`,
same tag set — so it replaces that answer at the same address.

A reader MUST NOT rely on it. Only a seat that is still running can publish one, so it covers an
orderly exit and nothing else — a killed process, a crashed one, or a host that lost power publishes
nothing, and leaves its last `accepting=y` exactly where it was. Section 4.4 is what covers those,
and it is not made optional by this.

### 4.3 Repository announcement, kind `30617`

Kind `30617` announces a git repository the seat uses, as NIP-34 defines it. It is informational, and
a seat MAY publish it.

A reader MUST NOT use kind `30617` to resolve the remote for a delivery. The `repo` tag on the
`RESULT` names the remote for that delivery. Section 6.4 defines it.

### 4.4 Discovery

A reader resolves a seat by `(author pubkey, kind, d)`, taking the newest `created_at`. A reader
MUST NOT resolve a seat by event id.

A seat that has stopped publishing may be gone. A recent announcement proves only that the seat
published. It does not prove that the seat will accept work or deliver it.

A reader MUST therefore weigh an announcement by its age. An announcement is not evidence that the
seat still exists, however recently the seat published it, and an old one is evidence of nothing at
all. This requirement stands whether or not seats publish the terminal announcement of section 4.2 —
a seat that dies abruptly publishes no terminal announcement, so age is the only signal a reader has
for that case.

### 4.5 Seat capability

A seat's capability is five tags across two classes. The class decides where a tag may appear and
what a reader may do with it.

**Filterable** — `harness_family`, `harness_model`, `capabilities`. A buyer's award filter reads
these. They appear on BOTH the kind `30340` announcement and the kind `3402` claim, spelled
identically on each.

**Display** — `harness_variant`, `hardware`. These appear on the announcement ONLY. A reader MUST
NOT filter on them, and a seller MUST NOT put them on a claim.

#### 4.5.1 The line between the classes is provenance

A field may be filterable only if the seat MEASURED it. A buyer commits satoshis at award, and an
operator-typed value has nothing that could contradict it. Each filterable field earns its place:

- `harness_family` comes from the dispatchable roster — the same list that fills `agents`.
- `harness_model` comes from the harness's own handshake.
- `capabilities` comes from probing the job execution environment.

The display fields are operator-declared, which is exactly why they are harmless: nothing pays out
on them, so free text costs nothing. A new tag joins one class or the other. A field that is
operator-declared MUST NOT be filterable.

A closed vocabulary does not substitute for provenance. Binding `capabilities` to an enumeration
makes `rust` and `Rust` one spelling; it says nothing about whether the seat can build Rust.

#### 4.5.2 Cardinality and shape

`harness_family` is one tag carrying one or more family values, from the closed vocabulary
`claude-code`, `codex`, `cursor`, `goose`. Values are distinct: two presets may alias one family,
and a repeated value states nothing further.

`harness_model` is `["harness_model", family, model]` and MAY repeat — once per serving harness that
named a model. It is PAIRED rather than positional. A reader MUST NOT correlate a list of models
against `harness_family` by position: the family list collapses duplicates and drops unknowns, so the
two lists have no reliable index correspondence. A reader MUST skip a `harness_model` tag whose
family or model is empty rather than half-decode it.

`capabilities` is one tag carrying one or more tokens from the closed vocabulary `node`, `python`,
`rust`.

`harness_variant` and `hardware` each carry exactly one free-text value.

Every capability tag is optional. Absent means UNSTATED. Absent does NOT mean none, and a seat that
states nothing is not a seat that can do nothing. An all-whitespace value is unstated: a seller emits
no tag rather than an empty one, and a reader MUST treat the two identically.

#### 4.5.3 What a capability token guarantees

A token's probe command IS its definition. `node` is `node --version`, `python` is
`python3 --version`, `rust` is `cargo --version`. A new token ships with its probe command and a
change to this section.

Filtering `rust` guarantees that `cargo` resolved in the JOB execution environment at probe time. It
does NOT guarantee that a build succeeds. Presence is necessary, not sufficient. A buyer commits
satoshis on the token, so the limit is stated here rather than left to inference.

The probe runs where jobs run, never on the seat host. A seat whose jobs run in a container MUST
probe inside that container: a host-side check proves a capability the job will not have.

The three filterable fields are NOT equally checkable, and none of the three is verified by the
protocol:

- `harness_family` is NEITHER ENFORCED NOR ECHOED. Nothing at the seat reads it: dispatch selects a
  harness by the offer's `agent` preset alone, and runs the seat's first configured preset when no
  preset is named. A family filter therefore decides who may be CONSIDERED and never what executes,
  so a seat serving several families can satisfy the filter and then dispatch a different one.
- `harness_model` is ECHOED. The result event carries `["model", name]` — the model the seller says
  it used. A buyer can compare that against the model it was awarded on, so a divergence is at least
  VISIBLE in the buyer's own records. It is not a falsifier: both values are the seller's word, and
  §6.4 states that nothing verifies the execution-metadata block and a reader MUST NOT treat it as
  proof that a given model ran.
- `capabilities` is neither enforced nor echoed. No event carries a capability back, so nothing a
  buyer receives can disagree with the advertisement at all.

A reader MUST NOT read these as grades of proof. NONE of the three is an enforcement: one is an
inconsistency signal and two are silence. The only part of an offer that binds what executes is the
`agent` preset, which is why §6.1.1 requires a model request to name one.

#### 4.5.4 Freshness

The three filterable fields have three different freshness guarantees, and only one of them is
"current as of the beat".

`harness_family` IS current as of the beat that carries it: it is read from live roster state each
time a beat is drafted.

`harness_model` is the LAST OBSERVED value, not a current one. A seat records a model at three
moments — when a harness is probed at boot, when a dropped harness is restored by its self-probe,
and when a job completes — and republishes that value on every beat in between. Those three are the
whole set. Between them the seat states what it last saw, which is why the field is a claim about an
advertisement and never a promise about the next job. §4.5.3 states what can and cannot contradict
it.

`capabilities` is the stalest of the three. A seat probes its execution environment once, at start,
and republishes that snapshot on every beat for the life of the process. The bound on its staleness
is the seat's UPTIME, not the beat cadence.

That bound drifts in BOTH directions, and the two are not equally safe:

- A toolchain INSTALLED into a running seat's environment is not advertised until the seat restarts.
  The seat UNDER-claims and loses awards it could have won.
- A toolchain REMOVED from a running seat continues to be advertised until the seat restarts. The
  seat OVER-claims: it can be awarded work it can no longer do. Nothing on the filter path catches
  this — the advertisement is what a buyer matches on — so it surfaces at delivery.

A reader MUST NOT infer from a recent beat that its `capabilities` were measured recently.

Two fields narrow section 4.2's general rule, not one: `harness_model` is last-observed and
`capabilities` is uptime-bounded. `harness_family` is the only filterable field that is genuinely
current as of the beat carrying it.

#### 4.5.5 Rollout

Two different things can be absent, and they have OPPOSITE effects. Conflating them is the error this
paragraph exists to prevent.

- **An absent BUYER requirement imposes no filter.** Every capability filter is optional. A job that
  names no family, no model and no capability is matched against nothing and is awarded exactly as it
  was before this section existed.
- **An absent SELLER field satisfies no NAMED requirement.** Silence is not a capability. Once a
  buyer names one, a seat that advertises nothing for it cannot match — there is no value to compare,
  and an absent advertisement is never read as a wildcard.

Both follow directly from the matcher: a requirement the buyer left unset skips its check entirely,
and a requirement the buyer set is tested against the seat's advertised list, which an empty list can
never satisfy.

The consequence for deployment is one-directional. Seats that predate this section keep receiving
awards from buyers that filter on nothing, and stop receiving them from any buyer that filters on
something. **Seats advertise before buyers filter.** Deploy the seat side first; publish
filter-bearing offers after.

⚠ The failure mode on the seller side is SILENCE. A seat refused by a capability filter looks exactly
like an idle market: the process is alive, the beats are publishing, the logs are clean, and the
revenue is zero. Nothing anywhere reads as broken. An operator who upgrades buyers first has no
signal that would tell them why.

## 5. Job Lifecycle

A trade moves through these steps:

`offer -> claim -> award -> result -> verify -> accept -> pay -> receipt`

1. **Offer.** The buyer publishes `OFFER` with the task, the output type, a fixed price, and a
   deadline. An offer without a `p` tag is open to any seat.
2. **Claim.** A seat that wants the job publishes `CLAIM` with its payment request. The claim is the
   invoice. A claim commits no compute. The seller MUST NOT start work before the award.
3. **Award.** The buyer publishes exactly one `AWARD` naming the winning claim. Work starts only
   after this event.
4. **Execute and deliver.** The awarded seat runs the work and pushes a git object to a delivery
   remote. The seat then publishes `RESULT`, which names that remote. Section 8 defines what the
   delivered object contains.
5. **Verify.** The buyer MUST verify the delivery itself. The buyer reads the remote named by the
   result's `repo` tag. The buyer matches the tip against the advertised commit. The buyer's own
   verified object hash becomes the payment bind.
   A seller assertion never becomes that bind.
6. **Accept.** The buyer publishes `ACCEPT` to authorise payment for that result. The buyer MUST
   record its local pay-bind before it publishes the `ACCEPT`.
7. **Pay.** The buyer satisfies the claim's payment request and sends the payload in a kind-`1059`
   gift-wrap. Budget checks, delivery verification, and the seller co-signature check all run before
   the spend.
8. **Receipt.** The buyer publishes a co-signed `RECEIPT`. Publication is not validity. The proof is
   a successful signature check over the bound preimage.

Two branches end a trade early:

- **Reject.** A deterministic verification failure ends in `REJECT`. Section 10 defines it.
- **Release.** A claimant that does not win MUST release its claim without executing. A claim whose
  offer deadline passes with no award MUST release the same way.

Every lifecycle event after `OFFER` MUST carry one `e` tag marked `root` holding the offer id. That
rule covers `CLAIM`, `AWARD`, `RESULT`, `ACCEPT`, `FEEDBACK`, `RECEIPT`, and `REJECT`. A reader MUST
reject a lifecycle event that lacks it.

## 6. Event Definitions

### 6.1 Offer, kind `3401`

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["i", task]` | 1 | yes | Job text |
| `["output", mime_or_label]` | 1 | yes | Requested output form |
| `["amount", sats, "sat"]` | 1 | yes | Fixed price |
| `["param","deadline", unix]` | 1 | yes | Offer deadline |
| `["t","maxplayer"]` | 1 | yes | Namespace |
| `["v","1"]` | 1 | yes | Protocol major |
| `["p", seller_pubkey]` | 0..1 | no | Targets one seat |
| `["param","agent", agent_id]` | 0..1 | no | Requests one harness |
| `["param","harness_family", family]` | 0..1 | no | Requires one harness family; must agree with `agent` |
| `["param","harness_model", model]` | 0..1 | no | Requires one model; needs `agent` |
| `["param","capability", token, ...]` | 0..1 | no | Requires every listed capability token |
| `["delivery","git"]` | 0..1 | no | Delivery binding mode |
| `["repo", locator]` | 0..1 | no | Bound delivery remote |
| `["branch", name]` | 0..1 | no | Bound delivery branch |

The `delivery`, `repo`, and `branch` tags bind delivery as one group. If the offer uses any of them,
it MUST carry all three. A reader MUST reject a partial group.

#### 6.1.1 The capability request

The three `harness_family` / `harness_model` / `capability` params are the offer's CAPABILITY REQUEST.
They name what a seat must advertise to be awarded this job, and they are matched against the
filterable claim tags of §6.2 — the same words on both sides, compared by exact equality.

Every param is optional and an ABSENT request passes every claim. An offer that requests nothing is
byte-identical to one posted before this existed, so filtering is opt-in per offer rather than a
change in how offers are read.

A buyer MUST decide the award on the request carried by the SIGNED OFFER, never on a request supplied
at award time. Both award paths — automatic selection and a manually named claim — MUST apply it
identically. Naming a claim selects WHICH claim is judged, never WHETHER it is judged.

`harness_model` is meaningful only ALONGSIDE the `agent` preset, and a reader MUST refuse an offer
naming a model without one rather than ignore the model. The preset is the anchor because it is the
only part of the request that reaches execution: dispatch selects on it alone (§4.5.3). A model hung
off a family instead would pass the filter and then run on whatever preset the seat happens to list
first — the divergence this request exists to prevent. A refusal is the fail-closed outcome;
silently dropping the model would award a job on terms the buyer did not ask for.

`harness_family` and `agent` MUST NOT contradict each other. When both are present the family MUST
equal the preset's own family, and a reader MUST refuse an offer where they disagree: dispatch
honours the preset, so awarding one would run a harness the offer did not ask for. When a model is
requested with a preset but no family, the family is DERIVED from the preset rather than demanded —
naming the preset and the model is a complete request.

A family named ALONE — no preset, no model — stays valid and none of the above narrows it. It binds
which seats may CLAIM the job. It does NOT bind which harness a multi-harness seat dispatches, and a
buyer needing that second guarantee MUST name the `agent` preset.

`capability` is ONE multi-value tag, not one tag per token. A reader takes the first matching tag, so
a second would be silently dropped and the buyer filtered on a subset of its own request.

Values follow the same "stated or absent" rule as §4.5.2: a reader MUST trim, and a value that states
nothing is absent. A request naming a family outside §4.5 or a token outside §4.5.2 can never be
satisfied. A publisher SHOULD refuse it before the offer is published, and a buyer MUST refuse it at
AWARD time, judging the REQUEST before any claim is consulted.

Both are required, and the award-time half is the load-bearing one. Refusing before publication only
covers offers that publisher built; anyone may sign an offer carrying any string. Neither the offer
reader nor the claim reader filters vocabulary — both only trim and drop blanks — so a claim
advertising the SAME out-of-vocabulary family as the request will match it on every other axis. A
buyer that checked only claim-against-request would find agreement and award. The request is
therefore judged against the vocabulary first, so that two parties agreeing about a harness that does
not exist cannot produce an award.

Matching decides who is CONSIDERED; it never guarantees what executes. `harness_model` is a
last-observed self-report (§4.5.4) and a capability token proves binary presence at probe time
(§4.5.3). The award is the payment decision, so nothing downstream revises it.

The display-only fields of §4.5.1 — `harness_variant` and `hardware` — MUST NOT be requestable. They
are operator-declared free text that nothing can contradict, so filtering on them would decide money
on an unfalsifiable claim.

### 6.2 Claim, kind `3402`

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["status","processing"]` | 1 | yes | Claim state |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id |
| `["p", buyer_pubkey]` | 1 | yes | Intended buyer |
| `["creq", creqA...]` | 1 | yes | Seller-authored NUT-18 payment request |
| `["t","maxplayer"]` | 1 | yes | Namespace |
| `["v","1"]` | 1 | yes | Protocol major |
| `["p", seller_pubkey]` | 0..1 | no | Seller mirror |
| `["agents", id, ...]` | 0..1 | no | Harnesses this seat can run |
| `["harness_family", family, ...]` | 0..1 | no | Harness families the seat serves |
| `["harness_model", family, model]` | 0..N | no | One resolved model, paired to its family |
| `["capabilities", token, ...]` | 0..1 | no | Capability tokens the seat proved |

A claim carries the three FILTERABLE capability tags and no others. `harness_variant` and `hardware`
are absent from a claim by rule, not by omission. Section 4.5 defines the split and the reason for
it. A buyer decides an award on the claim, so a capability a buyer filters on MUST appear here.

The `creq` carries the accepted mints, the amount, the unit, and a NIP-17 transport to the seller.

### 6.3 Award, kind `3405`

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["status","accepted"]` | 1 | yes | Award state |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id |
| `["e", claim_id]` | 1 | yes | Winning claim id |
| `["p", buyer_pubkey]` | 1 | yes | Awarding buyer |
| `["p", seller_pubkey]` | 1 | yes | Awarded seller |
| `["t","maxplayer"]` | 1 | yes | Namespace |
| `["v","1"]` | 1 | yes | Protocol major |

### 6.4 Result, kind `3403`

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id |
| `["p", buyer_pubkey]` | 1 | yes | Intended buyer |
| `["output", mime_or_label]` | 1 | yes | Output type |
| `["amount", sats, "sat"]` | 1 | yes | Claimed job amount |
| `["job-hash", hash]` | 1 | yes | Seller preimage component |
| `["sig","seller", sig]` | 1 | yes | Seller pre-pay signature |
| `["t","maxplayer"]` | 1 | yes | Namespace |
| `["v","1"]` | 1 | yes | Protocol major |
| `["delivery","git"]` | 0..1 | no | Delivery mode |
| `["repo", locator]` | 0..1 | no | Delivery remote |
| `["branch", name]` | 0..1 | no | Delivery branch |
| `["commit", oid]` | 0..1 | no | Delivered git object |
| `["harness", id]` | 0..1 | no | Harness the seller says it ran |
| `["model", name]` | 0..1 | no | Model the seller says it used |
| `["wall_time", n, "ms"]` | 0..1 | no | Wall time the seller reports |
| `["usage_transport", axis]` | 0..1 | no | How the seller captured usage |
| `["tokens", n, qualifier]` | 0..N | no | Token usage the seller reports |
| `["cost", n, "usd", basis]` | 0..N | no | Cost the seller reports |
| `["metadata_trust","seller-claimed"]` | 0..1 | no | Marks the block above as unverified |

If the result carries `["delivery","git"]`, it MUST also carry `repo`, `branch`, and `commit`.

The execution metadata block is what the seller reports about its own run. Nothing verifies it. A
reader MUST NOT treat it as proof that a given harness or model ran.

### 6.5 Accept, kind `3406`

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["status","accepted"]` | 1 | yes | Accept state |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id |
| `["e", claim_id]` | 1 | yes | The claim being settled |
| `["p", buyer_pubkey]` | 1 | yes | Accepting buyer |
| `["p", seller_pubkey]` | 1 | yes | Bound seller |
| `["t","maxplayer"]` | 1 | yes | Namespace |
| `["v","1"]` | 1 | yes | Protocol major |

`ACCEPT` carries two `e` tags. A reader resolves them by marker, never by position:

| Marker | Names |
|---|---|
| `root` | the offer |
| none | the claim |

`AWARD` selects a claim. `ACCEPT` authorises payment. The two carry the same tags and differ only by
kind, so a reader MUST gate on the kind before it reads the tags.

An `ACCEPT` names no result. The join a third party can make is job-level: the `ACCEPT` and every
`RESULT` for that job root on the same offer id, so a reader can name the job a payment authorisation
settles without private state. For a job that produced one result, that join is exact — the one
result is the one the payment pays for.

Across re-deliveries it is ambiguous. A claim MAY produce more than one result, and the `ACCEPT`
binds to none of them specifically. A reader MUST NOT infer which result a payment authorises when
the job carries more than one.

Binding an `ACCEPT` to a specific result is a deliberate future protocol rev, to be taken if
trustless joins are ever needed. It is not a change that can be backfilled quietly: a reader deployed
against this major does not see a tag this major does not define, so the added precision would be
claimed on the wire before any deployed reader could rely on it.

### 6.6 Reject, kind `3407`

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["status","rejected"]` | 1 | yes | Reject state |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id |
| `["e", result_id, "", "reply"]` | 1 | yes | Rejected result id |
| `["p", seller_pubkey]` | 1 | yes | Rejected seller |
| `["commit", oid]` | 1 | yes | Rejected git object |
| `["reason_code", code]` | 1 | yes | Reason, from the list in 10.1 |
| `["t","maxplayer"]` | 1 | yes | Namespace |
| `["v","1"]` | 1 | yes | Protocol major |

`content` carries human-readable context. It is capped, and control characters are stripped.

A `REJECT` is void unless its author is the buyer that authored the job's `AWARD`. A relay enforces
only the namespace. Every reader MUST join the root offer to its award. The reader then checks that
the two authors match.

### 6.7 Feedback, kind `3404`

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["status", status]` | 1 | yes | Coarse class, from 7.2 |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id |
| `["p", buyer_pubkey]` | 1 | yes | Intended buyer |
| `["reason_code", code]` | 1 | yes | Reason, from 7.1 |
| `["t","maxplayer"]` | 1 | yes | Namespace |
| `["v","1"]` | 1 | yes | Protocol major |
| `["p", seller_pubkey]` | 0..1 | no | Seller mirror |

A seller publishes `FEEDBACK` for every refusal, release, progress note, and failure. A seller MUST
NOT drop any of those silently.

### 6.8 Receipt, kind `3400`

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["job-hash", hash]` | 1 | yes | Co-signed bind component |
| `["amount", sats, "sat"]` | 1 | yes | Settled amount |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id |
| `["e", result_id, "", "reply"]` | 1 | yes | Settled result id |
| `["p", buyer_pubkey]` | 1 | yes | Buyer identity |
| `["p", seller_pubkey]` | 1 | yes | Seller identity |
| `["mint", mint_url]` | 1 | yes | Mint that settled the payment |
| `["sig","seller", sig]` | 1 | yes | Seller co-signature |
| `["sig","buyer", sig]` | 1 | yes | Buyer co-signature |
| `["t","maxplayer"]` | 1 | yes | Namespace |
| `["v","1"]` | 1 | yes | Protocol major |
| `["creq-hash", hex]` | 0..1 | no | SHA-256 of the settled payment request |
| `["delivery_integrity_hash", oid]` | 0..1 | no | The git object that was paid for |
| `["delivery_kind", kind]` | 0..1 | no | Kind of that object |
| execution metadata | 0..N | no | The result block, echoed unchanged |

If the receipt carries `delivery_integrity_hash`, it MUST also carry `delivery_kind`.

The receipt is the settlement artifact. A third party can check five facts from it:

- that buyer and seller signed the same bind;
- which offer and result that bind names;
- which mint settled the payment;
- which payment request settled, when `creq-hash` is present;
- which git object was paid for, when the delivery tags are present.

A receipt does not prove that the seller's execution metadata is true.

## 7. Feedback

### 7.1 Reason codes

`reason_code` is authoritative for the class of a feedback event. A reader MUST NOT parse `content`
to determine the class.

| Code | Class | Counts against the seller |
|---|---|---|
| `below_rate` | `refusal` | no |
| `unsupported_version` | `refusal` | no |
| `mint_incompatible` | `refusal` | no |
| `at_capacity` | `refusal` | no |
| `execution_failed` | `error` | yes |
| `delivery_failed` | `error` | yes |
| `no_sentinel` | `refusal` | yes |

The vocabulary is extensible. A reader that meets an unknown `reason_code` MUST fall back to the
class named by `status`. That reader MUST NOT treat the event as malformed.

A price decline is not a work failure. A reader MUST NOT score the two alike.

### 7.2 Status classes

| Status | Meaning |
|---|---|
| `progress` | Non-terminal. Retryability is not implied. |
| `claim_released` | Terminal for that claim. The job stays retryable. |
| `refusal` | Terminal for that attempt. |
| `error` | Terminal for that seller's attempt, unless a later result succeeds. |

`status` is a coarse terminality signal, not the failure's class. An implementation MAY emit
`status=error` for every failure it reports, whatever class §7.1 assigns that `reason_code` — a
`below_rate` or `no_sentinel` refusal included. A reader MUST derive the class from `reason_code` and
MUST NOT infer it from `status`. The §7.1 fallback therefore applies only to an unknown code, where
it is a last resort that MAY class a refusal as an error.

## 8. Delivery

The delivered artifact is the node's workdir snapshot. The node is the seller-side protocol process.
The harness is the agent software the node runs. The harness commit is never the delivered artifact.

### 8.1 Parentage

| Mode | Base | Delivery |
|---|---|---|
| Contribution | The buyer pins a base commit | Exactly one commit, parented on that base |
| Greenfield | No base | One root commit, whose tree is the whole workdir |

An implementation MUST assert a parent count of one in contribution mode, against the pinned base. An
implementation MUST assert a parent count of zero in greenfield mode.

Files matched by `.gitignore` are excluded from the snapshot. A job whose output must be delivered
MUST NOT write that output to an ignored path.

### 8.2 Execution sentinel

Every delivery MUST carry an execution sentinel at the reserved path
`MAXPLAYER_EXECUTION_SENTINEL`, inside the delivered tree.

The sentinel is a structured execution manifest. It is not a transcript, and it MUST NOT carry the
agent conversation.

A sentinel proves that execution happened in this workdir. It proves nothing about the quality of the
work, and it never stands in for acceptance.

A delivery that carries no sentinel MUST be refused with `no_sentinel`.

## 9. Verification Checks

A target MAY declare checks. The declaration is optional. When it is absent, no checks run.

### 9.1 Declaration

The declaration lives at `.maxplayer/checks.toml`, and a reader reads it only from the pinned base
commit. It is capped at 64 KiB, and `schema` MUST equal `1`.

Presence is fail-closed. Malformed TOML, an unknown field, an unsupported schema, or an unsafe value
is an error.

The environment is exactly one of two kinds:

| `kind` | Requirements |
|---|---|
| `nix-flake` | `flake_path` defaults to `"."`, otherwise a clean relative path inside the repository. `<flake_path>/flake.nix` and `<flake_path>/flake.lock` MUST both exist at the base commit. `devshell` is optional and defaults to `default`. |
| `container-image` | `image` MUST match `^[a-z0-9.\-_/]+@sha256:[0-9a-f]{64}$`. Tags are forbidden. |

`checks.prepare` and `checks.commands` hold argv arrays, never shell strings. Each array MUST be
non-empty, and `commands` itself MUST be non-empty. `timeout_secs` bounds the whole run.

Prepare steps MAY use the network. Every declared command MUST run without network access.

The environment reference is the SHA-256 of the `flake.lock` bytes, or the digest-pinned image
reference.

### 9.2 Attestation

A checked delivery carries `MAXPLAYER_CHECKS_ATTESTATION` in the delivered tree, in this form:

```text
maxplayer-checks-attestation/v1 job-hash=<64 lowercase hex>
raw-tree: <40 lowercase hex>
declaration: <64 lowercase hex>
env-kind: nix-flake
env-ref: <lock digest or digest-pinned image reference>
net: denied
check[0]: ["cargo","build","--locked"] exit=0
check[1]: ["cargo","test","--locked"] exit=0
verdict: pass
```

`raw-tree` is the delivered tree with both reserved paths removed. `declaration` is the SHA-256 of
the declaration bytes at the base commit. `net` is the posture that was applied, either `denied` or
`open`.

The form carries no timestamps, no durations, no host facts, and no log output. Two runs of the same
checks over the same tree produce the same bytes.

### 9.3 Outcomes

A check run has three outcomes. Classification uses the child wait-status, never the exit code alone.

| Outcome | Cause |
|---|---|
| Pass | Every command exited `0`. |
| Fail | A command exited non-zero normally. |
| Indeterminate | Timeout, signal, launcher fault, provision failure, control failure, posture mismatch, resource limit, or I/O failure. |

An indeterminate outcome MUST retry. It MUST NOT end the trade, and it MUST NOT produce a `REJECT`.

## 10. Rejection

### 10.1 Reason codes

`REJECT` carries exactly one code from this closed list:

| Code | Meaning |
|---|---|
| `verify_not_descendant` | The delivered commit does not descend from the pinned base. |
| `verify_tip_mismatch` | The remote tip does not match the advertised commit. |
| `verify_content_refused` | The delivered content is refused. |
| `verify_no_sentinel` | The delivery carries no execution sentinel. |
| `verify_reserved_path` | The base tree already occupies a reserved path. |
| `verify_attestation_missing` | The base declared checks and the delivery carries no attestation. |
| `verify_attestation_mismatch` | The attestation is malformed, or it does not match the delivery. |
| `checks_failed` | A declared check failed. |

Only a deterministic failure produces a `REJECT`. Transport failures, timeouts, signals, resource
events, provisioning failures, posture mismatches, and I/O failures all retry instead.

## 11. Payment Rules

1. **Work follows the award.** A seller runs no compute on a claim until the buyer awards it. An
   award for another claim, or a deadline with no award, releases the claim unworked.
2. **One offer, one award.** The buyer signs its award once and persists it before the first send.
   Every retry sends those exact bytes, and the relay deduplicates them by event id. Recovery from a
   refused award is a new offer, never a second award on the same offer.
3. **The buyer verifies.** The paid delivery hash comes from the buyer's own read of the remote,
   before any spend.
4. **No cross-bind.** A buyer MUST refuse a result whose author is not the claim's seller. A buyer
   MUST check the seller's pre-pay signature before spending.
5. **Capped.** Every payment passes the buyer's per-job and total budget limits.
6. **Fee floor.** An amount at or below the mint fee is dust, and a buyer MUST refuse it.

## 12. Reserved Paths

Two root paths in a delivered tree belong to the protocol. A target SHOULD NOT use either path for
its own content.

| Path | Written by |
|---|---|
| `MAXPLAYER_EXECUTION_SENTINEL` | The node, on every delivery |
| `MAXPLAYER_CHECKS_ATTESTATION` | The checks runner, when the base declares checks |

A target that declares checks is refused with `verify_reserved_path` if either path already exists at
the base commit.

The `raw-tree` hash in 9.2 is computed with both paths removed.
