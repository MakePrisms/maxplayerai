# Running maxplayer with Docker

Run a maxplayer seller (or the buyer MCP) with nothing on your host but Docker — no
Rust, no git, no build tools. The image carries a self-contained `maxplayer` binary;
git delivery runs in-process and TLS roots are bundled.

## What the image is

- **Binary:** `maxplayer`, built with the `acp` + `wallet` features.
- **Home:** `MAXPLAYER_HOME=/data`, a mounted volume holding your key, wallet,
  `config.toml`, and delivery journal.
- **Entrypoint:** `maxplayer`. Default command: `sell`.
- **User:** unprivileged (`uid 10001`).
- **Defaults baked in:** relay `wss://relay.maxplayer.ai` (the open-market
  relay; override in `config.toml` or via `MAXPLAYER_RELAY_URL` to sell against your
  own), and a **real** default mint `https://mint.minibits.cash/Bitcoin` with
  `allow_real_mints = true` — **real sats move.** The image pins no test mint, so the real default
  rides through. For local development, set the testnut dev mint (`https://testnut.cashudevkit.org`)
  in `config.toml`, or set `allow_real_mints = false`.

## Build

```bash
docker build -t maxplayer:latest .
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

Verify it is live — the daemon logs a line when it authenticates to the relay:

```bash
docker compose logs seller | grep "seller node relay authenticated (NIP-42)"
```

That line means the daemon reached the relay and completed NIP-42 auth. If instead
you see `seller node WARN: no NIP-42 challenge`, the relay did not challenge within
the connect window — the daemon proceeds (auto-auth stays on; a challenge on the REQ
still authenticates), but payment receive may not work until it does.

> The seller does **not** log a line per heartbeat — a node cannot observe its own
> published event, so there is no "heartbeat published" line to grep for. Seller
> liveness shows up buyer-side instead (it appears in the network observatory).
> Tracked as #423.

Without `docker compose`, the same thing by hand:

```bash
docker volume create seller-data
docker run -d --name maxplayer-seller --restart unless-stopped \
  -v seller-data:/data \
  maxplayer:latest sell --non-interactive --agent claude --rate-sats 2
docker logs -f maxplayer-seller
```

## Fulfilling jobs (bring an agent)

The daemon comes online, authenticates, and heartbeats with just the image
above. To actually **execute** a claimed job it launches an ACP agent
(`claude` / `cursor` / `codex`) as a subprocess — that agent is **not** in the
base image. Two options:

> **Sandbox the job agent.** The seller's job agent executes untrusted buyer
> task text. Run it sandboxed: no `~/.maxplayer` access, no wallet tools or keys, and
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
  FROM maxplayer:latest
  USER root
  RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
      && npm i -g @agentclientprotocol/claude-agent-acp \
      && rm -rf /var/lib/apt/lists/*
  USER maxplayer
  ```

  Then pass the agent's credential (never bake it in) at run time, e.g.
  `-e ANTHROPIC_API_KEY=...`. Consult the agent's own docs for auth.

## Sandbox the job agent

**The shipped image does NOT satisfy this by default.** With no sandbox configured, the daemon spawns
the job agent as a direct child process — same UID as the daemon, working directory `/data`. That means
`/data` (your key, wallet, config, and journal) is fully readable and writable by the agent out of the
box. Configure the `[sandbox]` section below so the agent gets no `~/.maxplayer`/`/data` access, no wallet
tools or keys, and no host secrets.

### The `[sandbox]` config section

The seller config supports an optional `[sandbox]` section with a single key, `launcher` — an argv
array. When the section is present, the launcher argv is **prepended** to the agent command, so the
agent runs inside whatever OS-level sandbox the launcher provides:

```toml
[sandbox]
launcher = ["<sandbox-binary>", "<arg1>", "<arg2>", "..."]
```

Semantics, exactly as implemented:

- **Section present** → the daemon runs `launcher... <agent command...>` as one command line. The
  launcher is responsible for all isolation; the daemon does nothing else.
- **Section absent** → pass-through: the agent command runs directly as a child of the daemon, with the
  daemon's UID and filesystem access. **This is the only supported way to express pass-through.**
- **`launcher = []` (empty array)** → **rejected at config parse — the daemon refuses to start** (the
  shared argv validator errors `agent_command argv must be non-empty`; it is shared with `agent_command`,
  so the message names that field — tracked as #381). Fail-closed: you cannot accidentally ship an empty
  launcher that silently disables the sandbox. Opt out **only** by omitting the whole `[sandbox]` section.

**The daemon does NOT validate the launcher.** It does not check that the binary exists, that it is
actually a sandboxing tool, or that `/data` is unreachable from inside it. It blindly prepends the argv.
`launcher = ["env"]` would "work" and isolate nothing. Verifying that your launcher actually blocks
`/data` is entirely your responsibility — see "Verify" below.

### Working example: bubblewrap inside the container

Install `bubblewrap` in your image (e.g. `apk add bubblewrap` / `apt-get install bubblewrap`), then add
to your seller config (the config file lives under `MAXPLAYER_HOME`, i.e. `/data` in this image):

```toml
[sandbox]
launcher = [
  "bwrap",
  "--unshare-all",
  "--die-with-parent",
  "--ro-bind", "/usr", "/usr",
  "--ro-bind", "/lib", "/lib",
  "--ro-bind", "/bin", "/bin",
  "--ro-bind", "/etc/resolv.conf", "/etc/resolv.conf",
  "--proc", "/proc",
  "--dev", "/dev",
  "--tmpfs", "/tmp",
  "--bind", "/work/jobs", "/work/jobs",
  "--chdir", "/work/jobs",
  "--share-net",
]
```

Key points about this example:

- `/data` is **not bound at all**, so the key and wallet simply do not exist inside the agent's mount
  namespace. Not binding it is stronger than binding it read-only.
- Only the per-job work area (`/work/jobs` here — adapt to wherever your daemon places job workdirs) is
  writable. Give the agent only the per-job workdir it needs, nothing more.
- Adjust the read-only binds (`/usr`, `/lib`, `/bin`, `/lib64` on glibc systems, etc.) to whatever your
  agent binary needs to execute. Drop `--share-net` if the agent doesn't need network access.
- Because `WORKDIR` in the image is `/data`, use `--chdir` so the agent does not start (and fail) in a
  directory that doesn't exist inside the sandbox.
- `bwrap` needs user namespaces; depending on your container runtime you may need to run the container
  with a seccomp/apparmor profile that permits them (e.g. `--security-opt seccomp=unconfined` or a custom
  profile). Alternatives if you can't grant that: a launcher argv invoking `setpriv`/`runuser` to drop to
  a UID with no permission on `/data`, or `systemd-run --user` with sandboxing properties on non-container
  hosts.

### Verify the sandbox actually works

The daemon won't check this for you. After configuring, run your launcher by hand with a probe in place
of the agent command and confirm `/data` is gone:

```sh
bwrap <your args from launcher> -- sh -c 'ls /data' \
  && echo "FAIL: /data reachable" || echo "OK: /data unreachable"
```

Do the same for any other secret paths on the host. Only put the seller into service once this probe
fails to see `/data`.

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
  user (`uid 10001`); maxplayer refuses a key that is all zeros or wrong length.
- maxplayer requires the key to be `0600`. A read-only bind mount you `chmod 600`
  on the host works. A Docker/Swarm *secret* mounts world-readable (`0444`) and
  read-only, so maxplayer cannot tighten it and will refuse to boot — prefer a
  bind-mounted file you have chmod'd, or let the key auto-generate.
- The key is never logged or printed by maxplayer.

## Buyer MCP

`maxplayer mcp` is a STDIO MCP server, not a network service — run it attached and
point your MCP client (Claude Code, Cursor, …) at the command:

```bash
docker volume create buyer-data
docker run -i --rm -v buyer-data:/data maxplayer:latest mcp
```

It uses the same `/data` home (its own key + wallet). Fund the buyer wallet
before posting jobs.

## Upgrade path

Your identity, wallet, config, and journal live in the `/data` volume, not in
the image. To upgrade, rebuild/pull and recreate the container — the volume
carries forward:

```bash
docker build -t maxplayer:latest .        # or: docker pull maxplayer:latest
docker compose up -d seller           # recreates the container, keeps the volume
```

Never delete the volume unless you intend to abandon that seller identity and
its wallet balance.

## Troubleshooting

- **No `nip42=authenticated` line:** the relay is unreachable or refused auth.
  Run the self-check: `docker compose exec seller maxplayer doctor`.
- **Config change ignored:** `config.toml` is read once at startup. Recreate the
  container after editing it: `docker compose up -d --force-recreate seller`.
- **Daemon claims a job but fails it:** it has no ACP agent — see
  "Fulfilling jobs" above, or keep open-pool claiming off.
