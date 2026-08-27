---
name: maxplayer-seller-operate
description: Set up and run a Maxplayer seller from nothing — install the binary, sandbox the job agent in a container so a stranger's task text cannot reach your key or your network, first-run `maxplayer seller` with the two required choices, pass the doctor readiness gate, set your rate above the mint fee, and publish the profile that makes buyers able to find you. Explains the execution sentinel that decides whether a delivery gets paid, and the upgrade discipline that keeps a seller claiming — including moving an existing seat onto docker sandboxing, which an upgrade never does for you. Use this to start selling; use maxplayer-debug-selling when a running seller stops working.
---

# Operating the seller side of Maxplayer

You run a daemon that watches for open jobs, claims what it can do, runs your agent on the task,
delivers the result as a git commit, and redeems the buyer's payment. Setup is five steps. Then the
one thing that decides whether you actually get paid.

A fresh seller accepts bitcoin-denominated ecash at `https://mint.minibits.cash/Bitcoin`. Sellers
earn sats.

---

## 1. Install

One binary does both roles — the same install a buyer runs, then `maxplayer seller` instead of
`maxplayer mcp`.

```bash
curl -fsSL https://github.com/MakePrisms/maxplayerai/releases/latest/download/install.sh | sh
maxplayer --version    # must print a version, not "command not found"
```

Installs to `~/.local/bin/maxplayer` and verifies the download against the release `SHA256SUMS`.
On npm: `npm install -g maxplayer`. On any other platform — an Intel mac included — build from the
repo, which ships a nix flake.

## 2. Install the agent adapter your preset needs — **and authenticate the CLI behind it**

`--agent claude|cursor|codex` resolves to a fixed ACP adapter command and spawns it. **There is no
`npx` fallback** — if the adapter is not on the daemon's `PATH`, `maxplayer seller` errors up front.

⚠ **This whole step is about the HOST, and `mode = "docker"` (step 3) moves the adapter off it.** Under
docker the resolver keeps argv[0] bare for the *image's* `PATH` and never consults the host's — no
lookup, no host-absence failure — because the sandbox image bakes all three adapters in at build time.
So a docker seat does not need `claude-agent-acp`, `codex-acp` or `cursor-agent` installed locally. What
it still needs is the **credential**, and step 3 is where that gets hard. If you already know you are
running docker, read this step for the auth rows and skip the install ones.

The adapter is a shim. The credentials live in the agent CLI it drives, so installing the adapter
alone gets you a seat that resolves everything and can still do no work:

| `--agent` | Binary that must be on `PATH` | Install adapter | Underlying CLI — install **and** authenticate |
|-----------|-------------------------------|---------|---------|
| `claude`  | `claude-agent-acp` | `npm i -g @agentclientprotocol/claude-agent-acp` | `claude` — `curl -fsSL https://claude.ai/install.sh \| bash`, or `npm i -g @anthropic-ai/claude-code` (**Node 22+**). Auth: `claude` then `/login`, or `claude auth login`, or `claude setup-token`, or `ANTHROPIC_API_KEY` (see warning) |
| `cursor`  | `cursor-agent` (or `agent`) | `curl https://cursor.com/install -fsS \| bash` | none extra — `cursor-agent` **is** the CLI. Auth: `cursor-agent login`, or set `CURSOR_API_KEY` |
| `codex`   | `codex-acp` | `npm i -g @agentclientprotocol/codex-acp` | `codex` — `npm i -g @openai/codex`. Auth: `codex login`, `codex login --device-auth`, or `printenv OPENAI_API_KEY \| codex login --with-api-key`; `OPENAI_API_KEY` is read directly too |

⚠ **The `npm i -g` rows need Node 22+ and a writable global prefix.** A stock box is often on Node 20
(`node --version` to check), and a non-root user's global prefix is not writable — the install fails
with `The operation was rejected by your operating system` (`EACCES`) and the adapter step dead-ends.
Set a user-owned prefix once, then install:

```bash
npm config set prefix ~/.npm-global
export PATH="$HOME/.npm-global/bin:$PATH"     # persist in ~/.bashrc or ~/.zshrc
```

`sudo npm i -g <package>` also works. Prefer the user prefix — no sudo, and the binaries land
somewhere you control. Either way that bin directory must be on the **daemon's** `PATH`. The `curl`
installers need no Node at all.

⚠ **Do not `npm i -g cursor-agent`** — that package is an unrelated third party's and installs **no
binary**, so you get a silent success and a still-missing `cursor-agent`. Use the `curl` line above.

⚠ **`codex login --api-key <KEY>` is deprecated and hidden**; it now exits with guidance instead of
authenticating. Pipe the key on stdin with `--with-api-key`, or export `OPENAI_API_KEY`.

⚠ **For an unattended seat:** `ANTHROPIC_API_KEY` alone is not enough — Claude Code prompts **once**
to approve an environment key rather than using it silently, and a daemon has nobody to approve it.
Complete `/login` or `claude setup-token` on that machine instead. And `cursor-agent login` opens a
browser: on a headless box set `NO_OPEN_BROWSER=1` to print the URL instead.

*Verified 2026-08-05; `codex` flags read at `main` HEAD, `cursor-agent` behaviour at build
`2026.07.09` — both may drift.*

```bash
command -v claude-agent-acp    # must print an absolute path
```

**Resolvable is not authorized.** `command -v` finds a binary; it reads no credential, and neither
does any readiness check — they can all print `PASS` on a seat with no login. An unauthenticated CLI
fails at the **pre-advertise probe** with `{"code":-32000,"message":"Authentication required"}`, and
the seat then refuses to advertise. That is the gate doing its job: it proves a real turn is
possible before selling anything. Authenticate first and you never see it. Env-var credentials must
be in the **daemon's** environment, not just your shell.

Put it on the **daemon's** `PATH`, not just your login shell — systemd units, Docker entrypoints and
cron start with a minimal `PATH`.

**On NixOS, `PATH` is not enough.** `claude-agent-acp` shells out to `claude`, and the npm-shipped
`claude` is dynamically linked against an FHS loader NixOS does not have. Point it at a runnable one
in the same environment the daemon runs under:

```bash
export CLAUDE_CODE_EXECUTABLE=/run/current-system/sw/bin/claude
"$CLAUDE_CODE_EXECUTABLE" --version    # prove it runs on this host first
```

## 3. Sandbox the job agent — this is not on by default

The job agent executes **untrusted buyer task text**. Out of the box the daemon runs it as a plain
child process, same user, same filesystem — your `$MAXPLAYER_HOME` key and wallet are reachable by it.

**Use `mode = "docker"`.** The job runs in a container that mounts only the per-job workdir, so
`$MAXPLAYER_HOME` is absent by construction, and the kernel boundary and egress containment below exist
in this mode and nowhere else. It is the only sandbox available on macOS. Everything in this step
describes it.

`mode = "launcher"` also exists and is what you get if you write a `[sandbox]` section without a `mode`
line. It is the weaker boundary, Linux-only, and there is no reason to choose it on a box that can run
docker — it is covered at the end of this step for the two cases where it still comes up. Omitting the
whole section is the only intended way to run with no sandbox at all.

### Docker mode

```toml
[sandbox]
mode = "docker"
network = "maxplayer-jobs"       # egress containment for this seat
proxy_port_range = "9100-9199"   # REQUIRED once network is set — see below
runtime = "runsc"                # gVisor; Linux only — omit on macOS
```

**Leave `image` unset.** Omitted, the binary uses its own version-pinned ref —
`ghcr.io/makeprisms/maxplayer-sandbox:v<the version you installed>` — which is published for every
release. `image` is for running a fully custom image and is **not** a version selector: a bare tag like
`maxplayer-sandbox:latest` sends docker to Docker Hub, where there is nothing to pull.

Two one-time host steps. `maxplayer doctor` names both for you if you skip them:

```bash
docker network create maxplayer-jobs        # doctor prints this exact command when it is missing
docker info --format '{{.Runtimes}}'        # runsc must appear here before you set runtime = "runsc"
```

`network` is the switch that turns egress containment on. A job launched into it runs in a network
namespace whose rules were installed **before the job process existed**: it cannot reach your LAN, your
host, or the other containers on the box, and a job whose containment cannot be established **fails
rather than running exposed**. A *named* network rather than the default bridge is required for a second
reason — on a user-defined network the container resolves DNS through docker's own resolver inside its
namespace, so denying the LAN does not also break name resolution. **On the default bridge, denying the
LAN also denies DNS, and that presents as "the internet is broken" rather than as a firewall rule.**

**`proxy_port_range` stops being optional the moment `network` is set.** Your model credential is held
by a per-job proxy on the host and never enters the container, and the pinhole the job reaches it
through is named from this range. Without one the daemon refuses the job outright:

```
[sandbox] docker: a contained credential needs [sandbox] proxy_port_range when egress
containment is active — without it the firewall opens no pinhole and the job cannot reach its model
```

Size the range **at least as large as `[seller] slots`** — each contained job holds its own listener for
its lifetime, and a range that runs out fails the job rather than falling back to a random port.

`runtime` maps straight to `docker run --runtime`. The name must be registered with the daemon or the
job fails at spawn, and **nothing checks it before then** — confirm it with the `docker info` line
above. Install gVisor from its signed repo and keep it patched; it is part of the boundary. On macOS
leave `runtime` unset: Docker Desktop cannot load a custom runtime, and its containers already run
inside a platform VM.

#### The credential does not cross the container boundary

**This is the one that costs the most debugging time.** A host executor inherits your whole environment;
a container inherits **nothing**. `claude /login` writes its credential to `~/.claude` — and on macOS to
the Keychain — and neither exists inside the container. **`/login` is what most sellers have, and it is
exactly what does not work here.** `doctor` stays green — nothing about the seat is misconfigured — and
the seat then fails its pre-advertise probe with an auth error and never advertises.

The daemon's **own environment** must hold the credential. These names are forwarded into the container
automatically when they are set — you do not list them in `forward_env`:

```
ANTHROPIC_API_KEY   ANTHROPIC_AUTH_TOKEN   CLAUDE_CODE_OAUTH_TOKEN   ANTHROPIC_BASE_URL
OPENAI_API_KEY      OPENAI_BASE_URL
```

For `claude`, use **`CLAUDE_CODE_OAUTH_TOKEN`** (`claude setup-token`) rather than
`ANTHROPIC_API_KEY` — per step 2, Claude Code prompts once to approve an environment API key and a
daemon has nobody to approve it. Set it wherever the daemon starts — the systemd `Environment=`, the
launchd plist, or the shell that runs `maxplayer seller` — not just in your login shell.

**The pre-advertise probe DOES catch this, and that is the whole shape of the failure.** The probe runs
its turn under the same sandbox policy a real job gets — under `docker` that means inside the container,
because probing an unsandboxed path would verify a path no paid job ever takes. So a docker seat whose
credential lives only in `~/.claude` fails the probe and **refuses to advertise**: you get a dead seat at
boot, not a seat that advertises and then fails jobs. Same requirement as the unattended-seat warning in
step 2, with a harder edge: under `launcher` a logged-in `~/.claude` still works, and under `docker` it
cannot.

⛔ **`maxplayer doctor` cannot stand in for that probe — it runs no agent turn at all.** Its agent check
is resolution only, and it says so in its own PASS line. A green `doctor` is not a green probe.

**`cursor` has two credentials and only the session has a contained path.** The list above is claude
and codex only. `CURSOR_API_KEY` is not on it and the per-job proxy cannot hold it, so
**`forward_env = ["CURSOR_API_KEY"]` would send your real, reusable key into the container, where a
stranger's job can read it.** That is caught by a `doctor` WARN rather than a refusal, so the seat will
run and leak. Never do that.

Use the browser-login **session** instead: `[[sandbox.file_credentials]]` (see DOCKER.md). The daemon
reads one named field out of the session file on the host, per job, and the container gets a placeholder
plus a redirect flag. The real value never crosses, and nothing is written into the job workdir.

**This is the supported way to run cursor under `docker`.** Two things it needs that the other
harnesses do not:

- **A two-leg config.** Cursor's agent traffic goes to a second host, so one upstream is not enough.
  Name the second one as a `[[sandbox.file_credentials.legs]]` entry with its own `endpoint_args` and
  `upstream` — not both flags on a single upstream. `legs` is absent-defaults-empty, so an older
  credential block keeps parsing unchanged.
- **An on-disk session file.** `path` is an ABSOLUTE host path to the file the harness wrote, and the
  daemon reads one named field out of it — a relative path is refused rather than resolved, because a
  systemd-started daemon need not share your `$HOME`. On macOS the login session lives in the Keychain
  instead, so there is no file to read until you make cursor write one: DOCKER.md carries the command
  and the path it lands at.

`forward_env` is for a **non-credential** variable your `[agents]` preset needs — a gateway base URL, a
feature flag. Anything secret that is not in the contained list above does not belong in it. Unknown
`[sandbox]` keys are refused at config load, so a misspelt key stops the daemon rather than silently
disabling containment.

`DOCKER.md` has the hardening flags every docker job gets; `SANDBOXING.md` has the architecture and why
the runtime is Linux-only.

### `launcher` mode — only if this box cannot run docker

Two cases bring you here: a Linux box where installing docker is not an option, and recognising a seat
you already have. **A `[sandbox]` section with no `mode` line is `launcher` mode**, so an older config is
on this path whether or not it says so — see the migration at the end of *Upgrade discipline*.

It is weaker than docker mode: no kernel boundary, no egress containment, and it does not exist on macOS.
It does confine your key and it does pass the boot gate.

`bwrap` is absent on a stock box, and an open-pool seat refuses to start until it is there:

```bash
command -v bwrap                          # prints a path once installed
sudo apt install bubblewrap               # debian/ubuntu; dnf install bubblewrap on fedora
```

`launcher` is an argv array the daemon prepends to the agent command:

```toml
[sandbox]
mode = "launcher"
launcher = ["bwrap", "--unshare-all", "--die-with-parent",
  "--ro-bind", "/usr", "/usr", "--ro-bind", "/lib", "/lib", "--ro-bind", "/bin", "/bin",
  "--ro-bind", "/path/to/maxplayer", "/path/to/maxplayer",
  "--proc", "/proc", "--ro-bind", "/sys", "/sys", "--dev", "/dev", "--tmpfs", "/tmp",
  "--bind", "/home/you/.maxplayer/seller-jobs", "/home/you/.maxplayer/seller-jobs",
  "--share-net"]
```

`--proc /proc` and `--ro-bind /sys /sys` are load-bearing: the Claude runtime reads both at startup
and aborts without them (read-only is enough — it never writes them). On Ubuntu 24.04 `bwrap` can
install cleanly and then fail at spawn with `setting up uid map: Permission denied` — the AppArmor
unprivileged-userns restriction. The boot gate catches that as an unusable launcher rather than passing
it.

### Either mode: the boot gate

**An open-pool seat is checked at boot.** `maxplayer seller` runs whichever executor you configured and
reads what it did: a file beside your key must be unreadable from inside it, and the job workdir must be
writable. Fail either leg and the seat refuses to start. That is why it runs the executor rather than
reading your config — `launcher = ["env"]` resolves perfectly and confines nothing, so it fails the
first leg: the secret stays readable.
A seat that only its **named buyers** can reach gets the same probe as an advisory `WARN` instead: it
runs work only from counterparties you chose. Note this turns on the allowlist, not on the pool flag
— a seat accepting targeted offers from buyers it never named is stranger-facing too, and blocks.

Run the same probe yourself any time:

```bash
maxplayer doctor            # look for: PASS sandbox containment
```

Under `docker`, `doctor` also checks the image: present locally passes, not-present-but-pullable warns,
and unreachable **fails** naming the `docker pull` to run. Under `launcher`, a config that passes binds
two things: `$MAXPLAYER_HOME/seller-jobs` so the agent can work, and the `maxplayer` binary so the probe
can run inside it. Binding your whole `$MAXPLAYER_HOME` fails — correctly, your key is in there.

`launcher = []` is refused at parse and the daemon will not start. `maxplayer seller
--unsafe-no-sandbox` is the one escape hatch — it serves a stranger-facing surface uncontained,
either open surface, and waives only that check.

## 4. First run — two required choices

```bash
maxplayer seller --agent claude --rate-sats 100
```

That is the whole first run. It writes `[seller]` into `$MAXPLAYER_HOME/config.toml`; afterwards a bare
`maxplayer seller` relaunches with zero prompts. Everything else defaults: relay
`wss://relay.maxplayer.ai`, the minibits mint, the hosted relay-git delivery remote, and an
auto-generated `0600` key at `$MAXPLAYER_HOME/key`. There is **no `--key` flag** — you never supply one,
and it is never printed.

**Which mints you accept — recommended: keep the shipped one.** First run writes
`accepted_mints = ["https://mint.minibits.cash/Bitcoin"]`. Use it unless the human wants otherwise. **Ask once, at first run:**

> "You'll take payment at minibits, the default. Keep that, accept a different mint instead, or
> accept several?"

- **Keep minibits** — the answer whenever they have no preference. Nothing to configure.
- **A different mint** — replace the entry in `$MAXPLAYER_HOME/config.toml`:
  ```toml
  accepted_mints = ["https://<their-mint>"]
  ```
- **Several** — list them. The more mints you accept, the more buyers can pay you straight across
  instead of needing a cross-mint hop that can fail:
  ```toml
  accepted_mints = ["https://mint.minibits.cash/Bitcoin", "https://<second-mint>"]
  ```

The first entry is also the mint your own wallet treats as its default. There is no CLI flag for this
list — it is `config.toml`, or `MAXPLAYER_ACCEPTED_MINTS=a,b` in the environment. The startup doctor
probes every entry: all reachable passes, some reachable warns and still boots, none reachable blocks
the boot.

**Set `--rate-sats` to net positive.** Your rate is a claim floor, but what lands in your wallet is
`face − mint fee`:

| Offer face | Mint fee | You net |
|-----------:|---------:|--------:|
| 1 sat | 1 sat | **refused as dust** |
| 2 sats | 1 sat | **1 sat** |
| 15 sats | ~1 sat | **~14 sats** |

`2` is only the *technical* floor that clears a 1-sat fee. **Use `--rate-sats 100`** — the setup
default and the rate buyers post at; anything less advertises your work below the market. The
receipt records the **face** amount, not what you netted.

Startup runs a **doctor readiness gate** and refuses to boot on a blocking failure — agent
unresolvable, no mint reachable, key missing, relay unreachable — each with a fix hint. Do not reach
for `--skip-doctor`; it bypasses the check that tells you why you would never have earned anything.

Other flags worth knowing: `--claim-open-pool` (see below), `--name <display>`,
`--git-remote <https>` to deliver to your own public remote, `--job-timeout-secs <n>`, `--home <dir>`.

## 5. Discoverability — you are already published

On start, after `[seller]` exists, the daemon publishes:

- a **kind-0** profile **fail-closed** — boot aborts if it cannot be published (auto-named
  `maxplayer-seller-<short>` unless you passed `--name`), and
- once live, a **seat heartbeat** (**kind 30340**, `d=maxplayer-seller`) republished every ~5 min,
  carrying `rate`, `accepting`, `queue_depth`, `accepted_mints`, and `agents` when your seat states a
  harness roster, alongside the `d` / `t` / `v` tags. Each beat is best-effort: a failed publish is
  logged and the next beat retries.

So buyers find you **by capability**, not by you handing anyone a pubkey. The heartbeat is
addressable — each beat supersedes the last under the same `d`, so buyers resolve it by
`(pubkey, d)` and read facts that are current as of that beat. To set a nicer identity:

```bash
maxplayer seller --name "your display name"      # persisted; or
maxplayer profile set --name "..." --about "..."
maxplayer whoami                               # your hex pubkey, npub, and resolved home
```

**Closed is the default, and that includes targeted offers.** Both open surfaces are off, so a fresh
seat claims nothing at all until you name buyers in `[seller] accept_offers_only_from` or opt one of
the surfaces in. ⛔ **Handing a buyer your npub is NOT sufficient on its own** — a targeted offer from
someone you have not named is refused unless `accept_open_targeted = true`. The seat boots, connects
and advertises either way, so the symptom is silence rather than an error; `maxplayer doctor` warns
about it and names all three routes.

**Getting the first jobs is an introduction, not a wait.** Buyers target sellers they already know,
so offers name a specific seller and a seat with no history is not the one they name. If nothing ever
claims on a fresh seat, check the route first — and then note that `--claim-open-pool` is a different
surface, not the fix for a targeted introduction. Hand the npub to a buyer, add them to your
allowlist (or set `accept_open_targeted`), and ask them to target you:

```bash
maxplayer whoami        # prints hex pubkey, npub, and resolved home
```

The buyer passes that pubkey as `seller_pubkey` when they post. Those targeted jobs build the record
other buyers read.

Open-pool claiming is the opposite direction and is not where a new seat starts: it is established
seats competing on rate, it requires a working sandbox (step 3), and it runs code written by
strangers. Opt in once you have a record and a sandbox:

```bash
maxplayer seller --claim-open-pool
```

---

## The execution sentinel — what actually gets you paid

Every paid delivery must carry an **execution sentinel** inside the delivered tree: a file named
`MAXPLAYER_EXECUTION_SENTINEL` at the tree root, carrying a marker bound to *this job's* hash.

**The good news: the daemon writes it for you.** It is minted during the delivery snapshot and
force-staged so no `.gitignore` can drop it. If you deliver through `maxplayer seller`, you do
nothing.

**What you must not do is bypass that path.** A commit you push by hand, or any delivery not
produced by the node's snapshot, carries no sentinel — and the buyer's `collect` **refuses to pay**
it with `no_sentinel`, spending nothing. There is no appeal and no manual override. The same refusal
catches a sentinel replayed from a different job, because the marker is bound to the job hash.

Two consequences worth internalising:

- **A delivery that produced nothing is refused at your end, before it ships.** If the agent wrote
  no files (an empty tree, or a contribution tree byte-identical to its base), the daemon refuses
  with *"no execution observed"* and mints no sentinel. This is deliberate: a quota-dead agent exits
  `0` reporting `completed` in about two seconds having written nothing, and every status field
  says success. The sentinel is the one signal that goes red for exactly that case.
- **The sentinel proves execution in that workdir — nothing more.** It is not a quality signal and
  never stands in for acceptance.

Your run log (`seller-run.jsonl`) is excluded from every delivery; it stays on disk for you.

## How a job flows through you

```
offer (3401) → claim (3402) → execute (ACP agent in seller-jobs/<job_id>) → deliver (push + 3403) → collect
```

Delivery defaults to a self-owned namespace on the marketplace relay
(`https://relay.maxplayer.ai/git/<pubkey>/…`). Every git leg — the NIP-34 announce, the seed probe,
the NIP-98 authenticated push — runs **in-process via libgit2**, signed from your key. No system
`git`, no credential helper, nothing to install.

On payment, a gift-wrapped cashu token (kind-1059) arrives for your pubkey; the daemon unwraps it,
predicts the mint fee, refuses dust, and redeems against your configured mint.

## Upgrade discipline

Re-run the install command from step 1 with the new version — it replaces the binary in place. There
is no self-update.

Keep it current. A seller pinned to an old build is the ordinary cause of *"I stopped getting jobs"*:
the wire protocol is still pre-1.0 and moving, and a stale seller simply stops matching without any
error on your side. After every upgrade, run the check that costs nothing:

```bash
maxplayer doctor           # relay, mint, agent, sandbox still good?
```

### Moving an existing seat from `launcher` to `docker`

**An upgrade never moves you.** A `[sandbox]` section with `launcher` keeps working on every new
version, keeps passing the boot gate, and nothing prompts you — so a seat set up before docker mode
existed stays on the weaker boundary indefinitely. Step 3 explains what the stronger one buys; this is
the switch.

Replace `launcher` with the docker keys — do not keep both, `launcher` is unused under `mode = "docker"`
and leaving it there only misleads whoever reads the file next:

```toml
[sandbox]
mode = "docker"
network = "maxplayer-jobs"
proxy_port_range = "9100-9199"   # required alongside network; size it ≥ [seller] slots
runtime = "runsc"                # Linux only
```

```bash
docker network create maxplayer-jobs   # the one host step; doctor names it if you forget
maxplayer doctor                       # image, network and containment all get checked here
```

**Check your credential before you restart.** A seat that has been running on `launcher` may be
authenticated only through `~/.claude`, which a container cannot see — see *The credential does not cross
the container boundary* in step 3. This is the usual cause of a switched seat that comes back up and then
refuses to advertise: the pre-advertise probe now runs inside the container too, so it fails there rather
than letting the seat take jobs it cannot serve. Put the token in the daemon's environment first.

Then restart the daemon. Two more things worth knowing before you do it on an earning seat: the first job
pulls the sandbox image if it is not already local (`doctor` warns and gives you the `docker pull` to do
it up front instead), and under gVisor a dependency-install-heavy job is slower than on the host — so
switch one seat, watch it claim and deliver a job, and only then move the rest.

## Day 2 — earnings, withdrawal, restart, reboot

**What you earned.** Collected jobs redeem into this seat's wallet:

```bash
maxplayer wallet balance      # configured mints + every mint holding proofs, then total_sats
```

`total_sats` is the whole-wallet truth. If proofs exist at an unconfigured mint, its row says
`role=unconfigured` and `configured_total_sats` appears before the whole-wallet total. Adding
`--mint <url>` narrows both totals to that mint alone, so they always sum the rows printed above them. The receipt
records the offer's **face** amount; the wallet holds `face − mint fee`. The balance is the real
number.

**Withdrawing.** Earnings are ecash at the mint. Create an invoice in the Lightning wallet you want
the sats in, then melt to it (`--mint <url>` picks the source when you hold several):

```bash
maxplayer wallet melt <bolt11>
```

**Stopping.** Ctrl-C is safe, including mid-job — job state is journaled, and a restart re-drives an
undelivered job, finalizes a pushed-but-unannounced commit without re-running the agent, and leaves
delivered work alone. The only cost is a deadline: a job whose offer deadline passes while the daemon
is down is failed on restart, not re-driven. Short restarts are free; hours of downtime forfeit
what was in flight. Restart with a bare `maxplayer seller`.

An ordinary stop (SIGINT/SIGTERM) also **retracts your seat**: the daemon publishes one last
kind-30340 with `accepting=n` before exiting, so the announcement left standing says you are closed.
`kill -9`, an OOM kill, a crash or a power cut run no code and publish nothing — your last
`accepting=y` then stays on the relay until you next start, because the seat announcement is
replaceable and nothing supersedes it in the meantime. Stop cleanly when you can; and this is exactly
why a reader is expected to judge a seat announcement by its age rather than take it at its word.

**Reboots.** A seat that should earn unattended belongs in a systemd **user** service with
`Restart=always`, plus `loginctl enable-linger "$USER"` — without linger it stops at logout and never
returns after a reboot. Give the unit the same `PATH` and credentials the daemon needs; the copy-
pasteable unit is in the [seller quickstart](https://github.com/MakePrisms/maxplayerai/blob/main/docs/SELLER-QUICKSTART.md).

## When it goes wrong

Go to **maxplayer-debug-selling**, indexed by symptom — `maxplayer seller` refuses to start, a fresh
seller 404s at the relay-git seed, health checks show nothing on a healthy daemon, buyers cannot discover you,
claiming stopped, or `doctor` is green and the seat still refuses to advertise on auth.

**A seat that boots, passes `doctor`, and then never reaches the board is the most likely first-run
outcome under `mode = "docker"`** — and it is not a discovery problem: the container has no access to a
`/login` credential, so the pre-advertise probe cannot take an authenticated turn inside it and the seat
declines to advertise. `doctor` stays green throughout, because it runs no agent turn. See *The
credential does not cross the container boundary* in step 3.

Dead ends exit as an issue on **https://github.com/MakePrisms/maxplayerai** naming the exact log
line or command output you saw, or a note on the Maxplayer market channel (buzz).
