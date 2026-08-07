#!/usr/bin/env bash
#
# Capability gate for a BUYER-ONLY (racer) build — asserts WHICH features were compiled in.
#
# ★ Since #510 no release ships this build. One binary is published, carrying the full surface, and
#   `verify-seller-surface.sh` is what gates it. This script keeps the buyer-only FEATURE SET
#   honest: `ci.yml` builds with default features and runs this, so `--no-default-features --features
#   wallet` stays a real, acp-free build for anyone compiling from source. A release that started
#   running this again would be shipping a binary that cannot sell.
#
# `verify-static-artifact.sh` proves the artifact runs anywhere; it cannot tell which build it is,
# because `maxplayer version` succeeds identically in every feature combination. This script answers the
# other question, and it matters for two different reasons:
#
#   * `wallet` must be IN, or the published racer cannot hold ecash and the package is inert.
#   * `acp` must be OUT. That feature compiles the seller's agent-execution path — the code that
#     spawns an agent process against a task. A racer artifact carrying it would hand every user a
#     local execution surface nobody asked for, and the flake's `buyer-static` deliberately omits it.
#
# Both probes are driven through the real CLI rather than inspected, and both were measured to need
# no network (verified inside `docker run --network none`), so this is safe to run on an offline
# builder.
#
# ★ `--help` cannot substitute for either probe. The usage text is a static string: it lists
#   `maxplayer run` even in a build where `acp` is absent. Only invoking the subcommand distinguishes
#   them.
#
# Usage:
#   ./scripts/verify-racer-surface.sh [path-to-binary]        # default: result/bin/maxplayer

set -euo pipefail

BINARY="${1:-result/bin/maxplayer}"

die() { echo "verify-racer-surface: $*" >&2; exit 1; }

[ -f "$BINARY" ] || die "no binary at $BINARY — build one with: nix build .#buyer-static"
[ -x "$BINARY" ] || die "$BINARY is not executable"

# A scratch home, unconditionally. With MAXPLAYER_HOME unset, maxplayer falls back to ~/.maxplayer — a real
# wallet home on a developer machine — and `wallet balance` below would read it. The value is forced
# rather than checked so a caller's home can never leak in.
MAXPLAYER_HOME="$(mktemp -d)"
export MAXPLAYER_HOME
trap 'rm -rf "$MAXPLAYER_HOME"' EXIT

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
acp_out="$("$BINARY" run --agent-command /nonexistent/maxplayer-acp-probe --task probe --log /dev/null 2>&1)"
acp_rc=$?
set -e

if [ "$acp_rc" -eq 0 ]; then
    die "\`maxplayer run\` succeeded — acp is compiled into this artifact, which must ship the racer surface only"
fi
if grep -q "$ACP_PRESENT" <<<"$acp_out"; then
    die "acp IS compiled into this artifact: \`maxplayer run\` reached the agent-execution path. The racer artifact must be built without it."$'\n'"$acp_out"
fi
if ! grep -q "$ACP_ABSENT" <<<"$acp_out"; then
    die "\`maxplayer run\` failed for a reason this check does not recognise, so it proves nothing either way:"$'\n'"$acp_out"
fi
echo "ok: acp absent — the seller agent-execution path is not compiled in"

# ── the seller ADVERTISE surface must be ABSENT ─────────────────────────────────────────────────
# `sell` is the ONLY CLI entry that publishes seller discoverability (kind-0 + NIP-89) and boots the
# heartbeat loop (#360). On the racer artifact (acp out) it is compiled out — not merely refused — so
# a buyer-only build cannot advertise a seat it can never deliver on and cost a buyer their sats at
# award. `run` above stays present on both builds and fails honestly; `sell` is the one that would
# PUBLISH before it could fail, so it must be GONE. The usage text is where a compiled-in `sell`
# still shows — an acp build lists it, the racer must not — which is what moves this check's verdict
# with the feature rather than with anything incidental.
set +e
help_out="$("$BINARY" --help 2>&1)"
help_rc=$?
set -e
[ "$help_rc" -eq 0 ] \
    || die "\`maxplayer --help\` exited $help_rc — cannot read the surface to check for \`sell\`:"$'\n'"$help_out"
if grep -q 'maxplayer seller' <<<"$help_out"; then
    die "\`maxplayer seller\` is listed in --help — the seller advertise surface is compiled into the racer artifact, which must ship buyer-only (#360):"$'\n'"$help_out"
fi
# Belt-and-suspenders: invoking it must land on the SAME generic usage error an unknown command
# gets — positive proof the arm fell through to `usage`, not that `sell` booted and happened to fail
# early (which would still be nonzero and might not print any of the advertise log strings below).
set +e
sell_out="$("$BINARY" seller --agent claude --rate-sats 100 2>&1)"
sell_rc=$?
set -e
if [ "$sell_rc" -ne 1 ]; then
    die "\`maxplayer seller\` exited $sell_rc, not the usage-error 1 an absent command falls through to — the seller surface may be compiled in:"$'\n'"$sell_out"
fi
if ! grep -q 'Usage:' <<<"$sell_out"; then
    die "\`maxplayer seller\` did not print the generic usage text, so it did not fall through to \`usage\` — the seller surface may be compiled in and failing after boot:"$'\n'"$sell_out"
fi
# Secondary: it must never have reached any advertise/boot log line (kind-0/NIP-89/heartbeat/NIP-34).
if grep -qE 'discoverable kind0=|relay-git seed probe|relay-git NIP-34 announce|discoverability publish' <<<"$sell_out"; then
    die "\`maxplayer seller\` reached the discoverability/boot path on the racer artifact:"$'\n'"$sell_out"
fi
echo "ok: sell absent — the seller advertise surface (kind-0/NIP-89/heartbeat) is not compiled in"

# ── wallet must be PRESENT ──────────────────────────────────────────────────────────────────────
# A local read of the wallet store in the scratch home: it reports a configured mint and a zero
# balance without touching the network.
set +e
wallet_out="$("$BINARY" wallet balance 2>&1)"
wallet_rc=$?
set -e

if [ "$wallet_rc" -ne 0 ]; then
    die "\`maxplayer wallet balance\` exited $wallet_rc — the wallet feature looks absent or the artifact is broken:"$'\n'"$wallet_out"
fi
if ! grep -q 'balance_sats=' <<<"$wallet_out"; then
    die "\`maxplayer wallet balance\` printed no balance, so the wallet surface is not usable:"$'\n'"$wallet_out"
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

echo "PASS: $BINARY is the racer surface — wallet in, acp out"
