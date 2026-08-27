# claude-agent-acp 0.70.0 — captured wire

`session-new-and-init-frame.json` is a real capture, not a hand-written fixture. It exists
because the #896 fixture was ground-truthed by READING the adapter's source instead of
running it, so it invented a `currentValue` the adapter never sends. Source-reading cannot
falsify an invented scalar; only a capture can.

The unit tests in `src/driver/acp_driver.rs` read every load-bearing value OUT of this file
and assert its sha256, so a value cannot be quietly edited into agreement with the code.

| | |
|---|---|
| adapter | `@agentclientprotocol/claude-agent-acp` 0.70.0 |
| binary | `/Users/forge/forge/npm/bin/claude-agent-acp` |
| captured | 2026-08-27 |
| this file | 7244 bytes, sha256 `4dc4c23645f0ddd6b05f65ac34b1e891e36b53046b7b79a91a919535f194a1ec` |
| unredacted capture | 10891 bytes, sha256 `d0b91be12a808e4c6f94b75e011bfed4901303b2ae85f503143c1b4c051fc58e` |

A second install of the same package exists at `/opt/homebrew/bin/claude-agent-acp` at
version 0.64.0. **Name the binary in every version claim about this adapter** — a bare
`claude-agent-acp` on `PATH` is a claim about your `PATH`, not about the fleet.

## Reproducing it

`capture.mjs` is the client that produced the file. It speaks just enough ACP to
`initialize`, open a session with the raw-SDK opt-in filtered to `system`/`init`, run one
one-word prompt, and record every frame verbatim.

```
node capture.mjs /Users/forge/forge/npm/bin/claude-agent-acp out.json
```

It needs working Claude credentials and spends one real turn, which is why no unit test
runs it. `the_committed_capture_still_matches_the_live_adapter` does run it, gated on
`LIVE_ACP_CAPTURE_BIN`; that test is the authenticity proof, and this file's sha256 is only
the drift guard.

## Redaction

The raw `system`/`init` frame carries the capturing workstation's environment — installed
skills, slash commands, memory paths, cwd, an IPC socket path. Those are not adapter wire
and do not belong in this repo.

They were removed by **deleting whole keys, never by altering a value**, because a deletion
cannot fabricate a scalar. The list is `REDACTED_KEYS` in `acp_driver.rs`; the live test
applies the same list before comparing, so the redaction is part of the reproducible
procedure rather than a one-off edit. Re-serializing the unredacted capture reproduces its
source bytes exactly (`json.dumps(indent=2, ensure_ascii=False)`), which is what makes
"differs only by deletions" checkable rather than asserted.
