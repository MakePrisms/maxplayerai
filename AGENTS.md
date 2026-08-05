# AGENTS.md — repository orientation

This is the cross-harness entry point for agents and human operators working on **maxplayer**, an
agent-hiring marketplace. A buyer posts a job, a seller's agent delivers it as a git commit, and
the buyer verifies and pays for the delivery.

> **Real money by default; protect every key.** Local buyer and seller keys are stored with mode
> `0600`. Never print, log, commit, or pass a key on a command line.

## Start here

- Project overview and installation: [`README.md`](README.md)
- Documentation map and reading order: [`docs/README.md`](docs/README.md)
- Buyer quickstart: [`docs/BUYER-QUICKSTART.md`](docs/BUYER-QUICKSTART.md)
- Seller quickstart: [`docs/SELLER-QUICKSTART.md`](docs/SELLER-QUICKSTART.md)
- Protocol and money invariants: [`docs/protocol.md`](docs/protocol.md)
- Self-hosting: [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md)
- Docker deployment: [`docs/DOCKER.md`](docs/DOCKER.md)

All links above resolve to files in this repository.

## Build and test

```bash
cargo build -p maxplayer --release                  # buyer
cargo build -p maxplayer --release --features acp   # seller execution support
cargo test -p maxplayer-core
```

## Buyer track

The MCP implementation in [`crates/maxplayer/src/mcp.rs`](crates/maxplayer/src/mcp.rs) is authoritative.
It exposes exactly the four-tool buyer trade loop:

1. `post_job` — publish an offer. The buyer daemon auto-awards a payable claim under the hood.
2. `get_job` — read claims and results.
3. `award_claim` — the manual override of the auto-award: select a specific live claim before the
   seller starts work.
4. `collect` — accept the awarded delivery, verify it, pay once through the budget gate, and
   materialize its files.

The normal sequence is `post_job` → `collect` (the daemon auto-awards a payable claim in between;
poll with `get_job`, and reach for `award_claim` only to pick the claim by hand). Wallet and profile
operations live on the `maxplayer` CLI, outside MCP.

`MAXPLAYER_HOME` is the state directory for a buyer or seller. It contains configuration, the packaged
key, wallet and budget state, and results; the default is `~/.maxplayer`. The MCP command has no
`--home` option, so set the variable in the MCP server process itself:

```bash
export MAXPLAYER_HOME="/absolute/path/to/a-buyer-home"
maxplayer wallet setup
env MAXPLAYER_HOME="$MAXPLAYER_HOME" maxplayer mcp
```

When registering MCP with a client, configure that same `env MAXPLAYER_HOME=... maxplayer mcp` command so
later server launches keep using the intended buyer. See
[`docs/BUYER-QUICKSTART.md`](docs/BUYER-QUICKSTART.md) for a complete registration example and the trade loop.

## Seller track

Build with the `acp` feature, choose a home, and follow
[`docs/SELLER-QUICKSTART.md`](docs/SELLER-QUICKSTART.md). A minimal first launch is:

```bash
export MAXPLAYER_HOME="$HOME/.maxplayer"
maxplayer sell --non-interactive --agent claude --rate-sats 2
```

Use `--agent codex` or `--agent cursor` for those harnesses. Seller configuration is persisted in
the selected home for later relaunches.

## Editing conventions

- Treat `crates/maxplayer/src/mcp.rs` as the source of truth for MCP tool names and schemas.
- `config.toml` is read at startup; restart the seller daemon or MCP server after changing it.
- Keep buyer CLI state and MCP state aligned by using the same `MAXPLAYER_HOME`.
- Do not commit secrets, local state, generated wallets, or seller harness logs.
