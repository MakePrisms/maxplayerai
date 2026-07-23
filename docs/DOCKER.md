# Running mobee with Docker

Run a mobee seller (or the buyer MCP) with nothing on your host but Docker — no
Rust, no git, no build tools. The image carries a self-contained `mobee` binary;
git delivery runs in-process and TLS roots are bundled.

## What the image is

- **Binary:** `mobee`, built with the `acp` + `wallet` features.
- **Home:** `MOBEE_HOME=/data`, a mounted volume holding your key, wallet,
  `config.toml`, and delivery journal.
- **Entrypoint:** `mobee`. Default command: `sell`.
- **User:** unprivileged (`uid 10001`).
- **Defaults baked in:** relay `wss://relay.example` (set to your relay's wss
  URL), the default mint `https://testnut.cashudevkit.org` (a test mint).

## Build

```bash
docker build -t mobee:latest .
```

## Run a seller (quickstart)

```bash
docker compose up -d seller
docker compose logs -f seller
```

On first start the seller:

1. **Generates a fresh key** into the volume (`/data/key`, mode `0600`). It is
   never printed and never baked into the image.
2. **Writes `config.toml`** with the working defaults above.
3. **Comes online and authenticates** to the relay.
4. **Publishes a heartbeat** so buyers can discover it.

Verify it is live — look for these lines in the logs:

```bash
docker compose logs seller | grep "seller daemon online" | grep "nip42=authenticated"
docker compose logs seller | grep "seller heartbeat published id="
```

`nip42=authenticated` means the daemon reached the relay and authenticated;
`no-challenge` is a warning state (payment receive may not work).

Without `docker compose`, the same thing by hand:

```bash
docker volume create seller-data
docker run -d --name mobee-seller --restart unless-stopped \
  -v seller-data:/data \
  mobee:latest sell --non-interactive --agent claude --rate-sats 2
docker logs -f mobee-seller
```

## Fulfilling jobs (bring an agent)

The daemon comes online, authenticates, and heartbeats with just the image
above. To actually **execute** a claimed job it launches an ACP agent
(`claude` / `cursor` / `codex`) as a subprocess — that agent is **not** in the
base image. Two options:

> **Sandbox the job agent.** The seller's job agent executes untrusted buyer
> task text. Run it sandboxed: no `~/.mobee` access, no wallet tools or keys, and
> no host secrets. The `/data` volume (key + wallet) must never be reachable from
> the agent's execution environment.

- **Recommended:** leave open-pool claiming OFF (the default). The daemon then
  claims only offers targeted at its pubkey, so it never claims work it cannot
  complete.
- **To execute claimed jobs (bring an agent):** extend the image with your chosen agent and its runtime,
  then supply the agent's own auth (e.g. an API key) via the container
  environment. Each preset requires its ACP adapter binary on `PATH` (a missing
  adapter fails with an install hint — there is no auto-download). For the
  `claude` preset, install `claude-agent-acp` into the image:

  ```dockerfile
  FROM mobee:latest
  USER root
  RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
      && npm i -g @agentclientprotocol/claude-agent-acp \
      && rm -rf /var/lib/apt/lists/*
  USER mobee
  ```

  Then pass the agent's credential (never bake it in) at run time, e.g.
  `-e ANTHROPIC_API_KEY=...`. Consult the agent's own docs for auth.

## Bring your own key

The default is fine for most operators: the key auto-generates in the volume and
persists. To run a specific identity, mount a key file instead:

```bash
mkdir -p secrets
# 64 hex chars, no newline needed beyond a trailing one; keep it 0600.
printf '%s' "$YOUR_64_HEX_SECRET" > secrets/key
chmod 600 secrets/key
```

Compose:

```yaml
    volumes:
      - seller-data:/data
      - ./secrets/key:/data/key:ro
```

Requirements and caveats:

- The file must be **64 hex characters** and owned/readable by the container
  user (`uid 10001`); mobee refuses a key that is all zeros or wrong length.
- mobee requires the key to be `0600`. A read-only bind mount you `chmod 600`
  on the host works. A Docker/Swarm *secret* mounts world-readable (`0444`) and
  read-only, so mobee cannot tighten it and will refuse to boot — prefer a
  bind-mounted file you have chmod'd, or let the key auto-generate.
- The key is never logged or printed by mobee.

## Buyer MCP

`mobee mcp` is a STDIO MCP server, not a network service — run it attached and
point your MCP client (Claude Code, Cursor, …) at the command:

```bash
docker volume create buyer-data
docker run -i --rm -v buyer-data:/data mobee:latest mcp
```

It uses the same `/data` home (its own key + wallet). Fund the buyer wallet
before posting jobs.

## Upgrade path

Your identity, wallet, config, and journal live in the `/data` volume, not in
the image. To upgrade, rebuild/pull and recreate the container — the volume
carries forward:

```bash
docker build -t mobee:latest .        # or: docker pull mobee:latest
docker compose up -d seller           # recreates the container, keeps the volume
```

Never delete the volume unless you intend to abandon that seller identity and
its wallet balance.

## Troubleshooting

- **No `nip42=authenticated` line:** the relay is unreachable or refused auth.
  Run the self-check: `docker compose exec seller mobee doctor`.
- **Config change ignored:** `config.toml` is read once at startup. Recreate the
  container after editing it: `docker compose up -d --force-recreate seller`.
- **Daemon claims a job but fails it:** it has no ACP agent — see
  "Fulfilling jobs" above, or keep open-pool claiming off.
