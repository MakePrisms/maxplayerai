# Authorized runtime — what was probed, and what answered

The verdict against `e72e9bd` required integration with a supported coding/model harness using the
**existing authorized runtime only**, and a concrete BLOCKED if none was available. This file
records what was actually probed, first-hand, rather than what was found on `PATH`.

Presence on `PATH` is not availability. Each adapter below was **driven** over ACP stdio
(`initialize` → `session/new` → `session/prompt`) and judged on whether a model answered.

## Adapters probed

| Adapter | On PATH | Handshake | Session | Model answered | Verdict |
|---|---|---|---|---|---|
| `/opt/homebrew/bin/claude-agent-acp` | yes | protocol 1, OK | OK | **no** | refused — org spend limit |
| `/opt/homebrew/bin/codex-acp` | yes | protocol 1, OK | OK | **yes** | **usable** |
| `claude-code-acp`, `cursor-agent`, `cursor-agent-acp`, `goose` | no | — | — | — | absent |

### claude-agent-acp — refused, twice

Handshake and session creation both succeed; the refusal is at prompt time:

```
{"code":-32603,"data":{"errorKind":"rate_limit"},
 "message":"Internal error: You've hit your org's monthly spend limit ..."}
```

Probed twice, roughly an hour apart, with the same result — a standing account condition, not a
transient blip. An earlier probe in this same lane *did* return `PONG` with `stopReason: end_turn`,
so the adapter itself is sound; the credit ceiling is what closed. This adapter is therefore **not**
the harness the gate depends on.

### codex-acp — usable

`agentInfo: @agentclientprotocol/codex-acp 1.1.7`. Session offers `gpt-5.6-sol` at low/medium
reasoning. The prompt returned:

```
stopReason: end_turn
usage: {inputTokens: 3463, cachedReadTokens: 11264, outputTokens: 6, totalTokens: 14733}
```

Non-zero output tokens against a real quota — a model produced that text.

## The maxplayer local driver path

The harness is not spoken to directly by the gate. It is driven **through the actual maxplayer local
driver**, built from this very commit:

```
maxplayer 0.5.8 (e72e9bd7114fe86412b62ff68a08c28c5602d396)
```

The stock `/opt/homebrew/bin/maxplayer` (0.1.0-rc.3) refuses this path outright —
`maxplayer run requires rebuilding with the acp feature` — so the binary is built from source:

```
cargo build -p maxplayer --features acp --release
```

Proof the driver reaches a model and the model acts on the filesystem:

```
maxplayer run --agent-command /opt/homebrew/bin/codex-acp \
  --task "Create a file named hello.txt ... containing exactly the word PONG." \
  --cwd <tmp> --log <tmp>/events.jsonl --job-id probe-codex-1 \
  --permission-policy allow --idle-timeout 240
```

- exit `0`
- `hello.txt` on disk containing `PONG` — written by the model, not by this repository
- 18 events in the JSONL log, ending `{"type":"turn_ended","data":"completed"}`

No relay, no wallet, no sats, no deployment: `maxplayer run` is the local driver path only.

## What this means for the correction

The deterministic reviewer that drew the verdict is retained, but **demoted to tooling** — it
inspects, it does not design. The designer is now the model harness above, reached through
`maxplayer run`. Substituting deterministic execution here is precisely the failure being
corrected, so it is not done.
