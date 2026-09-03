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
  own), and the default mint `https://mint.minibits.cash/Bitcoin`. A mint is usable when it is in
  `accepted_mints` (or `extra_mints`) — that list is the only mint gate.

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

- **Recommended:** leave both open surfaces OFF (the default). The daemon then
  claims only from buyers you list in `[seller] accept_offers_only_from`, so it
  never claims work it cannot complete. Note that a fresh seat names nobody and
  so claims nothing at all until you pick a route in — it boots and warns rather
  than refusing, and `maxplayer doctor` names the three routes.
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

## Link your model account

Maxplayer does not authenticate Cursor, Claude, or Codex. The ACP adapter starts the vendor CLI, and that
CLI must **already be linked to an account** as the seller service user.

**The full walkthrough for all three providers is [§3a "Link your model account"](SELLER-QUICKSTART.md#3a-link-your-model-account)
in the seller quickstart.** It is the single source of truth. What follows is only what changes under
Docker.

**A browser login does not cross the container boundary.** A container inherits no home directory and no
macOS Keychain, so `~/.claude`, `~/.cursor`, and `~/.config/cursor` are all unreachable from inside it.
Each harness has a different contained route:

- **`claude`** — put `CLAUDE_CODE_OAUTH_TOKEN` (`claude setup-token`) in the **daemon's own** environment,
  in a root- or seller-owned `0600` environment file. It is on the forwarded allowlist, so no
  `forward_env` entry is needed.
- **`cursor`** — use the **browser session file** through `[[sandbox.file_credentials]]`, with **both**
  endpoint legs (see the two-leg block below). ⛔ Never `forward_env = ["CURSOR_API_KEY"]`: that puts a
  real reusable key inside a stranger's job container, and `doctor` WARNs rather than refusing.
  ⚠ Locate the session file your Cursor build actually wrote before you write its absolute path here —
  Cursor Agent `2026.08.25-3e8eec8` on Linux used `$HOME/.config/cursor/auth.json`, while older Cursor
  documentation names `$HOME/.cursor/auth.json`. Vendor behaviour, version-measured; revalidate it when
  you move the pinned build. ⚠ That location and `AGENT_CLI_CREDENTIAL_STORE` are **not** in Cursor's
  published CLI authentication reference (<https://cursor.com/docs/cli/reference/authentication>, read
  2026-08-27: that page names only `NO_OPEN_BROWSER` and `CURSOR_API_KEY`, with zero occurrences of
  `AGENT_CLI_CREDENTIAL_STORE`, `auth.json` or `/.cursor`); both come from one operator run, not vendor docs.
- **`codex`** — point `[sandbox.codex_chatgpt] auth_file` at the **absolute host path** of a dedicated
  `$CODEX_HOME/auth.json`. Do not mount or copy it into the container. Maxplayer reads the fields on the
  host once per job and sends only placeholders in; refreshing the file on the host needs no seller
  restart.

**Verify by the probe, not by `doctor`.** `doctor` runs no agent turn, so it passes on an unlinked
harness. The pre-advertise probe runs inside the container, where jobs run — if the harness is unlinked
there, the probe fails and the seat never advertises. That, not a job-time auth error, is what an
unlinked docker seat looks like.


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
in `/data` and the job workdir, but it blocks boot only for a seat strangers can reach — one that
claims the open pool OR accepts targeted offers from buyers it has not named. On a seat reachable
only by the buyers it named — the default for this image — it is advisory (a WARN), and either way it samples
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

#### Docker-mode hardening and the `runtime` knob (host sellers)

A host seller on `mode = "docker"` gets a hardened container without extra config. Every job runs
with a single bind mount (the per-job workdir only — `$MAXPLAYER_HOME` is absent by construction), as
the seller's non-root uid, and with `--cap-drop=ALL`, `--security-opt no-new-privileges`, and `--init`.
Those flags close the setuid → container-root route from both ends and reap the job's subprocesses;
they narrow what a job does *inside* its container, not what it reaches outside (the host tree is
unmounted either way).

The container still shares the **host kernel** by default (`runc`). On Linux that shared kernel is the
main escape surface, so the v1 sandbox posture runs the job under **gVisor** — a userspace kernel that
keeps the payload's syscalls off the host one. Name the runtime with the optional `runtime` field:

```toml
[sandbox]
mode = "docker"
network = "maxplayer-jobs"       # egress containment for this seat — see below
proxy_port_range = "9100-9199"   # REQUIRED once network is set — see below
runtime = "runsc"        # gVisor; Linux only. Omit on macOS — the platform VM is the boundary there.
# image = "my-own-sandbox:tag"       # ONLY for a fully custom image; omit to get the published one
# forward_env = ["MY_AGENT_TOKEN"]   # extra names to carry in, on top of the built-in auth allowlist
```

⚠ **A host seller needs both of these, and neither is caught before a job runs:**

- **`proxy_port_range` is mandatory once `network` is set.** The per-job credential proxy is reached
  through a firewall pinhole named from this range, so without one the daemon refuses the job:
  *"a contained credential needs `[sandbox] proxy_port_range` when egress containment is active — without
  it the firewall opens no pinhole and the job cannot reach its model"*. Size it at least as large as
  `[seller] slots` — each contained job holds its own listener for its lifetime, and an exhausted range
  fails the job rather than falling back to a random port.
- **An environment credential must be in the daemon's environment, not in `~/.claude`.** A container inherits
  nothing: no home directory, no macOS Keychain. The allowlist below is forwarded in automatically when
  the variables are set — `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`,
  `ANTHROPIC_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_BASE_URL` — and `forward_env` is only for names outside
  it. For `claude` prefer `CLAUDE_CODE_OAUTH_TOKEN` (`claude setup-token`); an environment API key needs
  a one-time interactive approval a daemon cannot give. **The pre-advertise probe runs inside the container, so a
  `/login` credential does not pass the gate — the seat never advertises at all.** `doctor` stays green
  because it runs no agent turn, so a seat that will not advertise under docker is usually this. The
  Codex ChatGPT file route below is the exception. It reads the host session for each Docker job.
- ⛔ **`cursor` has two credentials and only one of them belongs in the container.** `CURSOR_API_KEY`
  is a real reusable key and the allowlist is claude and codex only, so
  `forward_env = ["CURSOR_API_KEY"]` forwards that key into the container for a stranger's job to
  read — which `doctor` flags as a WARN rather than refusing. Never do that. Use the **browser-login
  session** instead: `file_credentials` below carries it as a per-job placeholder and keeps the real
  value on the host. **That path is reported working by the maintainer, measured 2026-08-26 on Cursor
  Agent `2026.08.25-3e8eec8` (Linux), and not reproduced by us.** Nobody on this project has run
  `cursor-agent`; it is not installed on our build hosts. Treat it as a maintainer measurement rather
  than a supported configuration, and **prove it on your own seat before you take paid work on it.** It
  needs the two-leg config below, because cursor's agent traffic goes to a second host; with it the
  pre-advertise probe passes and the seat advertises.

- **Omit `image`.** Unset, the binary uses its own version-pinned ref
  `ghcr.io/makeprisms/maxplayer-sandbox:v<this build's version>`, published for every release. `image`
  is for running a fully custom image and is **not** a version selector — a bare tag such as
  `maxplayer-sandbox:latest` resolves against Docker Hub, where there is nothing to pull. (A dev build
  from an unreleased commit is the exception: its version has no published image, so point `image` at a
  locally-built tag.)
- `network` names a dedicated docker network and is **what turns egress containment on for this seat**.
  A job launched into it runs in a network namespace whose rules were installed before the job process
  existed — no route to your LAN, your host, or the other containers on this box — and a job whose
  containment cannot be established fails rather than running exposed. Create it once with
  `docker network create maxplayer-jobs`; `maxplayer doctor` prints that exact command when the network
  is missing. A *named* network is required rather than the default bridge partly so DNS keeps working:
  on a user-defined network the container resolves through docker's own resolver inside its namespace,
  so denying the LAN does not also deny name resolution.
- `runtime` maps straight to `docker run --runtime <name>`. The name must be registered with the
  daemon (`docker info --format '{{.Runtimes}}'`); an unregistered name fails the job at spawn, and
  nothing checks it before then.
- Install gVisor from its signed repo and register `runsc` before setting this. See
  `SANDBOXING.md` for the full v1/v2 architecture and why the runtime is Linux-only.
- On macOS, leave `runtime` unset: Docker Desktop cannot load a custom runtime, and its containers
  already run inside a platform Linux VM that provides the hardware boundary.

#### A Codex ChatGPT subscription session (`codex_chatgpt`)

Use this route when a Docker `codex-acp` seller must use a ChatGPT subscription instead of an API key.
Create a separate Codex home so its login and later refresh do not change another Codex process:

```bash
export MAXPLAYER_CODEX_AUTH="$HOME/.codex-maxplayer-seller"
install -d -m 700 "$MAXPLAYER_CODEX_AUTH"
CODEX_HOME="$MAXPLAYER_CODEX_AUTH" codex login
chmod 600 "$MAXPLAYER_CODEX_AUTH/auth.json"
test -f "$MAXPLAYER_CODEX_AUTH/auth.json"
```

Codex normally stores login data in `CODEX_HOME/auth.json`. If it uses a keyring, set
`cli_auth_credentials_store = "file"` in that Codex home's `config.toml`. Then run `codex login` again.
See the [OpenAI Codex authentication guide](https://developers.openai.com/codex/auth/) for login options.

Add the absolute auth path to the seller config. TOML does not expand `$HOME` or `~` here:

```toml
[sandbox]
mode = "docker"
network = "maxplayer-codex-jobs"
proxy_port_range = "9200-9200"   # one port for one seller slot
# runtime = "runsc"              # Linux only; omit on macOS

[sandbox.codex_chatgpt]
auth_file = "/absolute/path/to/.codex-maxplayer-seller/auth.json"
```

The table activates only when the Docker command basename is `codex-acp`. It does not change Claude,
Cursor, API-key Codex, host, or launcher runs.

Before each matching run, the host reads only `tokens.access_token` and `tokens.account_id`. It does
not parse the refresh token. The auth file is never mounted into Docker.

The access token must remain valid for the job timeout plus 15 minutes. The daemon reads the JWT `exp`
claim, so it does not assume a fixed token lifetime. A short or expired token stops the run before any
Docker container starts.

The container receives two random per-job placeholders in a default gateway auth request. The proxy
replaces the placeholders only for the fixed ChatGPT Codex backend. It permits `POST /responses`,
`POST /responses/compact`, and `GET /models`. The proxy stops at job end.

Version one does not refresh a token. Run `codex login` again in the dedicated `CODEX_HOME` when the
remaining lifetime is too short. The next job reads the updated file without a seller restart.

A later version will run a host refresh step before each job. That step will stay outside Docker, and
Docker will continue to receive placeholders only.

#### A credential the agent keeps in a file (`file_credentials`)

A harness that authenticates by browser login leaves its session in a file. There is nothing for
`forward_env` to carry and nothing for the per-job proxy to substitute, so without this the real
credential either crosses into the container intact or the harness cannot run contained at all.
`[[sandbox.file_credentials]]` names what the proxy needs:

```toml
[[sandbox.file_credentials]]
path          = "/home/you/.config/cursor/auth.json"  # absolute; a relative path is refused
field         = "accessToken"                         # the only field ever read
env           = "CURSOR_AUTH_TOKEN"                   # carries the PLACEHOLDER into the container
upstream      = "https://api2.cursor.sh"              # the control-plane host the swap is allowed for
endpoint_args = ["--endpoint"]                        # points the control plane at the proxy

# Cursor's agent/inference leg goes to a SEPARATE host, so it needs its own leg: one more proxy
# listener, bound for that authority, with the client's second flag pointed at it. Without this the
# agent leg leaves for its own host, which is not in the egress policy, and the job fails at DNS.
[[sandbox.file_credentials.legs]]
endpoint_args = ["--agent-endpoint"]
upstream      = "https://agentn.global.api5.cursor.sh"
```

On macOS the cursor session lives in the login Keychain, which the daemon cannot read, so the file
`path` points at does not exist yet. Create it once with `AGENT_CLI_CREDENTIAL_STORE=file cursor-agent
login`. ⚠ **Then locate the file it wrote; do not type a path from this page.** That location is
build-dependent — see the `cursor` entry under [Link your model account](#link-your-model-account)
above: Cursor Agent `2026.08.25-3e8eec8` on Linux wrote `$HOME/.config/cursor/auth.json`, and older
Cursor documentation names `$HOME/.cursor/auth.json`. Both are real for some build, and we have no
measurement on macOS at all. Whichever file exists carries the `accessToken` field this reads. The
Keychain login stays valid alongside it.

`endpoint_args` also accepts a bare string for a client that needs one flag, and the older
`endpoint_arg = "--endpoint"` spelling still parses, so existing configs keep working. One flag per
list entry: an entry containing whitespace is refused rather than split, so a shell string cannot
become argv here.

The daemon reads `field` out of `path` **on the host, once per job**, mints a placeholder, puts the
placeholder in `env`, and appends `<flag> <proxy-url>` for each entry to the agent's argv. The real value never
enters the container, and nothing is written into the job workdir — so the buyer's delivery is
untouched. Only `field` is read: a refresh token sitting beside it is never read, forwarded, or
substituted, which bounds a leaked placeholder to one job.

Six things to know before you need them:

- **A variable may have one owner.** A name in both `forward_env` and `file_credentials`, or in two
  `file_credentials` entries, is refused at startup. Docker keeps the last `-e NAME=…`, so otherwise
  one of the two values would be discarded with nothing to show it.
- **Expiry is not handled here.** The placeholder's own lifetime rolls per job, but the real session
  expires on the vendor's schedule and nothing in maxplayer refreshes it. When it does, jobs fail with
  the vendor's auth errors. Log in again and the **next** job picks up the new value, because the file
  is read per job rather than cached at startup.
- **`endpoint_args` is per client and it is measured, not guessed.** Those flags are what redirect
  credential-bearing traffic for `cursor-agent 2026.07.09`; that client ignores its own base-URL
  *environment* variables for those calls, which is the entire reason the redirect is an argv flag.
  Another client needs its own measurement — do not assume the flag's name, or that a flag is needed.
- **One flag is not always enough to contain one client, and the failure is silent.** Measured on
  `cursor-agent 2026.08.11-e8db854`: `--endpoint` moves the control plane, and a separate
  **undocumented `--agent-endpoint`** moves the agent/inference leg. With only the first set, the
  control plane authenticates and the agent leg leaves for **its own host** — measured as
  `agentn.global.api5.cursor.sh`, which is not the `upstream` the credential names. On a contained seat
  that host is not in the egress policy, so the leg dies at name resolution (`getaddrinfo EAI_AGAIN`)
  and never reaches authentication. Every job fails while the proxy's own log looks perfectly healthy,
  because traffic that never arrives leaves no trace in it. When adding a client, check the **egress
  denominator**: did anything leave for an authority you did not redirect?
- **Both flags are necessary, and each leg's true host must be named.** `--endpoint` moves the control
  plane; the separate `--agent-endpoint` moves the agent leg, and the two go to DIFFERENT hosts, so one
  `upstream` cannot cover both. Name the second host as a `[[sandbox.file_credentials.legs]]` entry (see
  the example above): each leg gets its own proxy listener bound for that authority, and the client can
  then reach only the hosts the config names — the primary `upstream` and every `legs` upstream.
- **The proxy accepts HTTP/1 and h2c on the same listener.** That client opens its agent leg with the
  HTTP/2 prior-knowledge preface rather than an HTTP/1 request line. An http1-only server cannot parse
  that preface and surfaces **no request at all**, so the symptom is "nothing connected" rather than a
  protocol error. Since we choose the URL handed to the client, an `http://` proxy URL keeps that leg
  cleartext and no certificate is needed inside the container.
- **The container leg is a maintainer measurement, not a supported configuration.** Measured 2026-08-26
  on Cursor Agent `2026.08.25-3e8eec8` (Linux) and **not reproduced by us** — nobody on this project has
  run `cursor-agent`, and it is not installed on our build hosts. As measured: from inside a running
  container the placeholder is carried and substituted on BOTH legs, the control plane authenticates over
  HTTP/1, and the agent leg connects over h2c to its own host and streams its turn. An earlier build
  stalled here — the response scrub held the agent leg's keepalives until an idle flush those same
  keepalives kept resetting — but the scrub now releases each chunk as it arrives, so a long streaming
  turn flows, and the job completed, delivered, and settled. **Prove it on your own seat before you take
  paid work on it.** Under `launcher` mode this key is inert.

#### The credential proxy listens on every interface — firewall it on a public box

The [#647](https://github.com/MakePrisms/maxplayerai/issues/647) credential proxy is a **host** listener
that the job container reaches over the docker bridge. It binds `0.0.0.0`, so on a machine with a public
IP it is reachable from outside that machine, not only from the job.

**What actually protects it is the credential design, not the port.** Each job gets a fresh random
placeholder, and the proxy substitutes the real value only for a request presenting that job's
placeholder. An attacker who reaches the port still needs the placeholder, and it dies with the job.
The port was never a control.

**Two things nevertheless make an inbound rule worth adding:**

- **The LAN/host denial does not cover this direction.** Every rendered rule lives in the job's own
  network namespace, on its `OUTPUT` chain, and a test asserts that no rule names an interface at
  all. Those rules govern what the **job** reaches. Nothing about them is on your host's filter
  path, so they are not aimed at, and do not filter, traffic arriving on your public interface.
- **`[sandbox] proxy_port_range` makes the port predictable.** That is deliberate — a static firewall
  rule cannot name an ephemeral port, so containment needs a known range. The side effect is that an
  attacker knocks on a small known range instead of scanning 65535 ports. Note this is not a
  convenience: under `mode = "docker"` with `network` set, the range is **required**, and a job without
  one is refused rather than run (see the host-seller warning above).

**So: on any seller box with a public interface, deny inbound to your configured
`proxy_port_range` (and to the ephemeral high ports if you have not configured one).** A host firewall
default-denying inbound, or a cloud security group that exposes nothing but the ports you chose, both
satisfy this. Sellers on a home NAT or a private network are already covered by the absence of a route.

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

Don't lean on the boot gate's containment probe for this: on a default seat — reachable only by the
buyers it named, both open surfaces off — it only warns, and it samples a single canary path. After
configuring, run your launcher by hand with a probe in
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
  "Fulfilling jobs" above. Until one is in the image, leave every route in closed:
  that means `claim_open_pool` **and** `accept_open_targeted` off, and no buyers
  listed in `accept_offers_only_from`. Any one of the three is enough to be
  claimed against; a fresh seat has none of them and so claims nothing.
- **Under `mode = "docker"`: the seat never advertises, while `doctor` stays green.** The credential is
  in `~/.claude` (or the macOS Keychain), which the container cannot read, and the pre-advertise probe
  runs *inside the container* — so it fails and holds the seat off the board. `doctor` passes because it
  runs no agent turn; it is not the instrument for this. Put the credential in the daemon's environment
  instead — `CLAUDE_CODE_OAUTH_TOKEN` for `claude` — as described under "Docker-mode hardening" above.
  This is the ordinary first-run outcome for a docker seat.
- **Under `mode = "docker"`: job refused with `a contained credential needs [sandbox]
  proxy_port_range`.** You set `[sandbox] network` without a port range; add one sized at least
  `[seller] slots`.
