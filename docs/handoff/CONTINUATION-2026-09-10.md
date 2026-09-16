# Seller-tool onboarding — continuation, 2026-09-10

This document continues [`README.md`](README.md). Read the README first for the branch state, the
architecture, and the five review findings. This document records the work that cleared the
findings, and the plan for the production integration that remains.

Author: Petar's local agent, 2026-09-10.

**Current state:** sections 10 to 12 — the Proxy swap route is coded, tested, and accepted against
GitHub (2026-09-14); the Holder route is wired into the daemon and proved live through the daemon
code (2026-09-14). The branch is pushed to `MakePrisms/maxplayerai` and under review as pull request
#1004 (https://github.com/MakePrisms/maxplayerai/pull/1004), merged with `main` at `1569010`. The
first review round (Codex, 2026-09-15) returned eleven findings; section 12 records each one and its
fix. Read sections 10 to 12 first if you are resuming.

**2026-09-16:** pull request #1004 merged into `main` at `00c05bb`. The skill moved to the web skill
channel (`web/app/.well-known/skills/seller-tool-onboarding/skill.md`, live at maxplayer.ai), so a
seller outside this checkout can fetch it the way every other buyer or seller skill is fetched. See
section 6.

## 1. What changed since `a0cc31d`

| Area | Change |
| --- | --- |
| F1 | Defined `RUN_TMP` in `docker/demo.sh`. The demo no longer aborts on the first expansion. |
| F2 | Added `src/safeio.rs`. The holder resolves each job file with no-follow opens and stages it. |
| F3 | Rewrote the lifecycle proof and the tool-list check in `docker/demo.sh`. |
| F4 | Added a source-to-build receipt and the built image id to the demo manifest. |
| Kit | Added a skill and config templates. See section 6. |
| Docs | Updated the status note in `docs/specs/seller-tool-onboarding/04-token-grant-contract.md`. |

## 2. Finding status

| # | Finding | Status |
| --- | --- | --- |
| F1 | Credential-absence check could not fail | Fixed. See section 3. |
| F2 | Path re-opened between check and use | Fixed and tested. See section 3. |
| F3 | Lifecycle proof probes a detached endpoint | Fixed. See section 3. |
| F4 | Unpinned images, unlocked build, gitignored captures | Complete. Part 1 was already done. |
| F5 | Retention notes kept withdrawn grant gates | Was already done. Status note refreshed. |

## 3. How each finding was closed

### F2 — race-safe, no-follow consumption (the security defect)

The holder used to canonicalize a job path, check it, and return a string. The trusted CLI
re-opened that string later. A job could swap the name, or a parent directory, for a symlink
between the check and the open.

The fix removes the re-open. The steps are these.

1. `validate_call` in `src/validate.rs` checks grammar and confinement by name only. It does not
   touch the filesystem. It returns a job-relative path.
2. `src/safeio.rs` opens each file with `openat` and `O_NOFOLLOW` on every component. A symlink on
   any component is refused, not followed. This needs `libc`, because `std` has no `openat`.
3. The holder copies an input into a private staging directory. The CLI reads the staged copy.
4. The holder publishes an output with a no-follow create. A symlink at the output name is
   refused.

The vendor CLI keeps its `--in` and `--out` interface. It reads and writes the staged copy, which
a job cannot reach. This matches the staging contract in
[`../specs/seller-tool-onboarding/03-command-policy-mapping.md`](../specs/seller-tool-onboarding/03-command-policy-mapping.md).

Tests in `tests/negative_controls.rs` prove the boundary:
- `a_swap_between_validation_and_use_reads_and_writes_nothing_outside` plants a symlink after
  grammar validation, then shows the open and the create refuse it, and the outside file stays
  unread and unwritten.
- `a_planted_input_symlink_is_refused_at_the_live_endpoint` and
  `a_planted_output_symlink_does_not_write_outside_the_job` prove the same at the live endpoint.

### F1 — host-side credential scan with a working negative control

The demo exports the job container filesystem to the host and scans it there. The container
receives no secret. A capture failure is fatal, not a clean result. A negative control plants the
secret into a copy of the archive and asserts the same scanner reports one hit. The `RUN_TMP` fix
lets this section run for the first time.

### F3 — call, loss, restore on a live endpoint

The demo re-attaches the job after each restart. It proves a live call, then stops the daemon and
proves the loss, then starts the daemon, re-attaches, and proves the call works again with no new
login. The tool-list check parses the whole list with `jq` and compares it across both jobs and
the seller control view. It no longer compares a bracket-truncated prefix.

### F4 — source-to-build receipt

The manifest now carries a `build` object: the source commit, the crate git tree object, the
lockfile and Dockerfile hashes, the two base-image digests, the built image id, and the image
repo digests. It also carries a `raw_capture_sha256` map, which hashes every transcript.

## 4. Verification

| Gate | Result |
| --- | --- |
| `cargo test -p maxplayer-tool-kit` | 35 tests pass (6 fixture, 29 negative controls). |
| `cargo clippy -p maxplayer-tool-kit --all-targets` | No warnings. |
| `docker build` pinned and `--locked` | See section 8 for the re-run status. |
| `docker/demo.sh` | See section 8 for the re-run status. |

## 5. Evidence

Replace the two pre-repair bundles under `evidence/`. They carry the old, invalid F1 check and the
old F3 lifecycle probe. A fresh demo run writes a new bundle with the F1, F3, and F4 repairs.

## 6. Owed kit

| Item | Location |
| --- | --- |
| Skill | `web/app/.well-known/skills/seller-tool-onboarding/skill.md`, served at https://www.maxplayer.ai/.well-known/skills/seller-tool-onboarding/skill.md and listed in the skill index (2026-09-16). `.claude/skills/seller-tool-onboarding/SKILL.md` is a symlink to it, for agents in this checkout. |
| Config template | `crates/maxplayer-tool-kit/templates/seller-tool-config.template.json` |
| Field guide and examples | `crates/maxplayer-tool-kit/templates/README.md` |
| Worked example A (file CLI) | The demo and `fixtures/seller-tool-config.json` are the runnable Walk A. |
| Worked example B (HTTP) | Deferred. HTTP profiles are deferred in specs 03 and 06. |

## 7. Production integration plan (implemented 2026-09-14 — see section 11)

A detailed, code-grounded version of this plan is in
[`../specs/seller-tool-onboarding/09-production-integration.md`](../specs/seller-tool-onboarding/09-production-integration.md).
It carries the exact insertion points, code sketches, the feature gate, the test plan, and the open
decisions. The summary below stays here.

Nothing in this branch is wired into the product. The prototype runs beside it. Do not wire the
`mcp_servers` vector alone: a bridge with no mounted socket and no supervised holder breaks job
execution. Land the parts together.

The advisor scopes this as follow-on, not a prototype blocker. Land F2 first, which is done.

Insertion points, confirmed to exist:

| Step | Site | Action |
| --- | --- | --- |
| 1 | `seller_node/run.rs`, `seller_node/shutdown.rs` | Start `tool-holderd` at seller daemon boot. Stop it at daemon stop. Expose a failed start, a failed auth, and a health failure. |
| 2 | `home.rs:193` `SellerConfig` | Add a config surface for a held tool: the config path, the vendor URL, and the holder socket path. |
| 3 | `home.rs:550` `SandboxConfig` | Mount the job's own socket into the sandbox at `/run/holder/job.sock`. Mount nothing else from the holder. |
| 4 | `seller_exec.rs` before launch | Call `holderctl attach --job-id <id> --job-root <workdir>`. Call `detach` after the job. |
| 5 | `seller_exec.rs:2411` | Set `mcp_servers` to one `McpServer { name: "seller-tool", command: vec!["tool-mcp-bridge"] }`. |

Notes:
- `tool-mcp-bridge` reads `HOLDER_JOB_SOCKET`. It defaults to `/run/holder/job.sock`. `McpServer`
  carries no environment, so step 3 must mount the socket at that default path.
- The job container needs the `tool-mcp-bridge` binary. Add it to the sandbox image, or mount it.
- Keep the credential out of the job container. Mount only the job's own socket and work
  directory. Use `--network none` for the job, as the demo does.
- The vendor's own counters remain the oracle for any acceptance claim.

After step 5, demonstrate two real sequential jobs that share the seller session and list, the
custody and file controls, and the daemon lifecycle. Independent real-tool acceptance stays a
separate stage. A fake CLI and a fake vendor cannot satisfy it.

## 8. Demo re-run status

The demo re-run **passed: 39 checks, 0 failures**, against the digest-pinned image from
`docker/Dockerfile`. The bundle is `evidence/20260910T125544Z/`. Every F1, F2, F3, and F4 check
passed. The independent vendor counters confirm one enrolment plus one re-enrolment (two logins),
five transforms, and one auth failure from the revoke test. The build receipt shows a clean
committed source tree, the pinned base-image digests matching the Dockerfile, and the built image
id.

Running the demo also found and fixed two latent bugs in the F1 section of `docker/demo.sh`, both
from the earlier WIP commit that had never run:
- `capture_job_fs` let `tar` fail on the job's Unix socket. The fix excludes `*.sock`; a socket
  holds no file content.
- `scan_capture_for_secret` let `grep`'s no-match exit (status 1) abort the run under
  `set -o pipefail`. The fix separates a clean no-match from a real scanner error, so a scanner
  error still fails the check instead of reading as a false absence.

Note on the environment: the pinned build first hung on the Docker buildkit resolver, and a
graceful `docker desktop restart` cleared it. An earlier offline-built bundle stood in while the
resolver was wedged; it is retired now that the pinned build succeeds.

## 9. Browser-based authentication — not supported for now

Petar's decision, 2026-09-10: do not support browser-based authentication for now.

The reason, stated plainly:
- The holder model works when a login persists. You authenticate one time on the host, the holder
  holds the session for the daemon's life, and a refresh happens outside a job.
- A browser login fits that model only when the vendor issues a session the holder can renew
  without a browser. A refresh token is the usual form.
- Some vendors issue only short-lived tokens with no non-interactive refresh. That kind forces a
  person at a browser again and again, which the enroll-once model cannot hold.
- We cannot tell which kind a vendor is until we integrate one. So the safe choice is to not
  support a browser login now.

This becomes supportable when a specific vendor offers a browser login with a refreshable session.
The holder already accommodates that case; no redesign is needed.

## 10. Proxy swap production wiring (agreed 2026-09-11; coded 2026-09-11; accepted on GitHub 2026-09-14)

Petar's sequencing decision, 2026-09-11: finish all the coding first, then attempt the real GitHub
test. Do not attempt the real test with half-built code. The real run is the one thing that cannot
be tested synthetically, so everything else must be green before it.

Scope: the Proxy swap production wiring, enough to run the GitHub acceptance test. The Holder-route
production integration (section 7, doc 09) is separate and is NOT needed for the GitHub test. Do it
only if Petar asks for it in the same push.

### Definition of done, and what was built against it

"All the coding" is done when these four are built and green against synthetic fakes. As of
2026-09-11 all four are built; the status of the gates is in the next subsection.

1. **Config.** `[[sandbox.mcp_tools]]` in `config.toml`: `name`, `url`, `credential = { path,
   field }`, optional `transport = "stdio" | "http"`. `McpToolConfig` in `home.rs`, beside
   `FileCredential`, which it reuses in substance: the same `path` + `field` shape, read by one
   shared function. The `[sandbox]` template comment shows the block. Refusals at config
   resolution (`SandboxPolicy::from_config`): relative path, empty field, bad name, duplicate name,
   a URL the proxy cannot route.
2. **Wiring.** `seller_exec::start_credential_containment` reads the credential per job, mints a
   placeholder (`mxp-mcp-` + 48 random), registers it on the real `#647` engine with one upstream
   (the URL's scheme + host, which joins the proxy's destination allowlist), and returns one
   `McpServer` per tool in `Containment::mcp_servers` → `PreparedLaunch::mcp_servers`. The host
   agent launch puts them on `SessionConfig.mcp_servers`; the container-delivery launch carries them
   in `Phase1Inputs.mcp_servers` (serde default, back-compatible) and the orchestrator hands them to
   `run_agent_job_in_env`. A seat without the table is unchanged. A boot line per tool names the
   route and whether the credential file reads.

   Correction to the plan text: "adds the upstream to the job's egress allowlist" was wrong. The
   job never reaches the vendor; the proxy does, from the host. The vendor's host joins the PROXY's
   destination allowlist (`ProxyEngine::new`), which is what the wiring does.
3. **Image.** `docker/maxplayer-sandbox/Dockerfile` builds `mcp-http-bridge` in the builder stage
   (`cargo build --release -p maxplayer-tool-kit --bin mcp-http-bridge --locked`) and installs it at
   `/usr/local/bin/mcp-http-bridge` (`seller_exec::CONTAINER_MCP_BRIDGE_BIN`).
4. **Tests.** Core, `seller_exec::mcp_tool_tests`: config refusals, the URL split, the two session
   entry shapes, the boot line, and — against the REAL proxy with a stub vendor — the job gets the
   tool through the swap, nothing the container receives carries the credential (env, session
   entry, docker argv), a bypass placeholder gets `401` at the vendor, an unknown placeholder gets
   `502` at the proxy with no substitution, and job end revokes. Kit, `tests/proxy_swap_suite.rs`:
   the bridge against fakes with SSE, a session id, chunked framing, a notification, and the
   environment fallback. Plus unit tests for the bridge logic (`mcp_bridge.rs`) and the chunked
   decoder (`http.rs`).

No operation filter for this test. GitHub's fine-grained PAT is scoped, so the scope fork does not
apply.

### Decisions taken while building

- **The ACP `McpServer` type was wrong and is fixed.** `driver/acp.rs` had `{ name, command:
  Vec<String> }`, which no adapter reads. It is now the wire shape read off the baked
  `claude-agent-acp` 0.67.0 adapter: `McpServer::Stdio { name, command, args, env }` with NO `type`
  key (the adapter drops a stdio entry that carries one), or `McpServer::Http { type: "http",
  name, url, headers }`. Nothing constructed a non-empty list before, so nothing else changed.
- **Flags, not env, for the bridge.** The placeholder and the proxy address travel as `args`:
  a stdio server's `args` reach the child on every harness; its `env` only if the harness maps it.
  The placeholder is not a secret to the job that holds it, so argv visibility costs nothing. The
  bridge still reads `PROXY_URL` / `MCP_PATH` / `TOOL_PLACEHOLDER` as a fallback.
- **The `http` transport exists as a fallback for the real test.** If the bridge misbehaves against
  GitHub, `transport = "http"` lets claude's own MCP client dial the proxy directly, so a transport
  fault and a proxy fault can be told apart.
- **The bridge speaks Streamable HTTP for real.** Chunked decoding (hyper re-frames every proxied
  response as chunked), SSE `data:` parsing, `Mcp-Session-Id` echo, `MCP-Protocol-Version` after
  `initialize`, `Accept: application/json, text/event-stream`, notifications draw no reply, a
  malformed line is answered locally.

### Status of the gates (closed 2026-09-14)

All gates are green. The two that could not run on 2026-09-11 ran on 2026-09-14, after the
Docker Desktop registry client came back on its own (the host had been under a load average above
80 that evening; the machine was rebooted in between).

| Gate | Result |
| --- | --- |
| `cargo test -p maxplayer-core --locked --offline` | 419 pass. |
| `cargo test -p maxplayer-core --features acp --locked --offline` | 469 pass, 1 ignored. |
| `cargo test -p maxplayer-core --features wallet --locked --offline` | 1510 pass in the lib, all integration binaries pass. |
| `cargo test -p maxplayer-core --features wallet,acp --locked --offline` | 1575 pass in the lib, all integration binaries pass. Includes the 16 `mcp_tool_tests`. |
| `cargo test -p maxplayer --locked --offline` | 161 pass. |
| `cargo test -p maxplayer --features acp,wallet --locked --offline` | 199 pass, 1 ignored. Includes `sandbox_image_check_is_wired_into_the_boot_gate`, which needs Docker. |
| `cargo test -p maxplayer-tool-kit` | 52 pass. |
| `cargo clippy -p maxplayer-tool-kit --all-targets` | Clean. |
| `cargo clippy -p maxplayer-core --features wallet,acp --all-targets` | No new warning in the changed files; the pre-existing ones stand. |
| `docker build -f docker/maxplayer-sandbox/Dockerfile -t maxplayer-sandbox:mcp-bridge .` | Builds (rc 0; the builder stage compiled `mcp-http-bridge` under the workspace lockfile). |
| `docker run --rm --entrypoint /usr/local/bin/mcp-http-bridge maxplayer-sandbox:mcp-bridge` | Exits 2 with the usage line, as designed; `maxplayer --version` in the same image reports commit `e1bbb0e`. |

The local image tag `maxplayer-sandbox:mcp-bridge` is what a real GitHub run on this machine
should name in `[sandbox] image`, because the published default image predates the bridge.

### The real GitHub run — passed 2026-09-14

Petar supplied a fine-grained PAT (read-only, public repositories) on 2026-09-14. It sits in
`~/.config/maxplayer/github-mcp-readonly.json` on his machine, mode 0600, and never entered a
container. Three live runs passed, all through the real components and GitHub's own MCP server:
the bridge as the container command, uncontained and under the seat's egress containment; and a
REAL `claude-agent-acp` turn driven by `run_agent_job`, which called `mcp__github__get_me` through
the bridge and replied `login=pmilic021`. The bundle is `evidence/20260914T085619Z-github-proxy-swap/`; its README has the facts, the
limits, and the rerun commands. Doc 10 has the account.

Two things the run changed in the code:

- `JobLaunch` gained `mcp_servers`, and the docker argv opens the `host.docker.internal` alias when
  an MCP server entry names it. Before, only an environment value could open it, so a seat whose
  only contained credential is a vendor tool would have had no alias on Linux.
- `Containment` now hands every real credential value it holds (env, file, vendor, Codex) to the
  capture redactor's exact-value pass, so a capture that somehow showed one would still be
  redacted. The primary control is unchanged: none of them enters the container.

Two live tests carry the run: `seller_exec::mcp_tool_tests::live_a_…` and `live_b_…`, `#[ignore]`d,
configured by environment (`MAXPLAYER_MCP_LIVE_*`). The netfilter sidecar this dev build names
(`…:v0.5.8`) is a local tag alias of the published `:v0.5.7` on Petar's machine.

Gates for the acceptance commit, run the same day: core 419 / 468 / 1511 / 1576 across the four CI
rows; kit 52; clippy clean in the changed files; the `maxplayer` crate 160 of 161 and 198 of 199.
The one failure in both CLI rows is `doctor::tests::sandbox_image_check_is_wired_into_the_boot_gate`,
which needs `docker manifest inspect` on an unresolvable host to fail fast. It passed at 09:49 that
morning on the same code (161 and 199). By the evening run the Docker Desktop registry client had
wedged again — a direct `docker manifest inspect no-such-registry.invalid/nope:v0` hung past 45 s,
and one `docker desktop restart` did not clear it, exactly as on 2026-09-11. The failure is the
environment's, not the change's; rerun that test when Docker answers.

What is NOT covered, stated plainly: one vendor; one credential shape (static, header-borne); one
agent turn with one tool call. A broad credential still needs a trusted operation filter (the
Holder shape for a remote tool), which is not built. The Holder-route production integration
(section 7, doc 09) remains separate and not started.

## 11. Holder route production integration (done 2026-09-14)

Petar, 2026-09-14: "continue with the remaining planned work". The largest remaining piece was the
Holder route's production integration (section 7, doc 09). It is implemented and proved live through
the real daemon code. The bundle is `evidence/20260914T095551Z-holder-route/`; doc 09 now opens with what the implementation does
differently from the plan and why.

What was built:

- `crates/maxplayer-core/src/held_tool.rs`: `HeldTool::start` (remove a stale holder, create the two
  volumes, a one-shot root `chown` so the holder can own them as the job uid, `docker run -d` the
  holder, wait for `holderctl status`, probe that this daemon can mount a volume subpath),
  `HeldTool::attach` → `JobToolEndpoint` (the bridge entry and the `jobs/<job>` subpath mount),
  `JobToolEndpoint::detach`, `HeldTool::shutdown` (polite stop, remove the container and the runtime
  volume, keep the state volume), `Drop` fallbacks for both. Pure argv builders, unit-tested.
- `home.rs`: `[sandbox.held_tool]` → `HeldToolConfig` with `image`, `config`, `credential_file`,
  `vendor_base_url`, `vendor_cli`, `network`, `server_name`, `required`. Refusals: relative or missing
  host paths, an empty image.
- `seller_exec.rs`: `ExtraMount` (`Bind`, `VolumeSubpath`) replaces the `(PathBuf, String)` mount pair;
  `JobAttachments` (`mcp_servers`, `extra_mounts`) is what `run_agent_job_in_env` takes.
- `seller_node/run.rs`: the runner holds `held_tool: Option<Arc<HeldTool>>`; boot starts it before
  anything goes on the wire (required → refuse the boot; optional → log and serve without); both job
  paths attach after the workdir exists and detach after the run; `run_loop` shuts it down after the
  retraction. The container-delivery path adds the socket mount beside the exchange directory and hands
  the bridge entry to the orchestrator through `Phase1Inputs.mcp_servers`.
- `docker/maxplayer-sandbox/Dockerfile`: `tool-mcp-bridge` installed beside `mcp-http-bridge`.

Live tests (`#[ignore]`d, `held_tool::live_tests`): two jobs on one enrolment with job B under egress
containment, an escape refused, a restart that resumed the login (vendor `login_count` 1 throughout);
and a real `claude-agent-acp` turn that called `mcp__seller-tool__transform-file` through the socket
bridge. The vendor counters are the oracle; the vendor is the kit's fake, so this is the mechanism
through production code, not third-party acceptance.

Two facts measured on the way, both recorded in the code:

- A named volume first mounted over a directory that exists in the image is initialized from it, and a
  `chown` of the volume root in that first container is lost (Docker Desktop 29). The holder's volumes
  therefore mount at `/var/lib/maxplayer-holder` and `/run/maxplayer-holder`, paths no image ships.
- The per-job socket reaches the job through a `volume-subpath` mount, which needs Docker Engine 26 or
  newer; boot probes for it once.

### Several held tools per seat (added 2026-09-14, after "sounds good")

Petar asked how several tools would work and approved the design. The single `[sandbox.held_tool]`
table became the list `[[sandbox.held_tools]]`, each entry with a required, unique `server_name`:

- The name is what the agent addresses (`mcp__<name>__<operation>`), and it names the holder container
  and its volumes (`maxplayer-held-tool-<seat>-<name>`) and the job's socket directory
  (`/run/holder/<name>`). Config resolution refuses a name that is not plain, a duplicate, or one that
  a `[[sandbox.mcp_tools]]` entry also claims — both lists land on the job's session.
- One holder per entry, each with its own image, credential, login and volumes. Boot starts them
  concurrently; the fail posture applies per tool, and a refused boot stops the holders that did start.
- Per job, one socket per tool: `JobToolEndpoint::attachments` mounts each tool's `jobs/<job>` subpath
  at its own path and gives the bridge `--socket /run/holder/<name>/job.sock` as an argument.
  `tool-mcp-bridge` gained that flag (the environment form stays for the demo). `JobAttachments::merge`
  joins the per-tool attachments into the one list the launch takes.
- A tool that needs two credentials at once is still a kit change, not a daemon change: each holder
  holds one credential for one vendor.

Proved live through the daemon code the same day, bundle `evidence/20260914T103608Z-holder-route-two-tools/`: two holders enrolled once each;
one job drove both tools through their own sockets (its container had `/work`, `/run/holder/text-a`
and `/run/holder/text-b`, nothing else); a contained job; a restart resumed both logins; and a real
`claude-agent-acp` turn called `mcp__text-a__transform-file` and `mcp__text-b__transform-file`, each
vendor seeing one login and one transform.

What remains, stated plainly: real-vendor acceptance of a real CLI inside a seller-built holder image,
per vendor; and the review of pull request #1004. Petar gave the go to push and open it on 2026-09-14.
`origin` (the `maxy-player` fork) refused the push — read access only — so the canonical repository
`MakePrisms/maxplayerai` is the `upstream` remote and the pull request's home.

## 12. Review round 2 — the Codex review of pull request #1004 (2026-09-15)

Petar forwarded the review prompt from section 11 to a Codex agent. The verdict was "request
changes" with eleven findings. Petar asked whether I agree and, if so, to fix them. I agree with all
eleven. Two of them (2 and 1's proxy half) are defects the branch inherited from the credential proxy
(`#647`) and exposed through the generic MCP route. The table names each finding, the fix, and the
test that proves it.

| # | Severity | Finding | Fix | Proof |
| --- | --- | --- | --- | --- |
| 1 | DENY | The proxy substituted the placeholder in EVERY header. A vendor that reflects a custom header into a body with `\u` escapes returned the credential past the byte scrubber. | `JobCredential.substitute_in: HeaderScope`. An MCP tool registers with `HeaderScope::authorization()`: the placeholder is recognized and substituted in `Authorization` only; any other header goes out as written; a placeholder only outside the scope is `NoKnownPlaceholder` (502). Env and file credentials keep `HeaderScope::Any`, the behavior they had. The docs now state the residual risk: a vendor that reflects its own `Authorization` header. | `credential_proxy`: `a_scoped_placeholder_is_substituted_only_in_its_own_header`, `a_scoped_placeholder_outside_its_header_identifies_nothing`, `a_scope_that_names_no_header_is_refused_at_registration`; `seller_exec::mcp_tool_tests::the_job_gets_the_tool_through_the_swap_and_never_the_credential` (the stub vendor records a reflected header and sees the placeholder). |
| 2 | MUST-FIX | The redirect predicate compared authorities only: `https` to `http` on one host was approved, and `:443` equalled `:80`. | `allows_paired_redirect` compares ORIGINS: scheme, lowercased host, effective port (`origin_of`). `same_authority` no longer equates two different explicit ports. | `a_redirect_that_changes_the_scheme_or_the_port_is_refused`, `origins_parse_scheme_host_and_effective_port`, `an_explicit_default_port_matches_the_bare_host_but_not_another_port`; the existing redirect tests still pass. |
| 3 | MUST-FIX | A holder connection resolved its job id through the table at call time, so a connection held across detach and re-attach of the same id used the NEW directory. | `tool-holderd`: an immutable `Attachment` instance per attach; the accept loop binds each connection to the instance; every call checks the instance's `stop` before validation, before the tool runs, and before each publish; a live id cannot be attached again. | `tests/attachment_binding.rs` (real daemon, fake vendor): `an_old_connection_is_refused_after_the_same_job_id_is_attached_elsewhere`, `attaching_a_live_job_id_is_refused`, `a_detach_while_the_tool_runs_publishes_nothing`. Against the `HEAD` daemon the first test fails as the reviewer described. |
| 4 | MUST-FIX | A job-planted FIFO at an input or output name blocked a holder thread in `open`; an output FIFO could receive bytes after detach. | `safeio`: both opens add `O_NONBLOCK`; the held descriptor is `fstat`ed and must be a regular file before any read, before truncation (`set_len(0)` after the check), and before use; `ENXIO`, `EOPNOTSUPP`, `EISDIR` map to `NotARegularFile`. Connections per attachment are bounded (16); a job connection idles out in 30 s and a detached one ends. | `safeio::tests` (six, with `mkfifo`): the FIFO cases return at once and write nothing; `attachment_binding`: `a_fifo_planted_as_input_is_refused_at_once`, `a_fifo_planted_as_output_is_refused_and_receives_nothing`, `connections_over_the_bound_get_one_error_line_and_are_closed`. |
| 5 | MUST-FIX | A start that failed after `docker run` (the subpath probe, the status wait) returned before any guard owned the container: an enrolled holder stayed running with no owner. | `HeldTool::start` arms a `StartGuard` before the first `docker` call and runs the rest in `start_owned`. A failure removes the container and the runtime volume on the async path; a cancellation removes them from the guard's drop. The state volume stays. | The unit tests cover the pure parts; the live rerun below covers the happy path. The failure path is by construction: every `?` in `start_owned` returns into the `Err` arm of `start`. |
| 6 | MUST-FIX | `shutdown` set `stopped` and `detach` set `detached` BEFORE the docker call, so a failure disabled the drop fallback and `shutdown` reported a stop that did not happen. | Both set their flag only after a confirmed result: `remove_container` is `Ok` only when the container is gone (removed, or absent); `detach` is `Ok` on the holder's confirmation or its "no such attached job". Both return `Result` and the daemon logs an `Err`. | Type-level: the flag stores follow the `?`. `run.rs` handles every `Result` (five sites). |
| 7 | MUST-FIX | Every `docker` call ran `Command::output()` with no deadline; a hung `docker exec` held boot or a job forever. | `run_bounded`: spawn, drain both pipes on threads, poll `try_wait`, kill and reap at the deadline; per-class deadlines (query 20 s, control 30 s, run 120 s, remove 60 s). The `Drop` fallbacks run through it too. | `a_bounded_docker_call_is_killed_at_its_deadline`. |
| 8 | MUST-FIX | The HTTP bridge read a response to EOF before emitting anything and read stdin only between responses, so a server request sent mid-stream could never be answered. | `mcp-http-bridge` rewritten: a worker thread per stdin message; `http::request_streaming` and `SseSplitter` emit each JSON-RPC message when its event completes; stdin is read while a stream is open; a stdin line is classified (request, notification, response) and a response's `202` with an empty body draws no error line. `vendor-mcp --server-request` and the kit's `swap-proxy` test double stream too. | `proxy_swap_suite`: `the_bridge_relays_a_server_request_mid_stream_and_posts_the_answer_while_the_stream_is_open`, `the_bridge_serves_two_requests_at_once_and_each_reply_carries_its_own_id`; the six earlier suite tests pass on the new path. Limit: no `GET` stream for unsolicited server messages (documented in the binary). |
| 9 | SHOULD-FIX | `mode = "launcher"` accepted both tool tables and then served no tool. | `SandboxPolicy::from_config` refuses a non-empty `mcp_tools` or `held_tools` under launcher mode. | `mcp_tool_tests::launcher_mode_refuses_both_tool_tables`. |
| 10 | SHOULD-FIX | A boot removed only the configured holders' stale containers; a renamed or removed tool's holder from a killed daemon stayed. | `reconcile_stale_holders(seat, configured)` at boot (docker seats): `docker ps -a --filter label=maxplayer.held-tool.seat=<seat>`, remove every holder not named by the config and its runtime volume, keep state volumes. | `stale_holders_are_this_seats_unconfigured_holders_and_nothing_else` (the pure selection). |
| 11 | SHOULD-FIX | The GitHub evidence README credited the redacted diagnostics capture as proof of the credential's absence; the redactor removes the value before that scan. | The README separates the RAW observations (`docker inspect` before cleanup, the docker argv, the session entry, the MCP transcript) from the redacted capture, and names the reflection limit. Live test A now reads `docker logs` of the job container raw, before the redacting capture, and asserts absence there. | The README text; the rerun below. |

### Reruns on 2026-09-15, on the fixed tree and the rebuilt images

- `cargo test -p maxplayer-tool-kit --locked`: 74 pass (was 52). `cargo test -p maxplayer-core`:
  449 / 498 / 1564 / 1629 pass across the four feature rows (was 449 / 498 / 1555 / 1619). The CLI
  rows: 161 of 162 and 199 of 200; the one failure in each is
  `doctor::tests::sandbox_image_check_is_wired_into_the_boot_gate`, and its cause is now known and
  is not the branch: Docker Desktop's credential helper (`docker-credential-desktop get`) hangs on
  this machine, so `docker manifest inspect` never returns. The same test passes when the docker
  client runs with an anonymous config that names no credential store.
- Both images were rebuilt from this tree: `maxplayer-tool-kit:demo` (the new `tool-holderd` and
  `safeio`) and `maxplayer-sandbox:tools` (the new `mcp-http-bridge`). The builds needed the
  anonymous docker config for the same reason.
- The Holder live proof with two tools
  (`held_tool::live_tests::live_two_tools_serve_two_jobs_on_one_enrolment_each_and_a_restart_resumes_them`,
  contained, network `maxplayer-jobs`) passed on the new lifecycle code: two enrolments, two jobs,
  a restart that resumed both logins, confirmed shutdowns.
- The GitHub acceptance of the Proxy swap route
  (`mcp_tool_tests::live_a_the_bridge_in_the_sandbox_image_reaches_the_real_vendor_through_the_swap`,
  contained) passed with the streaming bridge: `initialize`, `tools/list`, `get_me` → `pmilic021`,
  a file read; the placeholder refused at GitHub and at the proxy; job end revoked it; and the new
  raw `docker logs` read carried no token. The run's files were scanned for the token: none carries
  it. The agent-turn live tests (`live_b`, `live_a_real_agent_turn…`) were not rerun; each spends a
  model turn, and neither the launch path nor the ACP wire shape changed in this round.

### Review round 3 — the re-review of round 2 (2026-09-15)

The same Codex agent re-reviewed `2f92830..367e899` with its round-1 reproductions and the fake
Docker harness. It closed findings 1, 2, 8 and 9 outright, closed 3 and 4 for the reported cases,
and returned eight new findings on the fixes themselves. I agree with all eight. The core five:

| # | Severity | Finding | Fix | Proof |
| --- | --- | --- | --- | --- |
| R3-1 | MUST-FIX | A call that passed the last `detached()` check could pause before it wrote (the reviewer paused it with a FIFO as the staged output); detach completed, the same job id attached again at the same pathname, and the old call wrote into the new directory. | `Attachment.publish: Mutex<()>`. Every staged output is read into memory first, outside the lock; the `stop` check and the writes are one unit under the lock; `stop_attachment` sets `stop`, then takes and releases the lock (waits for a publication in flight), then removes the socket. After detach returns, no old call can write. | `attachment_binding::a_call_that_pauses_before_publishing_cannot_write_after_the_detach`; the `HEAD` daemon fails it as the reviewer described. |
| R3-2 | MUST-FIX | `StartGuard` removed the holder's resources at once when the start future was dropped while `docker run` still ran on the blocking pool; the container then appeared with no owner. | `OwnedCall`: the blocking task records when the call ended, the future's owner records the abandonment, and whichever comes second runs the cleanup, exactly once and only after the effect exists. Every call in `start_owned` goes through it. `attach` owns its attachment the same way, which also closes the cancelled-attach gap round 2 left open. | `an_owned_call_dropped_in_flight_cleans_up_exactly_once_after_the_child_ends`, `an_owned_call_that_completes_cleans_up_only_when_it_stays_armed`. |
| R3-3 | MUST-FIX | `container_exists` read any non-zero `docker inspect` as "absent": a daemon that was down let `shutdown` mark a running holder stopped and disarm its fallback. | `inspect_outcome`: exit 0 is present, the daemon's own "no such" words are absent, anything else is an error that keeps the flag unset. | `inspect_words_decide_presence_and_an_unknown_answer_is_an_error`. |
| R3-4 | MUST-FIX | `run_bounded` collected the pipes for two seconds and turned a timeout into empty output with a success code; a descendant that held a pipe made `docker ps` read as empty and the reconcile remove nothing. | The child runs in its own process group; one deadline covers the run and the collection; a pipe held past it is an `Err`, and the group is killed so the descendant does not outlive the call. | `a_descendant_that_holds_the_pipes_fails_the_call_instead_of_emptying_its_output`. |
| R3-5 | SHOULD-FIX | A vendor that sent the final response and then kept its SSE stream open left the bridge's worker reading forever; every request added a worker and a connection. | A request worker stops reading and drops its connection once its answer is written. Request workers are bounded (`MAX_REQUESTS_IN_FLIGHT = 32`): a request over the bound gets one immediate error line on its id; notifications and responses are never bounded, so an answer to a server request always goes out. `vendor-mcp --hold-stream` and `--delay-ms` are the test modes. | `proxy_swap_suite::a_request_worker_ends_when_its_answer_arrives_even_if_the_vendor_holds_the_stream`, `requests_over_the_in_flight_bound_get_one_error_line_and_the_rest_are_served`. |
| R3-6 | SHOULD-FIX | The holder's read timeout only checked `detached()` and continued, so silent connections on an attached job held every slot, and section 12's "idles out in 30 s" was false. | `tool-mcp-bridge` opens one connection per message, so idle expiry is safe: a job connection with no complete request within the idle timeout is closed, attached or not. Daemon flag `--job-idle-timeout-secs` (default 30). | `attachment_binding::silent_connections_expire_after_the_idle_timeout`; the `HEAD` daemon fails it. |
| R3-7 | SHOULD-FIX | The boot reconciled stale holders only when the current mode was docker; a seat that left docker mode kept its old holders. | The mode is no longer the test. A marker file in the seat's home (`held-tools-started`) is written when holders start and removed once a boot with no held tools has reconciled; the reconcile runs when the config names held tools OR the marker exists. A seat that never held a tool makes no `docker` call at boot (an unconditional call shifted the timing of the relay-fixture tests). A host with no docker CLI is quiet (`DOCKER_NOT_RUNNABLE`). | Read: the predicate; `marker_path`, `write_marker`. |
| R3-8 | SHOULD-FIX | Live test A did not check that `docker logs` succeeded, so a failed retrieval passed the absence check on the error text; the README called run B's redacted wire a raw view. | The test asserts the exit status first. The README says run B has no raw observation of its own. | Read. |

#### Reruns after round 3 (2026-09-15, on `c015c66` and the marker change)

- Kit: 78 pass (was 74), clippy clean. Core: 449 / 498 / 1568 / 1633 across the four rows. CLI:
  162 / 162 and 200 / 200 with the anonymous docker config (the doctor test reaches the registry
  client that way). Clippy on core: no site inside the changed lines.
- One core test failed once in a full row and passed three times alone:
  `seller_node::run::tests::offer_backfill_recovers_an_offer_the_deaf_live_sub_never_delivered`,
  at its relay fixture's `expect("relay run")`, the #548 fixture flake. The reviewer saw the same
  shape once in `seller_node::lock::tests::second_acquire_fails_closed_while_first_is_held`; it
  passed five times alone here. Neither module is touched by the branch.
- Both images rebuilt from this tree; the Holder live proof with two tools (contained) and the
  GitHub acceptance of the Proxy swap route (run A, contained) passed again on them; the token is
  in none of the run's files. The agent-turn live tests were not rerun (each spends a model turn).

### Review round 4 — green light with one follow-up (2026-09-16)

The reviewer verified `5ed3b08` (78 kit tests, 13 holder tests, and its own publication,
cancellation, cleanup, pipe, idle-timeout and bridge reproductions) and gave a green light "once CI
passes", with one non-blocking follow-up: the attach error path ran its best-effort detach for
EVERY failure, so a duplicate attach the holder refused ("already attached; detach it first")
detached the attachment that already existed. `attach_answer` now classifies the call: attached
(exit 0), refused (a non-zero exit: nothing was attached by this call, nothing to take back), or
unknown (killed at its deadline, or `docker` failed), and only the unknown case detaches. Test:
`an_attach_the_holder_refused_is_not_taken_back_but_an_unknown_one_is`. No normal daemon path
issues a duplicate attach.

CI on `5ed3b08`: every job passed except "Money-path tests", where
`seller_node::run::tests::a_losing_open_pool_claimant_releases_its_slot_when_it_sees_the_award`
failed at its 5 s relay pump ("the winner must claim the open-pool offer", `run.rs:12928`, a test
from 2026-08-06 that the branch does not touch); 1577 other tests in that row passed. The reviewer
ran the test alone in release and it passed. The rule the reviewer set, and the right one: do not
merge while that check is red; a local pass does not clear GitHub's failed check.

What stays open after this round, stated plainly:

- The residual path of finding 1 is the vendor: a vendor that reflects its `Authorization` header
  into a body. The proxy does not read bodies. The routing doc and the skill say not to onboard such a
  vendor. Env and file credentials keep the any-header scope they had; a change there is a separate
  decision, since a forwarded agent credential rides a vendor-specific header.
- A detach does not kill a tool run in flight; the publish step refuses afterwards.
- The bridge opens no `GET` stream. A server message that does not ride on a response to a client
  request is not received.
- The holder's connection bound (16) is a constant; the idle timeout defaults to 30 s and the core
  daemon does not pass the flag. The bridge's in-flight request bound (32) is a constant.

