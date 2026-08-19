#!/bin/sh
# Applies a rendered containment plan inside the job's network namespace (#797).
#
# Input: the plan on stdin, one rule per line, as `<binary> <args...>` — exactly the argv rendered by
# `maxplayer-core::sandbox_net::NetPolicy::install_plan`. This script holds no policy of its own, so
# it cannot get containment subtly wrong in a way the renderer's tests do not see.
#
# Exit codes are what the daemon branches on, so they are specific rather than a bare 1:
#
#   0  every rule applied, and there was at least one
#   3  a rule failed. The namespace is now PARTIALLY configured. The caller must DESTROY THE HOLDER
#      rather than retry: a retry appends the whole plan again on top of the rules that did land, and
#      the second attempt then reports success over a duplicated, half-ordered ruleset.
#   4  the plan was empty — refusing to report success for having done nothing
#   5  a line named a binary other than iptables/ip6tables
set -u

applied=0

while IFS= read -r line; do
    [ -n "$line" ] || continue

    # The first field names the binary. Anything else is refused rather than executed: this container
    # is the one thing in the design that holds CAP_NET_ADMIN, so it must never become a
    # general-purpose exec surface for whatever can reach its stdin.
    binary=${line%% *}
    case "$binary" in
        iptables | ip6tables) ;;
        *)
            echo "apply-policy: refusing to run '$binary' — a plan may only name iptables or ip6tables" >&2
            exit 5
            ;;
    esac

    # Intentionally unquoted: POSIX sh word-splits, and the fields ARE the argv.
    # (Porting note: zsh does NOT word-split, so the same line there executes as one command NAME and
    # fails with "command not found" naming the entire rule.)
    # shellcheck disable=SC2086
    if ! $line; then
        echo "apply-policy: rule $((applied + 1)) failed: $line" >&2
        echo "apply-policy: namespace is PARTIALLY configured — destroy the holder, do not retry" >&2
        exit 3
    fi
    applied=$((applied + 1))
done

# An empty plan applies cleanly and proves nothing. Reporting success here would hand the daemon a
# green for an entirely uncontained namespace — the failure mode being avoided is not "the rules were
# wrong" but "there were no rules and everything said OK".
if [ "$applied" -eq 0 ]; then
    echo "apply-policy: empty plan — refusing to report success for an uncontained namespace" >&2
    exit 4
fi

# The count is echoed so the caller can cross-check it against the number of rules it rendered. A
# mismatch means stdin was truncated in transit, which no exit code would otherwise reveal.
echo "$applied"
