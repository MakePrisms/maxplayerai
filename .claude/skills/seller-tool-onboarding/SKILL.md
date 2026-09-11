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
- **Direct token** — handled, but manual, and its safe delivery is the deferred Proxy swap.
- **Proxy swap** — not handled; deferred. The swap mechanism exists; the template does not.
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

## Direct token (manual setup; its safe delivery is deferred)

Use this only when the vendor issues a job-scoped, revocable, close-bound token and all four
predicates hold with evidence.

Prefer to deliver the token through the proxy, not in the container. Reuse the Proxy swap mechanism:
a placeholder in the container, the real token swapped in at egress. The proxy makes the placeholder
worthless outside the life of the job, so job-close-binding is ensured for you. Inject a real token
into the container only when the proxy cannot mediate the traffic (non-header auth, a signing
protocol, or a client that will not route through the proxy), and record the residual leak risk.

Be honest about what is available today. The proxy delivery above is the Proxy swap route, and Proxy
swap is deferred. So the only self-serve option for a token tool today is the weaker one: the real
token in the container, with the eligibility gate above and the residual leak recorded. State that
plainly rather than implying the proxy delivery is ready.

## Proxy swap (deferred; not self-serve today)

Use this for an authenticated HTTP API or a vendor-hosted MCP server whose auth is a header value.
The job holds a placeholder; the credential proxy (`#647`) swaps the real credential in at egress.

**Status: mechanism demonstrated; production template owed.** The mechanism is proven synthetically
in the kit (`tests/proxy_swap_suite.rs`), and the transport shim `mcp-http-bridge` is built and
tested. What is not built is the production template: registering the vendor on the real `#647`,
the egress allow, the shim in the sandbox image, and a real-vendor acceptance run. So a seller still
cannot onboard this route by configuration today, and a run must never be reported as onboarded.
This is a platform build item, not a seller task. Do not improvise per-seller custody; that is the
unreviewed path the design refuses. If the tool also genuinely fits the Holder, offer that instead,
but never as a silent downgrade.

The credential swap extends the existing proxy (`#647`); it does not add a new one. What a Proxy
swap template must wire, for the platform to build (this is the design, not a seller recipe):

1. A credential entry (the `FileCredential` shape in `home.rs`): the file `path` and `field` for the
   real token, the `env` placeholder the container gets, the one `upstream` host, and the
   `endpoint_args` that point the client at the proxy.
2. The vendor host on the job's egress allowlist, or the request dies at name resolution.
3. Header-only auth: the proxy substitutes in header values only, never the body or the path.
4. The scope fork. Is the credential already scoped to the job's resources? If yes, the proxy swap
   alone is safe. If no, a trusted operation filter that is the job's only path to the vendor — never
   an in-container filter, which the job can skip because it holds the placeholder. That trusted
   filter is the Holder shape, for a remote tool.
5. The transport, already built: `mcp-http-bridge` is the stdio-to-HTTP shim that runs in the job
   container and forwards MCP to the proxy. It needs to be placed in the sandbox image. (`McpServer`
   is `{name, command}`, stdio only, which is why the shim is needed.)

Until that template exists, the honest outcome at this leaf is "recognized shape, template
deferred".

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
