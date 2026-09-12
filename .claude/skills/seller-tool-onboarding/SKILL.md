---
name: seller-tool-onboarding
description: Route a seller's third-party tool to the right sandboxing option and configure it. Use this when a seller wants to offer a vendor tool (a CLI, an authenticated HTTP API, or a vendor-hosted MCP server) to their jobs, and you must pick between the Public, Direct token, Proxy swap, Holder, and Dedicated machine routes, then configure the chosen one. Covers the decision tree, the per-route configuration, the safety rules, and how to test. The Holder route (maxplayer-tool-kit) is the one that ships today.
---

# Seller tool onboarding

Use this skill in two steps. First route the tool to one option. Then configure that option.

The routes have names, not numbers: Public, Direct token, Proxy swap, Holder, Dedicated machine.
The reference for them is
[`docs/specs/seller-tool-onboarding/10-routing-and-options.md`](../../../docs/specs/seller-tool-onboarding/10-routing-and-options.md).
Read it for the full model. This skill is the actionable guide.

## Where each route stands today

- **Holder** — handled and automated. Onboard by config.
- **Public** — handled, but manual. A human installs the tool in the image.
- **Direct token** — handled, but manual, and its safe delivery is the Proxy swap.
- **Proxy swap** — handled by configuration (`[[sandbox.mcp_tools]]`). The real-vendor acceptance run
  is pending; GitHub is first.
- **Dedicated machine** — not handled; deferred.
- **Browser login** — not supported for now.

## The model, in one paragraph

The seller is defined by the offering. There is no offering per job. A tool enrols one time and
stays available for the life of the seller daemon. A job gets an endpoint, never a grant. Keep the
credential out of the job container in every route. See
`docs/handoff/reference/03-scope-correction-GOVERNING.md`.

## Step 1 — route the tool

Answer these questions in order. Stop at the first match.

1. **Does the tool need auth at all?**
   - No → **Public**. Go to the Public section.
   - Yes → question 2.
2. **Can the vendor issue a job-scoped token that meets all four predicates?** No persistent refresh
   secret; lifetime fits the job; scoped to the job's resources; revocable or bound to job-close.
   - Yes, with evidence → **Direct token**. Go to the Direct token section.
   - No → question 3.
3. **Does auth travel in one replaceable header, can the client route through the proxy, and are the
   request semantics constrainable?** Body-dispatched APIs (GraphQL, MCP tool calls) are not
   constrainable by path or method alone.
   - Yes → **Proxy swap**. Go to the Proxy swap section.
   - No → question 4.
4. **Must a real CLI or browser hold the login state?**
   - Yes, a CLI in a container → **Holder**. Go to the Holder section. This route ships today.
   - Yes, but a browser holds the login → **Browser login** — not supported at this moment. Print
     the message in the Cross-cutting section and stop.
   - Yes, but machine- or hardware-bound → **Dedicated machine** — not supported at this moment. Go
     to the Dedicated machine section and print its message.

Never pick a weaker route because its template exists. If the only route left is one that is not
supported at this moment, say so plainly and stop; do not improvise a substitute.

## Holder (ships today)

Use this for a local CLI with a persistent login that acts on local files. The holder holds the
login; the job reaches it over a private socket; the holder validates each operation and confines
file access. The kit is `crates/maxplayer-tool-kit`.

Configure it.

1. Copy `crates/maxplayer-tool-kit/templates/seller-tool-config.template.json` to a new file.
2. Fill the fields. See `crates/maxplayer-tool-kit/templates/README.md` for each field.
3. Add one operation per vendor CLI subcommand you offer. Map each subcommand argument to one
   parameter, and choose the parameter `kind`:
   - `job_input_file` for an input path.
   - `job_output_file` for an output path.
   - `choice` for a closed option set.
   - `text` for bounded free text.
4. Set `max_output_bytes` for each operation.
5. Confirm the vendor CLI reads its credential from its own home. It must not take a credential on
   the command line.
6. Run the tests and the demo. See "How to test the Holder".
7. Ask a human with authority over the seller account to review the mapping against
   [03](../../../docs/specs/seller-tool-onboarding/03-command-policy-mapping.md).

Safety invariants the holder enforces. Keep them true in any change.

1. The holder builds argv in the spec order. It runs no shell.
2. The holder resolves a job file path with no-follow opens, in `src/safeio.rs`.
3. The holder copies an input into a private staging directory. The CLI reads the staged copy.
4. The holder publishes an output with a no-follow create. It refuses a symlink at the output name.
5. The credential never enters a job container.
6. The vendor's own counters are the oracle. The holder's self-report is not evidence.

### How to test the Holder

```bash
cargo test -p maxplayer-tool-kit
cd crates/maxplayer-tool-kit
docker build -f docker/Dockerfile -t maxplayer-tool-kit:demo .
IMAGE=maxplayer-tool-kit:demo ./docker/demo.sh
```

The demo writes evidence to `evidence/<UTC-timestamp>/`. The `manifest.json` carries a
source-to-build receipt and the built image id.

## Public (manual setup)

Use this for a tool that needs no credential. The seller bakes the tool into a custom sandbox
image, then points the seat at it. There is no automated adapter, so report the result as manual
setup, never as automated onboarding.

Configure it.

1. Write a Dockerfile that starts `FROM` the maxplayer sandbox base image, so the image keeps the
   agent runtime the job needs (node, the ACP adapter, git, CA certs). The base is
   `crate::seller_exec::DEFAULT_SANDBOX_IMAGE`; the reference Dockerfile is
   `docker/maxplayer-sandbox/Dockerfile`.
2. Add the tool in that Dockerfile — a package install, or a copied binary on the `PATH`.
3. Build and tag the image, for example `my-sandbox-with-tool`.
4. Point the seat at it: set `image` under the seat's `[seller.sandbox]` docker config
   (`SandboxConfig::image` in `home.rs`).
5. Confirm the tool needs no credential and no network the egress policy denies. If it needs auth,
   it is not Public; re-route it.

## Direct token (manual setup; its safe delivery is the Proxy swap)

Use this only when the vendor issues a job-scoped, revocable, close-bound token and all four
predicates hold with evidence.

Prefer to deliver the token through the proxy, not in the container. Reuse the Proxy swap mechanism:
a placeholder in the container, the real token swapped in at egress. The proxy makes the placeholder
worthless outside the life of the job, so job-close-binding is ensured for you. Inject a real token
into the container only when the proxy cannot mediate the traffic (non-header auth, a signing
protocol, or a client that will not route through the proxy), and record the residual leak risk.

What is available today. The proxy delivery above is the Proxy swap route. It is configured by
`[[sandbox.mcp_tools]]` for a vendor-hosted MCP server, and by `[[sandbox.file_credentials]]` for a
client that takes a base-URL flag and a token from the environment. Its real-vendor acceptance run
is still pending, so report the result as "configured, acceptance run pending". The weaker option,
a real token in the container, stays manual setup with the residual leak recorded.

## Proxy swap (handled by configuration; real-vendor acceptance pending)

Use this for a vendor-hosted MCP server, or an authenticated HTTP API, whose auth is one header
value. The job holds a per-job placeholder. The credential proxy (`#647`) swaps the real credential
in at egress, only for the vendor's host, only for the life of the job. The credential file stays on
the host. It is never mounted.

**Status (2026-09-11).** The wiring is built and green against synthetic fakes: the config surface,
the proxy registration, the MCP server entry on the job's session, the `mcp-http-bridge` shim in the
sandbox image, and the core-side tests against the real proxy. The real-vendor acceptance run is
still owed; GitHub is first (see
[10](../../../docs/specs/seller-tool-onboarding/10-routing-and-options.md)). Until that run passes,
report a Proxy swap onboarding as "configured, acceptance run pending". Never report it as
"accepted".

Two shapes, by what the job talks to:

- **A vendor-hosted MCP server** (GitHub's remote MCP, Figma's) → `[[sandbox.mcp_tools]]`. This is
  the new wiring. Follow the steps below.
- **A CLI or HTTP client in the job that takes a base-URL flag and a token from the environment**
  → `[[sandbox.file_credentials]]`, the existing mechanism (proven on cursor-agent). The client's
  redirect flag points it at the proxy; the placeholder rides in the named variable. See
  `FileCredential` in `crates/maxplayer-core/src/home.rs`.

Configure a vendor MCP server.

1. Scope the credential at the vendor first: read-only, one repository or one project. The proxy
   constrains the destination host, not the operations. A broad credential stays broad behind it.
   If the vendor cannot scope it, stop. That case needs a trusted operation filter (the Holder shape
   for a remote tool), which is not built.
2. Put the credential in a host JSON file, mode 0600, owned by the daemon's user:
   `{"token": "<the credential>"}`. Use an absolute path. Never put it in `config.toml`, never on
   an argv.
3. Add the table to the seat's `config.toml`, under a docker `[sandbox]`:

   ```toml
   [[sandbox.mcp_tools]]
   name = "github"                                # the MCP server name the agent sees
   url = "https://api.githubcopilot.com/mcp/"     # the vendor's MCP endpoint
   credential = { path = "/ABSOLUTE/path/github-mcp.json", field = "token" }
   # transport = "stdio"                          # default: the bridge. "http" only for claude
   ```

   With egress containment on (`network` set), `proxy_port_range` must be set too. The pinhole is
   how the job reaches the proxy.
4. Restart the seller daemon. Read the boot line:
   `seller node: [sandbox] mcp_tools: github -> https://api.githubcopilot.com/mcp/ through the
   credential proxy (stdio bridge); credential file /ABSOLUTE/path/github-mcp.json reads`. A line
   that says `UNREADABLE` means every job would fail to reach the tool. Fix the file first.
5. Run a job that uses the tool. Confirm with the vendor's own record (GitHub: the token's last-used
   time, the repository's access log), not with the job's word.

What the job sees: an MCP server with the configured name, whose command is
`/usr/local/bin/mcp-http-bridge` with the proxy address, the path and the placeholder as flags. No
environment variable and no file in the container carries the credential. The placeholder starts
with `mxp-mcp-` and is worthless outside this job: the vendor rejects it, and the proxy forgets it
at job end.

Invariants the wiring enforces. Keep them true in any change.

1. The host reads the credential per job and registers it on the proxy with one upstream: the
   scheme and host of `url`.
2. The proxy substitutes in header values only, never in the body or the path.
3. An unreadable credential file fails the launch before any proxy listens. No fallback puts the
   real value in the container.
4. Job end drops the proxy, which revokes the placeholder, open connections included.

How to test, synthetically, both halves:

- `cargo test -p maxplayer-tool-kit` — the bridge against a fake proxy and a fake vendor
  (`tests/proxy_swap_suite.rs`), with SSE, a session id, chunked framing and notifications.
- `cargo test -p maxplayer-core --features wallet,acp mcp_tool` — the config, the session entry,
  and the real proxy against a stub vendor (`seller_exec::mcp_tool_tests`).

Neither is third-party acceptance. The GitHub run is.

## Dedicated machine (not supported at this moment)

This is for a login bound to a specific machine or hardware licence, which cannot run in a job
container. It is not supported at this moment. If routing lands here, it is the only route left and
no other route fits. Print this plainly and stop:

> This tool needs a dedicated machine to hold its login, which the platform does not support at this
> moment. It cannot be onboarded now.

Do not improvise a host executor on the seller's own machine.

## Cross-cutting

- **Browser login is not supported at this moment.** It fits the Holder only when the login persists
  and the holder can refresh it without a browser; a short-lived, non-refreshable login would force
  a per-job re-login the enroll-once model cannot hold. If a tool's only viable route is a browser
  login, print this plainly and stop:

  > This tool needs a browser login, which the platform does not support at this moment. It cannot be
  > onboarded now.

- **Refuse a credential store that cannot separate its auth writes from job state.** See
  [08](../../../docs/specs/seller-tool-onboarding/08-gaps-and-unsupported.md) C.1.

## Evidence rules

State the truth about a run.

1. A run against a fake CLI and a fake vendor proves the mechanism only. Both were written for this
   contract, so they cannot falsify it.
2. A run does not prove third-party acceptance or general onboarding acceptance.
3. Keep a negative control for each claim. A check that cannot fail is worse than no check.
