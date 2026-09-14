# Evidence bundles

Two kinds of bundle live here, named by UTC start time.

## Real-vendor acceptance: `20260914T085619Z-github-proxy-swap/`

The Proxy swap route (`docs/specs/seller-tool-onboarding/10-routing-and-options.md`) accepted
against GitHub's remote MCP server on 2026-09-14: the bridge in the sandbox image, uncontained and
under egress containment, and a real `claude-agent-acp` turn, all through the real credential
proxy. The token owner's login came back from GitHub; the token was absent from everything the
container received; the placeholder was refused at the vendor and revoked at job end. This is the
one bundle here that is NOT `mechanism_only`: the oracle is a third party. Read its `README.md`
for what it proves, its limits, and how to rerun it.

## Holder route through the daemon: `20260914T095551Z-holder-route/`

The Holder route wired into the seller daemon (`held_tool.rs`, `[sandbox.held_tool]`) and proved
live through the real daemon code on 2026-09-14: one holder container, two jobs on one enrolment
(one under egress containment), an escape refused, a restart that resumed the persisted login, and
a real `claude-agent-acp` turn that called the seller's tool through `tool-mcp-bridge`. The vendor
is the kit's fake, so this is `mechanism_only` through production code — not third-party
acceptance. Read its `README.md`.

## Holder demo bundles

Each of the remaining subdirectories is one complete execution of
`crates/maxplayer-tool-kit/docker/demo.sh`. A bundle holds per-step verdicts (`results.txt`), both
MCP transcripts, holder and vendor logs, vendor counter snapshots, and a `manifest.json`.

## Current bundle: `20260910T125544Z` (post-repair, digest-pinned)

This is the current bundle. It is a run of the repaired demo against the digest-pinned image from
`crates/maxplayer-tool-kit/docker/Dockerfile`: **39 checks, 0 failures**. It demonstrates the F1,
F2, F3, and F4 repairs end to end, with no caveat.

- **F1**: `credential_absent_from_job_container` is 0, and the negative control
  `secret_scanner_detects_planted_credential` is 1. The scan runs host-side, and the control
  proves the scanner can detect the secret when it is present.
- **F2**: the container transforms run through the holder's private staging path. A cross-job
  path is refused with code 1003.
- **F3**: the lifecycle proof re-attaches the job, then proves call, loss, and restore on a live
  endpoint. The tool list is compared in full across both jobs and the seller control view.
- **F4**: `manifest.json` `build` records the exact pinned base-image digests, the built image
  id, and a clean committed source tree. `source_tree_dirty` is false and the recorded base
  digest matches the Dockerfile pin.

## Superseded bundles: `20260909T220053Z` and `20260909T220105Z` (pre-repair)

These two bundles are **superseded**. Do not cite them. The pre-repair demo produced them, so the
old, invalid F1 credential check and the old F3 lifecycle probe are in them. They are kept, not
deleted, because a superseded record is preserved rather than removed.

Every bundle here is `mechanism_only`. The vendor and the CLI were both written for this
contract and therefore cannot falsify it: these runs establish that the holder mechanism
behaves as specified, not that any third-party tool has been accepted.

## Why there are two bundles from 2026-09-09

`20260909T220053Z` and `20260909T220105Z` are two runs started twelve seconds apart. That
overlap was not intentional.

An earlier version of this file blamed a turn killed mid-flight by a provider error. That was a
guess presented as fact, and it was probably wrong: two sessions were writing this worktree and
branch at the time, and on 2026-09-09 15:06 PDT the second was ordered to stop and this author
was made sole writer. The second run is most likely the other session's.

**What the artifacts actually support:** two executions started twelve seconds apart, on
separate networks, volumes and container names, on the same Docker daemon, both completing
27/27. **What they do not support:** attribution. Both sessions committed under the worktree
identity `w-seller-tool-onboarding-r2`, and no artifact records the launching process, so which
session produced which bundle cannot be established from what is here. Stating it either way
would be invention.

They are kept rather than pruned, because deleting an inconvenient artifact is a worse habit
than explaining it, and because two runs that agree are mildly better than one.

| | `20260909T220053Z` | `20260909T220105Z` |
| --- | --- | --- |
| checks passed | 27 | 27 |
| checks failed | 0 | 0 |
| verdict | PASS | PASS |
| platform | linux/arm64, docker 29.5.2 | linux/arm64, docker 29.5.2 |

The check *names* are identical between the two; the only textual differences are the run id
and the output path. Either bundle can be read as the record of the run; citing one is not a
claim that the other disagrees.

One caveat worth stating rather than leaving implicit: because the runs overlapped in time on
a shared Docker daemon, they are not independent in the strong sense — a daemon-level fault
could in principle have affected both. Each run does use its own network, volumes and
container names, so they do not share holder state, vendor state or sockets.
