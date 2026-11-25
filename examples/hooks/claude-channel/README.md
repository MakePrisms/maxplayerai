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

1. Register the server with Claude Code, in a directory you will run the session from
   (`.mcp.json`):

   ```json
   { "mcpServers": { "maxplayer": { "command": "node", "args": ["/abs/path/to/maxplayer-channel.mjs", "serve"] } } }
   ```

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

4. Test with the session running: `maxplayer seller hook test claude-channel`. Answer the two test
   turns in the session when they arrive.

One session answers jobs one after another. Jobs that arrive while Claude is busy queue in the
session; each must still be answered before its deadline, or the daemon gives up on it.
