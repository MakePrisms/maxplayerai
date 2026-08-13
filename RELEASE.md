# Cutting a release

`maxplayer` ships two ways from one tag: downloadable archives on a GitHub Release, and `npx maxplayer` from
npm. Both come out of `.github/workflows/release.yml`, which runs on a pushed `v*` tag.

## Before the first release

One setup step, done once **per package**, and it is the only thing standing between a tag and a
publish. On npmjs.com, for each of `maxplayer`, `@maxplayerai/linux-x64`, `@maxplayerai/linux-arm64`
and `@maxplayerai/darwin-arm64`:

- Settings → **Trusted publishing** → add a GitHub Actions publisher: organization `MakePrisms`,
  repository `maxplayerai`, workflow `release.yml`.

There is no repository secret to add. The workflow authenticates by trusted publishing (OIDC): the
`publish` job mints a short-lived GitHub identity token and npm exchanges it for publish rights, so
no long-lived credential exists to expire, leak, or be rotated.

Until a package's trusted publisher is configured the workflow builds, verifies and creates GitHub
Releases as normal, and `npm publish` fails loudly for that package. There is no silent skip — an
unconfigured package fails the job rather than producing a release that looks published.

The launcher publishes as the unscoped package `maxplayer`; the per-platform payloads publish under
the `@maxplayerai/linux-x64`, `@maxplayerai/linux-arm64` and `@maxplayerai/darwin-arm64` packages, in
the npm organization `maxplayerai`. All four need their own trusted publisher entry — the setting is
per package, not per account or per org. The GitHub side of that entry is the same for all four, and
is unaffected by the npm scope: organization `MakePrisms`, repository `maxplayerai`.

⚠ `@maxplayerai/darwin-arm64` is new (#446) and needs its entry before the tag that first publishes
it. The setting lives on npmjs.com and needs an org admin — but whether it is really there is a
question this repo CAN answer, by publishing something worthless through the same path. See below.

### Proving an entry exists, before the tag

`npm publish` fails only after the packages ahead of it in the release loop have already published,
and npm does not allow republishing a version — so a missing entry discovered at tag time costs a
version, not a re-run. "I think I added it" is not worth that. Run the probe instead:

**Actions → Release → Run workflow**, set **npm_probe_package** to the package under test and
**npm_probe_version** to a fresh `0.0.<n>`. The `npm-probe` job publishes a placeholder — no binary,
no launcher, `probe` dist-tag, content generated in the job — through the same OIDC exchange the
release uses. Green means that package's trusted-publisher entry exists and admits this workflow.

Three things about it are load-bearing:

- **It runs from `release.yml`, not a workflow of its own.** npm scopes a trusted publisher to a
  workflow FILENAME, so a probe in `npm-oidc-probe.yml` would test a publisher entry for
  `npm-oidc-probe.yml` — a credential no release uses. A green run there would look like proof and
  be worth nothing.
- **A green run covers ONE package.** The entry is per package. Probing the darwin payload says
  nothing about the launcher or the linux payloads — those are proven by something else, not by this
  run: `maxplayer`, `@maxplayerai/linux-x64` and `@maxplayerai/linux-arm64` published through this
  same workflow file, tokenless, in the rc.1 and rc.2 releases. Production runs are the strongest
  evidence an entry exists. `@maxplayerai/darwin-arm64` is the only one with no such run behind it,
  which is why it is the one to probe.
- **Bump the version every run.** npm never allows reusing a version, probe or not.

A red has three causes that look identical — npm answers a trusted-publishing mismatch with a 404 or
ENEEDAUTH, which reads like a registry problem. The job prints them at the point of failure: the
version already exists; no entry for the package at all; or an entry that exists but names a
different workflow file (npm matches org, repo and FILENAME exactly, case-sensitively). The last two
are the same red and a different repair.

Leaving `npm_probe_package` blank is an ordinary dry run and publishes nothing. Every fence on that
job — dispatch-only, an explicitly named package, `0.0.<n>` only, payload packages only, the `probe`
dist-tag, and no checkout — is asserted by `verify-release-workflow.sh`, each one red-proven.

`--access public`, which the publish job already passes on every publish, is what the scoped payload
packages need on a first publish — a scoped package defaults to restricted, which on a free account
fails outright.

## Cutting one

1. **Bump the version.** `[workspace.package].version` in `Cargo.toml`, the workspace crate
   entries in `Cargo.lock` (refreshed by any `cargo` invocation — e.g. `cargo build` — since the
   release build passes `--locked` and fails on a stale lockfile), and `version` in every
   `npm/*/package.json`, and the `optionalDependencies` pins in `npm/maxplayer/package.json`. All of
   them must read the same string, and it must match the tag with the `v` dropped — the build
   asserts this and stops if anything disagrees.
2. **Open a PR from the bump branch to `main` and squash-merge it.** `main` rejects direct pushes, so
   the bump reaches `main` through a PR, and pr-feedback gates the merge:

   ```sh
   gh pr create --base main --head <bump-branch> --title 'release: cut v<version>'
   gh pr merge --squash
   ```

   Squash is right here: the bump is a pure version-cut, and there is no `dev` branch whose ancestry a
   squash would break. (`dev` is retired — the repo is main-only. An earlier revision of this doc cut
   through `dev` and required `gh pr merge --merge` to preserve a `main`→`dev` back-merge; both the
   back-merge and the `--merge` requirement are gone with `dev`.)

   Confirm the squashed `main` tree matches the branch that was reviewed and clean-seat-verified, before
   tagging — for a pure version-cut this is trivially empty:

   ```sh
   git fetch origin && git diff origin/main origin/<bump-branch>   # expect no output
   ```

3. **Tag `main` at the squash-merge commit and push the tag.** Fetch first so you tag the commit the PR
   produced, not a stale local `main`. The repo signs tags (`tag.gpgSign`, SSH format) and every prior
   release tag is signed, so the tag must be **annotated with a message** — a bare `git tag v0.2.0` fails
   `fatal: no tag message?`. Match the message convention `maxplayer vX.Y.Z`. The tag push still works as
   a direct push — `main`'s ruleset targets branches, not tags:
   ```sh
   git fetch origin && git tag -s -m "maxplayer v0.2.0" v0.2.0 origin/main && git push origin v0.2.0
   ```

## Version scheme: plain `0.x.y` while below v1

**Releases before v1 are the release candidates.** They are numbered `0.1.0`, `0.2.0` and so on —
plain versions with **no `-rc` suffix**. `v1.0.0` is deliberately unspent until the thing under it is
what we mean by 1.0.

That is not only a naming preference. **A suffix would break the install path it looks like it
supports.** Any hyphen in the version makes the workflow treat the release as a pre-release, and a
pre-release publishes to npm under the `rc` dist-tag *only*. The reasoning behind that — "so
`npm i maxplayer` still resolves the last stable version" — assumes a stable version already exists.
Before the first release there isn't one, so an `-rc` first release would leave `latest` unset and
`npx maxplayer` with nothing to resolve. The dist-tag branch is correct in every case except the one
that comes first.

The `latest`-unset half of that has never been executed against the live registry: `v0.1.0` was a
plain version, so it published to `latest` directly. Plain `0.x.y` keeps it that way. Should an
`-rc` ever ship without a stable behind it, the repair is one command:
`npm dist-tag add maxplayer@<version> latest`.

A genuine pre-release, once a stable exists, works as it always did: a semver suffix on both the tag
and the tree — tag `v0.3.0-rc1` against a tree that says `0.3.0-rc1`. The suffix marks the GitHub
Release a pre-release and publishes under the `rc` dist-tag, leaving `latest` where it was.

## What the tag does

- Builds `maxplayer` once per platform, on a runner of that architecture, with
  `--no-default-features --features wallet,acp` — the whole surface, buying and selling.
- Verifies each artifact: the version matches the tag, `acp` and `sell` are compiled in
  (`scripts/verify-seller-surface.sh`), and on Linux that it runs inside alpine and debian with no
  toolchain present.
- Attaches the three `maxplayer-<version>-<platform>.tar.gz` plus `SHA256SUMS` to a GitHub Release.
  Each is required by name before the release is created: a count cannot tell a complete release
  from one missing a platform.
- Publishes the npm packages: every payload package first, then the `maxplayer` launcher.

The publish order matters. The launcher pins its payload by exact version, so publishing it first
would leave a window where `npm i maxplayer` installs a launcher whose binary is not on the registry yet.

## Dry run

`workflow_dispatch` ("Run workflow" in the Actions tab) builds every platform and runs every
verifier, and **cannot publish** — the release and publish jobs are gated on a tag push, and a
dispatch is not one even if you aim it at a tag ref.

Use it after any change to the workflow. On a dispatch run the release and publish jobs should show
as skipped; that is the live confirmation that the gates hold, and it is worth watching once before
the first real tag.

### ⚠ Every trigger reads the workflow from a specific commit, not from the branch tip

**`workflow_dispatch` runs the copy of the workflow at the ref you dispatch.** Being *offered* at
all requires `release.yml` to exist on the default branch (`main`), but the run itself executes the
ref's own file — so a workflow change can be dry-run from its branch, before it is merged anywhere.
Measured: run 30975896474, dispatched at `seller-prebuilt-artifacts`, built the six jobs that branch
declares while `main` still declared three.

**A `v*` tag runs the copy of the workflow in the tagged commit.** So a tag cut from a `main` that
predates `release.yml` starts **no run at all** — no output, no failure, nothing in the Actions tab.
That is the failure mode to watch for, because an absent run looks identical to one nobody noticed.

The fix for the tag: tag a `main` that actually contains `release.yml` — which it does, since the
workflow lives on `main`. The trap is only reachable by tagging a commit that predates the workflow —
an old ref, or a `main` from before `release.yml` first landed.

If a tag produced no run, confirm the workflow was actually in that tree rather than assuming the
trigger misfired:

```sh
git cat-file -e "$TAG":.github/workflows/release.yml && echo present || echo absent
```

## Platforms

| platform | runner | shipped as |
|---|---|---|
| linux-x64 | `ubuntu-latest` | archive + `@maxplayerai/linux-x64` |
| linux-arm64 | `ubuntu-24.04-arm` | archive + `@maxplayerai/linux-arm64` |
| darwin-arm64 | `macos-14` | archive + `@maxplayerai/darwin-arm64` |

**darwin-x64 and Windows are out of scope.**

Adding a platform means: a matrix entry in `release.yml`, a package under `npm/`, an entry in
`PLATFORM_PACKAGES` in `npm/maxplayer/bin/maxplayer.js`, an `optionalDependencies` pin, the platform in
`RELEASE_PLATFORMS`, and a trusted-publisher entry for the new package on npmjs.com. Three of those
six are asserted rather than trusted — the pin by `verify-release-version.sh`, and both directions of
the platform list by the publish job: a payload package in the tree that is not published, and a
platform the release BUILDS with no payload behind it. The second is the one darwin needed. It was a
first-class release artifact while `npm i maxplayer` answered `no binary for darwin-arm64` on every
mac (#446), because the build matrix and the npm platform list were independent lists and nothing
compared them.

The trusted-publisher entry is the step no check can make: it lives on npmjs.com, needs an org admin,
and must exist BEFORE the tag. Without it the publish job fails on that package — loudly, which is
correct — but only after the packages ahead of it in the loop have published, and npm does not allow
republishing a version. Recovering from that means cutting the next patch version, not re-running the
job.

## Notes on the build

**The Linux static build uses rustup's musl target, not nix.** `nix build .#buyer-static` is the
derivation the artifact was originally proven with, and it remains the reference; CI does not use it
because a cold `pkgsStatic` build on a hosted runner has no bounded cost — the aarch64 route was
measured compiling rustc from source.

That substitution is safe only because the property is re-proved rather than inherited:
`verify-static-artifact.sh` runs on the binary CI actually produced, in containers with no nix and no
toolchain. If the musl toolchain ever produces something dynamically linked, the release fails. If it
turns out to produce something unacceptable for another reason, the fallback is
`cachix/install-nix-action` plus `nix build .#buyer-static`, and nothing else in the pipeline changes.

**The feature set is named explicitly, and the release build is not the CI build.** Two different
commands, two different surfaces:

- The release build (`.github/workflows/release.yml`, lines 170-172 — one command over three
  backslash-continued lines, `--target` included):

  ```
  cargo build -p maxplayer --release --locked \
    --no-default-features --features wallet,acp \
    --target ${{ matrix.target }}
  ```

- The default CI build (`.github/workflows/ci.yml`, line 21, step "Build workspace (default
  features)"): `cargo build --workspace --locked`, which takes `default` as-is.

Those are not the same feature set. `crates/maxplayer/Cargo.toml` has `default = ["wallet"]`, so the
released binary carries `acp` — the seller's agent-execution path — on top of the default surface.
That is deliberate: since #510 there is one universal binary and every buyer gets the seller surface
compiled in.

Naming the features rather than inheriting `default` is anti-drift, in either direction. `release.yml`
lines 156-160 put it as: a later edit to `default` must not be able to decide, silently, what a
release ships. That line is only the request, though — `scripts/verify-seller-surface.sh`, named at
`release.yml` line 159 and run on every platform, is what proves the features actually landed in the
artifact. `flake.nix` still relies on `default` for `buyer-static`; making it explicit there too is a
loose end.

## Reproducibility cross-check (darwin, optional)

A darwin binary built on a different machine will not hash the same as CI's even when the code is
identical: `LC_UUID` and the ad-hoc code signature differ per link. Comparing raw `shasum` output
therefore reports a mismatch on a perfectly good build, which looks exactly like tampering.

**This cross-check is manual, and as of today it has not been performed.** There is no tooling in the
repo for it: nothing normalizes a Mach-O binary, and no step of any workflow compares an independent
darwin rebuild against the released artifact. A meaningful comparison would have to account for the
per-link volatile regions described above rather than hashing the file as it sits, and the
requirements for such a tool are tracked separately.

If it does get built, it stays advisory. A mismatch is a prompt to investigate, not a release
blocker — an independent rebuild is a cross-check, and letting it gate the pipeline would quietly
make it load-bearing.

## If a release goes wrong

- **The tag produced no run at all.** The tagged commit has no `release.yml` — see the warning under
  Dry run. Delete the tag, then tag a `main` that contains `release.yml` and push it again.
- **Publish failed, Release exists.** Fix the cause and re-run just the publish job. The build is
  reproducible from the tag.
- **A version disagreed with the tag.** Nothing was published — the check runs before any upload.
  Delete the tag, fix the versions, tag again.
- **A bad version reached npm.** npm does not allow republishing a version. Bump and release again;
  `npm deprecate` the bad one.
