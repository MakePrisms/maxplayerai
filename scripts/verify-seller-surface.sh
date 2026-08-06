#!/usr/bin/env bash
#
# Capability gate for the SHIPPED artifact — asserts WHICH features were compiled in.
#
# Since #510 this runs against the one binary a release publishes: `wallet,acp`, able to buy and to
# sell. It exists because `maxplayer version` succeeds identically in every feature combination, so
# nothing else in the release pipeline can tell one build from another. A shipped binary built
# without `acp` compiles clean, packages clean, installs clean, and hands a seller a binary that
# cannot run a job or advertise a seat — the failure arrives at the first award, on someone else's
# sats.
#
# Its mirror, `verify-racer-surface.sh`, asserts the opposite shape. Nothing ships from that path any
# more; it runs in `ci.yml` against a default-feature build, so the buyer-only feature set — still
# supported from source — keeps compiling and keeps being acp-free.
#
#   * `wallet` must be IN, or the seller cannot hold ecash or be paid.
#   * `acp` must be IN. That feature compiles the agent-execution path (`run`) and, with it, the
#     `sell` advertise surface (#360). Both are checked, because they are separate CLI arms.
#
# ★ `--help` cannot substitute for the `acp` probe. The usage text lists `maxplayer run` even in a
#   build where `acp` is absent, so it moves with nothing. It IS the right instrument for `sell`,
#   which is written into the usage string under `#[cfg(feature = "acp")]` — and that probe is
#   paired below with one that invokes the command, because a usage string is a claim about the
#   surface and the dispatch arm is the surface.
#
# ★ Nothing here may publish. `maxplayer sell` with valid arguments BOOTS: it publishes kind-0 and
#   NIP-89 discoverability and starts the heartbeat, which on a builder would advertise a seat that
#   exists for the length of a CI job. The probe below therefore hands `sell` an argument its own
#   parser rejects, so it refuses inside `sell::run` before any relay, home or key is touched.
#
# Usage:
#   ./scripts/verify-seller-surface.sh [path-to-binary]        # default: result/bin/maxplayer

set -euo pipefail

BINARY="${1:-result/bin/maxplayer}"

die() { echo "verify-seller-surface: $*" >&2; exit 1; }

[ -f "$BINARY" ] || die "no binary at $BINARY — build one with: cargo build -p maxplayer --release --no-default-features --features wallet,acp"
[ -x "$BINARY" ] || die "$BINARY is not executable"

# A scratch home, unconditionally. With MAXPLAYER_HOME unset, maxplayer falls back to ~/.maxplayer — a real
# wallet home on a developer machine — and `wallet balance` below would read it. The value is forced
# rather than checked so a caller's home can never leak in.
MAXPLAYER_HOME="$(mktemp -d)"
export MAXPLAYER_HOME
trap 'rm -rf "$MAXPLAYER_HOME"' EXIT

# The two messages the acp probe has to distinguish. Both builds exit non-zero on it, so the exit
# code carries nothing; anything other than these is treated as inconclusive, so that a reworded
# message fails the check instead of silently passing it.
ACP_ABSENT='requires rebuilding with the acp feature'
ACP_PRESENT='spawn ACP agent'

# ── acp must be PRESENT ─────────────────────────────────────────────────────────────────────────
# The agent command is a path that deliberately does not exist. A build with acp gets as far as
# trying to spawn it and reports that failure — which is the signal we are looking for — and nothing
# is ever executed. Naming a real command instead would make the probe depend on that command
# existing on the builder, which is not true across platforms (`/bin/true` is absent on NixOS).
set +e
acp_out="$("$BINARY" run --agent-command /nonexistent/maxplayer-acp-probe --task probe --log /dev/null 2>&1)"
acp_rc=$?
set -e

if grep -q "$ACP_ABSENT" <<<"$acp_out"; then
    die "acp is NOT compiled into this artifact: \`maxplayer run\` answered with the rebuild hint. A seller artifact built without it cannot execute a job it has claimed."$'\n'"$acp_out"
fi
if ! grep -q "$ACP_PRESENT" <<<"$acp_out"; then
    die "\`maxplayer run\` failed for a reason this check does not recognise (rc=$acp_rc), so it proves nothing either way:"$'\n'"$acp_out"
fi
echo "ok: acp present — \`maxplayer run\` reaches the agent-execution path"

# ── the seller ADVERTISE surface must be PRESENT ────────────────────────────────────────────────
# `sell` is the ONLY CLI entry that publishes seller discoverability (kind-0 + NIP-89) and boots the
# heartbeat loop (#360). It is compiled in under `acp` — on the racer artifact it is not merely
# refused but absent — so this is what separates a seller artifact from a racer one that happens to
# carry `run`.
set +e
help_out="$("$BINARY" --help 2>&1)"
help_rc=$?
set -e
[ "$help_rc" -eq 0 ] \
    || die "\`maxplayer --help\` exited $help_rc — cannot read the surface to check for \`sell\`:"$'\n'"$help_out"
if ! grep -q 'maxplayer sell' <<<"$help_out"; then
    die "\`maxplayer sell\` is not listed in --help — the seller advertise surface is not compiled into this artifact (#360):"$'\n'"$help_out"
fi

# The usage text is a static string; this is the arm. An option `sell` does not have reaches
# `sell::run`'s own parser, which names it — a message that exists only in the module compiled in
# under `acp`. The racer artifact falls through to the generic top-level usage instead, which never
# mentions a sell option, so the verdict moves with the feature rather than with the exit code
# (both builds exit 1 here).
set +e
sell_out="$("$BINARY" sell --not-a-sell-option 2>&1)"
sell_rc=$?
set -e
if [ "$sell_rc" -ne 1 ]; then
    die "\`maxplayer sell --not-a-sell-option\` exited $sell_rc, not the usage-error 1 its parser returns:"$'\n'"$sell_out"
fi
if ! grep -q 'unknown sell option' <<<"$sell_out"; then
    die "\`maxplayer sell\` did not reach its own option parser — it fell through to the generic usage, which is what a build without the seller surface does:"$'\n'"$sell_out"
fi
# The probe must have REFUSED, not booted. `sell` publishes before it can fail, so a probe that got
# past the parser would have advertised a seat from a builder — assert none of the boot path was
# reached rather than trusting the argument to have been rejected early.
if grep -qE 'discoverable kind0=|relay-git seed probe|relay-git NIP-34 announce|discoverability publish' <<<"$sell_out"; then
    die "the \`sell\` probe reached the discoverability/boot path — it must refuse in the parser and publish nothing:"$'\n'"$sell_out"
fi
echo "ok: sell present — the seller advertise surface is compiled in, and the probe refused before boot"

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
# Every probe above reads a message out of the CLI. If this binary answered every invocation the
# same way, that agreement would be meaningless — so confirm an unknown subcommand is rejected.
set +e
"$BINARY" not-a-subcommand >/dev/null 2>&1
control_rc=$?
set -e
[ "$control_rc" -ne 0 ] \
    || die "control failed: an unknown subcommand exits 0, so the exit codes above carry no information"
echo "ok: control -> unknown subcommand exits nonzero"

echo "PASS: $BINARY is the seller surface — wallet in, acp in, sell in"
