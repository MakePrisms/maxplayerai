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
# them at run time, so a job's first turn is not spent installing its own harness. NOTE: each adapter
# shells out to its underlying CLI (the `claude` / `codex` binary, via CLAUDE_CODE_EXECUTABLE for
# claude); provisioning that CLI + its credentials into this image is the seller-operator step layered
# on top of this base.
RUN npm install -g \
    @agentclientprotocol/claude-agent-acp \
    @agentclientprotocol/codex-acp \
    && npm cache clean --force

# HOME is a CONTAINER-ONLY path, deliberately NOT the mounted workdir.
#
# The job runs as the seller's uid (`docker run --user`), which is a host uid with no /etc/passwd
# entry here — so there is no home directory owned by it, and the default HOME (`/`) is unwritable
# by a non-root user. That is the whole constraint: HOME must be writable by an uid unknown at build
# time. Hence a 0777 directory rather than a per-user one. It is a single-job container with one
# principal inside, so a world-writable home grants nothing to anyone who is not already the job.
#
# It must NOT be /work: the workdir is the DELIVERABLE, staged wholesale by the delivery snapshot
# (`seller_git`: `add_all(["*"])`, minus the one runtime-artifact exclusion) into a fresh repo that
# carries no .gitignore. Anything a tool caches under $HOME — rustup/cargo toolchains, the npm and
# pip caches, the ACP adapters' own config — would otherwise be committed and pushed as the agent's
# work product. On the overlay filesystem instead, `--rm` discards it and the delivery stays clean.
#
# Overlay, not --tmpfs: a toolchain install is GB-scale, and tmpfs would charge that to RAM on every
# concurrent slot.
RUN mkdir -p /home/agent && chmod 0777 /home/agent
ENV HOME=/home/agent

# The agent works in the mounted workdir — the ACP session cwd is pinned to it independently
# (`CONTAINER_WORKDIR`), and the delivery identity arrives as GIT_AUTHOR_*/GIT_COMMITTER_* env plus
# the workdir's own .git/config, so neither depends on HOME.
WORKDIR /work
