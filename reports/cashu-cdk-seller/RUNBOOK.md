# Cashu/CDK specialist seat — runbook (HISTORICAL — first round, 2026-09-08/09)

> ⚠️ **Superseded for operations. Do not follow this file to deploy or start the seat.**
> Use **`RUNBOOK-ADDENDUM.md`**, which is the authoritative operational handoff.
>
> This file is retained unchanged below as the historical first-round record, because its evidence
> is real and was accepted. But three things in it are now **stale and wrong to act on**: the image
> tag `maxplayer-cashu-sandbox:v0.5.8-local`, the image id `eb91a9…`, and the "22/8" acceptance
> figure. The addendum carries the current immutable image id, its provenance bounds, the non-root
> bind-mounted replay recipe, and the human-only selected-field seat setup.
>
> Nothing below has been edited; only this banner was added.

Everything below was run first-hand on rocky's Mac Studio on 2026-09-08/09 (PDT). Nothing here is
relayed from a prior note.

## What exists

| thing | value |
|---|---|
| worktree | `~/forge/v2/wt/w-cashu-cdk-seller`, branch `w/cashu-cdk-seller` |
| base commit | `d7b94db2dbb7aeeefdcbb087edd0c90df56a8bdb` (upstream/main, v0.5.8) |
| upstream resolved by URL | `https://github.com/MakePrisms/maxplayerai.git` (remote name `upstream`; `origin` is the fork) |
| seat home | `~/forge/v2/seats/cashu-cdk-seller` (OUTSIDE the worktree, survives worktree removal) |
| seller pubkey | `ece4939aa2ac61c661891ef81295d169be1762e4b74994e4929d57882343a86b` |
| npub | `npub1anjf8x4z43suvcvfrmup99w3dxlpwchykayefeyjn4tcsg6r4p4sreewtj` |
| host binary | `<worktree>/target/release/maxplayer` — `maxplayer 0.5.8 (c449ba293c26ba1b625c9a11953022c7a801693f)` |
| base image | `maxplayer-sandbox:v0.5.8-local` `sha256:4b644531ffe3fdc0c4414aa8d6f9027c58a3f18124bb7abe61bb622973731eea` |
| specialist image | `maxplayer-cashu-sandbox:v0.5.8-local` `sha256:eb91a9bfa200226409f023cf0b15606e32463554bcf2841966d2140828a79cd3` |
| job network | docker user-defined network `maxplayer-cashu-jobs` |
| deployment location | this Mac Studio, in the colima VM profile `default` (docker 29.5.2, Ubuntu 24.04.4, aarch64, 4 CPU, 4 GiB) |

## Pinned versions

- `cashu` / `cdk` / `cdk-sqlite` — `=0.17.2` (the workspace's own pins; the mint is built at the
  same version so a protocol mismatch cannot be mistaken for a product bug).
- `cdk-mintd` `0.17.2`, built `--no-default-features --features fakewallet,sqlite`.
- corpus: `cashubtc/nuts` @ `49a909ce4d0739824b3859d4b3da21e6c1abdaeb` (2026-08-23);
  `cashubtc/cdk` @ `6132607495ae0741e412a63f2acc34e4ccddfc55` (tag `v0.17.2`).
- base image ancestry: `nixos/nix:2.31.2`, `rust:1-bookworm`, `node:22-bookworm-slim`,
  `debian:bookworm-slim`.

## THE ACCEPTANCE COMMAND (run this one line)

```sh
docker run --rm maxplayer-cashu-sandbox:v0.5.8-local bash -c 'test-mint start >/dev/null && mint-acceptance'
```

No real funds, no external payment, no host state touched, container is `--rm`. Exit 0 = pass; it
prints an assertion count and every amount it asserted. Last run: **PASS — 22 assertions across 8
legs** (`acceptance-run-20260909.txt`).

## Rebuilding the images

The host docker CLI has **no buildx plugin**, so `docker build` cannot honour the repo's
`RUN --mount=type=cache` lines. The colima VM has buildx v0.34.1 and mounts `/Users/forge` over
virtiofs at the same path, so build inside the VM:

```sh
colima start          # if not running
colima ssh -- bash -lc 'cd /Users/forge/forge/v2/wt/w-cashu-cdk-seller && \
  docker buildx build -f docker/maxplayer-sandbox/Dockerfile \
    --build-arg MAXPLAYER_BUILD_COMMIT=$(git rev-parse HEAD) \
    -t maxplayer-sandbox:v0.5.8-local .'
colima ssh -- bash -lc 'cd /Users/forge/forge/v2/wt/w-cashu-cdk-seller && \
  docker buildx build -f docker/maxplayer-cashu-sandbox/Dockerfile \
    --build-arg BASE_IMAGE=maxplayer-sandbox:v0.5.8-local \
    -t maxplayer-cashu-sandbox:v0.5.8-local .'
```

The base image is built locally because **`ghcr.io/makeprisms/maxplayer-sandbox:v0.5.8` does not
exist** — the anonymous tag list ends at `v0.5.7`; 0.5.8 was cut at 19:23 PDT on 2026-09-08 and CI
has not pushed the image. When CI publishes it, `BASE_IMAGE` can point at the registry tag instead.

## The test mint

Inside any container from the specialist image:

```sh
test-mint start | status | info | restart | reset | logs [n] | url
```

- Binds `127.0.0.1:8085` and refuses any non-loopback bind.
- State in `$TEST_MINT_WORK_DIR` (default `/work/.test-mint`), dies with the container.
- `restart` keeps the database (restart-recovery testing); `reset` is the clean slate.
- Seed is a throwaway BIP-39 mnemonic generated per reset into a `0600` file, passed by
  `--seed-file`. Never printed, never an argument, never baked into the image.

**This mint is never added to any seller's `accepted_mints`.** The seat's accepted-mint set is
unchanged from the shipped default (`mint.minibits.cash`); doctor confirms "all 1 accepted mint(s)
reachable".

## Running the seat

```sh
cd ~/forge/v2/wt/w-cashu-cdk-seller
./target/release/maxplayer doctor --home ~/forge/v2/seats/cashu-cdk-seller
./target/release/maxplayer seller --agent claude --rate-sats 500 \
    --name "cashu-cdk specialist" --home ~/forge/v2/seats/cashu-cdk-seller
```

Deliberately **without** `--claim-open-pool` and **without** `--accept-open-targeted`, and with no
`[seller] accept_offers_only_from`. In that configuration the node states at boot that it claims
nothing. No job obligation, no payment path, no real-money operation enabled.

`config.toml` in the seat home carries only `[sandbox]` (mode/image/network) plus the `[seller]`
section the first boot wrote. No unrelated setting was replaced.

## Stopping / cleaning up

```sh
# seat
kill <maxplayer seller pid>
# job network (only if nothing else uses it)
docker network rm maxplayer-cashu-jobs
# container runtime, back to how it was found
colima stop
```

The colima VM was found **Stopped** and was started for this job. Nothing was installed on the
host; no infrastructure was acquired.
