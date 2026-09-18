# The containment sidecar (#797).
#
# WHAT IT IS FOR
#   A job's egress rules are installed INTO THE JOB'S OWN NETWORK NAMESPACE, before the job exists.
#   Something has to carry `CAP_NET_ADMIN` to do that, and it must not be the job: the job is a
#   stranger's code, and a container that can write its own firewall can erase it. So the capability
#   lives here, in a container that joins the namespace, applies a plan, and exits — while the job is
#   launched `--user <uid>:<gid> --cap-drop ALL --security-opt no-new-privileges` and can neither
#   alter nor read the rules.
#
# WHY IT IS A SEPARATE IMAGE FROM THE SANDBOX
#   The sandbox image must NOT contain iptables, and the job must never hold NET_ADMIN. Those two
#   facts are the whole reason this exists. Keeping them in one image would mean shipping the tool
#   and the capability to the party they are meant to constrain.
#
# WHAT IS *NOT* HERE, DELIBERATELY
#   No policy. The rules are rendered by `maxplayer-core::sandbox_net`, where they are unit-tested and
#   reviewable, and arrive on stdin. This image is a dumb applier — it cannot decide what containment
#   means, so it cannot get it subtly wrong in a way the tests do not see.
#
#   And no self-verification. The readback that proves the rules are in force is performed by the
#   daemon and compared in Rust against the rendered policy. A component must not certify its own
#   work: "the sidecar said it succeeded" is exactly the claim that needs independent checking, since
#   a partial application still leaves earlier rules behind.
#
# THE HOLDER IS NOT HERE EITHER
#   The container that owns the namespace for the job's lifetime reuses the SANDBOX image with an
#   exec-form `sleep` entrypoint, hardened at runtime with `--cap-drop ALL --read-only`. Reusing an
#   image we already trust keeps the trusted computing base from growing; a third image would add
#   supply-chain surface to hold a process whose entire job is to exist. It matters that the holder
#   runs nothing: `docker run -d` returns AFTER the entrypoint has begun executing, so the holder's
#   entrypoint is the one thing that runs in this namespace before the rules are installed.

# Pinned by digest: this image carries NET_ADMIN, so a moving tag is a supply-chain hole.
# alpine 3.24.1
FROM alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b

# `iptables` provides BOTH iptables and ip6tables. The policy denies the v6 LAN-equivalents as well,
# and an image with only the v4 binary would fail those rules at apply time rather than silently skip
# them — but there is no reason to rely on that.
#
# `iproute2` provides `tc` and `ip`, and they are as load-bearing as iptables is. A gVisor payload
# writes its packets straight onto the namespace's veth without traversing the host's `OUTPUT` chain,
# so the iptables plan alone contains a `runc` job and lets a `runsc` one through — measured, both
# families, TCP. The egress filter that closes that hole lives on the veth itself, and `tc` is the
# only tool that installs it. `ip` is here for the link enumeration the daemon validates the
# interface's identity from; both are read back by a separate container.
#
# The `test -x` assertions are the gate, not the `apk add`: a base image that moved `tc` into a
# subpackage would otherwise produce an image that builds clean and cannot filter, and the first
# thing that noticed would be a job running uncontained.
RUN apk add --no-cache iptables iproute2 \
    && test -x /usr/sbin/iptables \
    && test -x /usr/sbin/ip6tables \
    && test -x /sbin/tc \
    && test -x /sbin/ip

COPY apply-policy.sh /usr/local/bin/apply-policy
COPY apply-iface.sh /usr/local/bin/apply-iface
RUN chmod 0755 /usr/local/bin/apply-policy /usr/local/bin/apply-iface

ENTRYPOINT ["/usr/local/bin/apply-policy"]
