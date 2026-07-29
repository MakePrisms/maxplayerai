# The sandbox image the seller runs an awarded job inside (`[sandbox] mode = "docker"`).
#
# It carries only what a job legitimately needs: the node runtime, the ACP agent adapters the
# harness presets launch (claude/codex), git for the delivery commit, and CA certificates so the
# adapter's HTTPS calls (model API, relay, git host) validate. The seller mounts the per-job workdir
# at /work and runs the container as the seller's uid; NOTHING else of the host is present, so the
# wallet/keys/journal under $MAXPLAYER_HOME cannot be read from here.
#
# Build:  docker build -t maxplayer-sandbox:latest docker/maxplayer-sandbox
FROM node:22-bookworm-slim

# git for the delivery commit; ca-certificates for TLS to the model API / relay / git host.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The ACP adapters the built-in harness presets resolve on PATH (`claude-agent-acp`, `codex-acp` —
# see crates/maxplayer-core/src/agent_presets.rs). Baking them in means the job container does not fetch
# them at run time and can run under `network = "none"`. NOTE: each adapter shells out to its
# underlying CLI (the `claude` / `codex` binary, via CLAUDE_CODE_EXECUTABLE for claude); provisioning
# that CLI + its credentials into this image is the seller-operator step layered on top of this base.
RUN npm install -g \
    @agentclientprotocol/claude-agent-acp \
    @agentclientprotocol/codex-acp \
    && npm cache clean --force

# The job runs as the seller's uid (passed via `docker run --user`), working in the mounted workdir.
# HOME must be writable for whatever config the adapter caches; point it at the workdir mount.
ENV HOME=/work
WORKDIR /work
