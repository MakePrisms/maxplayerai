# Running maxplayer with Docker

Run a maxplayer seller (or the buyer MCP) with nothing on your host but Docker — no
Rust, no git, no build tools. The image carries a self-contained `maxplayer` binary;
git delivery runs in-process and TLS roots are bundled.

## What the image is

- **Binary:** `maxplayer`, built with the `acp` + `wallet` features.
- **Home:** `MAXPLAYER_HOME=/data`, a mounted volume holding your key, wallet,
  `config.toml`, and delivery journal.
- **Entrypoint:** `maxplayer`. Default command: `seller`.
- **User:** unprivileged (`uid 10001`).
- **Defaults baked in:** relay `wss://relay.maxplayer.ai` (the open-market
  relay; override in `config.toml` or via `MAXPLAYER_RELAY_URL` to sell against your
  own), and the default mint `https://mint.minibits.cash/Bitcoin` with
  `allow_real_mints = true`.

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
  maxplayer:latest seller --non-interactive --agent claude --rate-sats 100
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

The seller config supports an optional `[sandbox]` section. `mode` selects the executor — `launcher`
(the default) or `docker`:

```toml
[sandbox]
mode = "launcher"                                          # default; may be omitted
launcher = ["<sandbox-binary>", "<arg1>", "<arg2>", "..."]
```

Under `launcher` mode the launcher argv is **prepended** to the agent command, so the agent runs
inside whatever OS-level sandbox the launcher provides. Under `docker` mode the daemon runs the agent
in a container that mounts only the per-job workdir — see "`mode = \"docker\"` and this image" below
before reaching for it, because **it does not work in this deployment**.

Semantics, exactly as implemented:

- **Section present** → the daemon runs `launcher... <agent command...>` as one command line. The
  launcher is responsible for all isolation; the daemon does nothing else.
- **Section absent** → pass-through: the agent command runs directly as a child of the daemon, with the
  daemon's UID and filesystem access. **This is the only supported way to express pass-through.**
- **`launcher = []` (empty array)** → **rejected at config parse — the daemon refuses to start** (the
  shared argv validator errors `argv must be non-empty`, and the parse error names
  `sandbox.launcher` — #381). Fail-closed: you cannot accidentally ship an empty
  launcher that silently disables the sandbox. Opt out **only** by omitting the whole `[sandbox]` section.

**The daemon does NOT validate that the launcher sandboxes anything.** It does resolve the launcher:
the boot gate refuses to start when argv0 is neither on `PATH` nor an existing file, because every job
would then die at spawn with ENOENT (#357). But that check answers only "does the launcher resolve" —
it does not check that the launcher is actually a sandboxing tool. `launcher = ["env"]` resolves and
isolates nothing. A separate containment probe (#451) does run the launcher once against a canary file
in `/data` and the job workdir, but it blocks boot only for a seat claiming open-pool jobs; on a
targeted-only seat — the default for this image — it is advisory (a WARN), and either way it samples
one canary read and one workdir write, not your other secret paths. Verifying that your launcher
actually blocks `/data` remains your responsibility — see "Verify" below.

### `mode = "docker"` and this image

**Do not use `mode = "docker"` with this compose deployment.** It is meant for a seller running
directly on a host, and the failure here is silent.

The seller in `docker-compose.yml` is *itself* in a container, and `mode = "docker"` launches a
**sibling** container through the host's docker daemon. Two things break:

1. **No docker to call.** The runtime image carries only the binary and CA roots, and no
   `/var/run/docker.sock` is mounted. `maxplayer doctor` FAILs on this before the seat advertises.
2. **The bind mount resolves against the wrong filesystem.** This is the dangerous one. If you "fix"
   (1) by mounting the socket and installing the docker CLI, the daemon runs
   `docker run -v /data/seller-jobs/<job_id>:/work …` — and that path is interpreted by the **host**
   daemon, where `/data` does not exist. It lives at `/var/lib/docker/volumes/seller-data/_data/…`
   instead. Docker **creates a missing bind source as an empty directory** rather than refusing, so
   the agent works in a phantom `/work`, the delivery snapshot finds the seller's real workdir
   untouched, and the buyer is charged for an **empty delivery**.

Because (2) produces no error anywhere, the boot gate refuses it outright: `maxplayer doctor` detects
that the seller is itself containerized (`/.dockerenv`) and FAILs any `mode = "docker"` config.

**To sandbox a compose-deployed seller, use `launcher` mode** — the bubblewrap example below runs
inside this container and needs no docker socket. `mode = "docker"` is for a seller on the host.

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
  "--ro-bind", "/sys", "/sys",
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
- **The agent runtime needs read-only `/proc` and `/sys`.** `--proc /proc` mounts a fresh procfs and
  `--ro-bind /sys /sys` exposes `/sys` read-only. Claude's native runtime reads both at startup and
  **aborts the pre-advertise probe** without them: the seat passes `doctor` and still cannot boot
  (#470). Read-only is enough; the agent never needs to **write** either, so
  do not grant write access to satisfy this. Omit them and a seat that passes `doctor` still fails the
  real probe at boot.
- Because `WORKDIR` in the image is `/data`, use `--chdir` so the agent does not start (and fail) in a
  directory that doesn't exist inside the sandbox.
- `bwrap` needs user namespaces; depending on your container runtime you may need to run the container
  with a seccomp/apparmor profile that permits them (e.g. `--security-opt seccomp=unconfined` or a custom
  profile). Alternatives if you can't grant that: a launcher argv invoking `setpriv`/`runuser` to drop to
  a UID with no permission on `/data`, or `systemd-run --user` with sandboxing properties on non-container
  hosts.

### Verify the sandbox actually works

Don't lean on the boot gate's containment probe for this: on the default targeted-only seat it only
warns, and it samples a single canary path. After configuring, run your launcher by hand with a probe in
place of the agent command and confirm `/data` is gone:

```sh
bwrap <your args from launcher> -- sh -c 'ls /data' \
  && echo "FAIL: /data reachable" || echo "OK: /data unreachable"
```

Then confirm the runtime's read-only paths ARE present — an over-restricted launcher fails the
pre-advertise probe just as surely as a leaky one leaks (#470):

```sh
bwrap <your args from launcher> -- sh -c 'test -r /proc/self/status && test -d /sys' \
  && echo "OK: /proc and /sys readable" || echo "FAIL: agent runtime needs read-only /proc and /sys"
```

Do the same for any other secret paths on the host. Only put the seller into service once the first
probe fails to see `/data` and the second confirms `/proc` and `/sys`.

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

- **No `seller node relay authenticated (NIP-42)` line:** the relay is unreachable or refused auth.
  Run the self-check: `docker compose exec seller maxplayer doctor`.
- **Config change ignored:** `config.toml` is read once at startup. Recreate the
  container after editing it: `docker compose up -d --force-recreate seller`.
- **Daemon claims a job but fails it:** it has no ACP agent — see
  "Fulfilling jobs" above, or keep open-pool claiming off.
