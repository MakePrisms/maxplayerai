#!/usr/bin/env bash
#
# Verifier for the aarch64 release artifact.
#
# Separate from verify-static-artifact.sh on purpose: that script proves a binary RUNS on this
# machine, in two libcs, via containers. Neither claim is available for a cross-built aarch64 binary
# on an x86_64 host — `ldd` cannot report a foreign architecture's linkage, and the binary will not
# execute without emulation. So this asserts STRUCTURE, and runs the binary only if the host can
# actually execute aarch64.
#
# It never reports PASS for structure alone. A cross-built artifact nobody has executed is a
# reasonable thing to ship on evidence, but it is not the same claim as "this runs", and the two must
# not print the same word.
#
# Usage:
#   ./scripts/verify-arm64-artifact.sh [path-to-binary]     # default: result/bin/mobee
#     nix build .#buyer-static-aarch64

set -euo pipefail

BINARY="${1:-result/bin/mobee}"
ELF_INFO="$(dirname "$0")/elf-info.mjs"

die() { echo "verify-arm64-artifact: $*" >&2; exit 1; }

command -v node >/dev/null 2>&1 || die "node not found"
[ -f "$BINARY" ]   || die "no binary at $BINARY — run: nix build .#buyer-static-aarch64"
[ -f "$ELF_INFO" ] || die "missing $ELF_INFO"

# ── Structure ───────────────────────────────────────────────────────────────────────────────────
# elf-info.mjs parses the headers directly. readelf/objdump/file are not installed on a stock NixOS
# host, and a missing tool behind a suppressed stderr prints the same thing as a clean result.
INFO="$(node "$ELF_INFO" "$BINARY")" || die "$BINARY is not a 64-bit little-endian ELF"

get() { sed -n "s/^$1=//p" <<<"$INFO"; }
MACHINE="$(get machine)"
INTERP="$(get interp)"
NEEDED="$(get dt_needed)"

[ "$MACHINE" = "AArch64" ] || die "wrong architecture: expected AArch64, got $MACHINE"
[ "$INTERP" = "absent" ]   || die "requests an ELF interpreter ($INTERP) — it is not static"
[ "$NEEDED" = "0" ]        || die "links $NEEDED shared libraries — it is not static"
echo "ok: AArch64, no ELF interpreter, no shared-library dependencies"

# A store path the binary NEEDS would make it unrunnable off this machine. grep exits 1 on no match,
# which is the outcome we want, so invert explicitly rather than letting `set -e` end the script.
if grep -q '/nix/store' "$BINARY"; then
    die "contains /nix/store references"
fi
echo "ok: no /nix/store references"

# ── Execution, if this host can manage it ───────────────────────────────────────────────────────
# Reported honestly either way. A skipped check that prints nothing is indistinguishable from a
# check that passed, so the absence of emulation is stated as loudly as a failure would be.
RAN=no
if docker run --rm --platform linux/arm64 alpine:3 true >/dev/null 2>&1; then
    SHIPDIR="$(mktemp -d)"
    trap 'rm -rf "$SHIPDIR"' EXIT
    cp -L "$BINARY" "$SHIPDIR/mobee"
    chmod 755 "$SHIPDIR/mobee"

    OUT="$(docker run --rm --platform linux/arm64 -v "$SHIPDIR:/b:ro" alpine:3 /b/mobee version)" \
        || die "aarch64 emulation is available but the artifact failed to run"
    grep -Eq '^mobee [0-9]+\.[0-9]+\.[0-9]+$' <<<"$OUT" \
        || die "unexpected version output under emulation: $OUT"
    echo "ok: runs under aarch64 emulation -> $OUT"

    if docker run --rm --platform linux/arm64 -v "$SHIPDIR:/b:ro" alpine:3 /b/mobee not-a-subcommand >/dev/null 2>&1; then
        die "control failed: exits 0 on an unknown subcommand, so the run above proves nothing"
    fi
    echo "ok: control -> unknown subcommand exits non-zero"
    RAN=yes
else
    echo "note: this host cannot execute aarch64 — no qemu binfmt and no container emulation"
fi

if [ "$RAN" = yes ]; then
    echo "PASS: aarch64 artifact is static and runs under emulation"
else
    echo "PARTIAL: structure verified; NOT EXECUTED. Run this on an arm64 host, or install qemu"
    echo "         binfmt, before treating the artifact as known-good."
fi
