# Claude Code channel adapter

Serve Maxplayer jobs from a Claude Code session you keep running. `maxplayer-channel.mjs` has two
modes:

- `serve`: a [Claude Code channel](https://code.claude.com/docs/en/channels-reference), an MCP server
  that Claude Code starts over stdio. It listens on a unix socket
  (`$MAXPLAYER_CHANNEL_DIR/channel.sock`, default `~/.maxplayer-channel/`, mode 0600) and pushes each
  job into the session as a `<channel source="maxplayer" job_id=… turn=… invocation=…>` event. Claude
  answers with the `answer(invocation, status, text)` tool. Each answer is matched to exactly one
  waiting turn; a late or duplicate answer is refused.
- `push`: the hook the seller daemon runs. It hands the job to the socket and blocks until Claude
  answers, then prints the answer object. With no session attached, `ping` fails (the seat stops
  taking jobs) and a job fails.

Channels are a Claude Code research preview. Needs Node 18+; no packages.

## Setup

1. Register the server with Claude Code as `maxplayer`, in a directory you will run the session
   from (`.mcp.json`):

   ```json
   { "mcpServers": { "maxplayer": { "command": "node", "args": ["/abs/path/to/maxplayer-channel.mjs", "serve"], "alwaysLoad": true } } }
   ```

   Keep the name `maxplayer`: each job tells Claude to answer with `mcp__maxplayer__answer`.
   `alwaysLoad` keeps that tool loaded (see "Deferred tools" below).

2. Start the session with the channel loaded, and keep it running (tmux, screen):

   ```bash
   claude --dangerously-load-development-channels server:maxplayer
   ```

   Give the session no more tools than the work needs: every job is a stranger's text.

3. Point the seller at `push` (`config.toml` of the seller home):

   ```toml
   [agents.claude-channel]
   hook = ["/usr/bin/node", "/abs/path/to/maxplayer-channel.mjs", "push"]

   [seller]
   agents = ["claude-channel"]
   ```

   If the seller runs as another user or with another `HOME`, set the same `MAXPLAYER_CHANNEL_DIR`
   for both (e.g. `hook = ["/usr/bin/env", "MAXPLAYER_CHANNEL_DIR=/srv/mx-channel", "/usr/bin/node", …]`).

4. Test with the session running: `maxplayer seller hook test claude-channel`. Answer the three test
   jobs in the session when they arrive (two turns of a session, then a single-turn job).

## Deferred tools

Claude Code defers MCP tool definitions by default (tool search): the session sees only tool names
until it loads one. A session that has not loaded `answer` tends to reply in its chat instead. That
reply never reaches the buyer, nothing fails, and the job waits until its deadline. Three guards:

- The `answer` tool asks to load upfront (`_meta "anthropic/alwaysLoad"`, honoured by recent Claude
  Code), and `"alwaysLoad": true` in `.mcp.json` does the same from your side.
- Every job starts with a line naming `mcp__maxplayer__answer` and the invocation, saying a chat
  reply is not delivered and to load the tool with ToolSearch if needed.
- A job with no answer after `MAXPLAYER_CHANNEL_REMIND_SECS` (default 300, `0` = off) gets one
  reminder event in the session, and `serve` notes it on its stderr (Claude Code's debug log).

`maxplayer seller hook test` goes through the real session, so it catches a session that answers in
its chat: that turn fails at the cutoff.

One session answers jobs one after another. Jobs that arrive while Claude is busy queue in the
session; each must still be answered before its deadline, or the daemon gives up on it.
