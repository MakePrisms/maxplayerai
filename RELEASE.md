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
2. Open a PR to `dev` with the bump and let it merge. **Then cut `dev` into `main` as a pull request —
   `main` rejects direct pushes**, and the merge must create a merge commit:

   ```sh
   gh pr create --base main --head dev --title 'Cut dev to main: <what>'
   gh pr merge --merge          # NOT --squash, NOT --rebase
   ```

   `--merge` rather than the alternatives because both others rewrite the commits: a squash gives
   `main` a commit that exists nowhere in `dev`'s history, and a rebase does the same for every commit
   it moves. Either one breaks the ancestor relation that step 3 exists to maintain, so they defeat
   the next step rather than merely differing in style.
3. **Merge `main` back into `dev`.** Not housekeeping — the cut itself is what makes this necessary,
   so it belongs to the cut rather than to whoever notices later:

   ```sh
   git checkout dev && git merge --no-ff main && git push origin dev
   ```

   A `dev → main` merge leaves a commit on `main` that `dev` does not have, so `main` stops being an
   ancestor of `dev`. While that holds, anything reaching `main` by a fast-forward or a reset silently
   drops whatever `main` had and `dev` lacked — which includes every previous cut, and anything merged
   straight to `main` such as a site change. The back-merge restores the ancestor relation, so there is
   nothing for such a cut to drop.

   Verify rather than assume — this prints nothing and exits `0` when the invariant holds:

   ```sh
   git fetch origin && git merge-base --is-ancestor origin/main origin/dev
   ```

4. **Tag `main` and push the tag** — this still works as a direct push. `main`'s ruleset targets
   branches, not tags, so the tag that triggers the release is not gated by it:
   ```sh
   git tag v0.2.0 && git push origin v0.2.0
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

⚠ The `latest`-unset half of that has not been executed against the live registry — it is npm's
documented behaviour and the workflow already assumes it, but nothing here has published yet. Plain
`0.x.y` avoids depending on the answer. Should an `-rc` ever ship first, the repair is one command:
`npm dist-tag add maxplayer@<version> latest`.

A genuine pre-release, once a stable exists, works as it always did: a semver suffix on both the tag
and the tree — tag `v0.3.0-rc1` against a tree that says `0.3.0-rc1`. The suffix marks the GitHub
Release a pre-release and publishes under the `rc` dist-tag, leaving `latest` where it was.

## What the tag does

- Builds `maxplayer` for each platform on a runner of that architecture, at both surfaces: the racer
  (`wallet`) and the seller (`wallet,acp`).
- Verifies each artifact: the version matches the tag, the feature set is the surface it is being
  published as — `acp` out for the racer, `acp` and `sell` in for the seller — and on Linux that it
  runs inside alpine and debian with no toolchain present.
- Attaches `maxplayer-<version>-<platform>.tar.gz` and `maxplayer-seller-<version>-<platform>.tar.gz`
  plus `SHA256SUMS` to a GitHub Release. Every one of the six is required by name before the release
  is created: a count cannot tell a complete release from one missing a whole surface.
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

### ⚠ Every trigger reads the workflow from a specific commit, not from `dev`

**`workflow_dispatch` runs the copy of the workflow at the ref you dispatch.** Being *offered* at
all requires `release.yml` to exist on the default branch (`main`), but the run itself executes the
ref's own file — so a workflow change can be dry-run from its branch, before it is merged anywhere.
Measured: run 30975896474, dispatched at `seller-prebuilt-artifacts`, built the six jobs that branch
declares while `main` still declared three.

**A `v*` tag runs the copy of the workflow in the tagged commit.** So a tag cut from a `main` that
predates `release.yml` starts **no run at all** — no output, no failure, nothing in the Actions tab.
That is the failure mode to watch for, because an absent run looks identical to one nobody noticed.

The fix for the tag is unchanged: **merge `dev` into `main` before tagging**, which step 2 above
already does. The trap is only reachable by tagging without it — most easily by cutting a release
from a `main` merged minutes *before* a workflow change landed on `dev`.

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

**The feature set is named explicitly.** CI builds `--no-default-features --features wallet` rather
than relying on `default`. The two are equal today, but `acp` compiles the seller's agent-execution
path, and a later change to `default` should not be able to put that into the binary handed to
buyers. `flake.nix` still relies on `default` for `buyer-static`; making it explicit there too is a
loose end.

## Reproducibility cross-check (darwin, optional)

A darwin binary built on a different machine will not hash the same as CI's even when the code is
identical: `LC_UUID` and the ad-hoc code signature differ per link. Comparing raw `shasum` output
therefore reports a mismatch on a perfectly good build, which looks exactly like tampering.

A comparison worth trusting needs both of:

1. **Locate the volatile regions structurally** — read `LC_UUID` and the `LC_CODE_SIGNATURE`
   `dataoff`/`datasize` out of the load commands, and zero those. Never hardcode byte offsets: they
   move with code size, and a stale offset produces a false match.
2. **Negative-test the normalizer** — flip one byte deep in `__TEXT` and confirm the normalized hash
   changes. A normalizer that zeroes too much would call two different binaries identical, and that
   failure is invisible without this step.

Fail closed: not Mach-O, missing file, or an ambiguous `LC_UUID` must exit non-zero, never a quiet 0.

This is advisory. A mismatch is a prompt to investigate, not a release blocker — an independent
rebuild is a cross-check, and letting it gate the pipeline would quietly make it load-bearing.

## If a release goes wrong

- **The tag produced no run at all.** The tagged commit has no `release.yml` — see the warning under
  Dry run. Merge `dev` into `main`, delete the tag, tag again.
- **Publish failed, Release exists.** Fix the cause and re-run just the publish job. The build is
  reproducible from the tag.
- **A version disagreed with the tag.** Nothing was published — the check runs before any upload.
  Delete the tag, fix the versions, tag again.
- **A bad version reached npm.** npm does not allow republishing a version. Bump and release again;
  `npm deprecate` the bad one.
