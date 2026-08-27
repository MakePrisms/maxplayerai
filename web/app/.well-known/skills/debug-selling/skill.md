---
name: maxplayer-debug-selling
description: Debug selling on Maxplayer when your seller won't start, isn't earning, or looks dead. Covers `maxplayer seller` refusing to boot (the startup doctor / readiness gate), a fresh seller bricking at the relay-git seed with a 404, health-checks that show nothing on a perfectly healthy daemon (which log line actually proves liveness), a seat that buyers can't discover, and a seller that quietly stops claiming new jobs. Says exactly which command to run, which log line to grep, and where to report a dead end.
---

# Debugging the seller side of Maxplayer

The seller is `maxplayer seller` — the same binary a buyer installs, run in its other mode. It watches
the relay for open jobs, claims what it can do, delivers, and collects.

**The first move for almost everything here is the doctor:**

```
maxplayer doctor
```

It runs the same checks `maxplayer seller` runs at startup: `seller key`, `relay
reachability`, `mint reachability`, `agent preset`, `sandbox launcher`, plus advisory
`credential helper` and `telemetry`. Each prints `PASS`/`WARN`/`FAIL` and, when not PASS,
a one-line `(fix: …)` hint. Exit is `0` unless something `FAIL`ed; a `WARN` never fails.
Every check runs even after one fails, so one run shows the whole picture. Add
`--home <dir>` to diagnose a specific seat.

---

## Symptom: `maxplayer seller` refuses to start

On startup, `maxplayer seller` runs the doctor as a readiness gate and **refuses to boot if any
blocking check FAILs** — this is by design, so a seat never advertises work it cannot do.
You will see:

```
maxplayer seller — startup readiness checks (auto-doctor; pass --skip-doctor to bypass)
```

followed by the check lines, and on failure:

```
... REFUSING to start: N blocking readiness check(s) failed —
  FAIL <check> — <detail> (fix: <hint>)
resolve the item(s) above, then re-run `maxplayer seller`. To bypass these checks (NOT
recommended), pass --skip-doctor.
```

**Read it — the five blocking checks and their fixes:**
- `seller key` FAIL → *ensure the seller key file exists and is readable (mode 0600) — it
  is auto-generated on first run*
- `relay reachability` FAIL → *check relay_url in config.toml and network/relay
  availability*
- `mint reachability` FAIL (only when **every** accepted mint is down) → *check the mint
  URLs in [accepted_mints] and network availability*
- `agent preset` FAIL (no launchable harness) → *set [seller] agents = ["claude", …] (or
  agent_command) and install the harness adapter*
- `sandbox launcher` FAIL (launcher not on PATH / not a file) → *install the launcher
  program or fix [sandbox] launcher (or remove [sandbox] to run unsandboxed)*

A `WARN` (e.g. one of several mints down, or `no [seller] section configured`) prints but
does **not** block boot.

**Fix:** resolve the FAILed item using its hint, then re-run `maxplayer seller`. Re-running
`maxplayer doctor` confirms it before you retry. `--skip-doctor` bypasses the gate but is
not recommended — a bad launcher or unresolvable agent means every awarded job dies at
spawn and you lose the award.

**Dead end → report it:** if a check FAILs with a detail you cannot resolve, file on
**MakePrisms/maxplayerai** and paste the full `FAIL` line (check name + detail + hint),
or ask on the buzz market channel.

---

## Symptom: every readiness check PASSed, then the probe failed with `Authentication required`

```
seller node agent PASS codex binary resolves argv0=/usr/local/bin/codex-acp (auth not checked here …)
seller node pre-advertise probe FAILED codex: probe turn failed (seller agent error:
  ACP request 2 failed: {"code":-32000,"message":"Authentication required"})
seller node prove-before-advertise: none of 1 configured harness(es) produced a probe
  artifact; refusing to advertise
```

**This is the gate working, not a bug.** Nothing before the probe reads a credential — the
readiness checks find *binaries*. `PASS` means "the adapter resolves", which is why the line says
so explicitly. The probe is the first and only step that proves an authenticated turn is possible,
and a seat that cannot take one refuses to advertise rather than sell work it cannot deliver.

**Cause:** the ACP adapter is installed but the **agent CLI behind it** is not signed in. The
adapter is a shim; the credentials belong to the CLI it drives.

**Fix** — authenticate the CLI for your preset, in the environment the *daemon* runs under:

- `codex` → `npm i -g @openai/codex`, then `codex login` (or `codex login --device-auth` on a
  headless box, or `printenv OPENAI_API_KEY | codex login --with-api-key`; `OPENAI_API_KEY` is read
  directly too). **`codex login --api-key <KEY>` is deprecated and hidden** — it exits with guidance
  instead of authenticating, so if you scripted that, it is why you are here.
- `claude` → `curl -fsSL https://claude.ai/install.sh | bash` (or `npm i -g @anthropic-ai/claude-code`,
  Node 22+), then run `claude` and complete `/login`, or `claude auth login` / `claude setup-token`.
  **`ANTHROPIC_API_KEY` alone will not fix an unattended seat:** Claude Code prompts **once** to
  approve an environment key rather than using it silently, and a daemon has nobody to approve it —
  so the probe still fails on a box where the variable is plainly set.
- `cursor` → install with `curl https://cursor.com/install -fsS | bash`, then `cursor-agent login`
  (or set `CURSOR_API_KEY`). `cursor-agent` is itself the CLI — no separate shim. On a headless seat
  set `NO_OPEN_BROWSER=1` and `cursor-agent login` prints the browser URL instead of trying to open
  a local browser — *measured by the maintainer on Cursor Agent `2026.08.25-3e8eec8` (Linux), not
  reproduced on our build hosts.*
  **Do not `npm i -g cursor-agent`** — unrelated third-party package, installs no binary, succeeds
  silently.

**Confirm before re-running:** run the underlying CLI by hand and have it complete one turn
without prompting you to log in. If it prompts you, it will prompt the probe. An env-var credential
set only in your login shell will not reach a systemd unit, Docker entrypoint, or cron job.

---

## Symptom: under `mode = "docker"`, `doctor` is green and the seat still refuses to advertise on auth

`maxplayer doctor` passes every check, including the agent one, and the daemon then stops at the
pre-advertise gate with an agent authentication error — `{"code":-32000,"message":"Authentication
required"}` — and never reaches the board.

**Cause: a container inherits none of your login.** Your `claude /login` credential lives in `~/.claude`
(on macOS, the Keychain). Jobs run **inside the container**, which has no home directory, no Keychain and
no `~/.claude`. The pre-advertise probe runs its turn under the **same sandbox policy a real job gets**,
so under `docker` the probe runs in the container too and fails exactly where the job would. That is the
gate working: a seat that cannot deliver never advertises. This is the ordinary first-run outcome for a
docker seat.

⛔ **A green `doctor` is not a green probe, and it never was.** `doctor` runs **no agent turn at all** —
its agent check resolves the registry and says so in its own PASS text. Do not read it as proof that a
harness can deliver.

**Fix — put the credential in the daemon's own environment.** These names are forwarded into the
container automatically when they are set, with no `forward_env` entry needed:

```
ANTHROPIC_API_KEY   ANTHROPIC_AUTH_TOKEN   CLAUDE_CODE_OAUTH_TOKEN   ANTHROPIC_BASE_URL
OPENAI_API_KEY      OPENAI_BASE_URL
```

For `claude`, prefer **`CLAUDE_CODE_OAUTH_TOKEN`** (`claude setup-token`). `ANTHROPIC_API_KEY` is the
worse choice for an unattended seat for the reason in the previous symptom: Claude Code prompts once to
approve an environment key, and a daemon has nobody to approve it.

Set it where the daemon **actually starts** — a systemd `Environment=`, a launchd plist, or the launcher
script that `exec`s it. Not your login shell. And an `export` in an interactive shell cannot reach an
already-running daemon, so this takes effect on the **restart**: seeing the variable in your own `env`
verifies your shell, not the seat.

⛔ **If your harness is `cursor`, do not reach for `forward_env`.** The forwarded list is claude and
codex only, and the per-job proxy cannot hold `CURSOR_API_KEY`, so **`forward_env = ["CURSOR_API_KEY"]`
puts your real reusable key inside the container where a stranger's job reads it.** A `doctor` WARN
flags that and does not refuse it, so the seat runs and leaks.

The browser-login session has a contained path instead — `[[sandbox.file_credentials]]`, whose fields
are listed in DOCKER.md — which keeps the real value on the host and hands the container a per-job
placeholder. It needs two things the other harnesses do not: a second
`[[sandbox.file_credentials.legs]]` entry, because cursor's agent traffic goes to a different host than
its control plane; and an on-disk session file, because `path` must be an absolute host path and the
macOS login Keychain is not one.

⛔ **Two cautions before you build a cursor seat on this.** We have not verified a command for making
cursor write that session file on our hosts, so this page does not give you one — read Cursor's own
documentation, not this page. That bound is on our reach, not on the world. And while the tree no longer
contradicts itself about whether the contained path completes a job — the old "not sufficient" claim is
retracted — nobody here has run `cursor-agent` to reproduce it, so treat it as a maintainer measurement
and prove it on your own seat. See *step 3* of **maxplayer-seller-operate**, and **Link your model
account** there for the login itself.

Note that the real credential still does not enter the container: a per-job host proxy holds it and
passes a placeholder plus a base-URL override inward. That is also why `[sandbox] proxy_port_range` is
required once `[sandbox] network` is set — the proxy needs a firewall pinhole the job can reach it
through, and without a range the daemon refuses the job with `a contained credential needs [sandbox]
proxy_port_range when egress containment is active`.

---

## Symptom: a brand-new seller bricks right after it announces (relay-git seed 404)

**This is a known v0.1 blocker.** A fresh seller publishes its NIP-34 delivery-repo
announce, then probes that the relay seeded the repo with a signed in-process
`ls-remote` — and gets an HTTP **404**, so it aborts before it is discoverable. The exact
error:

```
maxplayer-hosted delivery not seeded after NIP-34 announce (ls-remote 404).
likely cause: relay-git global name collision on repo id, or seed side-effect failed.
provide --git-remote <https-url> for BYO delivery, or pick a unique remote leaf.
remote=<url>
```

**Read it:** the seed is meant to happen server-side as a side effect of the announce.
The announce goes to the **market relay** (`wss://relay.maxplayer.ai`), and the repo
materializes on that **same host** over its git path
(`https://relay.maxplayer.ai/git/<pubkey>/m<short>.git`, derived from `relay_url`). Seeding
can fail on a global repo-name collision: the announce is accepted but the seed is skipped,
so the `ls-remote` 404s and the seller refuses to boot. It fails closed: no money is exposed.

**Fix / workaround — bring your own delivery host (the tool's own recommended fix):**

```
maxplayer seller --agent <claude|cursor|codex> --rate-sats <n> --git-remote <https-url>
```

`--git-remote <https-url>` points delivery at a git host you control (e.g. an HTTPS repo
URL). It skips the relay-git announce/seed path entirely, so the 404 cannot occur. There
is **no `--relay` flag**; the market relay is set only via `relay_url` in
`~/.maxplayer/config.toml` or `MAXPLAYER_RELAY_URL`.

**Dead end → report it:** if you must use relay-git and cannot use `--git-remote`, this is
tracked as the v0.1 tag-blocker — file/comment on **MakePrisms/maxplayerai** with the full
404 block above including the `remote=` line, or raise it on the buzz market channel.

---

## Symptom: I can't tell if my seller is alive — my health check shows nothing

**There is now a status line that answers this directly.** Every ~5 minutes a healthy seller
prints its own state, timestamped:

```
14:32:07Z seller node status: ADVERTISING, ready for work · harness: claude · 0/1 job slot(s) busy
```

Read it as: it is alive (the line just arrived), it is advertising, and it has capacity. If it
says `NOT serving — no live harness`, the seat is up but every harness has faulted out — it will
take no work until one recovers.

Every operator-facing line carries a `HH:MM:SSZ` UTC stamp, so "has anything happened since I
last looked" is answerable by reading the last timestamp, and any line can be lined up against
relay events.

**On older builds** (before this line existed) a healthy seller printed no "online/healthy"
banner at all, and the kind-30340 heartbeat logged **only when it failed** — so grepping for
`online`, `healthy`, `watching`, or `heartbeat published` matched **nothing on a perfectly healthy
daemon**. If you are on such a build, the load-bearing liveness signal is instead the periodic
line below, which still prints on current builds:

```
seller node wrap backfill (periodic): fetching stored kind-1059(s) since ts=<n>
```

**Startup, once** — proves it authenticated and entered the loop:

```
seller node live: pubkey=<hex> relay=<url>
```

**Fix:** point your health check / supervisor grep at `seller node status:` (or, on older
builds, the `wrap backfill (periodic): fetching` line). Do not grep for `online` /
`heartbeat published` — no such success line exists. The **absence** of heartbeat-failure lines
is normal and healthy.

**Quieter or noisier:** routine no-ops (a re-seen offer already claimed, a duplicate award) are
hidden by default and print only with `MAXPLAYER_VERBOSE=1` in the daemon's environment. Nothing
that reports a state change or a failure is behind that flag.

**Dead end → report it:** if you see `seller node live:` but the periodic `seller node status:`
line never repeats, the loop may be wedged — file on **MakePrisms/maxplayerai** with the last few
stderr lines and the gap in timestamps.

---

## Symptom: buyers can't find my seller / I'm not on the board

Your seat has **two** discovery records, and they refresh differently.

The **kind-0 profile** — your name and identity — is published **only at boot**, in one place,
logged as:

```
seller node discoverable kind0=<id> name=<name> pubkey=<hex>
```

There is **no periodic re-publish** of the profile. So if the relay was unreachable when you
started, or a relay outage wiped the replaceable event, your profile is gone and does **not**
come back on its own.

Your **capability** — `rate`, `accepting`, `queue_depth`, `accepted_mints`, `agents` — rides the
**kind-30340 heartbeat** (`d=maxplayer-seller`), republished every ~5 min. Do not grep for a
kind-31990 / NIP-89 handler: #645 retired it and nothing publishes one any more. Any 31990 still on
a relay is pre-#645 residue, not live capability. The heartbeat *does* self-heal — one interval
after the relay comes back, your capability is current again. Boot confirms it is running with:

```
seller node heartbeat+watchdog enabled: kind-30340 every <n>s; …
```

A successful beat is silent; only failures print (`seller node heartbeat publish failed
(continuing): …`).

**Check:** confirm the `seller node discoverable …` line appeared at your last startup, and that
the relay was up at that moment. Then read your latest kind-30340 — resolve it by `(pubkey, d)`,
never by event id, since each beat supersedes the last in place. If it says `accepting=n`, you are
published but declining: the seat is busy (`queue_depth` > 0), has dropped every harness, or is the
**retraction** a cleanly-stopped daemon publishes on its way out — check `created_at` against your
last stop. That is a capacity or liveness problem, not a discovery one.

Note the converse, and do not read it as good news: an `accepting=y` beat left by a daemon that was
**killed** (`kill -9`, OOM, crash, power cut) stays on the relay unchanged, because kind-30340 is
replaceable and a dead seat publishes nothing to supersede it. A seat announcement is only worth what
its `created_at` says — a stale `accepting=y` means nothing.

**Fix:** **restart the seller** (`maxplayer seller`). Boot re-publishes the kind-0 profile and
resumes the heartbeat, and you are listed again. This is the correct response after any relay outage
that happened while your seat was already running — though if only capability looked stale, waiting
one heartbeat interval would have fixed that much on its own.

**Dead end → report it:** if you restart with the relay confirmed up and still are not
discoverable, file on **MakePrisms/maxplayerai** with your `pubkey` and the
`seller node discoverable` line from the restart.

---

## Symptom: my seller stopped claiming new jobs

**Check the admission config first — an upgrade can close a surface that used to be open.** An empty
`accept_offers_only_from` no longer means accept-all on the targeted surface. Both stranger-facing
surfaces are now their own opt-in, so a seat with no allowlist and both flags off claims **nothing**
while still advertising and staying connected. The daemon says so at boot — this is ONE line, wrapped
here only to fit the page:

```
seller node WARNING: this seat can claim NOTHING as configured — it names no buyers
(accept_offers_only_from is empty), does not accept targeted offers from buyers it has not named
(accept_open_targeted=false), and does not claim the open pool (claim_open_pool=false). It will
advertise and stay connected, but never claim a job. If this seat used to serve, an upgrade closed
the targeted surface that an empty allowlist used to leave open. THREE ROUTES BACK IN: list the
buyers you work with in `[seller] accept_offers_only_from`, or set `accept_open_targeted = true` to
accept targeted offers from buyers you have not named, or set `claim_open_pool = true` to claim
untargeted jobs from the open pool.
```

⚠ **A seat whose allowlist is populated but unusable gets a DIFFERENT line**, so grep for
`can claim NOTHING as configured` rather than for the tail above. An entry is matched byte for byte
against a wire pubkey, so a list of typos is not a narrow route in — it is no route. It does not
fence anyone out either: since #923 the open flags admit independently of the list, so an unusable
allowlist beside `accept_open_targeted = true` still lets unnamed buyers in. That line names the
count and tells you to correct the entries or remove them.

**Three routes back in**, and they are independent — nothing is inferred from a field being empty:

- list the buyers you work with in `[seller] accept_offers_only_from`
- `accept_open_targeted = true` — accept targeted offers from buyers you have not named
- `claim_open_pool = true` — claim untargeted jobs from the open pool

They are **additive**, and since #923 a populated allowlist no longer cancels the open flags: the
list admits the buyers it names, and each open flag adds its own public route beside it. So a seat
with entries in `accept_offers_only_from` *and* `accept_open_targeted = true` accepts targeted
offers from unnamed buyers. There is no inert combination for `maxplayer doctor` to report.

If the routes are right and the seat still stopped, it may have hit the awaiting-award backlog
cap. Look for this on stderr:

```
seller node offer skip id=<id>: awaiting-award backlog full (cap 32)
```

**Read it:** the node holds at most **32** claims that are awaiting an award. When that
fills, it **skips every new offer**. Normally a claim that is never awarded is released
after ~300s and frees its slot — but a claim that was made and then **orphaned across a
restart** stays at `state = 'claimed'` **forever**: the release sweep is in-memory and the
start-up reconcile deliberately does not touch claimed rows. Enough of these accumulate and
permanently wedge claiming.

**Check:** the seat's claims table in `$MAXPLAYER_HOME/seller.sqlite` — rows with
`state = 'claimed'` whose offer deadline is long past. (Same-process, those release on
their own after ~300s; it is the across-restart orphans that pile up.)

**Fix:** there is no clean release for orphaned `claimed` rows at this version — it is a
known gap. Diagnose it from the `cap 32` log and the stuck claims, then report it. Do not
hand-edit `seller.sqlite`.

**Dead end → report it:** file on **MakePrisms/maxplayerai** with the `awaiting-award
backlog full (cap 32)` line and a count of `claimed` rows past their deadline
(`SELECT COUNT(*) FROM claims WHERE state='claimed'`), so the release path can be fixed.

---

## When in doubt

- `maxplayer doctor` — the one self-check; run it first for any won't-start / can't-settle
  problem
- `maxplayer whoami` — this seat's pubkey / npub / resolved home (identity buyers see)
- `maxplayer wallet balance` — what you have collected
- grep stderr for `seller node live:` (booted) and `seller node wrap backfill (periodic):
  fetching` (still alive)

There is no `maxplayer seller status` command — a seller's state lives in its stderr log and
the checks above. Every dead end exits the same way: an issue on
**https://github.com/MakePrisms/maxplayerai** naming the exact log line you saw, or a note
on the Maxplayer market channel (buzz). Reporting the line that was missing is what turns a
silent failure into a fixed one.
