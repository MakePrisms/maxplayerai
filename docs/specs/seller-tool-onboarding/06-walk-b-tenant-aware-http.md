# 06 — Paper walk B: tenant-aware HTTP API (contrast)

Plan v3 §6 stage 0 requires a second walk through the same contracts, deliberately chosen to
contrast with the CLI. **HTTP is a contrast test, not shipped support.** The purpose of this
walk is to find where the contract breaks, and the walk succeeds by producing an honest
`deferred`, not by producing a profile.

## Status of the subject

Archetype: a multi-tenant SaaS HTTP API. Bearer-token auth, a tenant selector, a JSON body,
nested filter objects, batch endpoints, and server-side pagination. Not a named vendor.

## Step 1 — Routing (plan v3 §2)

| Rung | Verdict |
| --- | --- |
| 1 public | No — authenticated. |
| 2 direct vendor token | **Eligible only under all four predicates**, and each needs evidence: (a) a seller/vendor custodian issues it without exposing persistent refresh secrets; (b) its lifetime fits the job budget; (c) its resources/actions fit this job's approved service and resources; (d) enforceable vendor revocation **or** vendor-enforced job-close binding. Most SaaS APIs satisfy (a)–(c) with scoped keys and fail (d): they offer revocation by an API call that is not bound to our job close, and they will not enforce close for us. **If immediate close enforcement cannot be established, select mediation instead** — that is the rule, and "the token expires in an hour anyway" is not a substitute. An expiring stolen token is not harmless. |
| **3 key-swap proxy** | **The natural route** — auth travels in a replaceable `Authorization` header, and the client can be routed through a proxy. This is where the walk stops, below. |
| 4 MCP container | Possible but pointless: nothing needs a persistent login *inside* a container when the credential is a header value. |
| 5 host executor | No. |

So walk B routes to rung 3, and **rung 3's template is explicitly deferred in this release**
(plan v3 §2: rungs 3 and 5 return `recognized shape, template deferred`).

## Step 2 — Where the mapping contract breaks

This is the substantive finding. Plan v3 §3's per-field source-to-sink mapping was designed
against argv, where "one field → one complete argument" is a checkable statement. An HTTP body
breaks each half of that.

### B-1 · Nesting defeats "one complete field"

```json
{ "filter": { "owner": { "in": ["res:R"] } }, "limit": 50 }
```

`res:R` is grant-checkable. But the *path to it* — `filter.owner.in[0]` — is itself structure
the job supplied. A policy that validates leaf values while accepting arbitrary object shape
has validated nothing: swapping `owner` for `parent`, or `in` for `not`, changes the query's
meaning entirely while every leaf stays legal. The profile must therefore pin the **complete
body shape**, with every permitted key path enumerated, and reject unknown keys at any depth.
That is a schema language, not a mapping table, and it does not exist.

### B-2 · Batch endpoints defeat effect declaration

One request may carry N operations. The declared maximum effect is then a function of body
content rather than a profile constant, so the pre-execution reservation in
[04](04-token-grant-contract.md) cannot reserve the true maximum without parsing and bounding
the batch. **Batch counts must be constrained by the profile**, and an endpoint whose batch
size is unbounded is refused outright under "refuse unbounded operations".

### B-3 · Resource selectors are queries, not identifiers

A CLI takes `res:R`. This API takes a *filter that matches a set*. Grant membership is defined
over identifiers; a filter is a predicate whose extension the holder cannot evaluate without
asking the vendor. `{"owner": {"in": ["res:R"]}}` looks bounded; `{"owner": {"not": {"in":
["res:S"]}}}` selects everything except `S`, including resources never granted. **Selector
expressiveness must be reduced to enumerated identifiers**, or the grant check is decorative.

### B-4 · Redirects and destinations

A `302` to an attacker-influenced host turns a reviewed egress allowlist into a suggestion.
Webhook/callback/destination fields hand the vendor an outbound sink we do not control.
Redirect following must be off, and destination fields must be constants or absent.

### B-5 · Pagination is unbounded reads

Server-side pagination lets one authorized verb read the entire tenant across N calls. Call
budgets bound this only if the budget is set with pagination in mind; a `max_calls: 20` that
looked generous for a CLI is an exfiltration budget here.

## Step 3 — Custody

Rung 3 custody is genuinely simpler: no in-holder enrollment, no `HOME` binding, no credential
refresh race, no browser profile. The proxy holds the key and swaps it into the header; the job
never sees it. The `codex_subscription.rs` precedent (`:10` pinned upstream, `:12` lifetime
margin, `:14-19` no-`Debug` secret type) transfers cleanly.

This is exactly why rung 3 is tempting, and exactly why plan v3 warns against taking it for the
wrong reason: **never silently select a less secure route because its template exists** — and
here, the inverse temptation, never call a route supported because its *custody* is easy while
its *request semantics* are unconstrained.

## Step 4 — Grant and checker

> **SUPERSEDED, and doubly inert:** walk B was already deferred at rung 3, and the grant
> machinery it inherits from walk A is withdrawn. The sentence below that grant issuance
> "transfers unchanged" is now vacuous — there is nothing to transfer. The tenant-isolation
> question this walk exists to raise is untouched and still open. See [00](00-README.md).

Grant issuance, token verification, close and budgets all transfer unchanged; they are
transport-independent. The checker does not transfer:

| Check | Applies to walk B? |
| --- | --- |
| 5.1 schema/profile | Rewritten — the negative fixtures are nested-key and shape mutants, not argv mutants. |
| 5.2 discovery/result | Applies. |
| 5.3 operand grammar | **Does not apply as written.** There is no operand; the equivalent is body-shape conformance. |
| 5.4 artifact consumption | Applies only if the API takes uploads. |
| 5.5–5.6 auth / grant authority | Apply unchanged. |
| 5.7 leakage | Applies, and is *harder*: the permitted auth sink is a header on every request. |
| 5.8 isolation | Weaker meaning — no per-job filesystem. Becomes "no cross-job header or connection reuse". |
| 5.9 cleanup | Applies to in-flight requests and connection teardown, not descendants. |
| 5.10 budgets | Applies, with B-2 and B-5 above. |
| 5.11 lifecycle | Applies. |
| 5.12 registration | Applies. |

**Remote black-box checks cannot prove internal isolation** (plan v3 §5). For an HTTP route,
almost every check *is* remote and black-box, which sharply limits what a green run means.

## Verdict

**Deferred. Not supported, not unsupported.** The router returns `recognized shape, template
deferred`, and that result is distinct from both success and unsupported.

Requirements a future HTTP profile must satisfy before any support claim, restating plan v3 §3
in the specific terms this walk surfaced:

1. complete body-shape pinning with every key path enumerated and unknown keys rejected at any
   depth (B-1);
2. bounded batch counts, with unbounded endpoints refused (B-2);
3. resource selectors reduced to enumerated grant-bound identifiers, never predicates (B-3);
4. redirects disabled and destination/callback fields constant or absent (B-4);
5. call budgets set with pagination-based enumeration in mind (B-5);
6. its own acceptance run — **no claim of HTTP support until that profile and checker work
   passes it.**

> Fixed host plus fixed method plus arbitrary nested strings is not an acceptable policy, and
> must not be described as one.

## What the contrast proves about walk A

Walk A is not safe *because it is a CLI*. It is tractable because argv gives a checkable
"one field → one complete argument" boundary, its effects are declarable as constants, and its
resource reference is an identifier rather than a predicate. Where a CLI loses those three
properties — a config-file flag, an unbounded `--all`, a glob operand — it acquires walk B's
problems and is deferred on the same grounds.
