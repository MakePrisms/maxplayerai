# protocol-v1 rewrite map — REVIEW AID, NOT A SPECIFICATION

**This file is a temporary review aid for one pull request. It is not normative. Delete it after the
review.** The normative document is [`protocol-v1.md`](protocol-v1.md).

The rewrite reorganized `protocol-v1.md` and rewrote its prose in ASD-STE100 Simplified Technical
English. The rewrite claims to preserve normative meaning. This file is the evidence for that claim.
It lists every normative statement in the old document with its location in the new document. It is a
full denominator, not a changelog.

## How the denominator was derived

The statement list below was extracted mechanically, then mapped by hand. The extraction command
was:

```bash
grep -nE 'MUST|SHOULD|MAY |reject|refus|forbid|required|Silent drops' docs/protocol-v1.md
```

That command matched 151 lines in the old document. 72 of those lines are tag-table rows. Part A
covers the table rows with a mechanical proof. Part B covers the remaining prose statements one by
one.

## Part A — tables, proven byte-identical

Every markdown table row in the old document was compared against the new document as exact bytes.

| Measure | Count |
|---|---:|
| Table rows in the old document | 115 |
| Table rows in the new document | 126 |
| Old rows byte-identical in the new document | 113 |
| Old rows missing from the new document | 0 |
| Old rows changed | 2 |
| Rows new in this rewrite | 11 |

The two changed rows are both in the heartbeat table. Each changed only its cross-reference and its
dash style, never its normative cell:

- `["accepting","y" or "n"]` — `see 7.8.1` became `see 5.8.1`.
- `["queue_depth", n]` — `see 7.8.1` became `see 5.8.1`.

The 11 new rows are the two navigation tables added by this rewrite: the reserved-path registry in
Section 17 (1 header + 2 rows) and the code-citation index in Section 18 (1 header + 7 rows). Neither
table states a new requirement.

This covers, byte for byte, the `Card.` / `Req.` / `If absent` cells of every tag table, the event
kind table, the receipt field-semantics table, and the `reason_code` vocabulary table.

## Part B — prose normative statements, old section to new section

185 normative statements were mapped. None were dropped.

### Old §2 — Scope And Terms

| # | Statement | New location |
|---:|---|---|
| 1 | The trade sequence `offer -> claim -> award -> result -> verify -> accept -> pay -> receipt`. | 2 |
| 2 | The eight public v1 events and what each publishes. | 2 |

### Old §3 — Versioning And Upgrade

| # | Statement | New location |
|---:|---|---|
| 3 | Every maxplayer-owned event MUST carry exactly one `["v","1"]` tag. | 16.1 |
| 4 | `v` is a major version encoded as a decimal string. There is no minor version. | 16.1 |
| 5 | Additive changes MUST ship as new tags or new optional fields. | 16.1 |
| 6 | A change that cannot take that form is a new major. | 16.1 |
| 7 | Rule A: `v` absent, reject the event. | 16.1 |
| 8 | Rule A: `v == "1"`, accept and ignore unrecognized tags. | 16.1 |
| 9 | Rule A: `v != "1"`, reject the event. | 16.1 |
| 10 | Rule A rationale: an unknown major on a money path makes reject required. | 16.1 |
| 11 | Rule B: `protocol_versions` absent, reject the heartbeat. | 16.1 |
| 12 | Rule B: shared majors present, the seat is usable at the highest shared major. | 16.1 |
| 13 | Rule B: unknown majors, ignore those entries. | 16.1 |
| 14 | Rule B: no shared major, the seat is unusable, not faulty. | 16.1 |
| 15 | The Rule A / Rule B asymmetry is deliberate and MUST NOT be unified. | 16.1 |

### Old §4 — Event Kinds

| # | Statement | New location |
|---:|---|---|
| 16 | The kind table (13 rows). | 4 (Part A) |
| 17 | `3405` is `AWARD` only. `ACCEPT` is a separate event, never a second meaning of `3405`. | 4.1 |

### Old §5 — Namespace Tag

| # | Statement | New location |
|---:|---|---|
| 18 | Every maxplayer-owned event of kind `3400`–`3407` and `30340` MUST carry `["t","maxplayer"]`. | 16.2 |
| 19 | A reader of those kinds MUST reject an event lacking that exact tag. | 16.2 |
| 20 | Borrowed kinds MUST NOT be required to carry `["t","maxplayer"]`. | 16.2 |
| 21 | A reader MUST ignore `t` on borrowed kinds. | 16.2 |
| 22 | An observer subscribing by `#t` MUST request maxplayer-owned kinds separately. | 16.2 |
| 23 | A kind absent from the observer's allow-list is invisible to the site. | 16.2 |
| 24 | The `mobee_agent` tag name is a deliberate exception and is not `maxplayer_agent`. | 16.2 |

### Old §6 — Identity, Capability, And Delivery Discovery

| # | Statement | New location |
|---:|---|---|
| 25 | Kind `0` is display metadata only. Readers MAY resolve `name`, `display_name`, `picture`, `about`. | 3 |
| 26 | Readers MUST NOT use kind `0` for targeting, pay-bind, delivery verification, or budget decisions. | 3 |
| 27 | Readers MAY resolve seller capability facts from kind `31990`. | 3 |
| 28 | Readers MUST NOT treat kind `31990` capability facts as proof. | 3 |
| 29 | Readers MAY resolve freshness and spoken majors from kind `30340`. | 3 |
| 30 | Readers MUST resolve delivery remotes from kind `30617`, not `31990` or `0`. | 3 |
| 31 | A reader MUST resolve a heartbeat by `(author pubkey, kind, d)` with newest `created_at`. | 3 |
| 32 | A reader MUST NOT resolve a heartbeat by event id. | 3 |

### Old §6.1 — One value, one publisher

| # | Statement | New location |
|---:|---|---|
| 33 | v1 forbids publishing the same fact into two events. | 3.1 |
| 34 | Kind `0` is the sole authoritative source of a seat's display name. | 3.1 |
| 35 | Kind `31990` content MUST NOT carry `name`. | 3.1 |
| 36 | Readers MUST resolve names from kind `0` only. | 3.1 |
| 37 | `accepted_mints` is authoritative for mint membership. | 3.1 |
| 38 | `mint` is DEPRECATED in v1 content. | 3.1 |
| 39 | Where present, a reader MUST read `mint` as `accepted_mints[0]`. | 3.1 |
| 40 | A reader MUST accept either key, MUST take their union, MUST record which key answered. | 3.1 |
| 41 | The publisher rule and the reader rule are deliberately asymmetric. | 3.1 |

### Old §6.2 — Liveness and payability are separate events

| # | Statement | New location |
|---:|---|---|
| 42 | A buyer deciding payability and liveness MUST join kinds `30340` and `31990`. | 3.2 |
| 43 | That buyer MUST NOT infer either property from the other's presence. | 3.2 |

### Old §6.3 — Declared capability is not resolved capability

| # | Statement | New location |
|---:|---|---|
| 44 | Declared and resolved capability are different facts with different provenance. | 3.3 |
| 45 | A reader MUST NOT substitute one for the other. | 3.3 |
| 46 | A reader MUST NOT read a disagreement between them as a malformed seat. | 3.3 |
| 47 | A reader MUST read both. | 3.3 |

### Old §6.4 — Replaceable events are current state, never history

| # | Statement | New location |
|---:|---|---|
| 48 | A replaceable or addressable event carries CURRENT state only. | 3.4 |
| 49 | Such an event MUST NOT be cited as evidence of a past state. | 3.4 |
| 50 | An observation of a replaceable event is testimony, not the artifact. | 3.4 |

### Old §7.0 — Absence is never a negative

| # | Statement | New location |
|---:|---|---|
| 51 | *Treat as unstated* is a normative requirement, not a default. | 5.0 |
| 52 | A reader MUST NOT convert the absence of an optional field into a negative claim. | 5.0 |
| 53 | The set of possible absences is a property of the publishing implementation. | 5.0 |

### Old §7.1–§7.9 — Tag inventory

| # | Statement | New location |
|---:|---|---|
| 54 | All nine tag tables and their `If absent` cells. | 5.1–5.9 (Part A) |
| 55 | If any of `delivery` / `repo` / `branch` is used, all three MUST be present. | 5.1 |
| 56 | A reader attempting bound delivery verification MUST reject a partial group. | 5.1 |
| 57 | The claim is the invoice. A claim commits no compute. | 5.2 |
| 58 | `ACCEPT` is buyer-authored and MUST be separate from `AWARD`. | 5.5 |
| 59 | `ACCEPT` MUST carry the seven listed binding fields. | 5.5 |
| 60 | A reader MUST reject `ACCEPT` if any required binding field is absent. | 5.5 |
| 61 | `FEEDBACK` `content` carries the machine-readable reason form. | 5.6 |
| 62 | `queue_depth` MUST count jobs in a named non-terminal state, awarded or executing. | 5.8.1 |
| 63 | `queue_depth` MUST return to `0` when none remain; never a lifetime total, never a flag. | 5.8.1 |
| 64 | `accepting` MUST be the seller's assertion of intent: free of in-flight work AND ≥1 harness serving. | 5.8.1 |
| 65 | A seat MAY define either condition more conservatively, never more loosely. | 5.8.1 |
| 66 | Readers MUST NOT infer claim eligibility from `accepting` or `queue_depth`. | 5.8.1 |
| 67 | The authoritative signals are a claim, or a `FEEDBACK` refusal carrying `at_capacity`. | 5.8.1 |
| 68 | A seat MAY publish no `mobee_agent` while running an unadvertised harness. | 5.8.1 |
| 69 | Readers MUST treat an absent `mobee_agent` as *unstated*, never as *none*. | 5.8.1 |
| 70 | Kind `0`: readers MAY parse content; MUST treat malformed or absent fields as unset. | 5.9 |
| 71 | Kind `31990`: `d` and `k` tags SHOULD be present; malformed or absent content is an unset claim, never proof. | 5.9 |
| 72 | Kind `30617`: readers MUST treat malformed or missing locator data as unusable. | 5.9 |
| 73 | Kind `1059`: public observers SHOULD ignore it. | 5.9 |

### Old §8 — Event Flows

| # | Statement | New location |
|---:|---|---|
| 74 | An offer without a `p` tag is open-pool. | 6 (Offer) |
| 75 | The seller MUST NOT start compute on a claim before award. | 6 (Claim) |
| 76 | The buyer publishes exactly one `AWARD`; work starts only after it names the winner. | 6 (Award) |
| 77 | The buyer MUST verify delivery independently. | 6 (Verify) |
| 78 | The buyer's own verified object hash, not the seller's assertion, becomes the delivery bind. | 6 (Verify) |
| 79 | If `.maxplayer/checks.toml` exists at the pinned base, the buyer reads it, removes both reserved paths, and recomputes. | 6 (Verify) |
| 80 | Indeterminate outcomes retry and never terminalize. | 6 (Verify) |
| 81 | Exec metadata on the result is testimony, not proof. | 6 (Execute and deliver) |
| 82 | Publication is not validity; the proof is signature verification over the bound preimage. | 6 (Receipt) |
| 83 | A non-winning claimant MUST release its claim without executing. | 6 (Release) |
| 84 | A claim whose offer deadline passes with no award MUST release the same way. | 6 (Release) |

### Old §9 — Offer-Root Requirement

| # | Statement | New location |
|---:|---|---|
| 85 | Every lifecycle event after `OFFER` MUST carry one `e` tag marked `root` holding the offer id. | 16.3 |
| 86 | The rule covers `CLAIM`, `AWARD`, `RESULT`, `ACCEPT`, `FEEDBACK`, `RECEIPT`, `REJECT`. | 16.3 |
| 87 | Readers MUST reject a lifecycle event lacking that root marker. | 16.3 |
| 88 | Positional fallback is not part of v1. | 16.3 |
| 89 | The 992-event / 93-unjoinable-award measurement and its acceptance criterion. | 16.3 |

### Old §10 — Error And Reject Semantics

| # | Statement | New location |
|---:|---|---|
| 90 | All seller-side refusals, releases, progress notes, and failures publish `FEEDBACK`. | 8 |
| 91 | Silent drops are forbidden. | 8 |
| 92 | `status` names the coarse class of the feedback. | 8 |
| 93 | `FEEDBACK` MUST carry a `["reason_code", <code>]` tag from the v1 vocabulary. | 8 |
| 94 | A reader MUST treat `reason_code` as authoritative for the class. | 8 |
| 95 | A reader MUST NOT parse `content` to determine the class. | 8 |
| 96 | A reader meeting an unknown `reason_code` MUST fall back to the class named by `status`. | 8 |
| 97 | That reader MUST NOT treat the event as malformed; the vocabulary is extensible. | 8 |
| 98 | The seven-code vocabulary table with its scoring column. | 8 (Part A) |
| 99 | The four status categories and their terminality. | 8 |
| 100 | An unsupported protocol major MUST NOT be collapsed into "unparseable". | 8 |
| 101 | The scoring column is normative for scoring, not for transport. | 8 |
| 102 | One pass MUST enumerate every reject, decline, and error emission point in the seller daemon. | 8 |

### Old §11–§13 — Per-kind status, accept split, exec terminology

| # | Statement | New location |
|---:|---|---|
| 103 | The per-kind status shape for `CLAIM`, `AWARD`, `FEEDBACK`, `ACCEPT`. | 4.1 |
| 104 | `AWARD` and `ACCEPT` MUST NOT share one kind with tag-level discrimination. | 4.1 |
| 105 | `AWARD` stays `3405`; `ACCEPT` is `3406`; they are separate kinds. | 4.1 |
| 106 | Any duplicate-award detector or re-arm guard MUST key on true awards only. | 4.1 |
| 107 | An implementation MUST NOT rely on `ACCEPT` to satisfy an award-presence check. | 4.1 |
| 108 | v1 uses `exec`; `run` is not a wire token in v1. | 4.1 |

### Old §14 — Richer Receipts

| # | Statement | New location |
|---:|---|---|
| 109 | The five facts a receipt lets a third party determine. | 5.7 |
| 110 | A receipt does not prove a capability claim, the model, or the harness that ran. | 5.7 |
| 111 | The receipt field-semantics table (12 rows). | 5.7 (Part A) |
| 112 | A v1 buyer SHOULD echo seller exec metadata into `RECEIPT` unchanged when present. | 5.7 |
| 113 | That buyer MUST preserve `metadata_trust=seller-claimed`. | 5.7 |

### Old §15 — Freshness Filter

| # | Statement | New location |
|---:|---|---|
| 114 | Freshness proves only that the seat's publisher ran inside the window. | 15 |
| 115 | Freshness does not prove acceptance, harness presence, authorization, or delivery. | 15 |
| 116 | A freshness filter MAY remove seats from a listing. | 15 |
| 117 | It MUST NOT be read as, labeled as, or composed into a capability signal. | 15 |

### Old §16 — Money Invariants

| # | Statement | New location |
|---:|---|---|
| 118 | Work follows the award. | 7.1 |
| 119 | The buyer verifies, not the seller. | 7.2 |
| 120 | No cross-bind; pay verifies the seller's pre-pay co-signature before spending. | 7.3 |
| 121 | Capped: every pay passes per-job and total budget gates. | 7.4 |
| 122 | Fee floor: `amount <= mint fee` is dust and is refused. | 7.5 |
| 123 | Key custody: file-protected, never on a command line, never in tokens or logs. | 7.6 |

### Old §17 — Reputation Substrate

| # | Statement | New location |
|---:|---|---|
| 124 | The attested-by-artifact class and its examples. | 14 |
| 125 | The asserted-by-seller class and its examples. | 14 |
| 126 | A reputation score MUST weight attested and asserted inputs separately. | 14 |
| 127 | A score MUST state which class each input belongs to. | 14 |
| 128 | A single number over both classes is not defined by this specification. | 14 |
| 129 | An execution record inside the delivered tree is evidence, not testimony. | 14 |
| 130 | A reader MUST enumerate the delivered artifact's contents before concluding unverifiability. | 14 |
| 131 | "No live access" and "no evidence" are different findings. | 14 |
| 132 | The differential-request signal and its limits. | 14 |

### Old §18 — Delivery Artifact

| # | Statement | New location |
|---:|---|---|
| 133 | The paid delivery artifact IS the node's workdir snapshot. | 9 |
| 134 | The agent's own commit is not preserved and is an ancestor in no mode. | 9, 9.1 |
| 135 | Contribution mode: exactly one commit parented on the pinned base. | 9.1 |
| 136 | An implementation MUST assert a parent count of one, on the pinned base not a scratch tip. | 9.1 |
| 137 | Greenfield mode: a root commit whose tree is the whole workdir. | 9.1 |
| 138 | An implementation MUST assert a parent count of zero. | 9.1 |
| 139 | `.gitignore`d files are excluded; a delivered output MUST NOT rely on an ignored path. | 9.1 |
| 140 | Agent authorship and per-step history are not preserved. | 9.1 |

### Old §19 — Mandatory Execution Sentinel

| # | Statement | New location |
|---:|---|---|
| 141 | Every delivery MUST carry an execution sentinel inside the delivered tree. | 9.2 |
| 142 | The sentinel rides in the delivered tree, never as a tag on the delivery event. | 9.2 |
| 143 | Normative limit: a sentinel proves EXECUTION IN THIS WORKDIR only. | 9.2 |
| 144 | It never proves work quality and can never stand in for acceptance. | 9.2 |
| 145 | A delivery without a sentinel MUST be a defined refusal carrying `no_sentinel`. | 9.2 |
| 146 | A sentinel is a structured manifest, not a transcript. | 9.2 |
| 147 | Lapse is a protocol question, not a component defect. | 9.2 |
| 148 | Offer-root and the reason-code vocabulary together make reputation computable. | 14 (closing) |

### Old §20 — Checks declaration

| # | Statement | New location |
|---:|---|---|
| 149 | A target MAY declare verification in `.maxplayer/checks.toml`, read only from the pinned `base_oid`. | 11 |
| 150 | Absence means no checks. | 11 |
| 151 | Presence is fail-closed: malformed TOML, unknown field, unsupported schema, or unsafe value is an error. | 11 |
| 152 | The declaration is capped at 64 KiB. | 11 |
| 153 | `schema` MUST equal `1`. | 11 |
| 154 | `nix-flake`: `flake_path` defaults to `"."`, otherwise a clean relative path inside the repository. | 11 |
| 155 | `<flake_path>/flake.nix` and `<flake_path>/flake.lock` MUST both be blobs at `base_oid`. | 11 |
| 156 | An unpinned flake is refused. `devshell` is optional. | 11 |
| 157 | `container-image`: `image` MUST match `^[a-z0-9.\-_/]+@sha256:[0-9a-f]{64}$`. | 11 |
| 158 | Tags, including `latest`, are forbidden. | 11 |
| 159 | `prepare` and `commands` contain non-empty argv arrays, never shell strings. | 11 |
| 160 | `commands` itself is non-empty. | 11 |
| 161 | Prepare MAY use the network. Every declared command MUST run network-free. | 11 |
| 162 | `timeout_secs` is the overall bound. | 11 |
| 163 | The stable environment reference is the `flake.lock` digest or the digest-pinned image reference. | 11 |
| 164 | `MAXPLAYER_EXECUTION_SENTINEL` and `MAXPLAYER_CHECKS_ATTESTATION` are reserved protocol paths. | 11, 17 |
| 165 | A declaring target is refused with `verify_reserved_path` if either is already a blob at the base. | 11, 17 |

### Old §21 — Checks attestation

| # | Statement | New location |
|---:|---|---|
| 166 | The attestation file name and its deterministic line-oriented form. | 12 |
| 167 | `raw-tree` is the delivered tree with both reserved paths removed. | 12 |
| 168 | `declaration` is the SHA-256 of the exact declaration bytes at `base_oid`. | 12 |
| 169 | `net` is the posture actually applied, `denied` or `open`. | 12 |
| 170 | Declared commands require denied networking. | 12 |
| 171 | The form carries no timestamps, durations, host facts, or log bytes. | 12 |
| 172 | Absence when declared is `verify_attestation_missing`. | 12 |
| 173 | Malformed or mismatched content is `verify_attestation_mismatch`. | 12 |
| 174 | Classification uses child wait-status, never exit code alone. | 12 |
| 175 | A normal nonzero exit is `Fail`. | 12 |
| 176 | The eight indeterminate causes. | 12 |
| 177 | A wrapper fault never masquerades as a command failure. | 12 |

### Old §22 — REJECT kind 3407

| # | Statement | New location |
|---:|---|---|
| 178 | `REJECT` is buyer-authored with `status=rejected` and its seven tags. | 13 |
| 179 | Its content is capped and control-character-stripped. | 13 |
| 180 | The closed eight-code vocabulary. | 13 |
| 181 | Transport, timeout, kill/signal, resource, provisioning/control, posture, and I/O failures are excluded. | 13 |
| 182 | Those outcomes retry and MUST NOT terminalize or emit `REJECT`. | 13 |
| 183 | Reader author-gate: kind `3407` is void unless its author authored the job's `AWARD`. | 16.4 |
| 184 | Relays enforce only the namespace. | 16.4 |
| 185 | Every reader MUST join the root offer to its award and verify `reject.author == award.author`. | 16.4 |

**Total: 185 normative statements mapped. 0 dropped.**

## Part C — statements dropped, with reason

None. No normative statement from the old document was dropped.

Four passages lost their historical framing while keeping their normative content. Each is a wording
change, not a meaning change:

| Old text | Why it changed | Where the content now lives |
|---|---|---|
| "The flag-day flip (#355) has shipped: the live wire is `t=maxplayer`, `v=1`, `d=maxplayer-seller`" | The brief forbids history. The fact is kept, the shipping event is dropped. | 1 ("The live wire uses `t=maxplayer`, `v=1`, and `d=maxplayer-seller`.") |
| "Absence means no checks and preserves v0.2.0 behavior." | Same. The behavior is stated directly instead of by reference to a past release. | 11 ("Absence means the target declares no checks.") |
| Heading "13. `run` -> `exec`" | The heading framed a migration. The rule is unchanged. | 4.1 ("v1 uses `exec` … `run` is not a wire token in v1.") |
| Old §10 "Today every seller-to-buyer negative signal … collapses into coarse status buckets with free-text reasons." | The sentence describes a state the same section's rule forbids, and `crates/maxplayer-core/src/gateway.rs:703` now attaches a `reason_code` tag at its emission sites. | 8, restated as the principle ("A coarse status alone cannot separate a price decline from a work failure."). Both measured instances are kept. |

## Part D — divergences found between the document and the code

These were found while cross-checking. **This pull request changed neither side.** They are reported
here for the reviewer to route.

### D1 — `ACCEPT` carries neither `job-hash` nor a reply-marked result `e` tag

Old §7.5, preserved unchanged as new Section 5.5, requires `ACCEPT` to carry
`["e", result_id, "", "reply"]` and `["job-hash", hash]`.

The shipped publisher emits neither. `crates/maxplayer-core/src/gateway.rs:480` builds the event with
the offer `e` tag marked `root`, an **unmarked `e` tag holding the `claim_id`**, both `p` tags, and
the `status`, `t`, and `v` tags added by `status_draft` at
`crates/maxplayer-core/src/gateway.rs:824`. The only publishing call site,
`crates/maxplayer-core/src/job_lifecycle.rs:1288`, adds no further tags. The buyer keeps `result_id`
and `job_hash` in its local `AcceptedBind` instead, at
`crates/maxplayer-core/src/job_lifecycle.rs:1300`.

A reader that applies the Section 5.5 rule as written rejects every `ACCEPT` this implementation
publishes. The specification and the implementation must be reconciled, and that decision is a
money-path decision rather than a documentation one. **The document was left as the specification
says.** A docs-only pull request is the wrong place to weaken a money-path requirement.

`docs/protocol.md:20` describes the shipped behavior, not the §7.5 requirement, so the two documents
in this repository already disagree on this point.

### D2 — the `reason_code` status class is not the emitted `status`

Section 8 classes `below_rate` and `no_sentinel` as `refusal`. Every site that builds feedback
through `error_draft` emits `status=error`, at `crates/maxplayer-core/src/gateway.rs:703`. No code
path emits `status` values `refusal`, `claim_released`, or `progress`; a repository-wide grep for
those three literals in `crates/**/*.rs` returns nothing.

The code comment at `crates/maxplayer-core/src/gateway.rs:695` names this gap and calls the
re-classing "a deliberate view change left as a follow-up". The table was preserved unchanged,
because it is the specification and the code documents itself as lagging it.

## Part E — content added by this rewrite, with its evidence

Everything below is new text. Each item was verified against the code named beside it.

| New location | Added statement | Evidence |
|---|---|---|
| 2 | Definitions of *reader*, *seat*, *node*, *harness*. | The old document used all four words without defining them. The node/harness distinction is stated in old §18 and preserved. |
| 4.1 | `ACCEPT` status is `accepted`; `REJECT` status is `rejected`. | `gateway.rs:480` (`accept_draft`, `"accepted"`); `gateway.rs:725` (`reject_draft`, `"rejected"`). |
| 10 | The checks layer gates nothing today. | No production caller exists for `parse_declaration`, `render_attestation`, `parse_attestation`, `validate_against_base`, `env_lock_ref`, `content_carries_attestation`, `resolve_backend`, or `argv_prefix`. Every workspace member lives under `crates/`, so a grep over `crates/` is a total instrument. |
| 10 | No production code path publishes `REJECT`. | `reject_draft` at `gateway.rs:725` has one call site, its own unit test at `gateway.rs:986`. |
| 10 | Payment does not depend on the checks layer. | `authorize_pay.rs` references no checks type. `collect.rs:1` documents the buyer path as tip-match verification followed by auto-pay in the same call. |
| 10 | The sentinel gate is separate and does gate payment. | `authorize_pay.rs:424` is the pre-pay sentinel refusal, with zero spend on failure. |
| 11 | `devshell` defaults to `default`. | `env_provision.rs:110` — `devshell.clone().unwrap_or_else(\|\| "default".to_owned())`. |
| 11 | Command composition: environment prefix, then declared argv, with the launcher outermost. | `env_provision.rs:62` (`compose`) and its test at `env_provision.rs:205`. |
| 11 | The container checks posture adds `--network=none`; the provision posture does not. | `env_provision.rs:51` and the exact-argv test at `env_provision.rs:171`. |
| 17 | The reserved-path registry table. | `checks.rs:12` (`CHECKS_ATTESTATION_FILE`), `delivery_sentinel.rs:38` (`SENTINEL_FILE`), `checks.rs:277` (`validate_against_base`). |
| 18 | The code-citation index. | See Part F. |

## Part F — cross-references

### F1 — a corrected cross-reference

Old §5 cited the `mobee_agent` capability tag as living in "7.1, 7.8". Old §7.1 is the **offer**
table, which carries `["param","agent",agent_id]` and no `mobee_agent` tag. The tag is in old §7.2
(claim) and old §7.8 (heartbeat). The new document cites **Sections 5.2 and 5.8**, which are those
two tables. This corrects the reference rather than relocating it.

### F2 — section numbers cited from Rust sources

Renumbering breaks citations that live outside this document. A repository-wide grep found 61
section-symbol citations in `crates/**/*.rs`. 55 of them cite protocol-v1, across seven distinct
sections:

| Cited as | Old section | New section | Example citation site |
|---|---|---|---|
| §6.1 | 6.1 | 3.1 | `profile.rs:364` |
| §7.0 | 7.0 | 5.0 | `authorize_pay.rs:429` |
| §7.5 | 7.5 | 5.5 | `delivery_sentinel.rs:20` |
| §10 | 10 | 8 | `gateway.rs:649` |
| §17 | 17 | 14 | `authorize_pay.rs:178` |
| §18.1 | 18.1 | 9.1 | `delivery_sentinel.rs:46` |
| §19 | 19 | 9.2 | `delivery_sentinel.rs:1` |

The remaining six citations point at other numbering and are unaffected: four cite an invariant
numbered §1, such as `buyer/lifecycle.rs:1470`, and two cite "design §4", such as
`payment_wallet.rs:442`.

Section 18 of the rewritten document carries this mapping, so an engineer following `§19` from a
source comment still lands on the right section. Updating the comments themselves touches `.rs`
files, which this pull request deliberately does not do. That is a follow-up.

## Part G — how to re-run the checks

Both scripts are throwaway review tooling and are not committed.

- Sentence length: every prose sentence in the new document is 20 words or fewer. Measured 0 of 669
  over the limit. The old document measured 53 of 426 over the limit.
- Table preservation: the Part A counts above.
