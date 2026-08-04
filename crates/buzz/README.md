# Vendored Buzz relay (`crates/buzz/`)

This directory vendors the **Buzz** events + git + payments relay into
maxplayerai as owned code, so we can build and deploy a relay that already
ingests the Mobee job kinds. It is a near-verbatim copy of upstream Buzz kept in
its original layout; see [`NOTICE`](./NOTICE) for the license and the exact list
of modifications.

## Decision record

### Source branch
`sync/upstream-recarry-2026-07-24` @ `e18020a63`, from `github.com/gudnuf/buzz`
(fork of `block/buzz`).

Chosen over `metadex/mobee-kind-scope` (`c2499afd9`) because:
- It **fully contains** the metadex kind commit (0 commits are metadex-only; it
  is 447 commits ahead), so nothing is lost.
- It sits **147 commits behind** upstream `block/buzz` vs metadex's 584 — i.e. it
  is the fresher base, closest to what orveth actually deploys.
- Both branches carry the **identical** Mobee kind set.

### Mobee kinds accepted
Both families below are defined as consts in
`crates/buzz-relay/src/handlers/ingest.rs`, mapped to `Scope::MessagesWrite` in
`required_scope_for_kind`, and covered by `mobee_job_kinds_require_messages_write_scope`.

**A. The fork's DVM kinds** (carried verbatim from the source branch, untouched):

| const | kind | note |
|-------|------|------|
| `KIND_MOBEE_JOB_RECEIPT`  | 3400 | co-signed receipt |
| `KIND_MOBEE_JOB_OFFER`    | 5109 | NIP-90 job-request range |
| `KIND_MOBEE_JOB_RESULT`   | 6109 | NIP-90 job-result range |
| `KIND_MOBEE_JOB_FEEDBACK` | 7000 | NIP-90 feedback |

**B. The mobee-core kind superset** (added per keeper:mobee ruling — smallest
divergence, mirroring `crates/mobee-core/src/kinds.rs`):

| const | kind | note |
|-------|------|------|
| `KIND_MOBEE_TRADE_OFFER`      | 3401 | buyer offer |
| `KIND_MOBEE_TRADE_CLAIM`      | 3402 | seller claim (bid + creq invoice) |
| `KIND_MOBEE_TRADE_RESULT`     | 3403 | seller result |
| `KIND_MOBEE_TRADE_FEEDBACK`   | 3404 | seller progress / error / refusal |
| `KIND_MOBEE_TRADE_AWARD`      | 3405 | buyer award (claim selection) |
| `KIND_MOBEE_TRADE_ACCEPT`     | 3406 | buyer accept (pay-bind, #329) |
| `KIND_MOBEE_SELLER_HEARTBEAT` | 30340 | addressable seller liveness |
| `KIND_MOBEE_NIP89_HANDLER`    | 31990 | NIP-89 handler advertisement |
| `KIND_MOBEE_DM_RELAY_LIST`    | 10050 | NIP-17 DM-relay list (size-bounded, no `p`-tag req) |

3400 is shared between the two families. kind-0 profile and the 30617 NIP-34
git-repo announcement mobee also emits are **already** scoped by the relay
(`KIND_PROFILE` -> `UsersWrite`, `KIND_GIT_REPO_ANNOUNCEMENT` -> `ReposWrite`).

> **Two flags for reviewers:**
> 1. **Numbering mismatch (DVM vs mobee-core).** The fork uses NIP-90 DVM numbers
>    (5109/6109/7000); mobee-core uses the contiguous 3400-3406 block. Both are now
>    accepted. The brief's "3401-3405 + 31990" was close but off: mobee-core's block
>    runs to **3406** (ACCEPT #329 — omitting it would reject live pay-bind events),
>    and 31990 lives in `mobee-relay-write-policy`'s `DISCOVERY_KINDS`, not `kinds.rs`.
> 2. **Provenance.** Tonight's live trade posted its offer as kind **3401**, and
>    weeks of testnut ran 3400-3406 — yet BOTH gudnuf branches define only the DVM
>    numbers (no 3401). So the **deployed relay matches neither vendored branch**;
>    the fork here is an older DVM iteration. Worth locating the branch/rev that
>    actually carries 3401 — but the superset accepts both, so nothing gates on it.
>
> **Scope note:** all eight added kinds use `MessagesWrite` (matching the DVM arm).
> 31990 is discovery-class (grouped with kind-0 in mobee's `DISCOVERY_KINDS`); if
> keeper:mobee prefers `UsersWrite` for it, that is a one-line change.

### Crates taken vs left
Taken (12 = the relay's transitive `[dependencies]` closure):
`buzz-relay, buzz-core, buzz-conformance, buzz-db, buzz-pubsub, buzz-auth,
buzz-search, buzz-audit, buzz-workflow, buzz-media, buzz-sdk, buzz-relay-mesh`.

`buzz-search` (typesense) and `buzz-workflow` are **hard** `[dependencies]` of
the relay and cannot be dropped without editing relay source, so they are in
despite being client/harness-adjacent. The AI-agent harness, device pairing,
desktop/mobile/web clients, and CLIs are left behind (see `NOTICE`).

Also vendored (verbatim, DB artifacts the relay's schema layer needs):
`migrations/` (embedded at compile time by `buzz-db`'s
`sqlx::migrate!("../../migrations")`) and `schema/schema.sql` (the canonical
fresh-database schema; drift-checked against `migrations/0001_initial_schema.sql`).

### Dependency alignment
- **nostr-sdk: no collision.** maxplayerai `mobee-core` pins `nostr-sdk 0.44.1`;
  this vendored relay resolves `nostr 0.44.x` / `nostr-sdk 0.44.1`. Both on 0.44.
- sqlx 0.9, tokio 1, axum 0.8 resolve from the vendored `Cargo.lock`.

### Isolation approach
`crates/buzz/` is a **self-contained nested Cargo workspace** (its own
`Cargo.toml` with `[workspace]`, its own `Cargo.lock`, `resolver = 2`,
`edition = 2021`). maxplayerai's root workspace lists members explicitly and does
**not** glob, so the vendored workspace is invisible to it and needs no
reconciliation of the maxplayerai root manifest. This deliberately sidesteps
dep-version reconciliation (nostr/sqlx/tokio) for the initial vendor-in; unifying
into a single workspace, if ever wanted, is a separate follow-up owned by nix/dep
reconciliation.

### License
Upstream is Apache-2.0; its `LICENSE` is kept here and `NOTICE` records the
modifications per Apache §4(b). maxplayerai is MIT-OR-Apache-2.0 → clean.

### #397 gift-wrap payment bound
Added natively in `ingest.rs`: kind 1059 (NIP-17 gift wrap) must carry >=1 `p`
tag and content <= 128 KB. The remainder of #397's write-policy is folded as
native code by market-orch — this vendor does **not** add a namespace/`t`-tag
predicate; the kind allowlist is the scoping.

## Building

The relay's `sqlx 0.9` requires **rustc >= 1.94**. The maxplayerai nix devShell
(nixos-25.11) currently provides 1.91.1, so overlay a newer toolchain:

```sh
# from the maxplayerai repo root
nix develop -c nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#pkg-config nixpkgs#cmake -c \
  cargo build --manifest-path crates/buzz/Cargo.toml -p buzz-relay
```

**For nix packaging / deploy (market-orch):** the relay needs its own
`buildRustPackage` derivation with `cargoLock.lockFile = ./crates/buzz/Cargo.lock`
and `cargoBuildFlags = ["-p", "buzz-relay"]`, built with rustc >= 1.94 and
`nativeBuildInputs = [ pkg-config cmake ]` (cmake for aws-lc-sys via rustls). The
`[patch.crates-io]` `aws-creds` git fork in the vendored `Cargo.toml` must be
preserved.
