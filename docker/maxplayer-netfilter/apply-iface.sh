#!/bin/sh
# Applies a rendered EGRESS INTERFACE plan inside the job's network namespace (#797 follow-on).
#
# WHY THIS EXISTS ALONGSIDE apply-policy
#   `apply-policy` installs the iptables plan. That plan contains a `runc` job and does not contain a
#   `runsc` (gVisor) one: gVisor's netstack writes the payload's packets onto the namespace's veth
#   without traversing the host's `OUTPUT` chain, so the denied destinations stay reachable — measured
#   on both families over TCP. The filter that closes it has to sit on the veth itself, which means
#   `tc`, which means a second plan and a second applier.
#
# Input: the plan on stdin, one rule per line, as `tc <args...>` — exactly the argv rendered by
# `maxplayer-core::sandbox_iface::IfacePlan`. This script holds no policy of its own, and in
# particular it does not choose the interface, the prefixes or the order: all three are rendered and
# unit-tested in Rust, and read back out of the kernel by a different container before any payload
# starts. A dumb applier cannot get containment subtly wrong in a way the renderer's tests do not see.
#
# Exit codes are what the daemon branches on, so they are specific rather than a bare 1:
#
#   0  every rule applied, and there was at least one
#   3  a rule failed. The namespace is now PARTIALLY filtered. The caller must DESTROY THE HOLDER
#      rather than retry: the qdisc and the filters that did land are still there, and a retry stacks
#      a second copy on top of them at the same priorities, leaving a ruleset whose order nobody
#      rendered.
#   4  the plan was empty — refusing to report success for an unfiltered interface
#   5  a line named a binary other than tc
#   6  `tc` is not in this image at all. Distinct from 3 because it is a BUILD fault, not a runtime
#      one: it means the sidecar image shipped without iproute2, and no amount of retrying or
#      re-rendering will fix it.
set -u

# Checked once, before anything is read, so the error names the real fault instead of surfacing as
# rule 1 of N failing with "not found". An image without `tc` can still apply an iptables plan
# perfectly, so this is exactly the skew that would otherwise ship quietly.
command -v tc >/dev/null 2>&1 || {
    echo "apply-iface: no 'tc' in this image — the sidecar was built without iproute2" >&2
    exit 6
}

applied=0

while IFS= read -r line; do
    [ -n "$line" ] || continue

    # The first field names the binary. Anything else is refused rather than executed: this container
    # is one of the two things in the design that hold CAP_NET_ADMIN, so it must never become a
    # general-purpose exec surface for whatever can reach its stdin. `ip` is deliberately NOT allowed
    # here even though the image carries it — enumerating links is a read the daemon does from an
    # unprivileged container, and an applier that can also run `ip link` can move an interface.
    binary=${line%% *}
    case "$binary" in
        tc) ;;
        *)
            echo "apply-iface: refusing to run '$binary' — an interface plan may only name tc" >&2
            exit 5
            ;;
    esac

    # Intentionally unquoted: POSIX sh word-splits, and the fields ARE the argv.
    # (Porting note: zsh does NOT word-split, so the same line there executes as one command NAME and
    # fails with "command not found" naming the entire rule.)
    # shellcheck disable=SC2086
    if ! $line; then
        echo "apply-iface: rule $((applied + 1)) failed: $line" >&2
        echo "apply-iface: namespace is PARTIALLY filtered — destroy the holder, do not retry" >&2
        exit 3
    fi
    applied=$((applied + 1))
done

# An empty plan applies cleanly and proves nothing. Reporting success here would hand the daemon a
# green for an interface with no egress filter at all — the failure mode being avoided is not "the
# filters were wrong" but "there were none and everything said OK".
if [ "$applied" -eq 0 ]; then
    echo "apply-iface: empty plan — refusing to report success for an unfiltered interface" >&2
    exit 4
fi

# The count is echoed so the caller can cross-check it against the number of rules it rendered. A
# mismatch means stdin was truncated in transit, which no exit code would otherwise reveal.
echo "$applied"
