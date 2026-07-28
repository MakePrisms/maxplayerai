#!/usr/bin/env bash
#
# Capability gate for a shipped buyer artifact — asserts WHICH features were compiled in.
#
# `verify-static-artifact.sh` proves the artifact runs anywhere; it cannot tell which build it is,
# because `mobee version` succeeds identically in every feature combination. This script answers the
# other question, and it matters for two different reasons:
#
#   * `wallet` must be IN, or the published buyer can't hold ecash and the package is inert.
#   * `acp` must be OUT. That feature compiles the seller's agent-execution path — the code that
#     spawns an agent process against a task. A buyer artifact carrying it would hand every user a
#     local execution surface nobody asked for, and the flake's `buyer-static` deliberately omits it.
#
# Both probes are driven through the real CLI rather than inspected, and both were measured to need
# no network (verified inside `docker run --network none`), so this is safe to run on an offline
# builder.
#
# ★ `--help` cannot substitute for either probe. The usage text is a static string: it lists
#   `mobee run` even in a build where `acp` is absent. Only invoking the subcommand distinguishes
#   them.
#
# Usage:
#   ./scripts/verify-buyer-surface.sh [path-to-binary]        # default: result/bin/mobee

set -euo pipefail

BINARY="${1:-result/bin/mobee}"

die() { echo "verify-buyer-surface: $*" >&2; exit 1; }

[ -f "$BINARY" ] || die "no binary at $BINARY — build one with: nix build .#buyer-static"
[ -x "$BINARY" ] || die "$BINARY is not executable"

# A scratch home, unconditionally. With MOBEE_HOME unset, mobee falls back to ~/.mobee — a real
# wallet home on a developer machine — and `wallet balance` below would read it. The value is forced
# rather than checked so a caller's home can never leak in.
MOBEE_HOME="$(mktemp -d)"
export MOBEE_HOME
trap 'rm -rf "$MOBEE_HOME"' EXIT

# Both builds exit non-zero on the probe below, so the exit code cannot tell them apart — only the
# message can. These are the two it has to distinguish, and anything else is treated as inconclusive
# so that a reworded message fails the check instead of silently passing it.
ACP_ABSENT='requires rebuilding with the acp feature'
ACP_PRESENT='spawn ACP agent'

# ── acp must be ABSENT ──────────────────────────────────────────────────────────────────────────
# The agent command is a path that deliberately does not exist. A build with acp gets as far as
# trying to spawn it and reports that failure — which is the signal we are looking for — and nothing
# is ever executed. Naming a real command instead would make the probe depend on that command
# existing on the builder, which is not true across platforms (`/bin/true` is absent on NixOS).
set +e
acp_out="$("$BINARY" run --agent-command /nonexistent/mobee-acp-probe --task probe --log /dev/null 2>&1)"
acp_rc=$?
set -e

if [ "$acp_rc" -eq 0 ]; then
    die "\`mobee run\` succeeded — acp is compiled into this artifact, which must ship the buyer surface only"
fi
if grep -q "$ACP_PRESENT" <<<"$acp_out"; then
    die "acp IS compiled into this artifact: \`mobee run\` reached the agent-execution path. The buyer artifact must be built without it."$'\n'"$acp_out"
fi
if ! grep -q "$ACP_ABSENT" <<<"$acp_out"; then
    die "\`mobee run\` failed for a reason this check does not recognise, so it proves nothing either way:"$'\n'"$acp_out"
fi
echo "ok: acp absent — the seller agent-execution path is not compiled in"

# ── wallet must be PRESENT ──────────────────────────────────────────────────────────────────────
# A local read of the wallet store in the scratch home: it reports a configured mint and a zero
# balance without touching the network.
set +e
wallet_out="$("$BINARY" wallet balance 2>&1)"
wallet_rc=$?
set -e

if [ "$wallet_rc" -ne 0 ]; then
    die "\`mobee wallet balance\` exited $wallet_rc — the wallet feature looks absent or the artifact is broken:"$'\n'"$wallet_out"
fi
if ! grep -q 'balance_sats=' <<<"$wallet_out"; then
    die "\`mobee wallet balance\` printed no balance, so the wallet surface is not usable:"$'\n'"$wallet_out"
fi
echo "ok: wallet present — $(head -1 <<<"$wallet_out")"

# ── Negative control ────────────────────────────────────────────────────────────────────────────
# Both probes above read a message out of the CLI. If this binary answered every invocation the same
# way, that agreement would be meaningless — so confirm an unknown subcommand is actually rejected.
set +e
"$BINARY" not-a-subcommand >/dev/null 2>&1
control_rc=$?
set -e
[ "$control_rc" -ne 0 ] \
    || die "control failed: an unknown subcommand exits 0, so the exit codes above carry no information"
echo "ok: control -> unknown subcommand exits nonzero"

echo "PASS: $BINARY is the buyer surface — wallet in, acp out"
