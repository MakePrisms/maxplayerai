# The sandbox image the seller runs an awarded job inside (`[sandbox] mode = "docker"`).
#
# It carries only what a job legitimately needs: the node runtime, the ACP agent adapters the
# harness presets launch (claude/codex), git for the delivery commit, CA certificates so the
# adapter's HTTPS calls (model API, relay, git host) validate, and nix so a nix-flake-provisioned job
# has a provisioner. The seller mounts the per-job workdir at /work and runs the container as the
# seller's uid; NOTHING else of the host is present, so the wallet/keys/journal under
# $MAXPLAYER_HOME cannot be read from here.
#
# Build:  docker build -t maxplayer-sandbox:latest docker/maxplayer-sandbox

# nix arrives as a copied store rather than through the installer: the installer wants root and sets
# up a daemon, and the job container has neither — it runs as a host uid with `--cap-drop ALL` and
# `no-new-privileges`. Pinned to an exact release for the same reason the adapters below are.
FROM nixos/nix:2.31.2 AS nix
# Only nix's OWN closure. The upstream image's default profile also carries git, curl, coreutils, man
# and ssh — 702MB of store this image has no use for, against 114MB for nix alone. `local?root=`
# writes a real store (paths plus the registration db), so the next stage can use it as-is instead of
# importing a binary cache at run time.
RUN set -eux; \
    pkg="$(dirname "$(dirname "$(readlink -f "$(command -v nix)")")")"; \
    nix --extra-experimental-features nix-command \
        copy --no-check-sigs --to "local?root=/nixroot" "$pkg"; \
    ln -s "$pkg/bin" /nixroot/nix/nixbin

FROM node:22-bookworm-slim

# git for the delivery commit; ca-certificates for TLS to the model API / relay / git host.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The ACP adapters the built-in harness presets resolve on PATH (`claude-agent-acp`, `codex-acp` —
# see crates/maxplayer-core/src/agent_presets.rs). Baking them in means the job container does not fetch
# them at run time, so a job's first turn is not spent installing its own harness. NOTE: each adapter
# shells out to its underlying CLI — the `claude` / `codex` binary. codex-acp bundles `@openai/codex`,
# so its CLI comes for free; the claude adapter needs the `claude` binary provisioned separately, so we
# install `@anthropic-ai/claude-code` here too. Both CLIs are now IN this base image; the seller-operator
# only needs to forward CREDENTIALS at run time (`docker run -e …` / the #647 credential proxy) — no
# token is baked. On a standard glibc image the adapter auto-resolves `claude` on PATH, so no
# CLAUDE_CODE_EXECUTABLE is needed here (that override is only for hosts like NixOS whose loader can't
# run the npm-shipped binary — see docs/SELLER-QUICKSTART.md Gotcha 2).
#
# Versions are PINNED to exact releases. An unpinned `npm install -g` resolves to whatever is latest at
# build time, so two builds of the same commit could bake different adapters — which makes the published
# image's digest identify the artifact without the artifact being reproducible. Pinning is what lets the
# digest correspond to a known adapter/CLI set, closing the gap called out in #792 (babu's F6 from the #221
# security review). Bump these deliberately alongside a release.
RUN npm install -g \
    @agentclientprotocol/claude-agent-acp@0.67.0 \
    @agentclientprotocol/codex-acp@1.2.0 \
    @anthropic-ai/claude-code@2.1.223 \
    && npm cache clean --force

# The nix a nix-flake-provisioned job needs. `EnvBackend::NixFlake` shells out to a bare `nix`
# (crates/maxplayer-core/src/env_provision.rs), and under `[sandbox] mode = "docker"` that name
# resolves HERE, not on the host — the host-side boot doctor's nix check cannot speak for the job's
# toolchain. The provisioner runs on an awarded job, so a missing nix is not a cheap boot failure;
# it is ENV_UNPROVISIONABLE on work the buyer is already committed to.
COPY --from=nix /nixroot/nix /nix

# `experimental-features` — `nix develop` and the rest of the `nix <subcommand>` surface are gated
# behind these two, and the provisioner's argv is exactly `nix develop <dir>#<devshell> --command`.
# `build-users-group=` — no daemon and no nixbld group here, so a build runs as the calling uid.
# `sandbox=false` — nix's build sandbox needs mount and user namespaces, which `--cap-drop ALL` and
# `no-new-privileges` deny. Everything unlisted keeps nix's own defaults, cache.nixos.org included.
#
# `use-xdg-base-directories` — nix uses $HOME only when the calling uid OWNS it, and $HOME here is
# root-owned by construction (see the HOME note below). This moves nix's state, cache and config to
# $XDG_STATE_HOME/$XDG_CACHE_HOME/$XDG_CONFIG_HOME, which is what keeps it off that ownership check.
# BOTH halves are required and neither is sufficient: the setting alone still derives its defaults
# from $HOME, and the XDG variables alone are ignored without it. Measured in this image as a uid with
# no passwd entry — either one on its own aborts with `cannot determine user's home directory`;
# together, `nix eval`, `nix-store --add` and `nix store ping` all succeed.
#
# The store is world-writable for the same reason HOME below is: the job's uid is unknown at build
# time, and a single-user store is written by whoever runs the build. The argument that makes it
# safe is also the same one — a single-job container with one principal inside, discarded whole by
# `--rm`. It stops holding the moment a store is shared BETWEEN jobs, which is why the store-integrity
# question gates that optimization rather than following it.
#
# The `per-user` directories are REMOVED rather than made writable, and by shape rather than by
# name: nix chmods each of them to 0755 when it opens the store, unconditionally, and chmod is
# refused to anyone who does not own the path — so a copied-in per-user owned by root fails every
# nix invocation with EPERM. Absent, the job's first nix call creates them and therefore owns them.
# There are two (`profiles/` and `gcroots/`) and fixing only the one an error happens to name leaves
# the other live. This is also the failure `nix --version` does not reach, because version reporting
# never opens the store — so it reports success against a store nix cannot use.
RUN set -eux; \
    mkdir -p /etc/nix; \
    { \
        echo 'experimental-features = nix-command flakes'; \
        echo 'build-users-group ='; \
        echo 'sandbox = false'; \
        echo 'use-xdg-base-directories = true'; \
    } > /etc/nix/nix.conf; \
    chmod 1777 /nix/store; \
    chmod -R a+rwX /nix/var; \
    find /nix/var -type d -name per-user -prune -exec rm -rf {} +

# nix reaches PATH as a direct symlink per entry point, so `nix`, `nix-build`, `nix-store` and the
# rest all resolve to the staged closure's own binaries with nothing in between.
#
# The loop asserts it linked something. An unmatched glob does not vanish in sh — it stays literal,
# so the loop would run once on the pattern itself, `ln -s` would create a dangling link named `*`,
# and `set -e` sees nothing wrong. The build would then go green having produced an image with no nix
# on PATH at all, and the first thing to notice would be a job that has already been awarded. Quoting
# does not help here, because the expansion is what is wanted; the check is on the count.
RUN set -eux; \
    links=0; \
    for binary in /nix/nixbin/*; do \
        [ -e "$binary" ] || { echo 'nix closure staged no binaries' >&2; exit 1; }; \
        ln -s "$binary" "/usr/local/bin/$(basename "$binary")"; \
        links=$((links + 1)); \
    done; \
    [ "$links" -ge 1 ]

# HOME is a CONTAINER-ONLY path, deliberately NOT the mounted workdir.
#
# The job runs as the seller's uid (`docker run --user`), which is a host uid with no /etc/passwd
# entry here — so there is no home directory owned by it, and the default HOME (`/`) is unwritable
# by a non-root user. That is the whole constraint: HOME must be writable by an uid unknown at build
# time. Hence a 0777 directory rather than a per-user one. It is a single-job container with one
# principal inside, so a world-writable home grants nothing to anyone who is not already the job.
#
# Writable is not the same as owned, and nix wants owned: it uses $HOME only when the calling uid
# owns it, and otherwise falls back to a getpwuid() lookup this container cannot satisfy. A 0777
# directory created at build time is owned by root, so an unconfigured nix aborts on every invocation
# with `cannot determine user's home directory`. That is what the XDG settings above answer, and they
# answer it for nix ALONE — anything else added here that resolves its own home the way nix does needs
# its own answer.
#
# The XDG directories do not exist in the image: nix creates them on first use, under this same 0777
# parent, and therefore owns them. So this path carries a run-time precondition — /home/agent must
# still be writable by the job uid when the container starts. Mounting over it, tightening the 0777,
# or moving the XDG variables to a read-only path breaks every nix call, and it surfaces as
# ENV_UNPROVISIONABLE on a job that has already been awarded, not as a boot failure the operator sees
# first. The same precondition carries anything told to install under $HOME.
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
ENV HOME=/home/agent \
    XDG_STATE_HOME=/home/agent/state \
    XDG_CACHE_HOME=/home/agent/cache \
    XDG_CONFIG_HOME=/home/agent/config

# The agent works in the mounted workdir — the ACP session cwd is pinned to it independently
# (`CONTAINER_WORKDIR`), and the delivery identity arrives as GIT_AUTHOR_*/GIT_COMMITTER_* env plus
# the workdir's own .git/config, so neither depends on HOME.
WORKDIR /work
