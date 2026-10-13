# Vendored Buzz relay (`crates/buzz/`)

This directory vendors the **Buzz** events + git + payments relay into
maxplayerai as owned code, so we can build and deploy a relay that already
ingests the Mobee job kinds. It is a near-verbatim copy of upstream Buzz kept in
its original layout; see [`NOTICE`](./NOTICE) for the license and the exact list
of modifications.

## Decision record

### Source
block/buzz `main` @ `30873ba46` (2026-10-11), merged into the previous copy so
the open-relay fork's changes and every Maxplayer change carry over. The merge
took in 1,056 upstream commits.

The previous copy was gudnuf/buzz `sync/upstream-recarry-2026-07-24` @
`e18020a63` (based on block/buzz `3bd3a014c`, 2026-07-24). It had been chosen
over `metadex/mobee-kind-scope` (`c2499afd9`) because it fully contained that
branch's kind commit on a fresher base.

### Mobee kinds accepted
Both families below are defined as consts in
`crates/buzz-relay/src/handlers/ingest.rs` and scoped in `required_scope_for_kind`
(trade block + heartbeat -> `MessagesWrite`; the discovery/config kinds 31990 &
10050 -> `UsersWrite`), covered by `mobee_kinds_require_messages_write_scope`.

**A. The fork's DVM kinds** (carried verbatim from the source branch, untouched):

| const | kind | note |
|-------|------|------|
| `KIND_MOBEE_JOB_RECEIPT`  | 3400 | co-signed receipt |
| `KIND_MOBEE_JOB_OFFER`    | 5109 | NIP-90 job-request range |
| `KIND_MOBEE_JOB_RESULT`   | 6109 | NIP-90 job-result range |
| `KIND_MOBEE_JOB_FEEDBACK` | 7000 | NIP-90 feedback |

**B. The maxplayer-core kind superset** (added per keeper:mobee ruling — smallest
divergence, mirroring `crates/maxplayer-core/src/kinds.rs`):

| const | kind | note |
|-------|------|------|
| `KIND_MOBEE_TRADE_OFFER`      | 3401 | buyer offer |
| `KIND_MOBEE_TRADE_CLAIM`      | 3402 | seller claim (bid + creq invoice) |
| `KIND_MOBEE_TRADE_RESULT`     | 3403 | seller result |
| `KIND_MOBEE_TRADE_FEEDBACK`   | 3404 | seller progress / error / refusal |
| `KIND_MOBEE_TRADE_AWARD`      | 3405 | buyer award (claim selection) |
| `KIND_MOBEE_TRADE_ACCEPT`     | 3406 | buyer accept (pay-bind, #329) |
| `KIND_MAXPLAYER_REVIEW` | 3408 | signed execution-safety assessment |
| `KIND_MAXPLAYER_REVIEW_REQUEST` | 3409 | request an execution-safety assessment |
| `KIND_MOBEE_SELLER_HEARTBEAT` | 30340 | addressable seller liveness |
| `KIND_MOBEE_NIP89_HANDLER`    | 31990 | NIP-89 handler advertisement |
| `KIND_MOBEE_DM_RELAY_LIST`    | 10050 | NIP-17 DM-relay list (size-bounded, no `p`-tag req) |

3400 is shared between the two families. kind-0 profile and the 30617 NIP-34
git-repo announcement mobee also emits are **already** scoped by the relay
(`KIND_PROFILE` -> `UsersWrite`, `KIND_GIT_REPO_ANNOUNCEMENT` -> `ReposWrite`).

> **Two flags for reviewers:**
> 1. **Numbering mismatch (DVM vs maxplayer-core).** The fork uses NIP-90 DVM numbers
>    (5109/6109/7000); maxplayer-core owns the contiguous 3400-3407 block, of which this
>    relay accepts 3400-3406. Both are now accepted. The brief's "3401-3405 + 31990" was
>    close but off: the accepted trade block runs to **3406** (ACCEPT #329 — omitting it
>    would reject live pay-bind events; 3407 REJECT is not accepted here), and 31990 lives
>    in `mobee-relay-write-policy`'s `DISCOVERY_KINDS`, not `kinds.rs`.
> 2. **Provenance.** Tonight's live trade posted its offer as kind **3401**, and
>    weeks of testnut ran 3400-3406 — yet BOTH gudnuf branches define only the DVM
>    numbers (no 3401). So the **deployed relay matches neither vendored branch**;
>    the fork here is an older DVM iteration. Worth locating the branch/rev that
>    actually carries 3401 — but the superset accepts both, so nothing gates on it.
>
> **Scope (keeper:mobee-ruled):** the trade block 3400-3406 and heartbeat 30340
> → `MessagesWrite`; the self-describing discovery/config kinds 31990 (NIP-89) and
> 10050 (DM-relay list) → `UsersWrite`, grouped with kind-0's identity class so an
> auth-tiered deploy inherits the right permission surface. (Moot on today's open
> relay, where every authed key holds all scopes.)

### Crates taken vs left
Taken: `buzz-relay` and every workspace crate it depends on — `buzz-audit,
buzz-auth, buzz-conformance, buzz-core, buzz-datastore-tracing, buzz-db,
buzz-deletion, buzz-media, buzz-pubsub, buzz-relay-mesh, buzz-sdk,
buzz-search, buzz-workflow` — plus Maxplayer's `maxplayer-private-protocol`.

`buzz-search` (typesense) and `buzz-workflow` are **hard** `[dependencies]` of
the relay and cannot be dropped without editing relay source, so they are in
despite being client/harness-adjacent. `buzz-deletion` and
`buzz-datastore-tracing` became relay dependencies upstream after the previous
copy. The AI-agent harness, push gateway, device pairing, desktop/mobile/web
clients, and CLIs are left behind (see `NOTICE`).

Also vendored from upstream, verbatim:
- `migrations/`, embedded at compile time by `buzz-db`'s migrator. Maxplayer's
  own migrations use versions from 100000 up, so upstream's four-digit
  sequence never collides with them.
- `schema/`, upstream's desired-state schema. Tests check it against the
  migrations; production never applies it.
- `Dockerfile`, `docs/nips/NIP-MP.fixtures.json` and
  `scripts/reconcile-schema-after-pgschema.sql`, which tests read, and
  `docs/thread-window-deployment.md` (see Upgrading).

### Dependency alignment
- **nostr: no collision.** The relay's lock resolves `nostr 0.44.7`; the
  maxplayerai root lock has `nostr 0.44.4` / `nostr-sdk 0.44.1`. Separate
  locks, same 0.44 line.
- Everything else resolves from the vendored `Cargo.lock`.

### Isolation approach
`crates/buzz/` is a **self-contained nested Cargo workspace** (its own
`Cargo.toml` with `[workspace]`, its own `Cargo.lock`, `resolver = 2`,
`edition = 2021`). maxplayerai's root workspace lists members explicitly and does
**not** glob, so the vendored workspace is invisible to it and needs no
reconciliation of the maxplayerai root manifest. The one link is
`maxplayer-core`'s path dependency on `crates/maxplayer-private-protocol`,
which the root workspace builds with its own lock. Unifying into a single
workspace, if ever wanted, is a separate follow-up.

### License
Upstream is Apache-2.0; its `LICENSE` is kept here and `NOTICE` records the
modifications per Apache §4(b). maxplayerai is MIT-OR-Apache-2.0 → clean.

### #397 gift-wrap payment bound
Added natively in `ingest.rs`: kind 1059 (NIP-17 gift wrap) must carry >=1 `p`
tag and content <= 128 KB. The remainder of #397's write-policy is folded as
native code by market-orch — this vendor does **not** add a namespace/`t`-tag
predicate; the kind allowlist is the scoping.

### Branch-scoped push tokens and their lifetime
Two changes serve the maxplayer seller delivery push (see `NOTICE`, part B):

1. **Scope (PR #929).** A NIP-98 push token may carry one `["ref", "<refname>"]` tag.
   The pre-receive policy endpoint denies the push (403) unless every ref update
   matches that name exactly — no glob, no prefix. A token with no `ref` tag behaves
   as before.
2. **Lifetime.** A SCOPED token that also carries a NIP-40 `["expiration", <unix>]`
   tag is accepted while `now <= expiration`, up to a cap. UNSCOPED tokens keep the
   ±60 s NIP-98 window, always. The cap is
   `BUZZ_SCOPED_TOKEN_MAX_LIFETIME_SECS` (default 21600 s = 6 h; the NixOS option is
   `services.maxplayer.relay.scopedTokenMaxLifetimeSecs`), and the relay advertises the
   effective value in its NIP-11 `limitation` object as
   `scoped_token_max_lifetime_secs`.

The seller mints one token on the host before a sandboxed job starts and pushes with
it minutes later, so the ±60 s window cannot serve that push. The scope is what makes
the longer life safe: a leaked scoped token can only write one branch of one repo.
Ship the two together — a long-lived UNSCOPED token would be a push-anywhere
credential. Work order:
`docs/superpowers/briefs/2026-08-31-relay-scoped-token-lifetime.md`.

## Building

The relay needs **rustc >= 1.94** (sqlx 0.9). From the maxplayerai repo root:

```sh
nix build .#maxplayer-relay   # the deployed package, built with the flake's rustc 1.96
# or, with rustc >= 1.94, pkg-config and cmake (for aws-lc-sys):
cargo build --manifest-path crates/buzz/Cargo.toml --locked -p buzz-relay --bin buzz-relay
```

`flake.nix` vendors the dependencies under a fixed `cargoHash`. After a
`Cargo.lock` change, set it to `pkgs.lib.fakeHash`, build once, and copy in the
hash nix reports. Keep the lock free of two crates with the same name and
version: fetchCargoVendor cannot vendor them.

## Testing

```sh
cargo test --manifest-path crates/buzz/Cargo.toml --locked --workspace --lib --tests
```

A few `api::media` tests need Postgres at `DATABASE_URL`, built from
`schema/schema.sql` (`psql -f schema.sql` from `schema/`), and Redis at
`REDIS_URL`. The tests marked
`#[ignore = "requires Postgres"]` run through upstream's
`scripts/postgres-test-run.sh`, which is not vendored and gives each test its
own database: run them in a block/buzz checkout that carries this tree (see
Syncing upstream again). The private-job HTTP tests run here, against a
disposable database and Redis:

```sh
PRIVATE_JOB_TEST_DATABASE_URL=postgres://… PRIVATE_JOB_TEST_REDIS_URL=redis://… \
  cargo test --manifest-path crates/buzz/Cargo.toml -p buzz-relay --lib \
  private_jobs_http_tests -- --include-ignored --test-threads=1
```

## Upgrading a deployed relay

With `BUZZ_AUTO_MIGRATE=1` the relay migrates its database at startup, before
it binds its port. Each migration runs in its own transaction; if one fails,
the relay exits without serving and the next start retries it.

- A database migrated by the relay before this sync records the private-job
  migration as version 25. The relay moves that record to 100025, then applies
  upstream's 0025–0060 and Maxplayer's 100026.
- The upgrade is one-way: the earlier relay then refuses to start ("migration
  26 was previously applied but is missing in the resolved migrations"). Back
  up right before switching; rolling back means restoring that backup.
- Migration 0033 rewrites `events` (every partition) under an exclusive lock,
  so the first start takes longer the bigger `events` is. Because it is
  partitioned, `pg_total_relation_size('events')` reports 0 bytes; measure it
  with `SELECT pg_size_pretty(sum(pg_total_relation_size(relid))) FROM
  pg_partition_tree('events')`.
- Migration 0049 stops after a 1 s lock wait or a 5 s index build rather than
  block writes. If `thread_metadata` is large
  (`SELECT pg_size_pretty(pg_total_relation_size('thread_metadata'))`),
  prebuild the index first, while the current relay runs, as
  `docs/thread-window-deployment.md` describes.
- Several new migrations wait at most 5 s for a table lock. Start the relay
  while nothing else holds one, for example not during a `pg_dump`.
- The writer pool default grew from 20 to 50 connections
  (`BUZZ_DB_POOL_SIZE`).
- The relocation logs at `buzz_db` info, which the default log filter
  (`buzz_relay=info`) hides. To check the result, `SELECT version, description
  FROM _sqlx_migrations WHERE version IN (25, 100025, 100026)` lists `relay
  invites`, `private job repositories` and `private job repositories write
  fence`.

## Syncing upstream again

The merge base for the next sync is block/buzz `30873ba46`.

1. In a block/buzz clone, branch from `30873ba46`, copy this directory over it
   (`cp -a crates/buzz/. <clone>/`) and commit. That commit is the Maxplayer
   delta.
2. Merge the new upstream commit. Keep ours for `NOTICE`, `README.md`,
   `.gitignore` and the trimmed `members`, adding any crate the relay newly
   depends on. Take upstream's `Cargo.lock` and let cargo prune it, then check
   that every version left is upstream's, apart from the sqlite exception
   NOTICE names. Upstream migrations keep their numbers.
3. Build and test there; the Postgres tests need that full checkout.
4. Replace this directory with the result: `git rm -rq crates/buzz`, then
   `mkdir crates/buzz && git -C <clone> archive HEAD -- <paths in NOTICE> |
   tar -x -C crates/buzz` and `git add crates/buzz`. Update the commit in
   `NOTICE` and refresh `cargoHash`.
