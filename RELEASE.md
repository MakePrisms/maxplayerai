# Cutting a release

`mobee` ships two ways from one tag: downloadable archives on a GitHub Release, and `npx mobee` from
npm. Both come out of `.github/workflows/release.yml`, which runs on a pushed `v*` tag.

## Before the first release

One setup step, done once, and it is the only thing standing between a tag and a publish:

- Add an npm **automation** token as the repository secret `NPM_TOKEN`
  (Settings → Secrets and variables → Actions).

Until that secret exists the workflow builds, verifies and creates GitHub Releases as normal, and the
publish job stops on its first step. That is deliberate: the token lives in repository secrets and
never in the tree.

Publishing `@mobee/*` for the first time also needs the `mobee` npm org to exist and the token's
account to be able to publish into it.

## Cutting one

1. **Bump the version.** `[workspace.package].version` in `Cargo.toml`, and `version` in every
   `npm/*/package.json`, and the `optionalDependencies` pins in `npm/mobee/package.json`. All of
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

A pre-release works the same way, with a semver suffix on both the tag and the tree — tag
`v0.2.0-rc1` against a tree that says `0.2.0-rc1`. Any suffix marks the GitHub Release a pre-release
and publishes npm under the `rc` dist-tag, so `npm i mobee` still resolves the last stable version.

## What the tag does

- Builds `mobee` for each platform on a runner of that architecture.
- Verifies each artifact: the version matches the tag, the feature set is the buyer surface
  (`wallet` in, `acp` out), and on Linux that it runs inside alpine and debian with no toolchain
  present.
- Attaches `mobee-<version>-<platform>.tar.gz` plus `SHA256SUMS` to a GitHub Release.
- Publishes the npm packages: every payload package first, then the `mobee` launcher.

The publish order matters. The launcher pins its payload by exact version, so publishing it first
would leave a window where `npm i mobee` installs a launcher whose binary is not on the registry yet.

## Dry run

`workflow_dispatch` ("Run workflow" in the Actions tab) builds every platform and runs every
verifier, and **cannot publish** — the release and publish jobs are gated on a tag push, and a
dispatch is not one even if you aim it at a tag ref.

Use it after any change to the workflow. On a dispatch run the release and publish jobs should show
as skipped; that is the live confirmation that the gates hold, and it is worth watching once before
the first real tag.

### ⚠ Both triggers read the workflow from a specific commit, not from `dev`

**`workflow_dispatch` is only offered for a workflow that exists on the default branch (`main`).**
Change `release.yml` on `dev` and there is nothing to dispatch until that change reaches `main` — the
"Run workflow" button describes `main`'s copy, not yours.

**A `v*` tag runs the copy of the workflow in the tagged commit.** So a tag cut from a `main` that
predates `release.yml` starts **no run at all** — no output, no failure, nothing in the Actions tab.
That is the failure mode to watch for, because an absent run looks identical to one nobody noticed.

Both follow from the same fact and have the same fix: **merge `dev` into `main` before tagging or
dispatching**, which step 2 above already does. The trap is only reachable by tagging or dispatching
without it — most easily by cutting a release from a `main` merged minutes *before* a workflow change
landed on `dev`.

If a tag produced no run, confirm the workflow was actually in that tree rather than assuming the
trigger misfired:

```sh
git cat-file -e "$TAG":.github/workflows/release.yml && echo present || echo absent
```

## Platforms

| platform | runner | shipped as |
|---|---|---|
| linux-x64 | `ubuntu-latest` | archive + `@mobee/cli-linux-x64` |
| linux-arm64 | `ubuntu-24.04-arm` | archive + `@mobee/cli-linux-arm64` |
| darwin-arm64 | `macos-14` | archive only |

darwin-arm64 has no npm payload package yet, so macOS users download the archive; `npx mobee` tells
them so rather than failing obscurely. Adding it means a `npm/cli-darwin-arm64` package and its entry
in the launcher's platform map — at which point the publish job's platform list has to name it too,
and the workflow fails at release time if it does not. That failure is the point: a payload package
present in the tree but missing from the registry breaks every install on that platform, because the
launcher pins it by exact version.

**darwin-x64 and Windows are out of scope.**

Adding a platform means: a matrix entry in `release.yml`, a package under `npm/`, an entry in
`PLATFORM_PACKAGES` in `npm/mobee/bin/mobee.js`, an `optionalDependencies` pin, and the platform in
`RELEASE_PLATFORMS`. Two of those five are asserted rather than trusted — the pin by
`verify-release-version.sh`, and the platform list by the publish job.

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
