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
| **Proxy swap** (rung 3) | The job holds a placeholder credential and the host-side credential proxy swaps the real one into the outgoing request header, so the secret never enters the container. | Handled by configuration (`[[sandbox.mcp_tools]]`); accepted against GitHub, 2026-09-14. |
| **Holder** (rung 4) | A persistent supervisor logs the real tool in one time and holds the session, exposing it to each job over a private socket while the credential and local files stay on the holder's side. | Handled and automated, and wired into the seller daemon (`[sandbox.held_tool]`); proved live through the daemon code on 2026-09-14. |
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

## Proxy swap — what exists now, and what is owed

The route is wired into the product as of 2026-09-11. The credential swap **extends** the existing
proxy (`#647`); it does not add a new one. The proxy's engine is generic over credentials — its
destination allowlist is the union of every registered credential's upstream — so a vendor MCP
server is one more per-job credential. The one new component is the transport shim in the job
container.

Built and green against synthetic fakes:

- **Config.** `[[sandbox.mcp_tools]]` in the seat's `config.toml`: `name`, `url`,
  `credential = { path, field }` (the same two fields as `FileCredential`, read by the same code),
  and an optional `transport` (`stdio`, the default, or `http`). `McpToolConfig` in
  `crates/maxplayer-core/src/home.rs`.
- **Wiring.** `seller_exec` reads the credential per job, mints a placeholder (`mxp-mcp-…`),
  registers `(placeholder → real, upstream)` on the real proxy, and puts one MCP server entry on the
  agent's session. Both launch paths carry it: the host agent launch, and the container-delivery
  launch through `Phase1Inputs.mcp_servers`. A seat without the table is unchanged. The seller boot
  line names each tool, its route, and whether its credential file reads.
- **Image.** `mcp-http-bridge` is built in the sandbox image's builder stage from the workspace
  lockfile and installed at `/usr/local/bin/mcp-http-bridge` (`docker/maxplayer-sandbox/Dockerfile`).
- **Tests.** Core: `seller_exec::mcp_tool_tests` — config refusals, the URL split, the session entry,
  the boot line, and the real proxy against a stub vendor: the job gets the tool through the swap;
  nothing the container receives carries the credential; a bypass placeholder gets `401` at the
  vendor; an unknown placeholder gets `502` at the proxy with no substitution; job end revokes.
  Kit: `tests/proxy_swap_suite.rs` — the bridge against fakes, with SSE, a session id, chunked
  framing, and notifications.

A correction to the earlier plan: there is no job-side egress allowlist to add the vendor to. The
job never reaches the vendor. It reaches the proxy through the firewall pinhole, and the proxy, a
host process, reaches the vendor. The vendor's host joins the PROXY's destination allowlist, which
`ProxyEngine::new` builds from the registered credentials.

Two transport shapes, because the ACP `mcpServers` entry has two wire forms (read off the baked
`claude-agent-acp` 0.67.0 adapter, not guessed):

- `stdio` (default): the agent spawns the bridge with `--proxy-url`, `--path` and `--placeholder`
  as flags. Flags, not env: a stdio server's `args` reach the child whenever a stdio server works at
  all, while its `env` reaches it only if the harness maps it. Every ACP harness supports a stdio
  MCP server.
- `http`: the agent's own Streamable-HTTP MCP client dials the proxy with the placeholder as a
  configured `Authorization` header. No bridge process. `claude-agent-acp` maps this shape; a
  harness that does not gets no tool. Use it as the fallback if the bridge misbehaves against a real
  vendor, so a transport fault and a proxy fault can be told apart.

The real-vendor acceptance run passed on 2026-09-14; see the next section. The scope fork still
holds: a broad credential needs a trusted operation filter, which is the Holder shape for a remote
tool, and that is not built.

### First acceptance vendor: GitHub — passed 2026-09-14

GitHub was chosen on 2026-09-11 because a fine-grained Personal Access Token is header-borne
(`Authorization: Bearer`), static, and scopable to read-only, so no operation filter is needed, and
GitHub hosts a remote MCP server. The run passed on 2026-09-14 on Petar's machine. The bundle is
[`evidence/20260914T085619Z-github-proxy-swap/`](../../../evidence/20260914T085619Z-github-proxy-swap/README.md).

What ran, three times, all against GitHub's own MCP server and all through the real components (the
real proxy, the real launch argv, the real preparation and cleanup, the sandbox image built from this
branch):

| Run | In the container | Egress | Result |
| --- | --- | --- | --- |
| A, uncontained | `mcp-http-bridge` as the container command, driven with an agent's MCP dialogue | default bridge network, the proxy through the docker alias | `initialize`, `tools/list` (27 read-only tools), `get_me` → the token owner's login, a file read |
| A, contained | the same | the seat's egress containment: a namespace holder and the firewall pinhole | the same |
| B, contained | a REAL `claude-agent-acp` turn, driven by `run_agent_job` as an awarded job is | containment, as above | the agent called `mcp__github__get_me` through the bridge and replied `login=<owner>` |

The three facts the plan asked for, and how each was confirmed:

1. **The call arrived with the PAT.** GitHub's MCP server answers `401` without a token (measured),
   so its `200` and its `get_me` result naming the token owner are GitHub's word that the real
   token arrived, swapped in at egress.
2. **The PAT is absent from the container; the placeholder is present.** `docker inspect` of the
   job container shows the placeholder in the command and no credential in the environment; the
   real cleanup path's diagnostics capture, the MCP transcript and the agent's reply are scanned by
   the tests for the token and it is in none of them.
3. **The bypass fails.** The placeholder sent straight to `api.githubcopilot.com` gets `400`
   (GitHub answers `400` to a bearer that is not shaped like one of its tokens, `401` to a missing
   or GitHub-shaped bad one). An unknown placeholder at the proxy gets `502` with no substitution.
   After job end the proxy port refuses the connection.

Two facts learned at the vendor and folded back into the wiring:

- GitHub's MCP server answers over SSE (`text/event-stream`) under chunked framing and issues an
  `Mcp-Session-Id` on `initialize`. The bridge handles all three; a client that reads only one JSON
  document would not work here.
- GitHub offers a read-only endpoint, `https://api.githubcopilot.com/mcp/readonly`, which lists only
  read tools. Use it with a read-only token: the endpoint bounds the operations the way the token
  bounds the permissions, and the proxy bounds the destination.

The tests are `seller_exec::mcp_tool_tests::live_a_…` and `live_b_…`, `#[ignore]`d because they
need docker, the image, egress and a real credential. The bundle's README says how to rerun them.

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
- **Deferred (Dedicated machine, Browser login).** The tree recognizes the route,
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
