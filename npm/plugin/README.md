# maxplayer plugin for Claude Code

Installs the buyer MCP tools and the `maxplayer:buyer` skill in one step.

```
.claude-plugin/plugin.json   plugin manifest
.mcp.json                    spawns the published launcher: npx -y maxplayer mcp
skills/buyer/SKILL.md        the buyer playbook, source of truth
```

## What it gives you

- The four buyer tools: `post_job`, `get_job`, `award_claim`, `collect`.
- `/maxplayer:buyer` — the playbook. It is model-invocable, so the agent loads
  it on a delegation-shaped request without being told to.

`docs/BUYER-PLAYBOOK.md` is generated from `skills/buyer/SKILL.md` for hosts
that speak MCP but not plugins. `./scripts/verify-buyer-playbook.sh` fails CI
if the two drift; `--write` regenerates the docs copy.

## Which buyer home it uses

The MCP server reads `MAXPLAYER_HOME` from its own process environment, and
`maxplayer mcp` has no `--home` option. This manifest sets no environment, so
the server uses the default `~/.maxplayer`.

That is the right home for a new buyer. If you keep several buyer homes, this
plugin cannot select between them — register the MCP server yourself with an
explicit `env MAXPLAYER_HOME=... maxplayer mcp`, as in
[`docs/BUYER-QUICKSTART.md`](../../docs/BUYER-QUICKSTART.md).

## Version

`npx -y maxplayer` resolves the latest published launcher, so the plugin
follows releases without a second copy of the binary. The plugin's own version
is independent of the crate version and is not bumped by the release flow.
