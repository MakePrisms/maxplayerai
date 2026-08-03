#!/usr/bin/env bash
#
# Acceptance verifier for `install.sh` — drives the installer inside clean containers against a real
# published release and asserts every property #125 asks for, including the refusals.
#
# Usage:
#   ./scripts/verify-install-sh.sh <version> [path-to-install.sh]
#     e.g. ./scripts/verify-install-sh.sh 0.0.1
#
# `<version>` must be a version whose GitHub Release is published with linux assets. The installer is
# pointed at it explicitly, so this works against a pre-release too — which matters, because
# `/releases/latest` (the default path) has nothing to answer with until a stable release exists.
#
# ── Why containers, and why two ─────────────────────────────────────────────────────────────────
# The claim is "installs on a machine that has never heard of nix", so it has to be tested somewhere
# that has no nix, no rust and no prior maxplayer. The two images are not redundant:
#
#   alpine:3              musl; busybox wget, NO curl   → exercises the wget branch, zero packages added
#   debian:bookworm-slim  glibc; neither downloader     → curl installed, exercises the curl branch
#
# Between them both libc families and both download branches are covered. Committing to one image
# would leave whichever branch it lacks entirely unexecuted.
#
# ── Why the refusals are driven through shims ───────────────────────────────────────────────────
# A corrupt-download test needs the bytes to change between the release and the checksum comparison.
# The alternative — an env var telling install.sh where to fetch from, or one telling it to skip
# verification — would mean the shipped script carries a switch that turns the security property off,
# and the test would then be exercising a code path no user takes. So instead a wrapper is placed
# ahead of the real downloader on PATH and corrupts what it wrote. install.sh runs completely
# unmodified, exactly as a user gets it.
#
# ★ The pass-through control (leg 6) is what makes legs 7-11 mean anything. With a shim in the way, a
#   refusal could just as easily be the shim having broken downloading altogether — so the same shim
#   is first run in a mode that tampers with nothing and required to produce a successful install.

set -euo pipefail

VERSION="${1:-}"
INSTALLER="${2:-install.sh}"

IMAGES=("alpine:3" "debian:bookworm-slim")

die() { echo "verify-install-sh: $*" >&2; exit 1; }

[ -n "$VERSION" ] || die "usage: verify-install-sh.sh <version> [path-to-install.sh]"
case "$VERSION" in
    v*) die "pass the version without a leading 'v' (got '$VERSION')" ;;
esac
[ -f "$INSTALLER" ] || die "no installer at $INSTALLER"

command -v docker >/dev/null 2>&1 || die "docker not found — cannot verify without a nix-free container"

INSTALLER_ABS="$(cd "$(dirname "$INSTALLER")" && pwd)/$(basename "$INSTALLER")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# ── The in-container driver ─────────────────────────────────────────────────────────────────────
# Everything below runs inside the image. It is `sh`, not bash: the images do not all have bash, and
# the installer's own contract is POSIX sh.
cat > "$WORK/driver.sh" <<'DRIVER'
set -eu

VERSION="$1"
INSTALLER=/mnt/install.sh

fail() { echo "  FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

# A fresh, empty install directory per leg, so "installs nothing" is a statement about a directory
# that started empty rather than about one a previous leg may have populated.
leg_dir() {
    d="/legs/$1"
    rm -rf "$d"
    mkdir -p "$d"
    printf '%s\n' "$d"
}

# Nothing installed = no binary AND no staging file left behind. The staging file lives in the
# destination directory, so a leg that refused but littered it would leave a dotfile on PATH.
assert_empty() {
    d="$1"
    [ ! -e "$d/maxplayer" ] || fail "$2: a binary was installed at $d/maxplayer, but this leg had to install nothing"
    for leftover in "$d"/.maxplayer.install.*; do
        [ ! -e "$leftover" ] || fail "$2: left a staging file behind at $leftover"
    done
}

count_on_path() {
    n=0
    oldifs="$IFS"
    IFS=:
    for d in $PATH; do
        [ -n "$d" ] || d=.
        if [ -x "$d/maxplayer" ]; then n=$((n + 1)); fi
    done
    IFS="$oldifs"
    printf '%s\n' "$n"
}

# ── Premises ────────────────────────────────────────────────────────────────────────────────────
# Asserted, not assumed: an image that shipped a /nix or a maxplayer would make every result below
# a statement about something other than a clean machine.
[ ! -e /nix ] || fail "this image contains /nix — it cannot show the installer works without nix"
! command -v maxplayer >/dev/null 2>&1 || fail "this image already has a maxplayer on PATH"
! command -v cargo >/dev/null 2>&1 || fail "this image has a rust toolchain — it is not a clean target"
mkdir -p /legs

DL=""
if command -v curl >/dev/null 2>&1; then DL=curl
elif command -v wget >/dev/null 2>&1; then DL=wget
else fail "no downloader in this image — the harness was supposed to provide one"
fi
echo "  (downloader under test: $DL)"

# ── Leg 1 — install ─────────────────────────────────────────────────────────────────────────────
echo "leg 1: install $VERSION"
BIN1="$(leg_dir install)"
PATH="$BIN1:$PATH"
export PATH
MAXPLAYER_VERSION="$VERSION" MAXPLAYER_BIN_DIR="$BIN1" sh "$INSTALLER" \
    || fail "installer exited non-zero"
[ -x "$BIN1/maxplayer" ] || fail "installer reported success but there is no executable at $BIN1/maxplayer"

got="$(maxplayer version)" || fail "maxplayer version exited non-zero"
[ "$got" = "maxplayer $VERSION" ] || fail "maxplayer version printed '$got', expected 'maxplayer $VERSION'"
ok "maxplayer version -> $got (rc=0)"

# Both dispatch arms, because they are separate code paths in the binary.
got="$(maxplayer --version)" || fail "maxplayer --version exited non-zero"
[ "$got" = "maxplayer $VERSION" ] || fail "maxplayer --version printed '$got'"
ok "maxplayer --version -> $got (rc=0)"

# ── Leg 2 — the racer surface is the racer surface ───────────────────────────────────────────────
# `sell` is compiled out of the released (buyer) artifact, so it must NOT succeed. This is also the
# negative control on leg 1: without it, a binary that exits 0 on everything would have passed.
echo "leg 2: the seller subcommand is absent from the released artifact"
if out="$(maxplayer sell 2>&1)"; then
    fail "maxplayer sell exited 0 — the released artifact is not the buyer-only build it should be. Output: $out"
fi
out="$(maxplayer sell 2>&1 || true)"
case "$out" in
    *[Uu]sage* | *nknown* | *nrecognized* | *not*available* ) ok "maxplayer sell refuses (non-zero) -> $(printf '%s' "$out" | head -n 1)" ;;
    *) fail "maxplayer sell exited non-zero but said nothing recognisable: $out" ;;
esac

# ── Leg 3 — idempotency ─────────────────────────────────────────────────────────────────────────
echo "leg 3: re-running upgrades in place"
before="$(count_on_path)"
[ "$before" = 1 ] || fail "expected exactly one maxplayer on PATH after the first install, found $before"
MAXPLAYER_VERSION="$VERSION" MAXPLAYER_BIN_DIR="$BIN1" sh "$INSTALLER" \
    || fail "the second run exited non-zero"
after="$(count_on_path)"
[ "$after" = 1 ] || fail "after two runs there are $after maxplayer executables on PATH, expected 1"
got="$(maxplayer version)" || fail "maxplayer version exited non-zero after the second run"
[ "$got" = "maxplayer $VERSION" ] || fail "after the second run, version is '$got'"
ok "two runs, rc=0 both times, exactly one maxplayer on PATH, still $got"

# ── Legs 4-5b — platform detection ──────────────────────────────────────────────────────────────
# `uname` (and, for the Rosetta case, `sysctl`) is shimmed rather than the check being called
# directly: the property under test is what the WHOLE installer does, and a unit-level call could not
# show that it refuses BEFORE downloading or installing anything.
#
# ★★ WHAT THE DARWIN LEGS DO AND DO NOT PROVE. They run on linux, so they prove the PLATFORM
#    RESOLUTION — that Darwin+arm64 resolves to `darwin-arm64` and therefore constructs the
#    darwin asset name, that an Intel mac is refused, that the Rosetta branch fires. That logic is
#    plain shell and platform-independent, so exercising it here is real.
#    They CANNOT prove a darwin install works: no mac binary can execute in a linux container, and
#    this box has no mac emulation of any kind. `install.sh`'s own prove-step (run the binary, match
#    the version) is therefore unexercised on darwin, and stays unproven until a mac runs it — the
#    #249 rule that nothing darwin counts as proven until a mac runs the artifact.
shim_uname() {
    mkdir -p /shim
    cat > /shim/uname <<EOF
#!/bin/sh
case "\${1:-}" in
    -s) echo "$1" ;;
    -m) echo "$2" ;;
    *)  echo "$1" ;;
esac
EOF
    chmod 755 /shim/uname
}

# $1 = arm64 → the machine IS Apple Silicon (hw.optional.arm64 = 1)
# $1 = intel → the key does not exist, which is how a real Intel mac's sysctl answers it
shim_sysctl() {
    mkdir -p /shim
    if [ "$1" = arm64 ]; then
        cat > /shim/sysctl <<'EOF'
#!/bin/sh
case "$*" in
    "-n hw.optional.arm64") echo 1 ;;
    *) exit 1 ;;
esac
EOF
    else
        cat > /shim/sysctl <<'EOF'
#!/bin/sh
exit 1
EOF
    fi
    chmod 755 /shim/sysctl
}

# The mapping assertion the three darwin legs share. Requires: the installer announced the platform
# it resolved, installed nothing, and exited non-zero (a linux box must never end up holding a mac
# binary, whichever way the run failed).
assert_resolved_platform() {
    _out="$1"; _bin="$2"; _want="$3"; _label="$4"
    grep -q "for $_want" "$_out" \
        || fail "$_label: the installer did not resolve the platform to '$_want'. Output: $(cat "$_out")"
    assert_empty "$_bin" "$_label"
    ok "$_label -> resolved '$_want'; stopped without installing ($(grep -c . "$_out") lines, last: $(tail -n 1 "$_out" | cut -c1-72))"
}

echo "leg 4: Darwin + arm64 resolves to the darwin-arm64 asset"
BIN4="$(leg_dir darwin-arm)"
shim_uname Darwin arm64
shim_sysctl arm64
if PATH="/shim:$PATH" MAXPLAYER_VERSION="$VERSION" MAXPLAYER_BIN_DIR="$BIN4" \
        sh "$INSTALLER" >/legs/darwin-arm.out 2>&1; then
    fail "the installer exited 0 while installing a mac binary on linux"
fi
assert_resolved_platform /legs/darwin-arm.out "$BIN4" darwin-arm64 "leg 4 (Darwin/arm64)"
# Record which failure mode this release produced, because it differs by release and a reader should
# not have to guess: no darwin asset yet ⇒ the download refuses and NAMES the constructed asset;
# once one exists ⇒ the download succeeds and the prove-step refuses because it cannot exec.
if grep -q "maxplayer-$VERSION-darwin-arm64.tar.gz" /legs/darwin-arm.out; then
    ok "  …and the constructed asset name appears: maxplayer-$VERSION-darwin-arm64.tar.gz"
fi

echo "leg 4b: an Intel mac is refused by name, before any download"
BIN4B="$(leg_dir darwin-intel)"
shim_uname Darwin x86_64
shim_sysctl intel
if PATH="/shim:$PATH" MAXPLAYER_VERSION="$VERSION" MAXPLAYER_BIN_DIR="$BIN4B" \
        sh "$INSTALLER" >/legs/darwin-intel.out 2>&1; then
    fail "the installer exited 0 on an Intel mac, for which no asset is built"
fi
grep -qi 'Intel macs are not supported' /legs/darwin-intel.out \
    || fail "refused on an Intel mac, but not by name. Output: $(cat /legs/darwin-intel.out)"
# Refused at DETECTION, not after fetching something. `installing maxplayer …` is the first thing a
# run prints once it has committed to a platform, so its absence places the refusal before that.
# `if`, not `grep … && fail`: under `set -e` an AND-list whose left side fails takes the whole list
# non-zero and kills the driver — so the good case would abort the run instead of passing.
if grep -q 'installing maxplayer' /legs/darwin-intel.out; then
    fail "the Intel-mac refusal happened after the installer had already committed to a platform"
fi
assert_empty "$BIN4B" "leg 4b"
ok "Intel mac -> $(grep -i 'Intel macs' /legs/darwin-intel.out | head -n 1 | cut -c1-88) (non-zero, no download)"

echo "leg 4c: Apple Silicon under Rosetta is detected despite uname saying x86_64"
BIN4C="$(leg_dir darwin-rosetta)"
shim_uname Darwin x86_64
shim_sysctl arm64
if PATH="/shim:$PATH" MAXPLAYER_VERSION="$VERSION" MAXPLAYER_BIN_DIR="$BIN4C" \
        sh "$INSTALLER" >/legs/darwin-rosetta.out 2>&1; then
    fail "the installer exited 0 while installing a mac binary on linux"
fi
grep -qi 'running under Rosetta' /legs/darwin-rosetta.out \
    || fail "the Rosetta branch did not fire, so an Apple Silicon mac in a translated shell would be refused. Output: $(cat /legs/darwin-rosetta.out)"
assert_resolved_platform /legs/darwin-rosetta.out "$BIN4C" darwin-arm64 "leg 4c (Darwin/x86_64 + Rosetta)"

# ★ Control on 4b vs 4c: the two runs differ ONLY in what sysctl answers — same uname, same args. So
#   the different verdicts are attributable to the hardware probe and to nothing else. Without this
#   pairing, 4c could have been passing because of the `x86_64` uname rather than the sysctl.
ok "control: 4b and 4c differ only in sysctl's answer, so the hardware probe is what decides"
rm -f /shim/sysctl

echo "leg 5: an unsupported architecture is refused by name"
BIN5="$(leg_dir riscv)"
shim_uname Linux riscv64
if PATH="/shim:$PATH" MAXPLAYER_VERSION="$VERSION" MAXPLAYER_BIN_DIR="$BIN5" \
        sh "$INSTALLER" >/legs/riscv.out 2>&1; then
    fail "the installer exited 0 on riscv64"
fi
grep -q 'riscv64' /legs/riscv.out \
    || fail "refused on riscv64 without naming the architecture. Output: $(cat /legs/riscv.out)"
assert_empty "$BIN5" "leg 5"
ok "riscv64 -> $(grep 'riscv64' /legs/riscv.out | head -n 1) (non-zero, nothing installed)"

echo "leg 5b: an unsupported OS is refused by name"
BIN5B="$(leg_dir freebsd)"
shim_uname FreeBSD amd64
if PATH="/shim:$PATH" MAXPLAYER_VERSION="$VERSION" MAXPLAYER_BIN_DIR="$BIN5B" \
        sh "$INSTALLER" >/legs/freebsd.out 2>&1; then
    fail "the installer exited 0 on FreeBSD"
fi
grep -q 'FreeBSD' /legs/freebsd.out \
    || fail "refused on FreeBSD without naming the OS. Output: $(cat /legs/freebsd.out)"
assert_empty "$BIN5B" "leg 5b"
ok "FreeBSD -> $(grep 'unsupported operating system' /legs/freebsd.out | head -n 1) (non-zero, nothing installed)"
rm -f /shim/uname

# ── The download shim ───────────────────────────────────────────────────────────────────────────
# Wraps the real downloader and damages what it wrote, so the bytes install.sh verifies are not the
# bytes the release published. install.sh itself is untouched.
shim_downloader() {
    mkdir -p /shim
    real="$(command -v "$DL")"
    {
        echo '#!/bin/sh'
        echo "mode='$1'"
        echo "real='$real'"
        cat <<'INNER'
# install.sh writes with `-o <file>` (curl) or `-O <file>` (wget).
dest=""; prev=""
for a in "$@"; do
    case "$prev" in -o | -O) dest="$a" ;; esac
    prev="$a"
done
"$real" "$@" || exit $?
[ -n "$dest" ] || exit 0
case "$mode" in
    pass) ;;
    corrupt-tarball)
        case "$dest" in *.tar.gz) printf 'tampered' >> "$dest" ;; esac ;;
    sums-rename)
        # The sums file no longer mentions the asset we downloaded — the case a `--ignore-missing`
        # style check would pass by verifying nothing at all.
        case "$dest" in *SHA256SUMS) sed 's/maxplayer-/otherthing-/' "$dest" > "$dest.t" && mv "$dest.t" "$dest" ;; esac ;;
    sums-wrongsum)
        # A well-formed but wrong digest: the tarball is the real one, the expectation is not.
        case "$dest" in *SHA256SUMS) sed 's/^[0-9a-f]\{64\}/00000000000000000000000000000000000000000000000000000000000000ff/' "$dest" > "$dest.t" && mv "$dest.t" "$dest" ;; esac ;;
    sums-garbage)
        # The FILENAME is left correct on purpose. Rewriting it too would make the name lookup miss
        # and the installer would refuse one step earlier, leaving the digest-shape check itself
        # unexecuted — a refusal for the wrong reason, which is not evidence about this clause.
        case "$dest" in *SHA256SUMS) sed 's/^[0-9a-f]\{64\}/not-a-digest-just-an-html-error-page-saved-to-this-path/' "$dest" > "$dest.t" && mv "$dest.t" "$dest" ;; esac ;;
    sums-sha1)
        # All hex, wrong length — a sums file produced by sha1sum. Distinct shape from the above, and
        # it lands on the other half of the digest-shape check.
        case "$dest" in *SHA256SUMS) sed 's/^[0-9a-f]\{64\}/da39a3ee5e6b4b0d3255bfef95601890afd80709/' "$dest" > "$dest.t" && mv "$dest.t" "$dest" ;; esac ;;
esac
INNER
    } > "/shim/$DL"
    chmod 755 "/shim/$DL"
}

# ── Leg 6 — the shim itself is not the cause ────────────────────────────────────────────────────
echo "leg 6: control — the shim in pass-through mode still installs"
BIN6="$(leg_dir shimpass)"
shim_downloader pass
PATH="/shim:$PATH" MAXPLAYER_VERSION="$VERSION" MAXPLAYER_BIN_DIR="$BIN6" \
    sh "$INSTALLER" >/legs/shimpass.out 2>&1 \
    || fail "the pass-through shim broke the install, so the tamper legs below would prove nothing. Output: $(cat /legs/shimpass.out)"
[ -x "$BIN6/maxplayer" ] || fail "pass-through shim: nothing installed"
got="$("$BIN6/maxplayer" version)"
[ "$got" = "maxplayer $VERSION" ] || fail "pass-through shim installed something that reports '$got'"
ok "shim pass-through -> rc=0, installed $got"

# ── Legs 7-10 — verification is real ────────────────────────────────────────────────────────────
tamper_leg() {
    _mode="$1"; _label="$2"; _needle="$3"
    echo "leg $_label"
    _bin="$(leg_dir "$_mode")"
    shim_downloader "$_mode"
    if PATH="/shim:$PATH" MAXPLAYER_VERSION="$VERSION" MAXPLAYER_BIN_DIR="$_bin" \
            sh "$INSTALLER" >"/legs/$_mode.out" 2>&1; then
        fail "$_mode: the installer exited 0 on a download that does not match the release"
    fi
    grep -q "$_needle" "/legs/$_mode.out" \
        || fail "$_mode: refused, but not for the checksum reason (looked for '$_needle'). Output: $(cat "/legs/$_mode.out")"
    assert_empty "$_bin" "$_mode"
    ok "$_mode -> non-zero, '$_needle', nothing installed"
}

tamper_leg corrupt-tarball "7: a corrupted tarball is refused"          'CHECKSUM MISMATCH'
tamper_leg sums-wrongsum   "8: a wrong expected digest is refused"      'CHECKSUM MISMATCH'
tamper_leg sums-rename     "9: sums not covering our asset refused"     'does not list'
tamper_leg sums-garbage    "10: a non-hex digest field is refused"      'not a sha256 digest'
tamper_leg sums-sha1       "11: a hex digest of the wrong length too"   'not a sha256 digest'
rm -f "/shim/$DL"

# ── Leg 12 — a version with no release ──────────────────────────────────────────────────────────
echo "leg 12: a version that has no release refuses"
BIN_NOVER="$(leg_dir noversion)"
if MAXPLAYER_VERSION=99.99.99 MAXPLAYER_BIN_DIR="$BIN_NOVER" sh "$INSTALLER" >/legs/noversion.out 2>&1; then
    fail "the installer exited 0 for a version that has no release"
fi
grep -q 'could not download' /legs/noversion.out \
    || fail "refused an absent release without saying so. Output: $(cat /legs/noversion.out)"
assert_empty "$BIN_NOVER" "leg 12"
ok "99.99.99 -> non-zero, nothing installed"

# ── Leg 13 — an unknown option is refused, not ignored ──────────────────────────────────────────
echo "leg 13: an unknown option refuses"
BIN_BADOPT="$(leg_dir badopt)"
if MAXPLAYER_BIN_DIR="$BIN_BADOPT" sh "$INSTALLER" --not-a-flag >/legs/badopt.out 2>&1; then
    fail "the installer accepted an unknown option"
fi
assert_empty "$BIN_BADOPT" "leg 13"
ok "--not-a-flag -> non-zero, nothing installed"

# ── Leg 14 — flags through a pipe ───────────────────────────────────────────────────────────────
# The documented pipe invocation, since `sh -s --` is the part users get wrong.
echo "leg 14: flags survive the documented pipe form"
BIN_PIPED="$(leg_dir piped)"
cat "$INSTALLER" | sh -s -- --version "$VERSION" --bin-dir "$BIN_PIPED" >/legs/piped.out 2>&1 \
    || fail "the piped form failed. Output: $(cat /legs/piped.out)"
got="$("$BIN_PIPED/maxplayer" version)" || fail "the piped install produced a binary that will not run"
[ "$got" = "maxplayer $VERSION" ] || fail "the piped install produced '$got'"
ok "cat install.sh | sh -s -- --version $VERSION --bin-dir ... -> $got"

echo "PASS"
DRIVER

overall=0
for image in "${IMAGES[@]}"; do
    echo "════════ $image ════════"
    # The prep step is per-image and deliberately minimal: whatever the image needs to have ONE
    # downloader, and nothing else. alpine needs nothing at all — busybox wget is already there,
    # which is the point of including it.
    case "$image" in
        alpine:3) prep="true" ;;
        debian:bookworm-slim)
            prep="apt-get -qq update && apt-get -qq install -y --no-install-recommends curl ca-certificates >/dev/null" ;;
        *) prep="true" ;;
    esac

    if docker run --rm \
            -v "$INSTALLER_ABS:/mnt/install.sh:ro" \
            -v "$WORK/driver.sh:/mnt/driver.sh:ro" \
            "$image" \
            sh -c "$prep && sh /mnt/driver.sh '$VERSION'"; then
        echo "──────── $image: PASS"
    else
        echo "──────── $image: FAIL" >&2
        overall=1
    fi
done

[ "$overall" -eq 0 ] || die "at least one image failed"
echo "PASS: install.sh installs, verifies, upgrades in place, and refuses — on ${IMAGES[*]}"
