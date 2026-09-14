# Holder route with TWO held tools — proved live through the daemon code, 20260914T103608Z

This bundle proves the list shape of the Holder route (`[[sandbox.held_tools]]`,
`docs/specs/seller-tool-onboarding/09-production-integration.md`) through the REAL daemon code:
`held_tool::HeldTool::start` per tool, `HeldTool::attach` per tool per job, the real
`prepare_launch` and `SandboxPolicy::launch_with_mounts`, the real cleanup capture,
`HeldTool::shutdown`, and a restart. Two tools, `text-a` and `text-b`, each from the kit image,
each with its own fake vendor published on loopback so the HOST reads its counters. Two synthetic
credentials, generated for the run, each known only to its own vendor; no file here carries either.

| Run | What ran | Result |
| --- | --- | --- |
| Two holders, one enrolment each | Both holders started concurrently and each enrolled once with its own vendor. | `login_count` 1 at each vendor. Two containers, two pairs of volumes, named by seat and tool. |
| Job A drives both tools | One job, both tools attached: the container got its workdir plus ONE socket mount per tool, at `/run/holder/text-a` and `/run/holder/text-b`. The bridge for `text-a` (`--socket /run/holder/text-a/job.sock`) ran `initialize`, `tools/list`, `transform-file` and an escape attempt; then the bridge for `text-b` ran `transform-file`. | `out-a.txt` uppercased by tool a, `out-b.txt` reversed by tool b. The escape was refused. Each vendor ran exactly one transform. |
| Job B, under egress containment, drives tool b | `tools/list` and `transform-file`. | The same offering as job A saw, whole schema compared. `login_count` still 1 at each vendor. |
| Daemon stop, daemon start | Both holders shut down and started again. | Both resumed their persisted logins: `resumed_existing_session` true, zero enrolments, `login_count` 1 at each vendor. A further transform ran on the resumed session of tool a. |
| A real agent turn | `claude-agent-acp` (`claude-sonnet-5`), driven by `run_agent_job_in_env` exactly as an awarded job is, both tools attached. | The ACP wire (`holder-agent-acp-wire.txt`) shows `mcp__text-a__transform-file` and `mcp__text-b__transform-file`; tool a wrote `AGENT PAYLOAD`, tool b wrote `daolyap tnega`; the agent replied `done`; each vendor saw one login and one transform. |

## Facts of the run

| Fact | Value |
| --- | --- |
| Code under test | `4be07a8f0201d6d2734efe836df7b4fda1ff354c` plus the working-tree changes committed as the next commit (the list shape, the per-tool sockets, the bridge's `--socket` flag); this bundle is the commit after |
| Holder image, both tools | `maxplayer-tool-kit:demo` (`c4a743f4c067`) |
| Sandbox image | `maxplayer-sandbox:tools` (`f60f0245e5c1`), rebuilt from this branch so its `tool-mcp-bridge` understands `--socket` |
| Egress containment for job B and the agent turn | network `maxplayer-jobs`, proxy ports `49320-49329` and `49330-49339`, netfilter sidecar `ghcr.io/makeprisms/maxplayer-netfilter:v0.5.7` aliased locally as `:v0.5.8` |
| Host | Petar's macOS machine, Docker Desktop 29.1.3 |

## How to rerun

```sh
MAXPLAYER_HELD_TOOL_LIVE_NETWORK=maxplayer-jobs MAXPLAYER_HELD_TOOL_LIVE_PROXY_PORTS=49320-49329 \
MAXPLAYER_HELD_TOOL_LIVE_EVIDENCE_DIR=$PWD/evidence/$(date -u +%Y%m%dT%H%M%SZ)-holder-route-two-tools \
  cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture held_tool::live_tests::live_two_tools
set -a; source ~/.maxplayer/agent-creds.env; set +a
MAXPLAYER_HELD_TOOL_LIVE_NETWORK=maxplayer-jobs MAXPLAYER_HELD_TOOL_LIVE_PROXY_PORTS=49330-49339 \
  cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture held_tool::live_tests::live_a_real_agent
```

## Limits

- The vendors and the CLI are the kit's fakes, so this proves the MECHANISM through production
  code, not third-party acceptance of any real tool.
- Both tools here run the same kit image and the same offering; distinct images and offerings differ
  only in the config entries, not in the daemon path exercised.
- One agent turn with two tool calls.

The earlier bundle `20260914T095551Z-holder-route/` proved the single-tool shape with the config
table `[sandbox.held_tool]`, which this list shape replaced the same day. It stays as the record of
that run.
