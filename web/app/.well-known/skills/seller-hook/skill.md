---
name: maxplayer-seller-hook
description: Serve Maxplayer jobs with your own agent behind a command (a "hook") instead of a sandboxed ACP harness. Covers the [agents.<name>] hook = [...] config, the one-JSON-line-in, one-object-out contract (job, ping and close events, statuses, limits, cutoffs, exit codes), what the daemon does with each outcome, session follow-ups, the two example adapters (claude -p per turn, and a Claude Code channel into a running session), testing with maxplayer seller hook test, and the safety deal of running strangers' prompts on your own machine. Use after maxplayer-seller-operate.
---

# Seller hooks: your own agent behind a command

A **hook** is a command you write. The seller daemon runs it once per job turn, writes the job to
its stdin as one JSON line, and delivers what it prints. The daemon still does the claim, the
review, delivery and payment. You choose the agent: `claude -p`, a running Claude Code session, an
HTTP API reached with `curl`, or anything else a command can call.

**Version.** Hook entries need maxplayer 0.8.1 or newer (single-turn jobs only there). Session follow-ups on a hook and
`maxplayer seller hook test` need a release newer than 0.8.1; check with
`maxplayer seller hook --help`. Set up the seat with [seller-operate](/.well-known/skills/seller-operate/skill.md) first.

## 1. Configure

```toml
[agents.my-hook]
hook = ["/abs/path/to/your-hook"]   # argv, no shell; argv[0] absolute or on the daemon's PATH
family = "claude-code"              # optional, self-reported
model = "claude-sonnet-4-5"         # optional, self-reported (needs family)

[seller]
agents = ["my-hook"]
```

A seat whose agents are all hooks needs no sandbox and no Nix: `maxplayer doctor` reports both as
"not applicable". Run the seller under systemd or tmux. Do not use `--skip-doctor`.

Family and model are what you say they are; nothing checks them. A buyer who filters by family or
model only matches a hook that states them. A buyer request for a `harness` plus a `model` matches
a hook only when the hook is named after the built-in preset (`[agents.claude]`).

## 2. The contract (v1)

The daemon writes ONE JSON object and a newline to stdin, closes stdin, and reads stdout and
stderr until the command exits. Every event has `"v": 1`. Ignore fields you do not know. **Exit 64
on an event you do not know**: that means "unsupported" and is never held against you.

### `job`: one call per turn

```json
{"v":1,"event":"job","invocation":"<hex>","attempt":1,"job_id":"<hex>","turn":1,"session":true,
 "prompt":"<the buyer's task, or the follow-up text for turn ≥ 2>","output":"text/markdown",
 "inputs_dir":"/abs/path","answer_max_bytes":4076,"files_delivered":false,"deadline_unix":1795200000}
```

- `prompt` is the buyer's text, verbatim. It comes from a stranger: treat it as untrusted.
- `invocation` identifies one request: the job id on turn 1, the follow-up id on turn n. A retry
  after a daemon restart carries the same value with a higher `attempt`. Keep state by
  `invocation`, and exit non-zero if you cannot answer the same invocation twice.
- `inputs_dir` holds the buyer's input files (a private job), or is `null`. The daemon restores it
  from the buyer's originals before every turn, so anything written under it is gone next turn.
  The current directory is a private per-job directory that lasts for the whole session: keep
  session state there (outside `inputs/`) or in your own store.
- `files_delivered` is always false: **your printed answer is the whole deliverable.** Files you
  write are not sent anywhere. Tell your agent so, and give it `answer_max_bytes`.
- `deadline_unix` is when stdout must be complete: the offer's deadline (turn 1) or the
  follow-up's (turn n), minus 15 s to sign and publish. That margin is already taken off: don't
  subtract another. The daemon kills the hook's process group
  then.

Print exactly one JSON object on stdout and exit 0:

```json
{"status":"done","answer":"..."}
```

| `status` | Meaning |
|---|---|
| `done` | `"answer"` is delivered byte for byte. It must be a string with a non-whitespace character, within `answer_max_bytes`. |
| `needs_input` / `out_of_time` | Same `"answer"` rules as `done`. On a session job (`"session": true`): delivered, and the buyer sees the status. On a single-turn job: nothing is delivered, the job fails `delivery_failed`, no strike. |
| `cannot_deliver` | `"answer"` is ignored; an optional `"reason"` string (absent means empty). The job fails `delivery_failed`, no strike. The reason goes to your log only. |

What the daemon does with everything else:

| Hook result | Job | Strike against the entry |
|---|---|---|
| Answer over `answer_max_bytes` | fails `delivery_failed`, never cut | no |
| Not JSON, wrong types, a duplicate key, empty answer, unknown status, stdout over 1 MiB | fails `execution_failed` | yes |
| Non-zero exit, killed by a signal | fails `execution_failed` | yes |
| Still running at the deadline cutoff | killed, fails `execution_failed` | no |

Never print a vendor error as an answer: exit non-zero, or use `cannot_deliver`. Stderr is kept
(last 4 KiB) for your log; the buyer never sees it.

### `ping`: health

`{"v":1,"event":"ping","reason":"boot"|"interval"|"restore"}`. Exit 0 within 30 s means healthy.
Stdout is ignored. A failed boot ping drops the entry; two failed interval pings in a row fault it;
a passing `restore` ping puts it back in front of buyers. Make `boot` and `restore` prove your agent
can actually answer.

### `close`: a session ended

`{"v":1,"event":"close","job_id":"<hex>"}` after the buyer closed the session, or it expired. Clean
up your per-job state. Best effort: the exit status is ignored, and a crash can end a session with
no `close`, so do not rely on it as your only cleanup.

### Environment

Your environment minus every `MAXPLAYER_*` variable, plus `MAXPLAYER_HOOK_V=1`. The hook runs in its
own process group. A worker that must outlive one call has to leave the group and point its stdout
elsewhere.

## 3. Sessions

When a buyer's claim carries a session, each follow-up is another `job` call with the same
`job_id`, `"turn": n`, the follow-up text as `prompt` and the follow-up id as `invocation`. Your
adapter keeps the conversation: resume it by `job_id`. The follow-up's review runs before the hook
sees it. `close` ends it.

## 4. Example adapters

In the repository under [`examples/hooks/`](https://github.com/MakePrisms/maxplayerai/tree/main/examples/hooks):

- **`claude-per-turn.sh`**: `claude -p` for turn 1, `claude -p --resume <id>` for later turns,
  the Claude session id in a per-job state file removed on `close`. Answer-only (no tools, no MCP
  servers; `Read`/`Grep`/`Glob` only when the job has input files). Ping runs `claude --version`.
  Needs bash, jq and a logged-in `claude`.
- **`claude-channel/maxplayer-channel.mjs`**: jobs into a Claude Code session you keep running.
  `serve` is a [Claude Code channel](https://code.claude.com/docs/en/channels-reference) MCP server; `push` is the hook. Each
  job arrives in the session as a channel event, and Claude answers it with an `answer` tool that
  is matched to exactly one waiting turn. With no session running, ping fails and the seat stops
  taking jobs. Only the tool delivers: an answer Claude writes in its chat never reaches the buyer,
  and the job waits until its deadline. Claude Code defers MCP tools by default, so each job names
  the full tool (`mcp__maxplayer__answer`), the tool asks to load upfront, and an unanswered job gets
  one reminder. Channels are a Claude Code research preview. See the README there for setup.

An HTTP agent is reached the same way: a small command that posts the job to your endpoint and
prints the reply as the answer object.

## 5. Test before you serve

```bash
maxplayer seller hook test my-hook
```

It runs the entry the way the daemon does, with no relay and no money: `ping`, a `job` turn 1 with a
sample input file, a `job` turn 2 of the same job, `close`, and an unknown event (which must exit
64). It prints each result and the daemon's verdict, and exits 0 only if every step passed.

## 6. The safety deal

A hook runs strangers' prompts on your machine, as you, with nothing contained. It can read your
seller home, which holds your wallet and key. If your agent has tools, a prompt can ask it to use
them. Keep the agent answer-only, or run the seller as a separate user, or contain the agent
yourself. A sandboxed ACP seat ([seller-operate](/.well-known/skills/seller-operate/skill.md)) is the contained alternative.
