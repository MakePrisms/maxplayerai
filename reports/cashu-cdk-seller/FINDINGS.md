# cashu-cdk-seller — measured findings (worker lane)

Worktree: `~/forge/v2/wt/w-cashu-cdk-seller`, branch `w/cashu-cdk-seller`
Base: `d7b94db2dbb7aeeefdcbb087edd0c90df56a8bdb` (upstream `https://github.com/MakePrisms/maxplayerai.git` main, "release: cut v0.5.8 (#984)", 2026-09-08 19:23:56 -0700)
Workspace version: `0.5.8` (root `Cargo.toml`)

## Host / runtime (measured 2026-09-08 ~21:11 PDT)

- Docker daemon was DOWN. Existing colima profile `default` (aarch64, 4 CPU, 4 GiB, 40 GiB, docker
  runtime) was Stopped; started it. `docker info` now: server `29.5.2`, Ubuntu 24.04.4 LTS, aarch64,
  4 cpu, 4094005248 bytes. No infrastructure acquired; reversible with `colima stop`.
- Pre-existing images in that VM: buzz, minio, minio/mc, postgres:17-alpine, redis:7-alpine.

## CDK pins at this head (first-hand, not from the roster note)

`crates/maxplayer-core/Cargo.toml`: `cashu = "=0.17.2"` (:59), `cdk = "=0.17.2"` default-features
off + `wallet` (:80), `cdk-sqlite = "=0.17.2"` (:81, and :115 dev). `crates/maxplayer/Cargo.toml`:
same three at `=0.17.2` (:105-107). Gated behind the `wallet` feature (:41).

Cargo registry cache already holds 0.17.2 sources for: cashu, cdk, cdk-axum, cdk-cli, cdk-common,
cdk-fake-wallet, cdk-http-client, **cdk-mintd**, cdk-prometheus, cdk-signatory, cdk-sql-common,
cdk-sqlite.

## CORRECTION to the roster note's mintd config

The roster note (addendum-1-damian-roster.md §1) states `[payment_backend] backend = "fakewallet"`
and `CDK_MINTD_LN_BACKEND=fakewallet`. That is **not** the schema at the pinned 0.17.2. Read from
`~/.cargo/registry/src/index.crates.io-*/cdk-mintd-0.17.2/`:

- `src/config.rs:1002` `struct Settings` fields: `info`, `mint_info`, `ln: Vec<Ln>`
  (`deserialize_ln`), `onchain`, `limits`, ... `fake_wallet: Option<FakeWallet>` (:1019-1020),
  `database`, `mint_management_rpc`.
- `example.config.toml:116-119`: section is `[ln]` with `ln_backend = "fakewallet"`. Repeat
  `[[ln]]` for one backend per unit; duplicate (unit, method) pairs are rejected at startup.
  Comment at :118 — "NOTE: fakewallet is isolated testing mode and cannot be mixed with real
  payment backends."
- `example.config.toml:283` `[fake_wallet]` with `fee_percent`, `reserve_fee_min`,
  `custom_payment_methods`, `min_delay_time`, `max_delay_time`, optional
  `[[fake_wallet.keyset_rotations]]` (unit / version v1|v2 / input_fee_ppk / expired) for
  inactive/expired test keysets.
- `[onchain] onchain_backend = "fakewallet"` also exists (:126).
- Default listen is `[info] listen_host = "127.0.0.1"`, `listen_port = 8085` — loopback by default,
  which matches the containment requirement.
- `Cargo.toml [features] default` DOES include `fakewallet` (plus management-rpc, cln, lnd, lnbits,
  grpc-processor, sqlite, info-page, bdk). `fakewallet = ["dep:cdk-fake-wallet"]`. So a
  minimal-feature build for the test mint is `--no-default-features --features fakewallet,sqlite`
  (to be verified by building).

The roster note's default listen `127.0.0.1:8085` claim is CONFIRMED.

## Sandbox image

- Default image is `ghcr.io/makeprisms/maxplayer-sandbox:v<CARGO_PKG_VERSION>` —
  `crates/maxplayer-core/src/seller_exec.rs:236`.
- **v0.5.8 is NOT published.** ghcr tags list (anonymous pull token, 2026-09-08): v0.5.0-rc1..rc5,
  v0.5.0, latest, v0.5.1-rc1, v0.5.2, v0.5.3, v0.5.4, v0.5.5, v0.5.6, **v0.5.7** — no v0.5.8.
  The release was cut tonight and CI has not pushed the image yet.
- `docker/maxplayer-sandbox/Dockerfile` builds from the repo root, multi-stage: nixos/nix:2.31.2
  store copy + a cargo builder stage for the `maxplayer` binary (`acp,wallet`), with cache mounts.

## accepted_mints — the line to hold

`crates/maxplayer-core/src/payment_wallet.rs` enforces the seller's advertised mint set twice:
`:207,:231` realized mint must be in `accepted_mints` else `wrong_mint`; `:253-263` the NUT-18
payload mint must be in `accepted_mints` AND equal the declared mint. The test mint must never
appear there.

## Containment

`docs/SANDBOXING.md` §1: loopback is never denied inside the job network namespace (docker's
embedded resolver at 127.0.0.11 lives there); denies are destination-scoped on RFC1918, CGNAT
100.64/10, link-local 169.254/16, 198.18/15, multicast, reserved, and the host itself, across both
address families, in `INPUT` and `DOCKER-USER`. Policy source: `crates/maxplayer-core/src/sandbox_net.rs`.
So a mint bound to 127.0.0.1 inside the job container is reachable by the job and by nothing else.
