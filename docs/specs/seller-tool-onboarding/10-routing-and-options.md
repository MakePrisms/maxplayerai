# 10 — Routing and options: the decision tree

This document is the reference for how a seller tool is routed to one sandboxing option, and how the
options relate. It builds on the plan v3 §2 routing ladder and the discoveries in this repository's
credential proxy (`crates/maxplayer-core/src/credential_proxy.rs`, `#647`) and tool holder
(`crates/maxplayer-tool-kit`). The onboarding skill walks a seller through this tree.

## The five routes

| Rung | How it works, in one sentence | Ship status |
| --- | --- | --- |
| **1 · Public** | The tool needs no credential, so it is installed in the job's container image and the job calls it directly. | Guidance only; manual setup. |
| **2 · Direct vendor token** | The job is handed a short-lived, job-scoped token the vendor can revoke or bind to job-close, so a leak is bounded and the job calls the vendor itself. | Guidance only; manual setup. |
| **3 · Key-swap proxy** | The job holds a placeholder credential and the host-side credential proxy swaps the real one into the outgoing request header, so the secret never enters the container. | Recognized; template deferred. Mechanism exists (`#647`). |
| **4 · Holder** | A persistent supervisor logs the real tool in one time and holds the session, exposing it to each job over a private socket while the credential and local files stay on the holder's side. | Ships. This is `maxplayer-tool-kit`. |
| **5 · Host executor** | For a login bound to a specific machine or hardware licence, the tool runs on a dedicated isolated machine rather than in the job container. | Recognized; template deferred. |

Browser-based authentication is an enrolment method, not a rung. It rides on rung 4 when the login
persists and refreshes without a browser. Otherwise it is not supported for now (see
[08](08-gaps-and-unsupported.md) C.2).

## One idea at three strengths

Rungs 2, 3, and 4 are the same idea — keep the credential out of the job and mediate access — at
rising strength. Read this before the tree; it explains why the tree flows the way it does.

1. **Bare token in the container (rung 2).** The job holds the real token. This is the weakest form.
   The job can read its own environment and exfiltrate a reusable secret. A short lifetime bounds
   the damage; it does not remove it. An expiring stolen token is not harmless.
2. **Proxy swap (rung 3).** The job holds a per-job placeholder. The proxy swaps the real credential
   in at egress, only for an allowlisted host, and only for the life of the job. The job never holds
   the real credential, and job-close-binding is enforced by the proxy, not by the vendor.
3. **Proxy swap plus a trusted operation filter (rung 4 shape).** The job holds nothing that
   authenticates the vendor, its direct egress to the vendor is blocked, and a trusted mediator holds
   the credential and exposes only the allowed operations. This is the holder shape, for a remote
   tool instead of a local CLI.

Rung 4 is form 3 for a local CLI: the holder holds the login, the job reaches it over a socket, and
the holder validates each operation and confines file access.

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

Route to the rung the tool's constraints select. Never silently pick a weaker rung because its
template exists. Each rung has eligibility predicates that need evidence.

1. **Does the tool need auth at all?**
   - No → **Rung 1 (Public)**. Install it in the job image.
   - Yes → go to 2.
2. **Can the vendor issue a job-scoped token that meets all four predicates?** (a) it exposes no
   persistent refresh secret; (b) its lifetime fits the job budget; (c) it is scoped to the job's
   resources; (d) it is revocable or bound to job-close.
   - Yes, with evidence for all four → **Rung 2 (Direct token)**, and prefer to deliver it through
     the proxy (see the note below), which supplies (d) for you.
   - No → go to 3.
3. **Does auth travel in one replaceable header field, can the client be routed through the proxy,
   and are the request semantics constrainable?** Body-dispatched APIs (GraphQL, MCP tool calls) are
   not constrainable by path or method alone.
   - Yes → **Rung 3 (Key-swap proxy)**. Then answer the scope question in 3a.
   - No → go to 4.
   - **3a. Is the credential already scoped to the job's resources?**
     - Yes → the proxy swap alone is safe.
     - No → add a trusted operation filter that is the job's only path to the vendor. This is the
       rung 4 shape for a remote tool.
4. **Must a real CLI or browser hold the login state?** (a persistent session, local files, or an
   interactive enrolment)
   - Yes, and it runs in a container → **Rung 4 (Holder)**.
   - Yes, but it is machine- or hardware-bound → **Rung 5 (Host executor)**.

## Note — deliver a rung 2 token through the proxy

Do not inject a real token into the container. Reuse the proxy: put a placeholder in the container,
and let the proxy swap the real job-scoped token in at egress. The job never holds the real
credential, and the proxy makes the placeholder worthless outside the life of the job, so
job-close-binding is ensured out of band. This is why a rung 2 case, in practice, is delivered as
rung 3. Bare rung 2 (a real token in the container) survives only where the proxy cannot mediate the
traffic: non-header auth, a signing protocol, or a client that will not route through the proxy.

## Dead ends, named

- A credential store that cannot separate its auth writes from job state — unsupported
  ([08](08-gaps-and-unsupported.md) C.1).
- A body-dispatched HTTP API whose semantics cannot be constrained — deferred. This is where
  [walk B](06-walk-b-tenant-aware-http.md) stops.
- Browser-based authentication with short-lived, non-refreshable tokens — not supported for now.
