# Mobee deployment & packaging

> **Status (dev tip) — read this first.** Today `flake.nix` ships exactly one artifact:
> `packages.default` — the `mobee` client binary (built with `--features acp`) — plus
> `apps.default` (`nix run … -- mcp|sell`) and a `devShells.default`. The multi-service backend
> described below (relay / relay-git / blossom / Caddy / Postgres, Docker Compose, NixOS modules,
> split `packages.*`) is the **target architecture — not yet in-tree.** Read the rest of this file as
> the deployment *design + roadmap*; verify any target against `flake.nix` before relying on it.

This is the self-host design for the mobee marketplace: one Rust workspace, Nix as the packaging
foundation, targeting two runtimes (Docker, NixOS/systemd) across three operator personas
(relay-operator, seller, buyer). Only the client binary above exists today; the rest is roadmap.

## Principle

Nix + Rust is the foundation: the whole system is one cargo workspace, so every
persona is a package built from the same source and pinned by one `Cargo.lock` +
`flake.lock`. Docker images are built *from* the Nix packages (not a parallel
Dockerfile toolchain), so there is a single build path and no drift between the
two runtimes.

## Components (the backend bundle)

A mobee marketplace backend is three services behind one reverse proxy:

1. **Relay** — a nostr relay in *open mode* (open ingest + open read) accepting
   the mobee event kinds: 0 (profiles), 3401 (offer), 3402/3404 (claim/feedback),
   3403 (result), 3400 (receipt), 31990 (NIP-89 announce), 1059 (NIP-17
   gift-wrap payment). This is the coordination surface. Reference impl =
   buzz-relay in open mode; the contract is "any nostr relay that accepts these
   kinds without membership."
2. **relay-git** — a git-over-HTTP endpoint serving `/git/<owner>/<repo>`. This
   is the **primary git-management transport** and stays that way: delivery is
   git-objects, verified by the buyer tip-matching the exact commit OID before
   paying. Keeping git as git is what makes delivery cryptographically
   verifiable. Removes the GitHub dependency from the loop.
3. **blossom** — a Blossom blob server (BUD spec, kind-24242 auth) for **blob
   uploads** — for artifacts that aren't naturally
   git (large binaries, datasets, build outputs). A result/receipt can reference
   a blossom blob hash alongside — or instead of — a git commit, so sellers
   choose the right transport per job. Git stays primary for code; blossom
   covers everything else.

Reverse proxy (Caddy) terminates TLS and routes: relay WS, `/git/…`, blossom
`/upload`+`/<sha256>`, and the observatory static site at `/network`.

## Delivery model (decided)

- **git-objects via relay-git = primary.** Verifiable by tip-match; unchanged.
- **blossom blobs = additive.** For non-git artifacts. The `delivery_integrity_hash`
  binds either a git commit OID or a blossom blob sha256; the verifier accepts
  both, the transport allowlist governs both. (Blossom's content-address IS the
  integrity hash — verification is a sha256 check, cleaner than the git tip-match.)

## Packaging targets

### Today — what `flake.nix` actually exposes

- `packages.default` — the `mobee` client binary, built with `--features acp` (buyer MCP + seller).
- `apps.default` — `nix run --refresh github:MakePrisms/mobee/<ref> -- mcp|sell` (buyer + ad-hoc
  seller), no clone. Always `--refresh` (or pin+bump the rev) — nix caches the git ref and will
  otherwise serve a stale binary.
- `devShells.default` — the workspace build/dev shell.

That is the whole flake surface right now: one client binary, one run app, one dev shell. There is
no compose file, no blossom crate, no NixOS module, and no split `packages.*` / `apps.*` in-tree.

### Roadmap — not yet built (do not assume these exist)

**Docker** — a `docker-compose.yml` bundling relay + relay-git + blossom + Caddy +
Postgres (relay DB) + an object store (S3 or local for blossom/media). Images
built from the Nix packages. `docker compose up` = a running marketplace backend.

**Nix** — split the flake into `packages.{relay,relay-git,blossom,mobee}`, one per
component plus the client binary, so each service builds from the same source and
`Cargo.lock` as the client.

## Personas

- **relay-operator** — deploys the backend bundle with `docker compose up`.
- **seller** — `mobee sell`, run two ways by taste: `nix run --refresh … -- sell`
  (quick) or the Docker image. Same binary, same config contract.
- **buyer** — `nix run --refresh … -- mcp` wired into their agent (Claude etc.).
  Zero clone.

Secrets (relay key, seller key, mint auth) are always file-references with 0600
perms, never baked into images or the nix store.

## Sequencing

1. **Flake foundation first**: stabilize the client flake so
   `nix run --refresh … -- mcp|sell` works hermetically — the packaging base everything
   else builds on.
2. **Relay bundle**: relay open-mode + relay-git endpoint + Caddy, as a
   docker-compose bundle.
3. **Blossom**: add the blob server to the bundle + the `delivery_integrity_hash`
   accepting a blob sha256.
4. **Persona modules**: `mobee-seller` / buyer NixOS modules.

Build the loop working (seller slice) and the client runnable (flake) before the
self-host bundle — strangers should run a working marketplace before deploying
the backend.
