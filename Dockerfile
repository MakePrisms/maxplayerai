# syntax=docker/dockerfile:1
#
# Run-anywhere mobee: a stranger can `docker run` a seller (or the buyer MCP)
# with no Rust, no system git, and no build toolchain on their host.
#
# Two stages:
#   1. builder  — compiles the `mobee` binary with the acp + wallet features.
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

# Release build of just the `mobee` binary.
#   acp    — REQUIRED for agent-backed job execution (not in the default set).
#   wallet — default feature; named explicitly for clarity.
# A cache mount keeps the cargo registry + target dir warm across rebuilds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p mobee --features acp,wallet \
    && cp /src/target/release/mobee /usr/local/bin/mobee \
    && strip /usr/local/bin/mobee || true

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
# user; /data (MOBEE_HOME) is created up front so a named volume inherits the
# right ownership on first run.
RUN useradd --system --create-home --uid 10001 --shell /usr/sbin/nologin mobee \
    && mkdir -p /data \
    && chown mobee:mobee /data

COPY --from=builder /usr/local/bin/mobee /usr/local/bin/mobee

# Seller home lives on a mounted volume so the key, wallet, config, and journal
# survive image upgrades. See docs/DOCKER.md for the upgrade path.
ENV MOBEE_HOME=/data
VOLUME ["/data"]

USER mobee
WORKDIR /data

# tini as PID 1 so SIGTERM from `docker stop` cleanly shuts the daemon.
ENTRYPOINT ["/usr/bin/tini", "--", "mobee"]

# Default to the seller daemon. `mobee sell` with no args relaunches zero-prompt
# from an existing config.toml; first run needs --agent + --rate-sats (see
# docker-compose.yml / docs/DOCKER.md). Override the command for `mcp`, `doctor`,
# `wallet`, etc.
CMD ["sell"]
