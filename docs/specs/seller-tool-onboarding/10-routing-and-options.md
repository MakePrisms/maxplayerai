# 10 — Routing and options: the decision tree

This document is the reference for how a seller tool is routed to one sandboxing option, and how the
options relate. It builds on the plan v3 §2 routing ladder and the discoveries in this repository's
credential proxy (`crates/maxplayer-core/src/credential_proxy.rs`, `#647`) and tool holder
(`crates/maxplayer-tool-kit`). The onboarding skill walks a seller through this tree.

The routes have names, not numbers. Plan v3 numbers them rungs 1 to 5; this document uses the names
and gives the rung in parentheses, so you never have to memorize a number.

## The five routes

| Route | How it works, in one sentence | Where it stands today |
| --- | --- | --- |
| **Public** (rung 1) | The tool needs no credential, so it is installed in the job's container image and the job calls it directly. | Handled, but manual. |
| **Direct token** (rung 2) | The job is handed a short-lived, job-scoped token the vendor can revoke or bind to job-close, so a leak is bounded and the job calls the vendor itself. | Handled, but manual, and its safe delivery is the deferred Proxy swap. |
| **Proxy swap** (rung 3) | The job holds a placeholder credential and the host-side credential proxy swaps the real one into the outgoing request header, so the secret never enters the container. | Not handled; deferred. Mechanism exists (`#647`). |
| **Holder** (rung 4) | A persistent supervisor logs the real tool in one time and holds the session, exposing it to each job over a private socket while the credential and local files stay on the holder's side. | Handled and automated. This is `maxplayer-tool-kit`. |
| **Dedicated machine** (rung 5) | For a login bound to a specific machine or hardware licence, the tool runs on a dedicated isolated machine rather than in the job container. | Not handled; deferred. |

**Browser login** is an enrolment method, not a route. It rides on the Holder when the login
persists and refreshes without a browser. Otherwise it is not supported for now (see
[08](08-gaps-and-unsupported.md) C.2).

## One idea at three strengths

Direct token, Proxy swap, and Holder are the same idea — keep the credential out of the job and
mediate access — at rising strength. Read this before the tree; it explains why the tree flows the
way it does.

1. **Direct token (weakest).** The job holds the real token. It can read its own environment and
   exfiltrate a reusable secret. A short lifetime bounds the damage; it does not remove it. An
   expiring stolen token is not harmless.
2. **Proxy swap.** The job holds a per-job placeholder. The proxy swaps the real credential in at
   egress, only for an allowlisted host, and only for the life of the job. The job never holds the
   real credential, and job-close-binding is enforced by the proxy, not by the vendor.
3. **Proxy swap plus a trusted operation filter (the Holder shape).** The job holds nothing that
   authenticates the vendor, its direct egress to the vendor is blocked, and a trusted mediator holds
   the credential and exposes only the allowed operations.

The Holder is form 3 for a local CLI: the holder holds the login, the job reaches it over a socket,
and the holder validates each operation and confines file access.

## Two things the proxy does NOT do

State these plainly, because a reader assumes more than the proxy gives.

- **The proxy does not constrain the operations or resources inside the vendor.** It allowlists the
  destination host and swaps auth. A job whose request reaches the allowlisted host with the
  placeholder can invoke any operation the credential permits. To constrain the operations you need
  either a credential the vendor already scoped to the job's resources, or a trusted operation
  filter (form 3 above).
- **An in-container filter is not a boundary.** The job holds the placeholder, so it can skip an
  in-container filter and call the vendor directly, and the proxy still swaps auth. A filter is a
  boundary only when it runs on the trusted side and is the job's only path to the vendor.

## The decision tree

Route to the route the tool's constraints select. Never silently pick a weaker route because its
template exists. Each route has eligibility predicates that need evidence.

1. **Does the tool need auth at all?**
   - No → **Public**. Install it in the job image.
   - Yes → go to 2.
2. **Can the vendor issue a job-scoped token that meets all four predicates?** (a) it exposes no
   persistent refresh secret; (b) its lifetime fits the job budget; (c) it is scoped to the job's
   resources; (d) it is revocable or bound to job-close.
   - Yes, with evidence for all four → **Direct token**, and prefer to deliver it through the proxy
     (see the note below), which supplies (d) for you.
   - No → go to 3.
3. **Does auth travel in one replaceable header field, can the client be routed through the proxy,
   and are the request semantics constrainable?** Body-dispatched APIs (GraphQL, MCP tool calls) are
   not constrainable by path or method alone.
   - Yes → **Proxy swap**. Then answer the scope question in 3a.
   - No → go to 4.
   - **3a. Is the credential already scoped to the job's resources?**
     - Yes → the proxy swap alone is safe.
     - No → add a trusted operation filter that is the job's only path to the vendor. This is the
       Holder shape for a remote tool.
4. **Must a real CLI or browser hold the login state?** (a persistent session, local files, or an
   interactive enrolment)
   - Yes, and it runs in a container → **Holder**.
   - Yes, but it is machine- or hardware-bound → **Dedicated machine**.

## Note — deliver a Direct token through the proxy

Do not inject a real token into the container. Reuse the proxy: put a placeholder in the container,
and let the proxy swap the real job-scoped token in at egress. The job never holds the real
credential, and the proxy makes the placeholder worthless outside the life of the job, so
job-close-binding is ensured out of band. This is why a Direct token case, in practice, is delivered
as Proxy swap. A bare Direct token (a real token in the container) survives only where the proxy
cannot mediate the traffic: non-header auth, a signing protocol, or a client that will not route
through the proxy.

## What a route does when it has no template

A route without a shipped template is not one behavior. It is two, and they must not be confused.

- **Manual setup (Public, Direct token).** The route works; only the automation is missing. So the
  tree does real work: it confirms the route, runs the eligibility gate (Direct token's four
  predicates, each with evidence), gives the concrete known-safe steps, and reports the outcome as
  "manual setup", never as "onboarded". A human operator does a bounded, known-safe wiring.
- **Deferred (Proxy swap, Dedicated machine, Browser login).** The tree recognizes the route,
  returns "recognized shape, template deferred", and stops. It does not hand the route to the seller
  to improvise, and it does not silently drop to a weaker route that has a template. The missing
  template is reviewed platform machinery — the profile, the custody handling, the checker, the
  acceptance tests — and that review is meant to happen one time, on the template, so every seller
  then fills only a manifest. A seller hand-rolling their own custody is the unreviewed, per-seller
  path the design refuses. So a deferred route is a platform build item, not a seller task.

At a deferred route the useful outputs are: name what the template must build; offer a shipping route
only if the tool genuinely fits one; or escalate to the platform to build the template.

## Dead ends, named

- A credential store that cannot separate its auth writes from job state — unsupported
  ([08](08-gaps-and-unsupported.md) C.1).
- A body-dispatched HTTP API whose semantics cannot be constrained — deferred. This is where
  [walk B](06-walk-b-tenant-aware-http.md) stops.
- Browser login with short-lived, non-refreshable tokens — not supported for now.
