---
name: seller-tool-onboarding
description: Route a seller's third-party tool to the right sandboxing option and configure it. Use this when a seller wants to offer a vendor tool (a CLI, an authenticated HTTP API, or a vendor-hosted MCP server) to their jobs, and you must pick between the public, direct-token, key-swap-proxy, holder, and host-executor routes, then configure the chosen one. Covers the decision tree, the per-route configuration, the safety rules, and how to test. The holder route (maxplayer-tool-kit) is the one that ships today.
---

# Seller tool onboarding

Use this skill in two steps. First route the tool to one option. Then configure that option.

The reference for the options is
[`docs/specs/seller-tool-onboarding/10-routing-and-options.md`](../../../docs/specs/seller-tool-onboarding/10-routing-and-options.md).
Read it for the full model. This skill is the actionable guide.

## The model, in one paragraph

The seller is defined by the offering. There is no offering per job. A tool enrols one time and
stays available for the life of the seller daemon. A job gets an endpoint, never a grant. Keep the
credential out of the job container in every route. See
`docs/handoff/reference/03-scope-correction-GOVERNING.md`.

## Step 1 — route the tool

Answer these questions in order. Stop at the first match.

1. **Does the tool need auth at all?**
   - No → **Rung 1, Public**. Go to the rung 1 section.
   - Yes → question 2.
2. **Can the vendor issue a job-scoped token that meets all four predicates?** No persistent refresh
   secret; lifetime fits the job; scoped to the job's resources; revocable or bound to job-close.
   - Yes, with evidence → **Rung 2, Direct token**. Go to the rung 2 section.
   - No → question 3.
3. **Does auth travel in one replaceable header, can the client route through the proxy, and are the
   request semantics constrainable?** Body-dispatched APIs (GraphQL, MCP tool calls) are not
   constrainable by path or method alone.
   - Yes → **Rung 3, Key-swap proxy**. Go to the rung 3 section.
   - No → question 4.
4. **Must a real CLI or browser hold the login state?**
   - Yes, in a container → **Rung 4, Holder**. Go to the rung 4 section. This route ships today.
   - Yes, but machine- or hardware-bound → **Rung 5, Host executor**. Go to the rung 5 section.

Never pick a weaker rung because its template exists.

## Rung 4 — Holder (ships today)

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
6. Run the tests and the demo. See "How to test the holder".
7. Ask a human with authority over the seller account to review the mapping against
   [03](../../../docs/specs/seller-tool-onboarding/03-command-policy-mapping.md).

Safety invariants the holder enforces. Keep them true in any change.

1. The holder builds argv in the spec order. It runs no shell.
2. The holder resolves a job file path with no-follow opens, in `src/safeio.rs`.
3. The holder copies an input into a private staging directory. The CLI reads the staged copy.
4. The holder publishes an output with a no-follow create. It refuses a symlink at the output name.
5. The credential never enters a job container.
6. The vendor's own counters are the oracle. The holder's self-report is not evidence.

### How to test the holder

```bash
cargo test -p maxplayer-tool-kit
cd crates/maxplayer-tool-kit
docker build -f docker/Dockerfile -t maxplayer-tool-kit:demo .
IMAGE=maxplayer-tool-kit:demo ./docker/demo.sh
```

The demo writes evidence to `evidence/<UTC-timestamp>/`. The `manifest.json` carries a
source-to-build receipt and the built image id.

## Rung 3 — Key-swap proxy (template deferred; mechanism exists)

Use this for an authenticated HTTP API or a vendor-hosted MCP server whose auth is a header value.
The job holds a placeholder; the credential proxy (`#647`) swaps the real credential in at egress.
The mechanism ships for the model credential; the seller-tool template is deferred, so today this is
manual setup.

Configure it.

1. Add a credential entry (the `FileCredential` shape in `home.rs`): the file `path` and `field`
   for the real token, the `env` placeholder the container gets, the one `upstream` host, and the
   `endpoint_args` that point the client at the proxy.
2. Add the vendor host to the job's egress allowlist, or the request dies at name resolution.
3. Confirm the token is a header value. The proxy substitutes in header values only, never the body
   or the path.
4. **Answer the scope question.** Is the credential already scoped to the job's resources?
   - Yes → the proxy swap alone is safe.
   - No → the credential is broad, so add a trusted operation filter that is the job's only path to
     the vendor. Do not put the filter inside the job container; the job holds the placeholder and
     can skip it. A trusted filter that holds the credential and exposes only safe operations is the
     rung 4 holder shape, for a remote tool.
5. For a remote MCP, remember the transport gap: `McpServer` is `{name, command}`, stdio only. A
   remote HTTPS MCP needs a stdio-to-HTTP shim in the container pointed at the proxy, or driver-side
   HTTP MCP support.

## Rung 2 — Direct vendor token (guidance only; deliver through the proxy)

Use this only when the vendor issues a job-scoped, revocable, close-bound token and all four
predicates hold with evidence.

Prefer to deliver the token through the proxy, not in the container. Reuse the rung 3 mechanism: a
placeholder in the container, the real token swapped in at egress. The proxy makes the placeholder
worthless outside the life of the job, so job-close-binding is ensured for you. Inject a real token
into the container only when the proxy cannot mediate the traffic (non-header auth, a signing
protocol, or a client that will not route through the proxy), and record the residual leak risk.

## Rung 1 — Public (guidance only)

Use this for a tool that needs no credential. Install it in the job image. Report the result as
manual setup, never as automated onboarding.

## Rung 5 — Host executor (template deferred)

Use this only when a platform- or machine-bound login cannot run in a container. Run the tool on a
dedicated isolated machine or VM, never on the seller's everyday host. Hardware-bound licences may
make even this unsupported. The template is deferred, so this is not automated today.

## Cross-cutting

- **Browser-based authentication is not supported for now.** It fits rung 4 only when the login
  persists and the holder can refresh it without a browser. A short-lived, non-refreshable login
  would force a per-job re-login, which the enroll-once model cannot hold.
- **Refuse a credential store that cannot separate its auth writes from job state.** See
  [08](../../../docs/specs/seller-tool-onboarding/08-gaps-and-unsupported.md) C.1.

## Evidence rules

State the truth about a run.

1. A run against a fake CLI and a fake vendor proves the mechanism only. Both were written for this
   contract, so they cannot falsify it.
2. A run does not prove third-party acceptance or general onboarding acceptance.
3. Keep a negative control for each claim. A check that cannot fail is worse than no check.
