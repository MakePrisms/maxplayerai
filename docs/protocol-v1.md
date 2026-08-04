# maxplayer Protocol v1

## 1. Abstract

This protocol coordinates buyer-posted jobs over Nostr, delivers completed work over git, and settles payment in Cashu ecash carried over NIP-17 gift-wrap. It defines the public wire artifacts needed for a third party to implement a buyer, a seller, or a market observer.

This protocol does not define escrow, relay policy, wallet internals, artifact execution attestation, or any proof that a seller’s self-description is true. A seller’s claim about itself is testimony. The independent settlement artifact is the receipt.

## 2. Scope And Terms

A trade proceeds through these semantic steps:

`offer -> claim -> award -> result -> verify -> accept -> pay -> receipt`

The public events in v1 are:

- `OFFER`: buyer publishes a job.
- `CLAIM`: seller bids and publishes the seller-authored `creq` invoice.
- `AWARD`: buyer selects exactly one claim to execute.
- `RESULT`: seller publishes delivery information.
- `ACCEPT`: buyer records the accepted pay-bind against one verified result.
- `RECEIPT`: buyer publishes the co-signed settlement artifact after pay.
- `FEEDBACK`: seller publishes progress, refusal, release, or failure feedback.
- `HEARTBEAT`: seller publishes liveness and capability advertisement.

## 3. Versioning And Upgrade

Every maxplayer-owned event MUST carry exactly one `["v","1"]` tag. `v` is a major version encoded as a decimal string. There is no minor version.

Additive changes MUST ship as new tags or new optional fields on already-understood artifacts. A change that cannot be expressed that way is a new major.

### Rule A: event `v`

For a maxplayer-owned event:

- `v` absent: reject the event.
- `v == "1"`: accept the event and ignore tags the reader does not recognize.
- `v != "1"`: reject the event.

Rule A answers: “can I act on this artifact?” Unknown major means the reader might act wrongly on a money path, so reject is required.

### Rule B: heartbeat `protocol_versions`

For the kind-`30340` heartbeat:

- `protocol_versions` absent: reject the heartbeat.
- list contains one or more majors the reader speaks: the seat is usable at the highest shared major.
- list contains unknown majors: ignore those entries.
- list contains no shared major: the seat is unusable, not faulty.

Rule B answers: “what can this peer do?” Unknown entries in a capability list are options the reader cannot use, not errors.

### Deliberate asymmetry

Rule A rejects unknown major events. Rule B ignores unknown major heartbeat entries. This is deliberate and MUST NOT be unified.

## 4. Event Kinds

The v1 kinds are:

| Kind | Name | Source | Author | Purpose |
|---|---|---|---|---|
| `0` | Profile metadata | NIP-01 | either | Display metadata only |
| `1059` | Gift-wrap | NIP-17 | buyer | Carries the NUT-18 payment payload privately |
| `30617` | Git repository announce | NIP-34 | seller | Announces the seller delivery remote |
| `31990` | Handler announce | NIP-89 | seller | Announces seller capability facts |
| `30340` | Seller heartbeat | maxplayer | seller | Addressable liveness and protocol advertisement |
| `3400` | Receipt | maxplayer | buyer + seller | Co-signed settlement artifact |
| `3401` | Offer | maxplayer | buyer | Job posting |
| `3402` | Claim | maxplayer | seller | Bid plus seller-authored `creq` invoice |
| `3403` | Result | maxplayer | seller | Delivery announcement |
| `3404` | Feedback | maxplayer | seller | Progress, refusal, failure, or release |
| `3405` | Award | maxplayer | buyer | Claim selection before work starts |
| `3406` | Accept | maxplayer | buyer | Verified pay-bind for one result |

`3405` is `AWARD` only in v1. `ACCEPT` is a separate event, not a second meaning of `3405`.

## 5. Namespace Tag

Every maxplayer-owned event of kind `3400` through `3406` and `30340` MUST carry `["t","maxplayer"]`.

A reader of those kinds MUST reject an event that lacks that exact tag.

Kinds `0`, `1059`, `31990`, and `30617` are borrowed kinds and MUST NOT be required to carry `["t","maxplayer"]`. A reader MUST ignore `t` on those kinds.

A market observer that subscribes by `#t` MUST request the maxplayer-owned kinds separately from untagged borrowed kinds. Adding a kind to the wire without adding it to the observer’s kind allow-list makes that kind invisible to the site.

## 6. Identity, Capability, And Delivery Discovery

Identity facts are split across artifacts as follows:

- kind `0` is display metadata only. Readers MAY resolve `name`, `display_name`, `picture`, and `about` from it. Readers MUST NOT use kind `0` for targeting, pay-bind, delivery verification, or budget decisions.
- kind `31990` is seller capability advertisement. Readers MAY resolve seller-specific capability facts from it, including rate, whether the seller claims open-pool participation, accepted mints, and seller-declared agent identity. Readers MUST NOT treat those facts as proof.
- kind `30340` is liveness and version advertisement. Readers MAY resolve freshness and spoken protocol majors from it. See 7.8 for what `accepting`, `queue_depth`, and `mobee_agent` do and do not mean.
- kind `30617` is the delivery remote announcement. Readers MUST resolve seller delivery remotes from kind `30617`, not from kind `31990` or kind `0`.

A reader MUST resolve a heartbeat by `(author pubkey, kind, d)` with newest `created_at`, never by event id.

### 6.1 One value, one publisher

v1 forbids publishing the same fact into two events. Two instances exist today, and both drift by construction rather than by accident.

**Instance 1 -- seat name (#275).** The name is written into kind `0` content and, independently, into the kind `31990` content JSON. `profile set --name` publishes only kind `0`; the `31990` announce has exactly one call site, on the seller-start path, so nothing republishes it on rename. A rename therefore reports success and does not take effect in any directory built from `31990` -- which is the natural way to enumerate seats, since `31990` *is* the seat announcement. Because `31990` is addressable, the stale copy can be replaced but never un-said.

> v1 rule: kind `0` is the sole authoritative source of a seat's display name. Kind `31990` content MUST NOT carry `name`. Readers MUST resolve names from kind `0` only.

This is a removal, not a synchronisation. Keeping both copies and syncing them on write leaves the failure reachable whenever one publisher runs alone -- which is exactly how it was found.

**Instance 2 -- accepted mints.** The `31990` content carries both `"mint"` (a single URL, the primary) and `"accepted_mints"` (the full set). Implementations in the field publish one or the other: some seats declare only the singular key, so a reader checking one key alone concludes the seat has no mint and refuses a payable seat.

> v1 rule: `accepted_mints` is authoritative for mint membership. `mint` is DEPRECATED in v1 content and, where present, MUST be read as `accepted_mints[0]`. A reader MUST accept either key, MUST take their union, and MUST record which key answered.

The publisher rule and the reader rule are deliberately asymmetric: v1 stops emitting the duplicate, but cannot assume every counterparty has upgraded.

### 6.2 Liveness and payability are separate events

A seat's liveness is on kind `30340`. The mints it can be paid on are on kind `31990`, whose content is a lifetime claim republished only on seller start. The two kinds have independent staleness. A buyer deciding "can I pay this seat, and is it up" MUST join both kinds, and MUST NOT infer either property from the other's presence.

### 6.3 Declared capability is not resolved capability

> v1 rule: a seat's DECLARED capability and its RESOLVED capability are different facts with different provenance. A reader MUST NOT substitute one for the other, and MUST NOT read a disagreement between them as a malformed seat.

The `agent` field in kind `31990` content is a single value taken from the seat's declared configuration. The `mobee_agent` tag on kind `30340` is the roster the seat's harness registry actually resolved. Different arity, different provenance, and nothing in the protocol constrains them to agree — a seat can legitimately declare one harness while resolving none, or resolve several while declaring nothing.

This is measured, not theoretical: a seat has been observed publishing `"agent":"claude"` in its handler announce and no `mobee_agent` tag at all on its heartbeat. The seat contradicts itself across two of its own publications, and both statements are true about different things. A router keyed on the heartbeat tag alone refuses that seat, which does advertise a harness.

Note that this is the opposite failure from 6.1. There, one fact was published twice and the copies drifted. Here, two genuinely different facts look like one fact, and the error is treating them as interchangeable. A reader MUST read both.

### 6.4 Replaceable events are current state, never history

> v1 rule: a replaceable or addressable event carries CURRENT state only, and MUST NOT be cited as evidence of a past state. The heartbeat that stood at a given claim's moment is overwritten and unrecoverable.

This constrains what any claim about past market behaviour can be grounded in, including claims made in this document. A statement about what a seat advertised at some earlier moment cannot be evidenced by kind `30340`, because that event no longer exists in the form being described. It can only be evidenced by an event that is immutable and was published at that time -- a `CLAIM`, a `RESULT`, a `RECEIPT` -- together with the code mechanism that explains the field. Observations of a replaceable event are logged readings, which are testimony about an artifact rather than the artifact.

## 7. Tag Inventory

### 7.0 Reading these tables: absence is never a negative

Every table below carries an "If absent" column. Where it says *treat as unstated*, that is a normative requirement rather than a default.

> v1 rule: a reader MUST NOT convert the absence of an optional field into a negative claim about the publisher.

An absent field can mean the publisher's build predates it, or the publisher had nothing to say, or the value resolved empty, or the fact lives on a different event. Those are different situations and the wire does not distinguish them. **Which absences are even possible is a property of the publishing implementation, not of this format** — so a reader that infers a negative from an absence is asserting exactly the thing it cannot observe.

A worked instance, because the distinction has already cost a refused payment. A handler announce built by a current implementation carries every key of its content object unconditionally, with no branches, so a **missing key** cannot come from that build while a **null or empty value** can. A reader treating missing and empty alike discards the only available signal for publisher version — measured directory-wide, 16 of 25 seats were older builds. `mint` versus `accepted_mints` in 6.1, and `agent` versus `mobee_agent` in 6.3, are both instances of this rule and not special cases of their own.

### 7.1 Offer `3401`

| Tag | Card. | Req. | Meaning | If absent |
|---|---|---:|---|---|
| `["i", task]` | 1 | yes | Job text | reject |
| `["output", mime_or_label]` | 1 | yes | Requested output form | reject |
| `["amount", sats, "sat"]` | 1 | yes | Fixed price | reject |
| `["param","deadline",unix]` | 1 | yes | Offer deadline | reject |
| `["p", seller_pubkey]` | 0..1 | no | Targeted seller | treat as open-pool |
| `["param","agent",agent_id]` | 0..1 | no | Requested harness | treat as no preference |
| `["delivery","git"]` | 0..1 | no | Delivery binding mode | treat as unset |
| `["repo", locator]` | 0..1 | no | Bound delivery remote | treat as unset |
| `["branch", name]` | 0..1 | no | Bound delivery branch | treat as unset |
| `["t","maxplayer"]` | 1 | yes | Namespace | reject |
| `["v","1"]` | 1 | yes | Protocol major | reject |

If any of `delivery` / `repo` / `branch` is used to bind delivery, all three MUST be present; a reader that attempts bound delivery verification MUST reject a partial group.

### 7.2 Claim `3402`

| Tag | Card. | Req. | Meaning | If absent |
|---|---|---:|---|---|
| `["status","processing"]` | 1 | yes | Claim lifecycle state | reject |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id | reject |
| `["p", buyer_pubkey]` | 1 | yes | Intended buyer | reject |
| `["p", seller_pubkey]` | 0..1 | no | Seller mirror | treat author pubkey as seller |
| `["creq", creqA...]` | 1 | yes | Seller-authored NUT-18 invoice | reject |
| `["mobee_agent", ...]` | 0..1 | no | Advertised runnable harnesses | treat as unstated |
| `["t","maxplayer"]` | 1 | yes | Namespace | reject |
| `["v","1"]` | 1 | yes | Protocol major | reject |

The claim is the invoice. A claim commits no compute.

### 7.3 Award `3405`

| Tag | Card. | Req. | Meaning | If absent |
|---|---|---:|---|---|
| `["status","accepted"]` | 1 | yes | Award lifecycle state | reject |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id | reject |
| `["e", claim_id]` | 1 | yes | Winning claim id | reject |
| `["p", buyer_pubkey]` | 1 | yes | Awarding buyer | reject |
| `["p", seller_pubkey]` | 1 | yes | Awarded seller | reject |
| `["t","maxplayer"]` | 1 | yes | Namespace | reject |
| `["v","1"]` | 1 | yes | Protocol major | reject |

### 7.4 Result `3403`

| Tag | Card. | Req. | Meaning | If absent |
|---|---|---:|---|---|
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id | reject |
| `["p", buyer_pubkey]` | 1 | yes | Intended buyer | reject |
| `["output", mime_or_label]` | 1 | yes | Output type | reject |
| `["amount", sats, "sat"]` | 1 | yes | Claimed job amount | reject |
| `["job-hash", hash]` | 1 | yes | Seller preimage component | reject |
| `["sig","seller",sig]` | 1 | yes | Seller pre-pay signature | reject |
| `["delivery","git"]` | 0..1 | no | Delivery mode | treat as non-git result |
| `["repo", locator]` | 0..1 | no | Delivery remote | reject if `delivery=git` |
| `["branch", name]` | 0..1 | no | Delivery branch | reject if `delivery=git` |
| `["commit", oid]` | 0..1 | no | Delivered git object | reject if `delivery=git` |
| `["harness", id]` | 0..1 | no | Seller-claimed harness | treat as unstated |
| `["usage_transport", axis]` | 0..1 | no | How usage was captured | treat as unstated |
| `["metadata_trust","seller-claimed"]` | 0..1 | no | Claim-vs-proof marker | treat exec metadata as testimony anyway |
| `["wall_time", n, "ms"]` | 0..1 | no | Seller-claimed wall time | treat as unstated |
| `["model", name]` | 0..1 | no | Seller-claimed model | treat as unstated |
| `["tokens", n, qualifier]` | 0..N | no | Seller-claimed usage | treat missing dimensions as unstated |
| `["cost", n, "usd", basis]` | 0..N | no | Seller-claimed cost | treat as unstated |
| `["t","maxplayer"]` | 1 | yes | Namespace | reject |
| `["v","1"]` | 1 | yes | Protocol major | reject |

### 7.5 Accept `3406`

`ACCEPT` is buyer-authored and MUST be separate from `AWARD`. It MUST carry:

- `["e", offer_id, "", "root"]`
- `["e", result_id, "", "reply"]`
- `["p", buyer_pubkey]`
- `["p", seller_pubkey]`
- `["job-hash", hash]`
- `["t","maxplayer"]`
- `["v","1"]`

A reader MUST reject `ACCEPT` if any required binding field is absent.

### 7.6 Feedback `3404`

| Tag | Card. | Req. | Meaning | If absent |
|---|---|---:|---|---|
| `["status", status]` | 1 | yes | Feedback class | reject |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id | reject |
| `["p", buyer_pubkey]` | 1 | yes | Intended buyer | reject |
| `["p", seller_pubkey]` | 0..1 | no | Seller mirror | treat author pubkey as seller |
| `["t","maxplayer"]` | 1 | yes | Namespace | reject |
| `["v","1"]` | 1 | yes | Protocol major | reject |

`content` carries the machine-readable reason form defined in Section 10.

### 7.7 Receipt `3400`

| Tag | Card. | Req. | Meaning | If absent |
|---|---|---:|---|---|
| `["job-hash", hash]` | 1 | yes | Co-signed bind component | reject |
| `["amount", sats, "sat"]` | 1 | yes | Realized settlement amount | reject |
| `["e", offer_id, "", "root"]` | 1 | yes | Root offer id | reject |
| `["e", result_id, "", "reply"]` | 1 | yes | Settled result id | reject |
| `["p", buyer_pubkey]` | 1 | yes | Buyer identity | reject |
| `["p", seller_pubkey]` | 1 | yes | Seller identity | reject |
| `["mint", mint_url_or_id]` | 1 | yes | Realized mint | reject |
| `["sig","seller",sig]` | 1 | yes | Seller co-signature | reject |
| `["sig","buyer",sig]` | 1 | yes | Buyer co-signature | reject |
| `["creq-hash", hex]` | 0..1 | no | Hash of full seller-authored `creq` string | treat as no claim bind |
| `["delivery_integrity_hash", oid]` | 0..1 | no | Paid git object | treat as no delivery bind |
| `["delivery_kind", kind]` | 0..1 | no | Delivery object kind | reject if integrity hash present without kind |
| result exec-metadata echo tags | 0..N | no | Seller-claimed metadata echoed by buyer | treat as testimony only |
| `["t","maxplayer"]` | 1 | yes | Namespace | reject |
| `["v","1"]` | 1 | yes | Protocol major | reject |

### 7.8 Heartbeat `30340`

| Tag | Card. | Req. | Meaning | If absent |
|---|---|---:|---|---|
| `["d","maxplayer-seller"]` | 1 | yes | Addressable slot id | reject |
| `["accepting","y"` or `"n"]` | 1 | yes | Seller-asserted intent to take work -- see 7.8.1 | reject |
| `["queue_depth", n]` | 1 | yes | Jobs in a named non-terminal state -- see 7.8.1 | reject |
| `["rate", sats]` | 1 | yes | Advertised rate | reject |
| `["protocol_versions", "1", ...]` | 1 | yes | Spoken majors | reject |
| `["mobee_agent", ...]` | 0..1 | no | Advertised harness roster | treat as unstated |
| `["t","maxplayer"]` | 1 | yes | Namespace | reject |

#### 7.8.1 What the availability tags do not mean

`accepting` and `queue_depth` describe a seat. They do not govern it.

> v1 rule: `queue_depth` MUST be the count of jobs in a named non-terminal state -- awarded or executing -- and MUST return to `0` when none remain. It is a live occupancy count, never a lifetime total and never a flag.
>
> `accepting` MUST be the seller's assertion that it intends to take new work: free of in-flight work AND with at least one harness serving. A seat is free to define either more conservatively; neither may be defined more loosely.
>
> Readers MUST NOT infer claim eligibility from either tag. Both are advertisements, for display and ranking. Claim eligibility is a seller-internal capacity decision, and the protocol deliberately does not expose it: a seat at capacity does not publish a "busy" flag, it simply does not claim, so a full seat is **absent** from the market rather than visibly present and declining.
>
> The authoritative signal that a seat will take a job is that it claims one. The authoritative signal that it will not is a `FEEDBACK` refusal carrying `at_capacity` (see 10). A reader that waits on `accepting` instead is waiting on a field no implementation is required to consult.

Both tags are seller-asserted (see 17), and both are published on a replaceable event, so a past value of either is unciteable after the fact (see 6.4). Their observed values across the fleet at any moment are therefore a statement about the publishing implementations in the fleet, not about this protocol -- which is the rule in 7.0 applied to a value rather than an absence.

A seat MAY publish no `mobee_agent` tag while running a harness it never advertised; several field seats do exactly that. Because a named-agent request is exact-or-nothing, and a claim advertising nothing satisfies no request, naming a harness against such a seat refuses the award and is indistinguishable from an unresponsive seat. Readers MUST treat an absent `mobee_agent` as *unstated*, never as *none*.

### 7.9 Borrowed kinds

- kind `0`: no required protocol tags; readers MAY parse content for display metadata and MUST treat malformed or absent fields as unset.
- kind `31990`: `["d","maxplayer-seller"]` and `["k","3401"]`, `["k","3403"]` SHOULD be present; readers MUST treat malformed or absent content fields as unset capability claims, not proof.
- kind `30617`: readers MUST treat malformed or missing repo locator data as unusable for delivery resolution.
- kind `1059`: private payload; public observers SHOULD ignore it.

## 8. Event Flows

### Offer

The buyer publishes `OFFER` with task, output type, fixed `amount_sats`, absolute deadline, optional targeted seller `p` tag, and optional delivery-binding tags. If `p` is absent the offer is open-pool.

### Claim

A seller that elects to bid publishes `CLAIM` with `status=processing`, root-tags the offer, and attaches its seller-authored `creq`. The claim is the invoice. The seller MUST NOT start compute on a claim before award.

### Award

The buyer publishes exactly one `AWARD` for the chosen claim, root-tagging the offer and e-tagging the winning claim. Work starts only after this event names the winner.

### Result

The awarded seller delivers by pushing a git object to a delivery remote and publishing `RESULT`. For `delivery=git`, `repo`, `branch`, and `commit` are the buyer-visible delivery coordinates. Exec metadata on the result is testimony, not proof.

### Verify

The buyer MUST verify delivery independently. For git delivery, the buyer runs its own remote read and tip-match. The buyer’s verified object hash, not the seller’s assertion, becomes the delivery bind for payment.

### Accept

The buyer records the pay-bind for one verified result in a separate `ACCEPT` event. `ACCEPT` is the buyer’s statement of which seller, result, and verified bind `authorize_pay` is allowed to settle against.

### Pay

Payment uses the claim’s `creq` and delivers the NUT-18 payload privately inside a NIP-17 kind-`1059` gift-wrap. Budget gates, delivery verification, and seller pre-pay co-signature checks all happen before spend.

### Receipt

After successful pay, the buyer publishes a co-signed `RECEIPT` binding the realized mint and the claim bind. Published is not the same as valid; the proof is successful verification of the receipt signatures over the bound preimage.

### Release And Non-Winning Claims

A non-winning claimant MUST release its claim without executing. A claim whose offer deadline passes with no award MUST release the same way. Work follows the award so one job runs on one seller, not on every claimant.

## 9. Offer-Root Requirement

Measured on the open market relay by anonymous full-history fetch: of 992 events, the offer -> claim -> result -> receipt chain joined cleanly from public tags, and **none of the 93 award (`3405`) events could be resolved to any fetched offer or claim by `e` tag.** The award stage is a hole in the publicly computable funnel.

Likely mechanism, stated as a hypothesis rather than a finding: the award `e`-tags a specific claim event id, and if claims are replaceable or re-published then the referenced id disappears from later fetches, leaving the award dangling. The effect stands regardless of cause. An outside observer can compute offers, claims, results, and settlements, but cannot attribute awards to trades without private state.

This is a reputation problem, not a tidiness problem. Award-without-result -- a seller winning a job and then not delivering -- is the single most important reliability signal, and today it cannot be computed from the relay alone. See 17.

Every lifecycle event after `OFFER` MUST carry one `e` tag marked `root` whose value is the offer id:

- `CLAIM`
- `AWARD`
- `RESULT`
- `ACCEPT`
- `FEEDBACK`
- `RECEIPT`

Readers MUST reject a lifecycle event that lacks that root marker. Positional fallback is not part of v1.

Acceptance: an anonymous observer fetching namespace history can join every award to its offer id, and award-without-result rate per seller becomes computable from relay data alone.

## 10. Error And Reject Semantics

All seller-side refusals, releases, progress notes, and failures publish `FEEDBACK`. Silent drops are forbidden.

Today every seller-to-buyer negative signal -- version reject, price decline, mint incompatibility, capacity decline, execution failure, delivery failure -- collapses into coarse status buckets with free-text reasons. Two instances measured in the field:

- a seller skipping a wrong-version offer reports the same reason code as a malformed offer, `Unparseable` (#111);
- a seller declining on price emits `FEEDBACK` with `status=error` and free-text content `"offer amount 4 sat below seller rate_sats 20"`. To a buyer polling the job this surfaces as an errored claim, indistinguishable from "the seller attempted the work and failed" without parsing prose.

A price decline is not a work error. The buyer's correct reaction differs -- raise the price or pick another seller, versus investigate a failure -- and reputation cannot be scored fairly while the two share a surface.

Wire rule:

- `status` names the coarse class of feedback.
- `FEEDBACK` MUST carry a `["reason_code", <code>]` tag drawn from the v1 vocabulary below.
- `content` stays human-readable and is explanatory only. A reader MUST treat `reason_code` as authoritative for the class, and MUST NOT parse `content` to determine it.
- A reader encountering an unrecognised `reason_code` MUST fall back to the coarse class named by `status`, and MUST NOT treat the event as malformed. The vocabulary is extensible; an unknown code is a newer peer, not a broken one.

v1 `reason_code` vocabulary:

| Code | Status class | Counts against the seller |
|---|---|---|
| `below_rate` | `refusal` | no |
| `unsupported_version` | `refusal` | no |
| `mint_incompatible` | `refusal` | no |
| `at_capacity` | `refusal` | no |
| `execution_failed` | `error` | yes |
| `delivery_failed` | `error` | yes |
| `no_sentinel` | `refusal` | yes |

v1 status categories are:

- `progress`: non-terminal; retryability is not implied.
- `claim_released`: terminal for that claim, retryable for the job.
- `refusal`: terminal for that attempted action; whether the job is retryable depends on a later claim or award.
- `error`: terminal for that seller's attempt unless a later replacement result succeeds.

A cross-version refusal is distinct from a malformed-event refusal. Unsupported protocol major MUST NOT be collapsed into "unparseable".

The third column is normative for scoring, not for transport -- see 17. Work failures count against a seller; declines do not.

Implementation note: closing this requires one pass that enumerates EVERY reject, decline, and error emission point in the seller daemon. A vocabulary added at the sites that happened to prompt it, and not at the others, reproduces the original defect with a `reason_code` tag sitting on top of it.

## 11. Per-Kind Status Semantics

The current v1 shape is:

- `CLAIM`: `processing`
- `AWARD`: `accepted`
- `FEEDBACK`: seller-defined status classes such as `error` and `refusal`
- `ACCEPT`: the buyer's settlement bind, on kind `3406`

`AWARD` and `ACCEPT` MUST NOT share one kind with tag-level discrimination.

## 12. Accept Split

`AWARD` stays on `3405`. `ACCEPT` is kind `3406`. They are separate kinds, never one kind discriminated by a tag.

The safety consequence is also part of the spec: any duplicate-award detector or re-arm guard MUST key on true awards only. An implementation MUST NOT rely on `ACCEPT` accidentally satisfying an award-presence check.

## 13. `run` -> `exec`

v1 uses `exec` terminology for seller execution metadata and protocol prose. `run` is not a wire token in v1.

## 14. Richer Receipts

A receipt is the highest-value third-party artifact in v1. It lets a third party determine:

- that buyer and seller both signed the same settlement bind;
- which offer and result that bind references;
- which mint realized the payment;
- which `creq` was settled, if `creq-hash` is present;
- which delivered git object was paid for, if delivery binding tags are present.

A receipt does not, by itself, prove that a seller capability claim was true, that the advertised model was the real model, or that the named harness actually executed. Those remain testimony unless separately evidenced.

### Receipt field semantics

| Field | What it proves | What it only reports |
|---|---|---|
| `sig/seller`, `sig/buyer` | both parties signed the same preimage | nothing beyond successful signature verification |
| `amount` | signed realized amount | nothing about seller cost |
| `mint` | signed realized mint | nothing about other acceptable mints |
| `job-hash` | signed pay-bind component | nothing about delivery quality by itself |
| `creq-hash` | signed hash of the seller-authored request string | not the contents of a decoded or re-encoded request |
| `delivery_integrity_hash` + `delivery_kind` | signed bind to the paid git object, if present | not that the object contains good work |
| offer/result `e` tags | which public artifacts the receipt refers to | not proof unless the signatures and bind also verify |
| `harness` | seller-claimed harness echoed at settlement time | not proof that this harness actually ran |
| `usage_transport` | seller-claimed capture path | not proof |
| `model` | seller-claimed model | not proof |
| `wall_time`, `tokens`, `cost` | seller-claimed usage facts | not proof |
| `metadata_trust=seller-claimed` | explicit marker that these fields are testimony | not proof of truth |

A v1 buyer SHOULD echo seller exec metadata from `RESULT` into `RECEIPT` unchanged when present, and MUST preserve `metadata_trust=seller-claimed`.

## 15. Freshness Filter

A freshness filter answers exactly one question: has this seat published recently? It is a liveness predicate.

Freshness proves:

- the seat’s publisher ran inside the freshness window.

Freshness does not prove:

- that the seat can accept work;
- that the required harness is compiled in;
- that the seat is authorized;
- that the seat can deliver.

A freshness filter MAY remove seats from a listing. It MUST NOT be read as, labeled as, or composed into a capability signal. The independent artifact for successful work is a delivery receipt, not a timestamp.

## 16. Money Invariants

1. Work follows the award. A seller runs no compute on a claim until the buyer awards that claim. An award for another claim, or a deadline reached with no award, releases the claim unworked.
2. The buyer verifies, not the seller. The paid delivery hash comes from the buyer’s own verification of the advertised commit before spend.
3. No cross-bind. Accept and pay refuse a result whose author is not the claim’s seller, and pay verifies the seller’s pre-pay co-signature before spending.
4. Capped. Every pay passes budget gates for per-job and total spend.
5. Fee floor. `amount <= mint fee` is dust and is refused.
6. Key custody. Keys are file-protected, never passed on a command line, and never written into tokens or logs.

## 17. Reputation Substrate: Attested Versus Asserted

v1 distinguishes two epistemic classes of statement about a seat, because a score that mixes them is not measuring one thing.

**Attested by artifact.** The statement is true because something happened, and a third party can recheck it without the seller's cooperation: a delivered tree and its hash, a commit id, a settled amount, a co-signed receipt, the existence of an award, the existence of a result.

**Asserted by the seller.** The statement is true only because the seller said so: the advertised rate, the roster, `accepting`, `queue_depth`, the harness named on a result, token counts, wall-clock times. The result format already concedes this -- its harness metadata is explicitly marked seller-claimed.

> v1 rule: a reputation score MUST weight attested and asserted inputs separately, and MUST state which class each input belongs to. A single number computed over both classes is not defined by this specification.

Two consequences follow directly.

**A self-report cannot reveal that self-reports are unreliable.** For a seat outside your own fleet there is no process table, no live log, and no shell, so no *live* control exists. It does not follow that no control exists at all. **The delivered tree is an artifact the buyer independently fetches and hashes, so any execution record the seller ships inside it is evidence by the definition above, not testimony.**

This was measured the hard way, in the safe direction. A buyer concluded that no outside control existed for a foreign seat; the seat's own run record, carrying a runtime identifier, was already sitting in three delivery collects that had been downloaded and never opened. The search had stopped at the transport boundary instead of at the data already in hand.

> v1 rule: before concluding that a property is unverifiable for a foreign seat, a reader MUST enumerate what the delivered artifact already contains. "No live access" and "no evidence" are different findings, and only the second one licenses giving up.

Where no shipped artifact carries the property, the strongest remaining signal is differential: request a named harness, and compare that seat's self-report against the same seat with the harness unset. If the self-report changes, the request is honoured in the seat's own accounting. If it does not change, that is a finding too -- either the request is ignored or the label is wrong. Neither outcome establishes what actually ran, and v1 does not pretend otherwise.

## 18. Delivery Artifact: The Node Workdir Snapshot

The paid delivery artifact IS the node's workdir snapshot. The agent's own commit is not preserved and is an ancestor of the delivery in no mode.

This follows from the attested-versus-asserted rule (see 17). The node is the protocol participant that can be held to a specification; the harness is arbitrary third-party software. Defining delivery as *the agent's commit* would make the paid artifact depend on cooperation from a component the protocol cannot constrain, with no enforcement point. The node is the reliable committer, and delivery is defined against it.

### 18.1 Delivery parentage, per mode

Parentage is fixed per mode:

- **Contribution** (a buyer-pinned base exists): the delivery is exactly one commit parented on that base, which the node fetches to a base ref it controls. An implementation MUST assert a parent count of one, on the pinned base rather than on any scratch tip.
- **Greenfield** (no base): there is nothing to parent onto, so the delivery is a **root commit** whose tree is the whole workdir. An implementation MUST assert a parent count of zero.

The agent's own commit is an ancestor in neither mode; a discarded commit cannot be an ancestor of anything.

Two costs are part of the contract rather than left to be rediscovered:

- `.gitignore`d files are excluded from the snapshot, so a job whose output must be delivered MUST NOT rely on an ignored path.
- Agent authorship and per-step history are not preserved.

## 19. Mandatory Execution Sentinel

Every delivery MUST carry an execution sentinel inside the delivered tree.

The motivation is a measured failure mode, not a hypothetical: a quota-dead run can exit `0` with a `completed` status in about two seconds, having written nothing, so every status field reports success. The sentinel is the signal that catches it.

The sentinel rides IN THE DELIVERED TREE, never as a tag on the delivery event. This follows from the attested-versus-asserted rule (see 17): a tag is authored by the seller at publish time and can be emitted without the workdir ever being touched -- that is testimony. A file inside the delivered tree sits within the artifact the buyer independently fetches and hashes -- that is evidence.

> Normative limit: a sentinel proves EXECUTION IN THIS WORKDIR. It never proves work quality, and it can never stand in for acceptance.

A delivery produced without a sentinel MUST be a defined refusal carrying `no_sentinel` (see 10). Without that failure mode the requirement is decoration.

**A sentinel is not a transcript.** What belongs in the tree is a structured execution manifest -- the minimum that proves execution in this workdir. A verbatim agent conversation log is not that: it leaks prompt content into a public artifact, inflates every delivery, and still proves only what the harness chose to write. v1 requires the manifest and does not make the transcript part of the artifact.

**Lapse is a protocol question, not a component defect.** Buyer-side parked jobs and seller-side stuck claims are the same failed trades seen from two ends; a replication measured seven of seven cross-side correspondences. No component owns lapse, so it is specified here rather than tracked as a defect in either implementation.

Sections 9 and 10 are what make any of this computable. 9 makes awards joinable to their offers, so award-without-result becomes visible to an anonymous observer. 10 separates a seller's failure from a buyer's price. Without both, every reputation input available on the relay today is either unjoinable or class-ambiguous.
