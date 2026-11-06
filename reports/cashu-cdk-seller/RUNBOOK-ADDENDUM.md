# AUTHORITATIVE operational handoff — cashu-cdk-seller

**This file supersedes `RUNBOOK.md` for anything to do with images, the acceptance command, and
starting the seat.** `RUNBOOK.md` is kept unmodified as the historical record of the first round; its
image tag (`maxplayer-cashu-sandbox:v0.5.8-local`), its image id (`eb91a9…`) and its "22/8" gate
figure are **stale and must not be used**. Where the two disagree, this file wins.

Written 2026-09-09 by the worker seat. Everything below was measured on this host, not recalled.

## 1. The image to use

| | |
|---|---|
| Tag | `maxplayer-cashu-sandbox:v0.5.9-fix2` |
| **Immutable id** | `sha256:7fe93019c0fedc08242b3ff9f80d95fafa2f7c9cf3a7d1af6257bddff4ebc4c0` |
| Built from source commit | `a3b6a700f97067d733e04b0b24a5262b2dd1face` |
| Built | 2026-09-09T17:47:27-07:00 |
| Size | 999,306,918 bytes |
| CDK | 0.17.2, upstream commit `6132607495ae0741e412a63f2acc34e4ccddfc55` (tag v0.17.2) |
| NUTs corpus | commit `49a909ce4d0739824b3859d4b3da21e6c1abdaeb` |
| Test state root | `/var/lib/cashu-test-state` (container-local, outside `/work`) |

**Always run it by id, never by tag.** A tag is mutable and a rebuild silently moves it; the id is
the artifact. Every command in this file uses the id for that reason.

### Provenance — what the id does and does not prove

Stated plainly, because this is the part that is easy to overclaim:

- The image was built **locally on this host** (rocky's Mac Studio) inside the colima VM with
  `docker buildx`, from the worktree at commit `a3b6a700`. It is **not published to any registry**
  and has no registry digest.
- `ai.maxplayer.cashu.source-commit=a3b6a700…` is a label **I set at build time**. It is a
  self-asserted claim, not a cryptographic attestation: a label proves what the builder wrote, not
  that the layers correspond to that source. There is no signature, SBOM, or provenance attestation
  on this image.
- What *is* independently pinned is inside the Dockerfile: every `FROM` is digest-pinned, the base
  is `maxplayer-sandbox@sha256:4b644531…`, `cdk-mintd` and the three helper crates install
  `--locked` against committed `Cargo.lock` files, and the example builds `--offline --locked`.
  That gives reproducible **dependency graphs**, not bit-for-bit image equality — apt packages float
  by design and their exact versions are recorded in `/opt/rust/TOOLCHAIN.txt` inside the image.
- To rebuild and get a comparable artifact, check out `a3b6a700` and run the build in §5. Expect a
  different image id (timestamps and apt state differ); compare `/opt/rust/TOOLCHAIN.txt` and the
  labels, not the id.

## 2. Evidence status for this image — read before quoting numbers

- **Accepted, retained, and NOT re-run:** the 39-assertion / 10-leg acceptance run and the F1/F2/F3
  runtime evidence, captured on the previous image
  (`sha256:4fa79465132a6cd655245320982d6995fb0b97497f0b0f043cb18eec13cd5966`) and recorded in
  `delta-*-20260909.txt`. The ordering seat directed that this run stand and not be repeated.
- **This image adds** corrected F4/F5 predicates and the corrected baked knowledge topics. Its
  targeted evidence is `delta2-predicate-regression-20260909.txt` (10 oracle requirements, 3
  fixtures) and `delta2-offline-compile-check-20260909.txt`.
- **Therefore: do not quote an assertion count for the corrected gate.** The corrected
  `mint-acceptance` has more assertions than 39, and I have deliberately not run it to obtain a
  number, because the order was to produce targeted predicate evidence rather than another green
  transcript. Anyone who wants a corrected full-gate figure must run §4 and read the output.

## 3. Non-root, bind-mounted immutable replay recipe

This is the exact shape used to produce the retained runtime evidence: a **host directory bound at
`/work`** (so delivery isolation is real, not simulated) and the container running as a **non-root
host uid**, so anything written to the delivered workdir is owned by the seller and not by root.

```bash
IMG=sha256:7fe93019c0fedc08242b3ff9f80d95fafa2f7c9cf3a7d1af6257bddff4ebc4c0
WORK=$(mktemp -d)            # the delivered workdir; must be EMPTY at start

docker run --rm \
  --user "$(id -u):$(id -g)" \
  --network none \
  -v "$WORK:/work" -w /work \
  "$IMG" \
  bash -lc 'set -e; test-mint start >/dev/null; mint-acceptance; test-mint isolation'
```

Notes that matter, each one learned the hard way:

- `--user "$(id -u):$(id -g)"` — on this host that is `502:20`. The image has no passwd entry for
  that uid, which is why everything the job must read is world-readable and `CARGO_HOME` is
  world-writable. Running as root would invalidate the delivery-ownership property.
- `--network none` is correct for the acceptance gate: the mint is loopback-only and the toolchain
  is offline (`CARGO_NET_OFFLINE=true`), so the gate needs no route out at all. Proving it passes
  with no network is stronger than proving it passes with one.
- `bash -lc`, not `bash -c`: a **login** shell is what a job harness typically gives you, and it is
  the case that broke once — Debian's `/etc/profile` assigns `PATH` unconditionally, so the Rust
  toolchain vanished in an image that contained it. `/etc/profile.d/10-rust-toolchain.sh` now
  repairs that, and using `-l` here keeps the repair under test.
- `$WORK` must start empty. `test-mint isolation` asserts no mint state appears in `/work`; seeding
  the directory first makes that assertion meaningless.
- State lives at `/var/lib/cashu-test-state` **inside the container** and dies with it. Nothing to
  clean up on the host beyond `$WORK`.

For the F1 offline-build proof and the F4/F5 predicate regression specifically:

```bash
# F1: toolchain + CDK cache, offline compile, then run the example against the mint
docker run --rm --user "$(id -u):$(id -g)" --network none "$IMG" \
  bash -lc 'cashu-toolchain-check --run'

# F4/F5: the predicate regression. Source is mounted because the binary is a test artifact,
# not part of the shipped image surface.
docker run --rm --user "$(id -u):$(id -g)" --network none \
  -v "$PWD/docker/maxplayer-cashu-sandbox/acceptance:/src:ro" "$IMG" \
  bash -lc 'set -e; cp -a /src /tmp/acc && cd /tmp/acc
            CARGO_TARGET_DIR=/tmp/t cargo build --offline --locked --bin predicate-regression
            test-mint start >/dev/null
            /tmp/t/debug/predicate-regression'
```

The regression is the one that can genuinely fail: it reproduces a wallet with **42 of 43 sats
stranded in Reserved**, requires the shipped predicates to accept it, and requires the corrected
predicates to reject it. If a future edit weakens a predicate, this exits non-zero.

## 4. Corrected acceptance command

Replaces `RUNBOOK.md:17-18`. Prints the corrected gate's own count; do not assume 39.

```bash
IMG=sha256:7fe93019c0fedc08242b3ff9f80d95fafa2f7c9cf3a7d1af6257bddff4ebc4c0
WORK=$(mktemp -d)
docker run --rm --user "$(id -u):$(id -g)" --network none -v "$WORK:/work" -w /work "$IMG" \
  bash -lc 'test-mint start >/dev/null && mint-acceptance'
```

## 5. Rebuilding

Host `docker` CLI here has no buildx, so the build runs inside the colima VM, which sees the same
path over virtiofs:

```bash
cd ~/forge/v2/wt/w-cashu-cdk-seller
HEAD=$(git rev-parse HEAD)
colima ssh -- bash -lc "cd $PWD && docker buildx build \
  --label ai.maxplayer.cashu.source-commit=$HEAD \
  -f docker/maxplayer-cashu-sandbox/Dockerfile -t maxplayer-cashu-sandbox:local ."
colima ssh -- bash -lc 'docker image inspect maxplayer-cashu-sandbox:local --format "{{.Id}}"'
```

The build itself proves the offline CDK compile (it fails if the runtime toolchain, the crate cache
or the C linker is missing), so a successful build is already a meaningful check.

## 6. VPS / seat image setup — HUMAN ONLY, selected field only

**I have not performed this and it is not authorized to me.** Whoever does it changes **exactly one
field** and nothing else.

In the seat's `config.toml` (`~/forge/v2/seats/cashu-cdk-seller/config.toml`), set only the
`[sandbox]` image field to the id from §1:

```toml
[sandbox]
image = "sha256:7fe93019c0fedc08242b3ff9f80d95fafa2f7c9cf3a7d1af6257bddff4ebc4c0"
```

Preserve, do not touch:

- **`network` and `proxy_port_range`** — the dedicated `maxplayer-cashu-jobs` network and its
  per-job egress proxy are what contain a job and what contain the model credential. Removing or
  widening either breaks containment.
- **Admissions.** The seat runs with neither `--claim-open-pool` nor `--accept-open-targeted` and no
  `accept_offers_only_from`, so it claims nothing and owes nothing. Leave it that way; opening
  admissions is a separate authorization.
- **`accepted_mints`.** Shipped default (`mint.minibits.cash`) and unchanged. The sandbox test mint
  must **never** appear there — it issues worthless test ecash and is enforced in two places in
  `crates/maxplayer-core/src/payment_wallet.rs`.
- **Every other key in the file.** Edit the one field; do not regenerate or overwrite the config.

Verify afterwards, before starting the seat:

```bash
grep -n 'image\|network\|proxy_port_range\|accepted_mints' ~/forge/v2/seats/cashu-cdk-seller/config.toml
./target/release/maxplayer doctor        # expect the same 17 checks, exit 0
colima ssh -- bash -lc 'docker image inspect sha256:7fe93019c0fedc08242b3ff9f80d95fafa2f7c9cf3a7d1af6257bddff4ebc4c0 --format "{{.Id}}"'
```

The last command failing means the id is not present on the host that will run the jobs — build or
load it there first. A seat pointed at an absent image fails at job launch, not at `doctor`.

## 7. Auth

Credential provisioning is a separate, human step: see **`AUTH-PROVISIONING.md`**, which points back
to this file for the image and the start procedure. Do §6 **before** the human provisions and starts
the seat, so the first advertise happens on the corrected image.

Authentication succeeding is not deployment acceptance. After the seat starts, the things still to
capture are a contained probe artifact, actual kind-0 / kind-30340 discovery evidence, and a
separately authorized **test-only** end-to-end job. None of those exist today and none is claimed.
