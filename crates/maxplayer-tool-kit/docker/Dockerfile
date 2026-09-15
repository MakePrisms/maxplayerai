# Linux image carrying all five kit binaries.
#
# Build context is the crate directory, not the repository: this crate declares explicit
# dependency versions and no workspace membership requirement, so it compiles on its own. That
# keeps the image build honest about the crate's dependency surface — if it needed something
# from maxplayer-core, this build would fail rather than quietly pick it up.
#
# One image serves every role in the demo (vendor, holder, job container). The roles differ by
# entrypoint and, far more importantly, by what is mounted into them: the job container is given
# its own socket and its own directory, and nothing else.

# Both stages are pinned by digest, not by tag. `rust:1-slim-bookworm` and `debian:bookworm-slim`
# are mutable tags: the same Dockerfile would build different images on different days, and an
# acceptance run could not identify the image it tested. Resolved 2026-09-10 on linux/arm64:
#   rust:1-slim-bookworm   -> rustc 1.98.1 (48a229cea 2026-09-01)
#   debian:bookworm-slim
# Updating a base image is therefore a visible, reviewable edit to this file.
FROM rust@sha256:ebd900bae66fd508b466cef82d64a83a5fb34682e4c8b2797a42908bddc95a57 AS build
WORKDIR /src
# Cargo.lock is committed beside the crate manifest and copied in, so the image build resolves
# no versions of its own. `--locked` then fails rather than silently updating a dependency,
# which is what makes two builds of one source tree comparable.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY fixtures ./fixtures
RUN cargo build --locked --release && \
    strip target/release/vendor-service \
          target/release/vendor-cli \
          target/release/tool-holderd \
          target/release/holderctl \
          target/release/tool-mcp-bridge

FROM debian@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
# Nothing is installed beyond the base image. Note for readers of older evidence: the demo used
# to grep for the credential from *inside* a job container, which required passing the secret
# into that container and so could not prove absence (advisor F1). Credential-absence scanning
# is host-side and the secret never enters a job-shaped container.
COPY --from=build /src/target/release/vendor-service /usr/local/bin/
COPY --from=build /src/target/release/vendor-cli /usr/local/bin/
COPY --from=build /src/target/release/tool-holderd /usr/local/bin/
COPY --from=build /src/target/release/holderctl /usr/local/bin/
COPY --from=build /src/target/release/tool-mcp-bridge /usr/local/bin/
COPY fixtures/seller-tool-config.json /etc/maxplayer/seller-tool-config.json

# Short by construction: a Unix socket path is capped at SUN_LEN (108 bytes on Linux), and the
# per-job sockets live under this directory.
RUN mkdir -p /run/holder /var/lib/holder /srv/jobs

# No binary in this kit has a --help path: both CLIs print a usage line to stderr and exit
# non-zero when their arguments are wrong. So the default command does not pretend to be a
# help screen — it names what the image carries and exits 0. Every real use of this image
# overrides the command anyway (vendor-service, tool-holderd, or a job's own entrypoint).
CMD ["/bin/sh", "-c", "echo 'maxplayer-tool-kit image. Run one of: vendor-service, vendor-cli, tool-holderd, holderctl, tool-mcp-bridge.'; echo 'CLI usage: vendor-cli <login|health|whoami|transform|logout> | holderctl <status|health|tools|attach|detach|reenroll|shutdown> --socket <path>'"]
