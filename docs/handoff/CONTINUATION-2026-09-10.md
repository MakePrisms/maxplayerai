# Seller-tool onboarding — continuation, 2026-09-10

This document continues [`README.md`](README.md). Read the README first for the branch state, the
architecture, and the five review findings. This document records the work that cleared the
findings, and the plan for the production integration that remains.

Author: Petar's local agent, 2026-09-10.

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
| Skill | `.claude/skills/seller-tool-onboarding/SKILL.md` |
| Config template | `crates/maxplayer-tool-kit/templates/seller-tool-config.template.json` |
| Field guide and examples | `crates/maxplayer-tool-kit/templates/README.md` |
| Worked example A (file CLI) | The demo and `fixtures/seller-tool-config.json` are the runnable Walk A. |
| Worked example B (HTTP) | Deferred. HTTP profiles are deferred in specs 03 and 06. |

## 7. Production integration plan (still owed)

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
