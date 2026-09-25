---
name: maxplayer-seller-tool-onboarding
description: Offer a third-party tool to a Maxplayer seller's jobs without giving the job the credential. Use this after maxplayer-seller-operate, when the seat runs under docker and you want jobs to use a vendor tool: a CLI with a login, an authenticated HTTP API, or a vendor-hosted MCP server. Routes the tool to one of Public, Direct token, Proxy swap, Holder, or Dedicated machine, then configures the chosen route in the seat's config.toml ([[sandbox.mcp_tools]] for the Proxy swap, [[sandbox.held_tools]] for the Holder). Covers the decision tree, the per-route steps, the safety invariants, and how to test. Needs maxplayer 0.5.9 or newer.
---

# Seller tool onboarding

You run a Maxplayer seller, and you want your jobs to use a third-party tool. Use this skill in two
steps. First route the tool to one option. Then configure that option.

**Version.** This page matches maxplayer 0.6.0-rc5, the version of the source tree that publishes it.
Run `maxplayer --version` first. A daemon older than 0.5.9 knows neither `[[sandbox.held_tools]]`
nor `[[sandbox.mcp_tools]]`. Upgrade it before you continue.

**What you need.** A seat under `[sandbox] mode = "docker"`; step 3 of **maxplayer-seller-operate**
sets that up. For the Holder route, and for every test on this page, you also need a clone of the
source repository:

```bash
git clone https://github.com/MakePrisms/maxplayerai.git
cd maxplayerai
git checkout v0.6.0-rc5   # the tag that matches `maxplayer --version`
```

If that tag does not exist yet, stay on `main`. Every link on this page goes to that repository on
GitHub. The same files are in the clone.

**Keep it as a skill (optional).** This page works as one file read in context. To keep it as a
Claude Code skill, save it as:

```text
skill.md -> ~/.claude/skills/maxplayer-seller-tool-onboarding/SKILL.md
```

The routes have names, not numbers: Public, Direct token, Proxy swap, Holder, Dedicated machine.
The reference for them is
[`10-routing-and-options.md`](https://github.com/MakePrisms/maxplayerai/blob/main/docs/specs/seller-tool-onboarding/10-routing-and-options.md).
Read it for the full model. This skill is the actionable guide.

## Where each route stands today

- **Holder** — handled and automated, and wired into the seller daemon. Onboard by config
  (`[[sandbox.held_tools]]`, one entry per tool); proved live through the daemon code on 2026-09-14,
  with two tools.
- **Public** — handled, but manual. A human installs the tool in the image.
- **Direct token** — handled, but manual, and its safe delivery is the Proxy swap.
- **Proxy swap** — handled by configuration (`[[sandbox.mcp_tools]]`). Accepted against a real
  vendor (GitHub) on 2026-09-14.
- **Dedicated machine** — not handled; deferred.
- **Browser login** — not supported for now.

## The model, in one paragraph

The seller is defined by the offering. There is no offering per job. A tool enrols one time and
stays available for the life of the seller daemon. A job gets an endpoint, never a grant. Keep the
credential out of the job container in every route. See
[`03-scope-correction-GOVERNING.md`](https://github.com/MakePrisms/maxplayerai/blob/main/docs/handoff/reference/03-scope-correction-GOVERNING.md).

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
file access. The kit is
[`crates/maxplayer-tool-kit`](https://github.com/MakePrisms/maxplayerai/tree/main/crates/maxplayer-tool-kit);
the daemon side is
[`held_tool.rs`](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-core/src/held_tool.rs).
The seller daemon starts ONE holder container per held tool at boot, attaches each job's socket for
each tool around the job, and stops the holders at shutdown. Each login persists in its holder's
state volume across daemon restarts. A seat holds as many tools as it declares; each has its own
image, credential, holder and socket. Live proof through the daemon code, two tools:
[`evidence/20260914T103608Z-holder-route-two-tools/`](https://github.com/MakePrisms/maxplayerai/tree/main/evidence/20260914T103608Z-holder-route-two-tools).

Configure the offering (the kit config), then the seat.

Part 1 — the offering.

1. Copy
   [`seller-tool-config.template.json`](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-tool-kit/templates/seller-tool-config.template.json)
   to a new file.
2. Fill the fields. See the
   [templates README](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-tool-kit/templates/README.md)
   for each field.
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
   [`03-command-policy-mapping.md`](https://github.com/MakePrisms/maxplayerai/blob/main/docs/specs/seller-tool-onboarding/03-command-policy-mapping.md).

Part 2 — the holder image and the seat.

1. In the clone, build the kit image. It carries `tool-holderd`, `holderctl` and a fake
   `vendor-cli`:

   ```bash
   cd crates/maxplayer-tool-kit
   docker build -f docker/Dockerfile -t maxplayer-tool-kit:demo .
   ```

2. Build a holder image with `tool-holderd`, `holderctl` and the vendor CLI on `PATH`. Start `FROM`
   the kit image and add the vendor CLI, or copy the kit's binaries into an image that has the CLI.
   The kit image itself, with its fake `vendor-cli`, is the reference and the test double. Its
   Dockerfile is
   [`docker/Dockerfile`](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-tool-kit/docker/Dockerfile).
3. Put the vendor credential in a host file, mode 0600, owned by the daemon's user, in the shape
   the vendor CLI's `login` reads. Never in `config.toml`.
4. Add one table per tool to the seat's `config.toml`, under a docker `[sandbox]`. Every path is an
   absolute host path. `server_name` is the MCP server name the agent sees; it must be unique across
   `held_tools` and `mcp_tools`, and it names the holder container and the job's socket directory.

   ```toml
   [[sandbox.held_tools]]
   server_name = "figma"                                  # the agent sees mcp__figma__<operation>
   image = "my-figma-holder:latest"
   config = "/ABSOLUTE/path/figma-offering.json"          # the offering from part 1
   credential_file = "/ABSOLUTE/path/figma-cred.json"     # mounted read-only into this holder only
   # vendor_base_url = "https://vendor.example"           # overrides the config JSON's value
   # network = "my-tools-net"                             # the holder must reach the vendor
   # required = false                                     # true: refuse to boot or run without it

   [[sandbox.held_tools]]
   server_name = "jira"
   image = "my-jira-holder:latest"
   config = "/ABSOLUTE/path/jira-offering.json"
   credential_file = "/ABSOLUTE/path/jira-cred.json"
   ```

   Needs Docker Engine 26 or newer: the job's socket reaches its container through a volume
   subpath mount, and boot probes for it.
5. Restart the seller daemon. Read one boot line per tool. `HEALTHY, enrolled (1 login this start)`
   on the first boot; `HEALTHY, resumed the persisted login (no new enrolment)` after. `UNHEALTHY`
   names the vendor's answer; with `required = false` the seat still serves, without that tool.
6. Run a job that uses the tools. The agent sees one MCP server per entry, named by `server_name`,
   whose tools are the operations you declared for it. The job's outputs land in the job's own
   directory.

What the job container gets, and only that: its workdir at `/work`, and one socket directory per
tool at `/run/holder/<server_name>`, each a subpath of that holder's runtime volume. Not a
credential, not a holder's state, not another job's socket. Each holder runs as the job's uid, so
the outputs it publishes are the job's to read.

Safety invariants the holder enforces. Keep them true in any change.

1. The holder builds argv in the spec order. It runs no shell.
2. The holder resolves a job file path with no-follow opens, in
   [`safeio.rs`](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-tool-kit/src/safeio.rs).
3. The holder copies an input into a private staging directory. The CLI reads the staged copy.
4. The holder publishes an output with a no-follow create. It refuses a symlink at the output name.
5. The credential never enters a job container.
6. The vendor's own counters are the oracle. The holder's self-report is not evidence.

### How to test the Holder

In the clone, through the daemon code, against the kit's fake vendor (needs docker, the kit image
and the sandbox image with `tool-mcp-bridge`):

```sh
cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture held_tool::live_tests::live_two_tools
```

It starts two holders, runs two jobs against both on one enrolment each (one job under egress
containment when `MAXPLAYER_HELD_TOOL_LIVE_NETWORK` is set), refuses an escape, restarts the holders
and proves both logins resumed. `live_a_real_agent` adds a real agent turn that calls both tools.
The vendors' counters are the oracle.

The kit alone:

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
   agent runtime the job needs (node, the ACP adapter, git, CA certs). The base is the published
   image `ghcr.io/makeprisms/maxplayer-sandbox:v0.6.0-rc5`, the daemon's default when the seat names no
   image; its tag follows the daemon version. The reference Dockerfile is
   [`docker/maxplayer-sandbox/Dockerfile`](https://github.com/MakePrisms/maxplayerai/blob/main/docker/maxplayer-sandbox/Dockerfile).
2. Add the tool in that Dockerfile — a package install, or a copied binary on the `PATH`.
3. Build and tag the image, for example `my-sandbox-with-tool`.
4. Point the seat at it: set `image` in the seat's `[sandbox]` table.
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
client that takes a base-URL flag and a token from the environment. The weaker option, a real token
in the container, stays manual setup with the residual leak recorded.

## Proxy swap (handled by configuration; accepted against GitHub)

Use this for a vendor-hosted MCP server, or an authenticated HTTP API, whose auth is one header
value. The job holds a per-job placeholder. The credential proxy (`#647`) swaps the real credential
in at egress, only for the vendor's host, only for the life of the job. The credential file stays on
the host. It is never mounted.

**Status (2026-09-14).** The wiring is built, green against synthetic fakes, and accepted against
one real vendor: GitHub's remote MCP server, through the real proxy, the real launch, the sandbox
image, and a real agent turn. The bundle is
[`evidence/20260914T085619Z-github-proxy-swap/`](https://github.com/MakePrisms/maxplayerai/tree/main/evidence/20260914T085619Z-github-proxy-swap);
the account is in
[`10-routing-and-options.md`](https://github.com/MakePrisms/maxplayerai/blob/main/docs/specs/seller-tool-onboarding/10-routing-and-options.md).
For a NEW vendor, report the onboarding as "configured; accepted for GitHub, not yet for this
vendor" until a run against that vendor passes. A run against a fake proves the mechanism only.

Two shapes, by what the job talks to:

- **A vendor-hosted MCP server** (GitHub's remote MCP, Figma's) → `[[sandbox.mcp_tools]]`. This is
  the new wiring. Follow the steps below.
- **A CLI or HTTP client in the job that takes a base-URL flag and a token from the environment**
  → `[[sandbox.file_credentials]]`, the existing mechanism (proven on cursor-agent). The client's
  redirect flag points it at the proxy; the placeholder rides in the named variable. See
  `FileCredential` in
  [`home.rs`](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-core/src/home.rs).

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
   name = "github"                                      # the MCP server name the agent sees
   url = "https://api.githubcopilot.com/mcp/readonly"   # the vendor's MCP endpoint; GitHub's read-only one
   credential = { path = "/ABSOLUTE/path/github-mcp.json", field = "token" }
   # transport = "stdio"                                # default: the bridge. "http" only for claude
   ```

   Prefer a vendor endpoint that itself limits the operations (GitHub's `/mcp/readonly` lists only
   read tools) when the token is read-only. The endpoint bounds the operations, the token bounds
   the permissions, the proxy bounds the destination.

   With egress containment on (`network` set), `proxy_port_range` must be set too. The pinhole is
   how the job reaches the proxy.
4. Restart the seller daemon. Read the boot line:
   `seller node: [sandbox] mcp_tools: github -> https://api.githubcopilot.com/mcp/readonly through the
   credential proxy (stdio bridge); credential file /ABSOLUTE/path/github-mcp.json reads`. A line
   that says `UNREADABLE` means every job would fail to reach the tool. Fix the file first.
5. Run a job that uses the tool. Confirm with the vendor's own record (GitHub: the token's last-used
   time, the repository's access log), not with the job's word.

What the job sees: an MCP server with the configured name, whose command is
`/usr/local/bin/mcp-http-bridge` with the proxy address, the path and the placeholder as flags. No
environment variable and no file in the container carries the credential. The placeholder starts
with `mxp-mcp-` and is worthless outside this job: the vendor rejects it, and the proxy forgets it
at job end. The published sandbox image carries the bridge from 0.5.9. A custom job image must start
`FROM` it, or copy `/usr/local/bin/mcp-http-bridge` in.

Invariants the wiring enforces. Keep them true in any change.

1. The host reads the credential per job and registers it on the proxy with one upstream: the
   scheme and host of `url`.
2. The proxy substitutes in header values only, never in the body or the path. For an MCP tool the
   swap is scoped to the `Authorization` header. A placeholder in any other header goes to the
   vendor as the job wrote it, and a placeholder that appears only outside `Authorization` is
   refused at the proxy.
3. An unreadable credential file fails the launch before any proxy listens. No fallback puts the
   real value in the container.
4. Job end drops the proxy, which revokes the placeholder, open connections included.
5. A redirect is followed only to the same origin: scheme, host and port. A redirect from `https`
   to `http` on the same host is refused.
6. The proxy does not read the response body for an encoded credential. A vendor that reflects its
   `Authorization` header into a body can return the real value in an encoding the byte scrubber
   does not see. Do not onboard such a vendor on this route.

How to test, synthetically, both halves, in the clone:

- `cargo test -p maxplayer-tool-kit` — the bridge against a fake proxy and a fake vendor
  ([`tests/proxy_swap_suite.rs`](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-tool-kit/tests/proxy_swap_suite.rs)),
  with SSE, a session id, chunked framing, notifications, two requests in flight at once, and a
  server request sent mid-stream that the bridge answers while the stream is open.
- `cargo test -p maxplayer-core --features wallet,acp mcp_tool` — the config, the session entry,
  and the real proxy against a stub vendor (`seller_exec::mcp_tool_tests`).

Neither is third-party acceptance. The live tests are: `cargo test -p maxplayer-core --features
wallet,acp --lib -- --ignored mcp_tool_tests::live_a` and `live_b`, with the environment the bundle's
[README](https://github.com/MakePrisms/maxplayerai/blob/main/evidence/20260914T085619Z-github-proxy-swap/README.md)
names. They need docker, the image, egress and a real read-only credential.

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
  [`08-gaps-and-unsupported.md`](https://github.com/MakePrisms/maxplayerai/blob/main/docs/specs/seller-tool-onboarding/08-gaps-and-unsupported.md)
  C.1.

## Evidence rules

State the truth about a run.

1. A run against a fake CLI and a fake vendor proves the mechanism only. Both were written for this
   contract, so they cannot falsify it.
2. A run does not prove third-party acceptance or general onboarding acceptance.
3. Keep a negative control for each claim. A check that cannot fail is worse than no check.

## When it goes wrong

Report a dead end as an issue on https://github.com/MakePrisms/maxplayerai. Name the exact boot line
or command output you saw, and the route you chose.
