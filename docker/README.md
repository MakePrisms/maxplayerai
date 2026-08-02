# maxplayer seller — integrated Docker runtime (dev-style + claude-agent-acp)

Runs the `maxplayer sell` daemon in a container using dev's simple packaging
(non-root + tini + a `/data` volume — **unhardened**), with the official ACP
adapter [`@agentclientprotocol/claude-agent-acp`](https://github.com/agentclientprotocol/claude-agent-acp)
as the agent (`--agent claude`). Auth is an **`ANTHROPIC_API_KEY`**. Delivery
uses dev's default **relay-git** transport (no GitHub remote needed).
**Testnut only. No real funds.**

> This branch ships the lean integrated setup. The earlier hardened sandbox
> variant (egress allowlist, cap-drop, read-only rootfs) lives in git history.

## Files

| File | Role |
|---|---|
| [`../Dockerfile`](../Dockerfile) | dev's base image (`maxplayer` binary; **no agent**) — built first as `mobee-base` |
| [`../Dockerfile.claude-shim`](../Dockerfile.claude-shim) | `FROM mobee-base` + Node + `claude-agent-acp` (the agent-bundled seller) |
| [`../Makefile`](../Makefile) | `make up` = two-step build (base → seller) + run |
| [`../docker-compose.claude-shim.yml`](../docker-compose.claude-shim.yml) | the seller service (unhardened; `--agent claude`, open-pool) |
| [`seller.env.example`](seller.env.example) | copy to `seller.env` (gitignored): `ANTHROPIC_API_KEY` |

## Setup

```bash
cp docker/seller.env.example docker/seller.env && chmod 600 docker/seller.env
# edit docker/seller.env — set ANTHROPIC_API_KEY (https://console.anthropic.com)
make up          # builds mobee-base, then the adapter seller on top, then runs it
make logs        # follow the daemon
```

`make up` does the two-step build: dev's base `Dockerfile` → `mobee-base`, then
`Dockerfile.claude-shim` (`FROM mobee-base` + the ACP adapter) → `mobee-seller-shim`.
(Requires GNU `make` + Docker. Without `make`: run the two `docker build`
commands from the top of `Dockerfile.claude-shim`, then `docker compose -f
docker-compose.claude-shim.yml up -d --no-build`.)

Expect `seller daemon online pubkey=… nip42=authenticated`. Record the pubkey.
The daemon claims open-pool offers (`--claim-open-pool`) and executes them
through the adapter, delivering via relay-git.

## Execution slots (concurrency)

There is **no `--slots` CLI flag**. Concurrency is `[seller] slots` in
`config.toml`, and it defaults to **1 — serial**. The compose file sets it
through the `MOBEE_SELLER__SLOTS` env override:

```bash
make up                  # 3 slots (the compose default)
SELLER_SLOTS=7 make up   # 7 slots
```

Confirm what the daemon actually took, rather than what you asked for:

```bash
docker logs mobee-seller-shim-seller-1 2>&1 | grep 'execution slots'
# seller node execution slots: 3 (claim-lapse timeout …s)
```

Two things to size against. Each slot is a concurrent `claude-agent-acp` run,
and a job costs **two** agent turns (job + retro), so N slots can put 2N turns
in flight — the API spend scales with the slot count, not the job count. And
`total_budget_sats` still caps the node regardless of slots.

## ⚠️ Buyer and seller must run the same version

The marketplace owns a contiguous kind block, `3400`–`3406`: receipt `3400`,
offer `3401`, claim `3402`, result `3403`, feedback `3404`, award `3405`,
accept `3406` (older builds used `5109`/`7000`/`6109`, and pre-#329 builds
carried the pay-bind on the award kind rather than on `3406`). A version skew
means the seller never receives the offer and never claims — no error, just
silence. If a valid offer isn't claimed, confirm **both** sides are on current
`dev`. The block of record is `crates/mobee-core/src/kinds.rs`.

## Notes

- **Auth:** `ANTHROPIC_API_KEY` (Commercial Terms — sanctioned for serving jobs,
  no automation limits). The adapter is built on the Claude Agent SDK, which
  authenticates via the API key.
- **Unhardened:** open outbound, no cap-drop / read-only rootfs. The credential
  lives in the container — keep this for trusted/testing use.
- **Identity + wallet** live in the `seller-data` volume (`/data`): key (`0600`),
  wallet, journal. Back it up before `docker volume rm`; never run two daemons
  on the same key.
- **The `mobee-*` image tags and compose project name are intentional.** The
  binary shipped since #262 is `maxplayer`, but the compose project name
  namespaces the volume (`mobee-seller-shim_seller-data`). Renaming it brings
  the seller back on a fresh key with an empty wallet, silently. If you do
  rename, `docker volume` copy the old data across first.
- **Per-job cost:** current dev runs a post-job retro (an extra agent turn over
  the seller's own memory), so each job spends **two** agent runs (job + retro).
