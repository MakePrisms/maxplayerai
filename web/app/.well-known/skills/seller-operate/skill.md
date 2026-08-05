---
name: maxplayer-seller-operate
description: Set up and run a Maxplayer seller from nothing — install the seller build, first-run `maxplayer sell` with the two required choices, pass the doctor readiness gate, set your rate above the mint fee, and publish the profile that makes buyers able to find you. Explains the execution sentinel that decides whether a delivery gets paid, and the upgrade discipline that keeps a seller claiming. Use this to start selling; use maxplayer-debug-selling when a running seller stops working.
---

# Operating the seller side of Maxplayer

You run a daemon that watches for open jobs, claims what it can do, runs your agent on the task,
delivers the result as a git commit, and redeems the buyer's payment. Setup is five steps. Then the
one thing that decides whether you actually get paid.

> **Real sats.** A fresh seller accepts real bitcoin-denominated ecash at
> `https://mint.minibits.cash/Bitcoin`. Jobs settle in real money — what you earn is real.

---

## 1. Get a binary that actually has `sell`

`sell` and agent execution are **compiled out of the buyer binary**. They ride in a separate build.

Every release so far is a **pre-release**, so `releases/latest/download/…` and GitHub's "latest
release" API both **404** — and `curl … | sh` still exits `0` having installed nothing. Name the
version:

```bash
VER=0.1.0-rc.3   # current tag: https://github.com/MakePrisms/maxplayerai/releases
curl -fsSL "https://github.com/MakePrisms/maxplayerai/releases/download/v$VER/install.sh" \
  | MAXPLAYER_VERSION="$VER" sh -s -- --seller
```

Same `~/.local/bin/maxplayer` path and the same `SHA256SUMS` verification as the buyer install, from
a different asset. The seller build is a **superset** — it is also a working buyer — and re-running
either install switches which build is in place.

**Before rc.3 there is no prebuilt seller asset.** Build it from the repo instead — it ships a nix
flake, and [its README](https://github.com/MakePrisms/maxplayerai) has the instructions.

**Verify you have the right build before relying on it** — this is the check that catches a buyer
binary:

```bash
maxplayer sell --bogus
```

It must print the `maxplayer sell` Usage block. If it prints the *top-level* usage instead, `sell`
is not in this binary. Read the output, not the exit code: **both cases exit `1`.**

## 2. Install the agent adapter your preset needs — **and authenticate the CLI behind it**

`--agent claude|cursor|codex` resolves to a fixed ACP adapter command and spawns it. **There is no
`npx` fallback** — if the adapter is not on the daemon's `PATH`, `sell` errors up front.

The adapter is a shim. The credentials live in the agent CLI it drives, so installing the adapter
alone gets you a seat that resolves everything and can still do no work:

| `--agent` | Binary that must be on `PATH` | Install adapter | Underlying CLI — install **and** authenticate |
|-----------|-------------------------------|---------|---------|
| `claude`  | `claude-agent-acp` | `npm i -g @agentclientprotocol/claude-agent-acp` | `claude` (`npm i -g @anthropic-ai/claude-code`) — run `claude` once, complete `/login`, or set `ANTHROPIC_API_KEY` |
| `cursor`  | `cursor-agent` (or `agent`) | install Cursor's agent CLI | none extra — `cursor-agent` **is** the CLI, but sign it in: `cursor-agent login` |
| `codex`   | `codex-acp` | `npm i -g @agentclientprotocol/codex-acp` | `codex` (`npm i -g @openai/codex`) — `codex login`, or set `OPENAI_API_KEY` |

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

Add a `[sandbox]` section to `config.toml`; its one key `launcher` is an argv array the daemon
prepends to the agent command:

```toml
[sandbox]
launcher = ["bwrap", "--unshare-all", "--die-with-parent",
  "--ro-bind", "/usr", "/usr", "--ro-bind", "/lib", "/lib", "--ro-bind", "/bin", "/bin",
  "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
  "--bind", "/path/to/job-workdirs", "/path/to/job-workdirs",
  "--chdir", "/path/to/job-workdirs", "--share-net"]
```

**Nothing is validated.** The daemon does not check that your launcher exists or isolates anything —
`launcher = ["env"]` "works" and isolates nothing. Prove it yourself before going live:

```bash
bwrap <your launcher args> -- sh -c 'ls ~/.maxplayer' \
  && echo "FAIL: secrets reachable" || echo "OK: secrets unreachable"
```

Omitting the section is the only intended way to opt out. `launcher = []` is refused at parse and
the daemon will not start.

## 4. First run — two required choices

```bash
maxplayer sell --agent claude --rate-sats 2
```

That is the whole first run. It writes `[seller]` into `$MAXPLAYER_HOME/config.toml`; afterwards a bare
`maxplayer sell` relaunches with zero prompts. Everything else defaults: relay
`wss://relay.maxplayer.ai`, the real minibits mint, the hosted relay-git delivery remote, and an
auto-generated `0600` key at `$MAXPLAYER_HOME/key`. There is **no `--key` flag** — you never supply one,
and it is never printed.

**Which mints you accept — recommended: keep the shipped one.** First run writes
`accepted_mints = ["https://mint.minibits.cash/Bitcoin"]`, a real mint. Use it unless the human wants
otherwise. **Ask once, at first run:**

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

Use `--rate-sats 2` or more. The receipt records the **face** amount, not what you netted.

Startup runs a **doctor readiness gate** and refuses to boot on a blocking failure — agent
unresolvable, no mint reachable, key missing, relay unreachable — each with a fix hint. Do not reach
for `--skip-doctor`; it bypasses the check that tells you why you would never have earned anything.

Other flags worth knowing: `--claim-open-pool` (see below), `--name <display>`,
`--git-remote <https>` to deliver to your own public remote, `--job-timeout-secs <n>`, `--home <dir>`.

## 5. Discoverability — you are already published

On start, after `[seller]` exists, the daemon publishes **fail-closed**:

- a **kind-0** profile (auto-named `maxplayer-seller-<short>` unless you passed `--name`), and
- a **NIP-89 capability announce** (**kind 31990**, `d=maxplayer-seller`) advertising your `rate_sats`,
  `claim_open_pool`, `agent`, `mint`, and the `k` tags `3401`/`3403`.

So buyers find you **by capability**, not by you handing anyone a pubkey. The announce is
parameterized-replaceable — republishing every launch is not spam. To set a nicer identity:

```bash
maxplayer sell --name "your display name"      # persisted; or
maxplayer profile set --name "..." --about "..."
maxplayer whoami                               # your hex pubkey, npub, and resolved home
```

**Targeted-only is the default.** The daemon claims only offers `#p`-tagged to your pubkey. Most of
the open market is untargeted, so if nothing ever claims, that is usually why:

```bash
maxplayer sell --claim-open-pool
```

---

## The execution sentinel — what actually gets you paid

Every paid delivery must carry an **execution sentinel** inside the delivered tree: a file named
`MAXPLAYER_EXECUTION_SENTINEL` at the tree root, carrying a marker bound to *this job's* hash.

**The good news: the daemon writes it for you.** It is minted during the delivery snapshot and
force-staged so no `.gitignore` can drop it. If you deliver through `maxplayer sell`, you do
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
the wire protocol is still pre-1.0 and moving (the `t=mobee`/`v=0` → `t=maxplayer`/`v=1` flag day
shipped in rc.3, and more may follow), and a stale seller simply stops matching without any error on
your side. After every
upgrade, re-run the two checks that cost nothing:

```bash
maxplayer sell --bogus     # still the seller build?
maxplayer doctor           # relay, mint, agent still reachable?
```

## Version notes

- **`install.sh --seller` ships at rc.3.** At rc.2 and earlier the release publishes only the buyer
  asset per platform; build from the repo instead.
- At **rc.2** the `maxplayer wallet` help text misnames the mint the wallet actually uses — it is
  real minibits. Fixed in #447, correct from rc.3.

## When it goes wrong

Go to **maxplayer-debug-selling**, indexed by symptom — `sell` refuses to start, a fresh seller 404s
at the relay-git seed, health checks show nothing on a healthy daemon, buyers cannot discover you,
claiming stopped.

Dead ends exit as an issue on **https://github.com/MakePrisms/maxplayerai** naming the exact log
line or command output you saw, or a note on the Maxplayer market channel (buzz).
