# syntax=docker/dockerfile:1
#
# Run-anywhere maxplayer: a stranger can `docker run` a seller (or the buyer MCP)
# with no Rust, no system git, and no build toolchain on their host.
#
# Two stages:
#   1. builder  — compiles the `maxplayer` binary with the acp + wallet features.
#   2. runtime  — a slim Debian image carrying only the binary + CA roots.
#
# Delivery git is in-process (libgit2), so the runtime image needs NO system
# git. TLS roots for the relay/mint come from rustls' bundled Mozilla CA set,
# but we still install `ca-certificates` so any operator-supplied HTTPS mint
# with a private/enterprise root validates too.

# ---------------------------------------------------------------------------
# Stage 1: build
# ---------------------------------------------------------------------------
FROM rust:1-bookworm AS builder

WORKDIR /src

# Copy the whole workspace. .dockerignore already strips target/, .git, web/,
# and docs so the build context stays small.
COPY . .

# #818: `.dockerignore` strips `.git/`, so the build script in this stage has no repository to read
# and would stamp every image `(unknown)`. The caller is the only one who knows, so it says so:
#   docker build --build-arg MAXPLAYER_BUILD_COMMIT="$(git rev-parse HEAD)" .
# `ENV` as well as `ARG` because the value has to reach `cargo` itself, not just this Dockerfile.
# Left unsupplied the image still builds and prints an honest `(unknown)` — never a made-up sha.
ARG MAXPLAYER_BUILD_COMMIT=
ENV MAXPLAYER_BUILD_COMMIT=${MAXPLAYER_BUILD_COMMIT}

# Release build of just the `maxplayer` binary.
#   acp    — REQUIRED for agent-backed job execution (not in the default set).
#   wallet — default feature; named explicitly for clarity.
# A cache mount keeps the cargo registry + target dir warm across rebuilds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p maxplayer --features acp,wallet \
    && cp /src/target/release/maxplayer /usr/local/bin/maxplayer \
    && strip /usr/local/bin/maxplayer || true

# ---------------------------------------------------------------------------
# Stage 2: runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# CA roots (for operator-supplied HTTPS mints with non-Mozilla roots) and
# tini for correct signal handling / zombie reaping of the long-lived daemon.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged user. The key file must be 0600 and owned by this
# user; /data (MAXPLAYER_HOME) is created up front so a named volume inherits the
# right ownership on first run.
RUN useradd --system --create-home --uid 10001 --shell /usr/sbin/nologin maxplayer \
    && mkdir -p /data \
    && chown maxplayer:maxplayer /data

COPY --from=builder /usr/local/bin/maxplayer /usr/local/bin/maxplayer

# Seller home lives on a mounted volume so the key, wallet, config, and journal
# survive image upgrades. See docs/DOCKER.md for the upgrade path.
ENV MAXPLAYER_HOME=/data
VOLUME ["/data"]

USER maxplayer
WORKDIR /data

# tini as PID 1 so SIGTERM from `docker stop` cleanly shuts the daemon.
ENTRYPOINT ["/usr/bin/tini", "--", "maxplayer"]

# Default to the seller daemon. `maxplayer seller` with no args relaunches zero-prompt
# from an existing config.toml; first run needs --agent + --rate-sats (see
# docker-compose.yml / docs/DOCKER.md). Override the command for `mcp`, `doctor`,
# `wallet`, etc.
CMD ["seller"]
