# Packaging scope — getting `mobee` onto a machine that has never heard of nix

Scope of work only. Nothing here is built, and nothing here is approved: the prebuilt-binary path
was downgraded to later on 2026-07-23 ([#125](https://github.com/MakePrisms/mobee/issues/125)) and
that ruling stands until it is revisited. This document exists so that the revisit is a decision
about a specified thing rather than about a vibe.

Every measurement below was taken against `dev` @ `8debf43`.

---

## 1. What installing `mobee` means today

One path, stated in five files and nine places:

```
nix run --refresh github:MakePrisms/mobee -- mcp
```

`README.md` · `docs/ONBOARDING.md` (×2) · `docs/SELLER-QUICKSTART.md` (×2) · `docs/DEPLOYMENT.md` (×4)

It works, it is hermetic, and it is the right interim answer for a team that is entirely on nix.
It has three properties that only start to hurt once someone outside the team arrives:

- **It requires nix.** Not a package manager the buyer already has — nix.
- **It cannot go in an MCP client config.** MCP configs conventionally spawn `npx -y <pkg>`.
  Wiring the buyer MCP today means putting `nix run --refresh github:…` into
  `claude_desktop_config.json`, which is both unusual and slow on every cold start.
- **`--refresh` is load-bearing and easy to omit.** Without it the flake ref is served from cache,
  so the user silently runs an old `mobee`. Every doc that states the command states the flag; a
  user who retypes it from memory gets a stale binary and no warning.

## 2. There is no artifact to install

This is the finding that reframes the rest, and it is not a matter of flipping a target flag.

**There are zero GitHub releases.** `gh release list` is empty. The only tags are
`dev-pre-converge-20260722` (and its peeled form) — no version tags. `version = "0.1.0"` in
`[workspace.package]` has never been bumped. An installer would have nothing to point at.

**There is no release workflow.** `.github/workflows/ci.yml` is the only workflow. It builds
(`build-default`, `build-acp`) and publishes nothing.

**★ The release binary we build today cannot run on a non-nix box.** Not because of glibc
versioning — because the ELF interpreter is a nix store path. From a real
`cargo build -p mobee --release --features acp,wallet` artifact:

```
linux-vdso.so.1
libgcc_s.so.1 => /nix/store/chqq8mpmpyfi9kgsngya71akv5xicn03-gcc-15.2.0-lib/lib/libgcc_s.so.1
libm.so.6    => /nix/store/57iz36553175g3178pvxjij8z5rcsd4n-glibc-2.42-61/lib/libm.so.6
libc.so.6    => /nix/store/57iz36553175g3178pvxjij8z5rcsd4n-glibc-2.42-61/lib/libc.so.6
/nix/store/…-glibc-2.42-61/lib/ld-linux-x86-64.so.2 => /nix/store/l8si8gnvvq93yzms1jsgh5aixyf9rl5x-glibc-2.42-67/lib64/ld-linux-x86-64.so.2
```

A binary built in our dev environment is unshippable *by construction*. Producing a portable
artifact requires a build that is not a nix dev shell — which is precisely the work that does not
exist yet, and precisely why "later" has been cheap so far.

Size, for planning: **~32.7 MiB** (34,296,792 bytes), `x86_64-linux`, stripped, thin-LTO,
`--features acp,wallet`.

### 2.1 The portability constraint, measured rather than assumed

The blocker people expect from a Rust+musl build is openssl. **We do not have that problem** — it
was designed out, and the manifests say so explicitly: `git2` carries `default-features = false`
to keep the `https`/`ssh` cargo features (and `openssl-sys`/`libssh2-sys`) out, and `reqwest` uses
`rustls-tls` with bundled webpki roots so there is no ambient `SSL_CERT_FILE` requirement. The
`ldd` output above confirms it: nothing links openssl, sqlite, or libgit2 dynamically.

The constraint that *is* real: **two dependencies compile C into the binary.**

- `rusqlite` with `features = ["bundled"]` — builds SQLite from source
- `git2` → `libgit2-sys` — vendored libgit2

So a musl target must cross-compile **C**, not just Rust. That means a musl C toolchain in the
build environment (`cargo-zigbuild` or `cross`, or a musl-native container), not a bare
`rustup target add`. This is a solved problem with a known cost, but it is not free and it is the
thing most likely to be underestimated.

### 2.2 The feature combo that must ship

The flake builds with `acp` enabled and default features kept, i.e. **`acp,wallet`**. Release
artifacts must match: `acp` gates the seller-execute path and the `run` subcommand; `wallet` is
default and carries the money path. A release built without `acp` ships a seller that cannot
execute, and it compiles cleanly, so nothing catches it downstream.

### 2.3 `cargo install` is not a fallback today

`publish = false` in `[workspace.package]`, so `mobee` is not on crates.io and
`cargo install mobee` cannot work. `cargo install --git` works but requires a Rust toolchain —
the same class of prerequisite the whole exercise is trying to remove. `cargo-binstall`, named as
a fallback in #125, resolves through crates.io metadata or GitHub releases; with neither present
it has nothing to bind to. **It becomes available as a fallback only after track A ships**, not
independently of it.

---

## 3. The three tracks

They are ordered by dependency, not by preference. **A is a prerequisite for both B and C** — an
installer and an npm wrapper are both just delivery mechanisms for an artifact that does not yet
exist.

### Track A — release artifacts *([#211](https://github.com/MakePrisms/mobee/issues/211); foundation)*

A tag produces per-platform binaries and checksums on a GitHub release.

- Tag-triggered workflow, separate from `ci.yml`.
- Platform matrix, matching the four systems the flake already claims:
  `x86_64-linux`, `aarch64-linux`, `x86_64-darwin`, `aarch64-darwin`.
- Linux targets static via musl (needs the C cross-compile of §2.1); darwin targets built on
  macOS runners.
- Built with `--features acp,wallet --locked`.
- `SHA256SUMS` published alongside. Not decoration — it is what makes track C's `curl | sh`
  defensible at all.
- A real version tag, which means deciding what `0.1.0` becomes (see §5).

```acceptance
# Every artifact runs on a machine with no nix and no rust toolchain.
# Re-run per published asset, on a clean container of the target platform:
docker run --rm -v "$PWD:/w" -w /w alpine:3 ./mobee-x86_64-unknown-linux-musl version
#   → prints a version, rc=0
# The interpreter check that today's build fails:
ldd ./mobee-x86_64-unknown-linux-musl 2>&1 | grep -qi 'not a dynamic executable' \
  || { echo "FAIL: not static"; exit 1; }
# Checksums match what was published:
sha256sum -c SHA256SUMS

# ★ The seller EXECUTES from the downloaded artifact — not `--help`, not a mock layer.
# `--help` on an acp-gated subcommand proves it compiled in; a release built without `acp`
# compiles clean, installs clean, answers --help, and ships a seller that cannot execute.
# Rig (zero infrastructure, zero money): `nak serve --port 10547` as the marketplace relay;
# throwaway MOBEE_HOME (config.toml + 0600 64-hex key); `git_remote` with no "/git/" segment
# so the relay-git seed probe is skipped; `--skip-doctor`. Post one offer, let the artifact's
# seller claim and execute it, ASSERT a delivery is produced. Budget ~20s for boot — nak serve
# issues no NIP-42 challenge, so the daemon waits out `no NIP-42 challenge within 20s`.
#
# ★ And the tooth must go RED on a deliberately no-acp artifact. If it still passes, it is not
# testing what it claims and the trap survives under a green gate.
```

### Track A.1 — how nix produces the artifact

Track A says *what* ships. This says *how* it is built: nix emits the exact file npm carries, so
distribution is the only remaining problem.

**Named output — `packages.<system>.buyer-static`.** The buyer surface is a feature *narrowing* of
the existing derivation, not a second toolchain: `default = ["wallet"]`, and `acp` is simply left
out, so the seller's agent-execution path is not compiled in. The seller artifact is the same
derivation with `buildFeatures = [ "acp" ]`. One shape, two feature sets.

```nix
buyer-static = pkgs.pkgsStatic.rustPlatform.buildRustPackage (
  mobeeArgs // { nativeBuildInputs = [ pkgs.pkgsStatic.pkg-config ]; }
);
```

`mobeeArgs` holds what both builds share — `src = self`, `cargoLock.lockFile = ./Cargo.lock`
(hermetic vendoring, no network at build time), `cargoBuildFlags = [ "-p" "mobee" ]`, `doCheck =
false`. Guarded by `lib.optionalAttrs stdenv.hostPlatform.isLinux`, because static linking is a
Linux-only property (see the matrix below).

#### Option A (static musl) over Option B (patchelf)

**A is strictly better where it links, and B is a weak fallback rather than an equal option.**
patchelf rewrites the ELF interpreter to a path that must exist *on the user's machine*, which
either means shipping a loader and glibc alongside the binary, or pointing at the target's own
glibc and inheriting a **version floor** — the familiar `GLIBC_2.xx not found` failure on older
distros. It also leaves every `DT_NEEDED` shared library to be resolved. Static musl has no floor
and no dependencies to resolve.

#### ★ Why the cc-wrapper false-blocker does not apply here — and the reason it is *not* the obvious one

The tempting justification is "pkgsStatic is native-musl, so there is no cross-compile and no
wrapper." **Both halves of that are false**, measured against the pinned nixpkgs:

```
buildPlatform  = x86_64-unknown-linux-gnu     hostPlatform = x86_64-unknown-linux-musl
crossCompiling = true                          ccIsWrapper  = true
cc             = x86_64-unknown-linux-musl-gcc-wrapper-14.3.0
libc           = musl-static-x86_64-unknown-linux-musl-1.2.5
```

It *is* a cross set and it *does* use a cc-wrapper. It works anyway because **the wrapper carries
a libc for the target triple.** The failure documented in
`reference_nix_cc_wrapper_fakes_cross_compile_failure` was never about cross-compilation or about
wrappers as such — it was a **sysroot mismatch**: the wasm32 wrapper had no libc for its target,
fell back to injecting host glibc, and produced an error naming a real C crate (`secp256k1-sys`)
that had nothing wrong with it.

**The predictive test, stated so it can be reused:** compare `stdenv.cc.libc` against
`stdenv.hostPlatform`. If the wrapper's libc does not belong to the target triple, expect a
confident error blaming a C crate — and do not accept that error as a portability finding until an
unwrapped compiler has been tried. Here they match, so the wrapper is sound.

Getting the right answer from the wrong reason is still a hazard: the wrong reason predicts that
any pkgsStatic build is safe, which is not what was measured.

#### Output contract

One regular file per platform plus one checksum line — nothing else:

```
mobee-x86_64-unknown-linux-musl          # static ELF, no interpreter, no DT_NEEDED
SHA256SUMS                                # one line per artifact
```

That file *is* the payload of the track B per-platform sub-package (`@mobee/cli-linux-x64`), so the
npm side never compiles anything and never runs a postinstall downloader.

```acceptance
# ★ Two DIFFERENT predicates. Do not let the first stand in for the second.
#
# (1) PORTABILITY — does the artifact run at all with no nix and no toolchain present.
# Assert on the ELF structure, not on `file` (not installed everywhere, this box included):
readelf -lW result/bin/mobee | grep -qi interp && { echo "FAIL: has an ELF interpreter"; exit 1; }
readelf -dW result/bin/mobee | grep -qi needed && { echo "FAIL: has shared-library deps"; exit 1; }
# Then COPY THE BINARY OUT of /nix/store and run it where no /nix exists. Copying out is the
# point: a needed store path cannot be silently satisfied by the build machine's own store.
cp -L result/bin/mobee ship/mobee-x86_64-unknown-linux-musl
docker run --rm -v "$PWD/ship:/b:ro" alpine:3            /b/mobee-x86_64-unknown-linux-musl version
docker run --rm -v "$PWD/ship:/b:ro" debian:bookworm-slim /b/mobee-x86_64-unknown-linux-musl version
#   → both print a version, rc=0. Two libcs on purpose: alpine is musl, debian is glibc, so a
#     pass on both shows the artifact is libc-independent rather than merely alpine-compatible.
# ★ Capture rc WITHOUT a pipe — `docker ... | tail` reports tail's exit code, not docker's.
# ★ Negative control, or rc=0 means nothing: a bogus subcommand MUST give rc!=0.
#
# (2) BUYER CAPABILITY — that the buyer surface actually works. `version`/`--help` rc=0 CANNOT
# test this: the CLI surface is present in every build regardless of features, which is the same
# trap Track A documents for `acp`. Requires a real buyer operation against `nak serve --port
# 10547`: post one job, read it back by id, assert the job is returned.
#
# ★ And (2) must go RED against a binary built without `wallet`.
```

#### CI hook — design only

No release workflow exists; `ci.yml` is the only workflow, it triggers on push, and it builds with
`dtolnay/rust-toolchain` — **there is no nix in CI today.** So the hook is a new tag-triggered
`release.yml` that must first *install* nix (`DeterminateSystems/nix-installer-action`), then call
`nix build .#buyer-static` per Linux platform and upload the artifact plus its `SHA256SUMS` line.
`ci.yml` is left alone: it gates correctness on every push, and a release build is neither.

#### Measured — this derivation has been built and run

Not a projection. Built 2026-07-28 against the pinned nixpkgs, `nix build .#buyer-static` → rc=0:

```
size            39,816,976 bytes (38M), already stripped by the fixup phase
ldd             statically linked
PT_INTERP       absent          DT_NEEDED  absent
/nix/store refs 0 occurrences in the binary
alpine:3 (musl, no /nix)             mobee version → "mobee 0.1.0"  rc=0
debian:bookworm-slim (glibc, no /nix) mobee version → "mobee 0.1.0"  rc=0
bogus subcommand                                                     rc=1   (control)
sha256 de0b96258aa4dc83c6b72ba3a3e604de8bd91175d70dbfb0c957414a01b4f1bf
```

The three C dependencies that were the whole risk — `secp256k1-sys`, `libsqlite3-sys`,
`libgit2-sys` (vendored libgit2) — **link under pkgsStatic with no per-crate flags and no patching.**
Option A is therefore settled for `x86_64-linux`, and Option B (patchelf) is not needed.

**38M per platform is the honest cost**, and it is the one number worth carrying into track B: npm
will hold a ~38M binary per platform sub-package (materially smaller compressed). The
`optionalDependencies` split matters more because of this — a user installs one platform, not four.

#### Size and platform matrix

- **`x86_64-linux`** — **built and verified** (above). Remaining work is release plumbing, not
  feasibility. Roughly 2 engineering days.
- **`aarch64-linux`** — **measure-only.** Either a native build on an arm64 runner (same derivation,
  no new question) or `pkgsCross.aarch64-multiplatform-musl.pkgsStatic` from x86_64. The wrapper
  test above is what decides the cross route: its libc must be aarch64-musl.
- **darwin (both arches)** — **`buyer-static` deliberately does not exist here.** macOS does not
  support fully static executables; there is no static crt0 to link against. Darwin needs a
  dynamically linked binary with a declared minimum macOS version, which is a different artifact
  with a different portability argument — not a variant of this one.

### Track B — `npx mobee` *([#212](https://github.com/MakePrisms/mobee/issues/212); the MCP-config unlock)*

**Read this section before assuming what "npx" means here.** This repo *banned* npx in the other
direction: post-R4 the seller deliberately removed the npx auto-launch fallback for ACP adapters,
and a test enforces it (`crates/mobee-core/src/agent_presets.rs`, asserting `must not resolve to
an npx fallback`). A missing adapter must fail with an install hint rather than silently reach for
`npx`. **That stays true and is not in scope here.**

Track B is npx as *distribution of the mobee binary itself* — the standard esbuild/swc pattern:

- npm package whose `bin` shim resolves a platform-specific binary and `exec`s it, forwarding argv
  and exit codes verbatim.
- Per-platform binaries as `optionalDependencies` (`@mobee/cli-linux-x64` etc.) so the install
  downloads one platform, or a `postinstall` fetch from the track A release. Preference:
  optionalDependencies — a postinstall downloader breaks under `--ignore-scripts`, which security-
  conscious users and many CI setups set by default.
- Verify the downloaded artifact against track A's `SHA256SUMS`.

The payoff, and the reason this is not merely a second install path: it makes the buyer MCP
wirable the way every other MCP server is wired.

```jsonc
{ "mcpServers": { "mobee": { "command": "npx", "args": ["-y", "mobee", "mcp"] } } }
```

Names are free as of 2026-07-28: `mobee`, `mobee-cli`, and the `@mobee` scope all return 404 from
the npm registry. Claiming the name is cheap and worth doing before someone else does, independent
of when the work lands.

```acceptance
# On a box with node but no nix, no rust, and no prior mobee:
npx -y mobee@<version> version          # → prints <version>, rc=0
npx -y mobee@<version> --this-is-not-a-flag; echo "rc=$?"   # → argv forwarded, non-zero rc preserved
npm install --ignore-scripts -g mobee@<version> && mobee version   # → still works (no postinstall dependence)
# The ban this track must not undo:
cargo test -p mobee-core builtin_presets_resolve_to_binary_or_install_hint
```

### Track C — `install.sh` *(belongs on [#125](https://github.com/MakePrisms/mobee/issues/125))*

#125 is exactly this issue and it is still open. The spec goes there as a comment rather than into
a new issue: a duplicate would re-litigate a decision sideways instead of extending it where it
was made.

- `curl -fsSL <url> | sh` detects OS/arch, downloads the matching track A artifact, verifies its
  SHA256 against the published sums, installs to a PATH directory, and prints what it did and
  where.
- Refuses rather than guesses on an unsupported platform.
- Idempotent: re-running upgrades in place.
- Once it exists it becomes the canonical one-liner that docs and the MCP fix-hints point at —
  which means the nine `nix run` occurrences in §1 are part of this track's work, not a follow-up.
  The nix path stays documented and supported; it stops being the *only* answer.

```acceptance
# Clean container, no nix, no rust:
curl -fsSL <url> | sh && mobee version    # → rc=0, prints version
# Verification is real, not decorative — corrupt the artifact and it must refuse:
#   (point the installer at a tampered binary; expect non-zero rc and no install)
# Idempotent:
curl -fsSL <url> | sh && curl -fsSL <url> | sh && mobee version   # → rc=0, one binary on PATH
# Unsupported platform refuses rather than installing something wrong:
#   (run under an unsupported uname; expect a named refusal, non-zero rc)
```

---

## 4. Non-goals

- **Removing the nix path.** It stays supported and documented. This is about it not being the
  only door.
- **Undoing the ACP-adapter npx ban** (§ track B). Unrelated mechanism, same word.
- **Homebrew, AUR, apt/deb, Docker Hub publishing.** Docker packaging already exists
  (`Dockerfile`, `docker-compose.yml`, `docs/DOCKER.md`); publishing images is a separate call.
- **Shipping the seller's harness.** The ACP adapter is the seller's own binary by design. Nothing
  here changes that, and no installer should pretend to supply it.
- **`mobee-desktop`.** Deliberately outside `default-members` because it pulls egui's native deps;
  it is not part of a CLI release.

## 5. Open decisions — for gudnuf, not for us

1. **Does the 7/23 downgrade get revisited at all?** The ruling stands until he says otherwise.
   What has changed since is a fact worth putting in front of him rather than an argument:
   there is now a public orderbook at `/mobeemarket` with `.well-known/skills/` and an
   `npx skills add` payoff. "Whole team is on nix" was true of the team; a stranger arriving from a
   public marketplace is the non-nix buyer #125 was originally about, and that stranger did not
   exist on 7/23.
2. **Version scheme and first tag.** `0.1.0` has never moved and there are no version tags.
   Everything above needs something concrete to reference. What is the first release called, and
   does the tag drive the version or does the manifest?
3. **Which platforms are actually required at first cut.** The flake claims four. Shipping four
   costs macOS runners and a musl cross; shipping `x86_64-linux` alone is a fraction of the work
   and covers most servers. This is a scope dial, and it is his to set.
4. **npm namespace.** Bare `mobee` or the `@mobee` scope. All variants are unregistered today;
   the scope is easier to defend long-term, the bare name is what people will type.
5. **Signing.** Checksums are specified above; signatures are not. Whether release artifacts get
   signed (and with what) is a call, not an omission.

## 6. Ordering

`A → (B, C)` — i.e. **#211 → (#212, #125)**. B and C are independent of each other and can land in
either order or in parallel, but both consume A's artifacts and neither can be verified before A
exists. Any attempt to start with C produces an installer with nothing to download.
