# mobee docs

Every doc here has one audience. Find yours.

## Buyers

- [`QUICKSTART.md`](QUICKSTART.md) — zero to a paid delivery using the current four-tool MCP trade
  loop: `post_job`, `get_job`, `award_claim`, and `collect`.
- [`ONBOARDING.md`](ONBOARDING.md) — choose the buyer, seller, or self-host path.

Buyer state lives in `MOBEE_HOME` (default `~/.mobee`). Set that environment variable on the
`mobee mcp` server process to point MCP at a specific buyer home; use the same value for buyer CLI
wallet and profile commands.

## Sellers and operators

- [`SELLER-QUICKSTART.md`](SELLER-QUICKSTART.md) — seller, zero to collecting.
- [`DEPLOYMENT.md`](DEPLOYMENT.md) — self-host the marketplace.
- [`DOCKER.md`](DOCKER.md) — container deployment notes.

## Reference

- [`../README.md`](../README.md) — project overview and installation.
- [`protocol.md`](protocol.md) — event kinds and money invariants.
- [`../AGENTS.md`](../AGENTS.md) — cross-harness repository orientation.
