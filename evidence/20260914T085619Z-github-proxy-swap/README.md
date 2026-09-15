# Proxy swap — real-vendor acceptance on GitHub, 20260914T085619Z

This bundle is the first real-vendor acceptance run of the Proxy swap route
(`docs/specs/seller-tool-onboarding/10-routing-and-options.md`). The vendor is GitHub's remote MCP
server. The credential is a fine-grained personal access token, read-only, public repositories only,
owned by the GitHub user `pmilic021`. The token lives in a host file and never entered a container.
Every file in this bundle was scanned for the token before it was committed; none carries it. The
per-job placeholders (`mxp-mcp-…`) in the files were revoked at job end and are worthless.

Three runs, all green, all through the REAL components: the real credential proxy (`#647`), the
real sandbox launch argv (`SandboxPolicy::launch`), the real preparation (`prepare_launch`), the
real cleanup and capture path, the sandbox image built from this branch with `mcp-http-bridge` in
it, and GitHub itself as the oracle.

| Run | What ran in the container | Egress | Result |
| --- | --- | --- | --- |
| `a-uncontained/` | `mcp-http-bridge` as the container command, driven over stdio with the dialogue an agent holds | The default bridge network; the proxy reached through the `host.docker.internal` alias the launch opens | `initialize`, `tools/list` (27 tools), `get_me` → `pmilic021`, a file read; all through the swap |
| `a-contained/` | The same | The seat's egress containment: a namespace holder, the proxy reached through the firewall pinhole at the measured gateway address | The same |
| `b-agent/` | `claude-agent-acp`, a REAL agent turn (`claude-sonnet-5`), driven by `run_agent_job` exactly as an awarded job is | Egress containment, as above | The agent called `mcp__github__get_me` through the bridge and replied `login=pmilic021` |

## What each run proves

1. **The tool works through the swap.** GitHub's own server (`github-mcp-server`) answered every
   call. It authenticates nothing without the real token (measured: `401` with no bearer), so a
   `200` is GitHub's word that the real token arrived — swapped in by the proxy at egress.
2. **The host handed the container no credential.** The RAW observations, none of them redacted:
   `a-container-inspect.json` is `docker inspect` of the job container, read before cleanup (its
   environment carries only the git identity and the image's own variables; its command carries
   the placeholder); the docker argv the launch built; the session entry; and the MCP transcript
   the test drove over the container's stdio. The test asserts the token's absence from each of
   these. The diagnostics capture (`a-diagnostics-*`, `b-diagnostics-*`) is what the real cleanup
   path saved, and that path REDACTS every real value the launch held. The token's absence there
   proves the redactor, not the boundary. Review round 2 (2026-09-15) made this distinction; the
   test now also reads `docker logs` of the job container raw, before the redacting capture, and
   asserts the token's absence there (`a-container-logs-raw.txt` in a rerun).
3. **The placeholder is worthless outside the job.** Sent straight to GitHub it gets `400` (GitHub
   answers `400` to a bearer that is not shaped like one of its tokens, `401` to a missing or
   GitHub-shaped bad one). An unknown placeholder at the proxy gets `502` with no substitution.
4. **Job end is revocation.** The proxy answered on its port while the job lived and refused the
   connection after the preparation was dropped.
5. **The production path, end to end (run B).** `b-diagnostics-logs.txt` is the ACP wire captured
   from the container: the `tool_call` for `mcp__github__get_me`, the permission request the
   driver answered, the vendor's answer in the `tool_call_update`, then the reply chunks. The agent
   credential (`CLAUDE_CODE_OAUTH_TOKEN`) was contained by the same proxy; the run log
   (`b-seller-run.jsonl`) has the agent's reply in order.

## Facts of the run

| Fact | Value |
| --- | --- |
| Code under test | `ffdc31b` plus the working-tree changes committed as `7c6d43c` (the live tests, the alias pinhole, the redactor belt); this bundle is committed as the next commit |
| Sandbox image | `maxplayer-sandbox:mcp-bridge`, `sha256:7f3bc6fefd0182f3b7c4785bf6711fbbcada701dc34a7c1638831d11905980db` — built from this branch with `docker/maxplayer-sandbox/Dockerfile` |
| Netfilter sidecar for the contained runs | `ghcr.io/makeprisms/maxplayer-netfilter:v0.5.7` (`4e72bac53efc`), aliased locally as `:v0.5.8`, the tag this dev build names |
| Vendor endpoint | `https://api.githubcopilot.com/mcp/readonly` |
| Credential | fine-grained PAT, read-only, public repositories, expires 2027-08-08; host file mode 0600 |
| Docker network for the contained runs | `maxplayer-jobs`; proxy port ranges `49300-49309` (A) and `49310-49319` (B) |
| Host | Petar's macOS machine, Docker Desktop 29.1.3 |

## How to rerun

```sh
# The credential: a host JSON file, absolute path, mode 0600, {"token": "..."}.
export MAXPLAYER_MCP_LIVE_CREDENTIAL_FILE=/ABSOLUTE/path/github-mcp-readonly.json
export MAXPLAYER_MCP_LIVE_EXPECT_LOGIN=<the token owner's GitHub login>
export MAXPLAYER_MCP_LIVE_EVIDENCE_DIR=$PWD/evidence/$(date -u +%Y%m%dT%H%M%SZ)-github-proxy-swap/a-uncontained
cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture mcp_tool_tests::live_a
# Contained, as a real seat runs:
MAXPLAYER_MCP_LIVE_NETWORK=maxplayer-jobs MAXPLAYER_MCP_LIVE_PROXY_PORTS=49300-49309 \
  cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture mcp_tool_tests::live_a
# The real agent turn needs the agent credential in the environment too:
set -a; source ~/.maxplayer/agent-creds.env; set +a
MAXPLAYER_MCP_LIVE_NETWORK=maxplayer-jobs MAXPLAYER_MCP_LIVE_PROXY_PORTS=49310-49319 \
  cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture mcp_tool_tests::live_b
```

## Rerun after review round 2 (2026-09-15)

Run A was rerun, contained, on the tree that carries the review fixes (the swap scoped to the
`Authorization` header, the origin-exact redirect rule, the streaming bridge) and on a sandbox image
rebuilt from it. It passed: the same dialogue, the same refusals, the same revocation, and the new
raw `docker logs` assertion. The run's files are not added to this bundle; the facts are recorded in
`docs/handoff/CONTINUATION-2026-09-10.md`, section 12.

## Limits

- One vendor, one credential shape (a static header-borne token). A vendor whose credential is not
  static, or not header-borne, is not covered.
- The proxy scrubs the exact bytes of the token from the response stream and reads nothing else. A
  vendor that reflects a request header into a body in another encoding is not caught by that
  scrubber. Since 2026-09-15 the swap is scoped to the `Authorization` header, so a job cannot
  choose the reflected header; GitHub does not reflect `Authorization`. This bundle does not prove
  that for any other vendor.
- For run B the container's environment and command come from the same `prepare_launch` and
  `launch` code that run A inspected raw. Run B's own raw view is the ACP wire in
  `b-diagnostics-logs.txt`, which the redacting capture saved.
- The read-only endpoint and the read-only token together bound what a job can do. The proxy itself
  constrains the destination host, not the operations; that is the scope fork in doc 10, unchanged.
- Run B is one agent turn with one tool call. It proves the harness maps the session entry and
  drives the bridge; it is not a load or concurrency test.
