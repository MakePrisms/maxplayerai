# Holder route — production integration proved live, 20260914T095551Z

This bundle is the live proof of the Holder route (`docs/specs/seller-tool-onboarding/10-routing-and-options.md`)
running through the REAL daemon code, not the kit's demo script: `held_tool::HeldTool::start`,
`HeldTool::attach`, the real `prepare_launch`, the real `SandboxPolicy::launch_with_mounts`, the
real cleanup capture, `HeldTool::shutdown`, and a restart. The vendor is the kit's fake vendor,
published on loopback so the HOST reads its counters; they are the oracle. The credential is a
synthetic secret generated for the run; it exists only inside the fake vendor. Every file here was
scanned for it before commit; none carries it.

| Run | What ran | Result |
| --- | --- | --- |
| Two jobs, one enrolment | The holder started and enrolled once (vendor `login_count` 1). Job A, uncontained, and job B, under the seat's egress containment, each got its own socket directory mounted at `/run/holder` and drove `tool-mcp-bridge` as the container command: `initialize`, `tools/list`, `transform-file`. | Both transforms landed in the jobs' own directories. Both jobs saw the same offering, whole schema compared. `login_count` stayed 1. |
| An escape attempt | Job A asked the tool to read `../job-b-held-live/input.txt`. | Refused by the holder; nothing written outside the job. |
| Daemon stop, daemon start | `HeldTool::shutdown` removed the holder and its runtime volume; `HeldTool::start` ran again. | The holder resumed the login from its state volume: `resumed_existing_session` true, zero enrolments, `login_count` still 1. A third transform ran on the resumed session. |
| A real agent turn | `claude-agent-acp` (`claude-sonnet-5`), driven by `run_agent_job_in_env` exactly as an awarded job is, with the held tool attached. | The ACP wire (`holder-agent-acp-wire.txt`) shows `mcp__seller-tool__transform-file`; the holder wrote `AGENT PAYLOAD`; the agent replied `done`. |

What each job container was given (`holder-job-*-inspect.json`): the job's workdir at `/work`, and
ONE volume mount — the holder's runtime volume, subpath `jobs/<job>`, at `/run/holder`. Not the
holder's state volume, not the credential, not any other job's socket. Its environment holds only the
git identity and the image's own variables.

## Facts of the run

| Fact | Value |
| --- | --- |
| Code under test | `5883c795503a1a6b55a36449ff3ad61171846de9` plus the working-tree changes committed as the next commit (the `held_tool` module and its wiring); this bundle is the commit after |
| Holder image | `maxplayer-tool-kit:demo` (`c4a743f4c067`), the kit's digest-pinned image from 2026-09-10 — `tool-holderd`, `holderctl`, the fake `vendor-cli` and `vendor-service` |
| Sandbox image | `maxplayer-sandbox:tools` (`b7f4c9da8573`), built from this branch with `docker/maxplayer-sandbox/Dockerfile`; carries `tool-mcp-bridge` and `mcp-http-bridge` |
| Egress containment for job B and the agent turn | network `maxplayer-jobs`, proxy ports `49320-49329` and `49330-49339`, netfilter sidecar `ghcr.io/makeprisms/maxplayer-netfilter:v0.5.7` aliased locally as `:v0.5.8` |
| Holder volumes | `maxplayer-held-tool-state-<seat>` (kept across the restart), `maxplayer-held-tool-runtime-<seat>` (removed at shutdown, recreated at start) |
| Host | Petar's macOS machine, Docker Desktop 29.1.3 |

## One thing learned the hard way

A named volume first mounted over a directory that EXISTS in the image is initialized from that
directory, and on Docker Desktop 29 a `chown` of the volume root in that same first container
reports success and is then lost. The holder runs as the job uid and must own its volumes, so they
are mounted at paths no image ships: `/var/lib/maxplayer-holder` and `/run/maxplayer-holder`. The
one-shot `chown` then holds. `held_tool::HOLDER_STATE_DIR` records the measurement.

## How to rerun

```sh
# Needs docker, the kit image (crates/maxplayer-tool-kit/docker/Dockerfile → maxplayer-tool-kit:demo)
# and the sandbox image with the bridges (docker/maxplayer-sandbox/Dockerfile → maxplayer-sandbox:tools).
MAXPLAYER_HELD_TOOL_LIVE_NETWORK=maxplayer-jobs MAXPLAYER_HELD_TOOL_LIVE_PROXY_PORTS=49320-49329 \
MAXPLAYER_HELD_TOOL_LIVE_EVIDENCE_DIR=$PWD/evidence/$(date -u +%Y%m%dT%H%M%SZ)-holder-route \
  cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture held_tool::live_tests::live_two_jobs
# The real agent turn needs the agent credential in the environment too:
set -a; source ~/.maxplayer/agent-creds.env; set +a
MAXPLAYER_HELD_TOOL_LIVE_NETWORK=maxplayer-jobs MAXPLAYER_HELD_TOOL_LIVE_PROXY_PORTS=49330-49339 \
  cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture held_tool::live_tests::live_a_real_agent
```

## Limits

- The vendor and the CLI are the kit's fakes, written for this contract, so this run proves the
  MECHANISM through the real daemon code. It is not third-party acceptance of any real tool; that
  stays a separate stage per vendor, as `docs/specs/seller-tool-onboarding/08-gaps-and-unsupported.md`
  says.
- One held tool per seat. A list is a config change and a socket per tool; not built.
- One agent turn with one tool call.
