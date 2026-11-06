# syntax=docker/dockerfile:1
#
# The Cashu/CDK specialist job sandbox.
#
# It is the ordinary maxplayer sandbox image plus what a Cashu/CDK job cannot get at run time
# without paying for it:
#
#   * a PINNED Rust toolchain and a warm CDK dependency cache, so a job compiles CDK code OFFLINE
#     (advisor F1 — the first cut shipped only precompiled binaries, and BuildKit cache mounts
#     never become image layers);
#   * the Cashu spec and CDK source at pinned commits;
#   * the curated knowledge topics AT A CONTAINER PATH (advisor F2 — the injected MEMORY.md index
#     used to link host files that a contained job cannot reach);
#   * `cdk-mintd` built at the same CDK version this workspace pins, plus a start/stop/reset rig for
#     a loopback-only fakewallet test mint whose private state stays OUT of the delivered workdir
#     (advisor F3);
#   * a baked acceptance harness and a from-source example.
#
# The mint here is a FAKEWALLET mint: worthless test ecash, 127.0.0.1 only, state disposable. It
# must never appear in a seller's `accepted_mints` — checked twice in
# crates/maxplayer-core/src/payment_wallet.rs (realized mint, and the NUT-18 payload mint). Nothing
# in this image writes to a seller config.
#
# Build from the REPO ROOT, after the base image exists:
#   docker buildx build -f docker/maxplayer-sandbox/Dockerfile -t maxplayer-sandbox:v0.5.8-local .
#   docker buildx build -f docker/maxplayer-cashu-sandbox/Dockerfile \
#       --build-arg BASE_IMAGE=maxplayer-sandbox:v0.5.8-local \
#       -t maxplayer-cashu-sandbox:v0.5.8-local .
#
# Every FROM below is pinned by DIGEST (advisor F6): `rust:1-bookworm` and `debian:bookworm-slim`
# are moving tags, and a rebuild against a different toolchain is a different artifact. BASE_IMAGE
# defaults to the digest of the base built from this same commit; pass a tag only deliberately.

ARG BASE_IMAGE=maxplayer-sandbox@sha256:4b644531ffe3fdc0c4414aa8d6f9027c58a3f18124bb7abe61bb622973731eea
# rust:1-bookworm as of 2026-09-08 = rustc 1.98.1 (48a229cea 2026-09-01), cargo 1.98.1.
ARG RUST_IMAGE=rust@sha256:9a73a5088750b4c95158ab26629c854c3d6fc4b173cb7bc8079ad252d8ed7bfa
ARG DEBIAN_IMAGE=debian@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

# CDK version is pinned to the version THIS workspace depends on
# (crates/maxplayer-core/Cargo.toml:62,83-84 and crates/maxplayer/Cargo.toml:105-107 — all =0.17.2).
# A test mint on a different CDK than the wallet under test would make a protocol mismatch look
# like a bug in our code.
ARG CDK_VERSION=0.17.2
# cashubtc/cdk v0.17.2 tag object.
ARG CDK_COMMIT=6132607495ae0741e412a63f2acc34e4ccddfc55
# cashubtc/nuts main as of 2026-08-23T17:08:00Z. A corpus is stale from the moment it is baked;
# the seat's MEMORY.md states this pin so the agent never quotes an old NUT as current.
ARG NUTS_COMMIT=49a909ce4d0739824b3859d4b3da21e6c1abdaeb

# =============================================================================================
# builder — mint, seed generator, harness, and the runtime toolchain payload
# =============================================================================================
FROM ${RUST_IMAGE} AS mint-builder
ARG CDK_VERSION

# protoc is a BUILD-TIME requirement even with --no-default-features: cdk-mintd depends on
# cdk-signatory unconditionally (the local in-process signatory), and cdk-signatory 0.17.2's
# build.rs compiles src/proto/signatory.proto whether or not the `grpc` feature is on — it panics
# with "Could not find `protoc`" otherwise. Measured, not assumed: the first build of this image
# failed exactly there. protoc lives in this builder stage only; it is not in the final image.
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# --no-default-features drops cln, lnd, lnbits, bdk, ldk-node, grpc-processor and the management
# RPC. What is left is exactly the isolated testing mode: `fakewallet` (= dep:cdk-fake-wallet) and
# the sqlite store. cdk-mintd's own config comment is explicit that fakewallet "is isolated testing
# mode and cannot be mixed with real payment backends" — building without the real backends means
# this binary CANNOT be pointed at one, which is a stronger guarantee than a config convention.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cashu-mint-target,target=/tmp/target \
    set -eux; \
    CARGO_TARGET_DIR=/tmp/target cargo install cdk-mintd \
        --version "${CDK_VERSION}" --locked \
        --no-default-features --features fakewallet,sqlite \
        --root /out; \
    strip /out/bin/cdk-mintd

# The seed generator and the harness are real crates with COMMITTED lockfiles, installed --locked
# (advisor F6 — they were inline heredocs with ranged dependencies and no recorded resolution).
COPY docker/maxplayer-cashu-sandbox/seedgen /src/seedgen
COPY docker/maxplayer-cashu-sandbox/acceptance /src/acceptance
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cashu-mint-target,target=/tmp/target \
    set -eux; \
    CARGO_TARGET_DIR=/tmp/target cargo install --path /src/seedgen --locked --root /out; \
    CARGO_TARGET_DIR=/tmp/target cargo install --path /src/acceptance --locked --root /out; \
    strip /out/bin/test-mint-seed /out/bin/mint-acceptance /out/bin/test-mint-invoice

# The RUNTIME toolchain payload (advisor F1). Assembled in the same RUN as the registry cache mount,
# because a cache mount is not a layer: it has to be copied into a real directory here or it is gone.
#
# What goes in: the single installed toolchain (rustc, cargo, clippy, rustfmt, rust-std, rust-src)
# and a CARGO_HOME carrying the registry index plus the .crate archives for the whole CDK
# dependency graph. What stays out: rustup itself (a job pins one toolchain and must not fetch
# another), docs and man pages.
#
# `registry/src` is dropped deliberately: cargo extracts sources from `registry/cache/*.crate` on
# first use, so offline builds work without carrying both copies.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    set -eux; \
    toolchain="$(ls -d /usr/local/rustup/toolchains/* | head -n 1)"; \
    mkdir -p /out/rust; \
    cp -a "${toolchain}" /out/rust/toolchain; \
    rm -rf /out/rust/toolchain/share/doc /out/rust/toolchain/share/man; \
    mkdir -p /out/rust/cargo; \
    cp -a /usr/local/cargo/registry /out/rust/cargo/registry; \
    rm -rf /out/rust/cargo/registry/src; \
    /out/rust/toolchain/bin/rustc --version > /out/rust/TOOLCHAIN.txt; \
    /out/rust/toolchain/bin/cargo --version >> /out/rust/TOOLCHAIN.txt; \
    du -sh /out/rust/toolchain /out/rust/cargo/registry >> /out/rust/TOOLCHAIN.txt

# =============================================================================================
# corpus — the spec and CDK source, at pinned commits
# =============================================================================================
FROM ${DEBIAN_IMAGE} AS corpus
ARG CDK_COMMIT
ARG NUTS_COMMIT
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# --filter=blob:none keeps the clone small; the checkout still materialises every file at the pin.
# .git is removed afterwards so the corpus is not an ordinary git working copy inside a job. Note
# this prevents `git pull`, NOT file writes by a root process (advisor N4) — source pinning, not
# runtime immutability.
RUN set -eux; \
    mkdir -p /corpus; \
    git clone --filter=blob:none --no-checkout https://github.com/cashubtc/nuts /corpus/nuts; \
    git -C /corpus/nuts checkout -q "${NUTS_COMMIT}"; \
    rm -rf /corpus/nuts/.git; \
    git clone --filter=blob:none --no-checkout https://github.com/cashubtc/cdk /corpus/cdk; \
    git -C /corpus/cdk checkout -q "${CDK_COMMIT}"; \
    rm -rf /corpus/cdk/.git; \
    printf '%s\n' \
        "cashubtc/nuts   ${NUTS_COMMIT}" \
        "cashubtc/cdk    ${CDK_COMMIT}  (tag v0.17.2)" \
        > /corpus/PINS.txt

# =============================================================================================
# the specialist image
# =============================================================================================
FROM ${BASE_IMAGE}
ARG CDK_VERSION
ARG CDK_COMMIT
ARG NUTS_COMMIT

COPY --from=mint-builder /out/bin/cdk-mintd /usr/local/bin/cdk-mintd
COPY --from=mint-builder /out/bin/test-mint-seed /usr/local/bin/test-mint-seed
COPY --from=mint-builder /out/bin/test-mint-invoice /usr/local/bin/test-mint-invoice
COPY --from=mint-builder /out/bin/mint-acceptance /usr/local/bin/mint-acceptance
COPY --from=corpus /corpus /opt/cashu/corpus

# Runtime Rust (advisor F1).
#
# A Rust toolchain plus a warm registry is still not a build environment: rustc shells out to a C
# linker, and this base image ships no compiler at all (measured: no cc, no gcc, no ld, 119 dpkg
# entries). Without these three packages the offline build dies at `linker \`cc\` not found` after
# resolving every dependency correctly — which looks like a dependency problem and is not one.
#
# These are NOT version-pinned, deliberately. Pinning exact Debian package versions makes the image
# unbuildable the moment bookworm rotates a point release, and the reproducibility that matters here
# is the Rust/CDK dependency set, which IS locked. The exact versions installed are recorded into
# /opt/rust/TOOLCHAIN.txt at build time so a replay can be audited rather than assumed.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends gcc libc6-dev binutils; \
    rm -rf /var/lib/apt/lists/*
COPY --from=mint-builder /out/rust /opt/rust
ENV CARGO_HOME=/opt/rust/cargo \
    PATH=/opt/rust/toolchain/bin:/opt/rust/cargo/bin:$PATH
# A LOGIN shell throws the ENV above away: Debian's /etc/profile assigns PATH unconditionally
# (/etc/profile:5), so `bash -lc rustc` fails in an image where `bash -c rustc` works. That is a
# trap for anything invoking this image through a login shell, which is the common case for a job
# harness, so put the toolchain back on PATH for login shells too rather than relying on callers
# to know. Measured on this image before the fix: `bash -lc` saw the stock six-entry PATH.
RUN printf '%s\n' 'PATH="/opt/rust/toolchain/bin:/opt/rust/cargo/bin:$PATH"' 'export PATH' \
      > /etc/profile.d/10-rust-toolchain.sh \
 && chmod 0644 /etc/profile.d/10-rust-toolchain.sh \
 && bash -lc 'command -v rustc && command -v cargo' >/dev/null
# Offline by default: a specialist job must not silently reach crates.io and call that a warm cache,
# and a contained job may have no route there at all. `--offline` on the command line is redundant
# once this is set; `CARGO_NET_OFFLINE=false` in a job's environment is the deliberate opt-out.
ENV CARGO_NET_OFFLINE=true

# The curated knowledge topics, INSIDE the image (advisor F2). The injected MEMORY.md index names
# these container paths, so every link in it resolves for a contained job. Non-secret prose only —
# no seat home, no wallet, no key is mounted or copied here.
COPY docker/maxplayer-cashu-sandbox/seat/memory/ /opt/cashu/knowledge/
# The example is shipped as SOURCE, not as a binary, on purpose: compiling it is the toolchain proof.
COPY docker/maxplayer-cashu-sandbox/examples/ /opt/cashu/examples/
COPY docker/maxplayer-cashu-sandbox/test-mint.config.toml /opt/cashu/test-mint.config.toml
COPY docker/maxplayer-cashu-sandbox/bin/test-mint /usr/local/bin/test-mint
COPY docker/maxplayer-cashu-sandbox/bin/cashu-toolchain-check /usr/local/bin/cashu-toolchain-check

# The job runs as a host uid with no passwd entry, so everything it must read has to be
# world-readable and everything it must run world-executable (same rule as the base image's
# `install -m 0755`).
#
# CARGO_HOME is world-writable because cargo takes a lock file inside it (`.package-cache`) on every
# invocation, and the job's uid is not known at build time. This container is a single-principal
# disposable sandbox, so a shared-writable cache costs nothing here; it would be wrong in a
# multi-tenant image.
#
# TEST_MINT_STATE_ROOT is created world-writable for the same reason: the mint's private state lives
# there, OUTSIDE the delivered workdir (advisor F3).
RUN set -eux; \
    chmod 0755 /usr/local/bin/cdk-mintd /usr/local/bin/test-mint-seed \
        /usr/local/bin/test-mint-invoice /usr/local/bin/mint-acceptance \
        /usr/local/bin/test-mint /usr/local/bin/cashu-toolchain-check; \
    chmod -R a+rX /opt/cashu /opt/rust; \
    chmod -R a+rwX /opt/rust/cargo; \
    mkdir -p /var/lib/cashu-test-state; \
    chmod 1777 /var/lib/cashu-test-state; \
    cdk-mintd --version; \
    rustc --version; \
    cargo --version; \
    test-mint --help >/dev/null; \
    cashu-toolchain-check --help >/dev/null; \
    test -f /opt/cashu/knowledge/MEMORY.md; \
    test -f /opt/cashu/examples/wallet-roundtrip/Cargo.lock

# The offline build proof runs AT BUILD TIME, so the image cannot ship claiming an offline Rust/CDK
# environment it does not have. Both failures found by running it — a login shell losing PATH, and a
# missing C linker — would have shipped silently otherwise. Built in /tmp and deleted: the proof is
# that it compiled, and a stale target/ tree in the image would only be weight.
RUN set -eux; \
    { echo "# runtime build environment, recorded at image build"; \
      rustc --version; \
      cargo --version; \
      gcc --version | head -1; \
      ld --version | head -1; \
      dpkg-query -W -f='gcc ${Version}\n' gcc; \
      dpkg-query -W -f='libc6-dev ${Version}\n' libc6-dev; \
      dpkg-query -W -f='binutils ${Version}\n' binutils; \
    } >> /opt/rust/TOOLCHAIN.txt; \
    chmod 0644 /opt/rust/TOOLCHAIN.txt; \
    cp -a /opt/cashu/examples/wallet-roundtrip /tmp/offline-build-proof; \
    cd /tmp/offline-build-proof; \
    CARGO_TARGET_DIR=/tmp/offline-build-proof-target cargo build --offline --locked; \
    test -x /tmp/offline-build-proof-target/debug/wallet-roundtrip; \
    cd /; \
    rm -rf /tmp/offline-build-proof /tmp/offline-build-proof-target

LABEL ai.maxplayer.specialist="cashu-cdk" \
      ai.maxplayer.cashu.cdk-version="${CDK_VERSION}" \
      ai.maxplayer.cashu.cdk-commit="${CDK_COMMIT}" \
      ai.maxplayer.cashu.nuts-commit="${NUTS_COMMIT}" \
      ai.maxplayer.cashu.mint-backend="fakewallet (worthless test ecash, loopback only)" \
      ai.maxplayer.cashu.knowledge-path="/opt/cashu/knowledge" \
      ai.maxplayer.cashu.corpus-path="/opt/cashu/corpus" \
      ai.maxplayer.cashu.test-state-root="/var/lib/cashu-test-state"
