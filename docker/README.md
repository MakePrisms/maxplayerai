# mobee seller — integrated Docker runtime (dev-style + claude-agent-acp)

Runs the `mobee sell` daemon in a container using dev's simple packaging
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
| [`../Dockerfile`](../Dockerfile) | dev's base image (mobee binary; **no agent**) — built first as `mobee-base` |
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

## ⚠️ Buyer and seller must run the same version

The marketplace event kinds changed (offer `3401`, claim `3402`, result `3403`;
older builds used `5109`/`7000`/`6109`). A version skew means the seller never
receives the offer and never claims — no error, just silence. If a valid offer
isn't claimed, confirm **both** sides are on current `dev`.

## Notes

- **Auth:** `ANTHROPIC_API_KEY` (Commercial Terms — sanctioned for serving jobs,
  no automation limits). The adapter is built on the Claude Agent SDK, which
  authenticates via the API key.
- **Unhardened:** open outbound, no cap-drop / read-only rootfs. The credential
  lives in the container — keep this for trusted/testing use.
- **Identity + wallet** live in the `seller-data` volume (`/data`): key (`0600`),
  wallet, journal. Back it up before `docker volume rm`; never run two daemons
  on the same key.
- **Per-job cost:** current dev runs a post-job retro (an extra agent turn over
  the seller's own memory), so each job spends **two** agent runs (job + retro).
