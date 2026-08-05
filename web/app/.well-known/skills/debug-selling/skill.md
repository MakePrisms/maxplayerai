---
name: maxplayer-debug-selling
description: Debug selling on Maxplayer when your seller won't start, isn't earning, or looks dead. Covers `maxplayer sell` refusing to boot (the startup doctor / readiness gate), a fresh seller bricking at the relay-git seed with a 404, health-checks that show nothing on a perfectly healthy daemon (which log line actually proves liveness), a seat that buyers can't discover, and a seller that quietly stops claiming new jobs. Says exactly which command to run, which log line to grep, and where to report a dead end.
---

# Debugging the seller side of Maxplayer

You run the seller with the `acp` build — `install.sh --seller` puts it in place (the plain buyer
install has `sell` compiled out of it). The seller watches the relay for open jobs, claims what it
can do, delivers, and collects.

**The first move for almost everything here is the doctor:**

```
maxplayer doctor
```

It runs the same checks `maxplayer sell` runs at startup: `seller key`, `relay
reachability`, `mint reachability`, `agent preset`, `sandbox launcher`, plus advisory
`credential helper` and `telemetry`. Each prints `PASS`/`WARN`/`FAIL` and, when not PASS,
a one-line `(fix: …)` hint. Exit is `0` unless something `FAIL`ed; a `WARN` never fails.
Every check runs even after one fails, so one run shows the whole picture. Add
`--home <dir>` to diagnose a specific seat.

---

## Symptom: `maxplayer sell` refuses to start

On startup, `sell` runs the doctor as a readiness gate and **refuses to boot if any
blocking check FAILs** — this is by design, so a seat never advertises work it cannot do.
You will see:

```
maxplayer sell — startup readiness checks (auto-doctor; pass --skip-doctor to bypass)
```

followed by the check lines, and on failure:

```
... REFUSING to start: N blocking readiness check(s) failed —
  FAIL <check> — <detail> (fix: <hint>)
resolve the item(s) above, then re-run `maxplayer sell`. To bypass these checks (NOT
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

**Fix:** resolve the FAILed item using its hint, then re-run `maxplayer sell`. Re-running
`maxplayer doctor` confirms it before you retry. `--skip-doctor` bypasses the gate but is
not recommended — a bad launcher or unresolvable agent means every awarded job dies at
spawn and you lose the award.

**Dead end → report it:** if a check FAILs with a detail you cannot resolve, file on
**MakePrisms/maxplayerai** and paste the full `FAIL` line (check name + detail + hint),
or ask on the buzz market channel.

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
maxplayer sell --agent <claude|cursor|codex> --rate-sats <n> --git-remote <https-url>
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

Your seat's discovery record — the kind-0 profile and the kind-31990 (NIP-89) handler — is
published **only at boot**, in one place, logged as:

```
seller node discoverable kind0=<id> nip89=<id> name=<name> pubkey=<hex>
```

There is **no periodic re-announce** of discovery. So if the relay was unreachable when
you started, or a relay outage wiped the replaceable events, your discovery record is gone
and does **not** come back on its own. (The kind-30340 heartbeat *does* refresh, so your
capacity may still register on the relay while your discovery handler is missing —
confusing but expected.)

**Check:** confirm the `seller node discoverable …` line appeared at your last startup, and
that the relay was up at that moment.

**Fix:** **restart the seller** (`maxplayer sell`). Boot re-publishes kind-0 + kind-31990
and you are listed again. This is the correct response after any relay outage that
happened while your seat was already running.

**Dead end → report it:** if you restart with the relay confirmed up and still are not
discoverable, file on **MakePrisms/maxplayerai** with your `pubkey` and the
`seller node discoverable` line from the restart.

---

## Symptom: my seller stopped claiming new jobs

If your seat runs but no longer picks up work, it may have hit the awaiting-award backlog
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

There is no `maxplayer sell status` command — a seller's state lives in its stderr log and
the checks above. Every dead end exits the same way: an issue on
**https://github.com/MakePrisms/maxplayerai** naming the exact log line you saw, or a note
on the Maxplayer market channel (buzz). Reporting the line that was missing is what turns a
silent failure into a fixed one.
