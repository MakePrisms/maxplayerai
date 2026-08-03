#!/bin/sh
#
# maxplayer installer — get the buyer CLI onto a machine that has never heard of nix.
#
#   curl -fsSL https://github.com/MakePrisms/maxplayerai/releases/latest/download/install.sh | sh
#
# What it does: resolves a release, downloads that release's asset for THIS platform, verifies the
# download against the release's own SHA256SUMS, proves the binary runs and reports the version it
# was supposed to be, and only then puts it on PATH.
#
# Options (flags and environment are equivalent; the flag wins):
#   --version <x.y.z>   MAXPLAYER_VERSION   install this exact version instead of the latest release
#   --bin-dir <dir>     MAXPLAYER_BIN_DIR   install here instead of ~/.local/bin
#
# Through a pipe, flags need `sh -s --`:
#   curl -fsSL <url> | sh -s -- --version 0.1.0 --bin-dir /usr/local/bin
#
# ── Three properties this script is judged on ───────────────────────────────────────────────────
#
# ★ It refuses rather than guessing. An unsupported OS or architecture, a missing tool it needs to
#   verify with, a checksum that does not match, a binary that will not run — every one of those
#   exits non-zero having installed nothing. There is no flag to turn verification off, because an
#   installer that can be asked to skip it is an installer whose verification is decorative.
#
# ★ Every refusal happens in THIS shell. `die` is never reached from inside a `$(command
#   substitution)`, because `exit` there ends the subshell and the caller carries on with an empty
#   variable — a refusal that returns a value instead of refusing. The functions that can refuse
#   therefore assign globals rather than printing their result.
#
# ★ Nothing runs until the whole script has arrived. `curl | sh` feeds the shell a stream, so a
#   connection dropped mid-transfer executes the prefix that made it through — with a bare sequence
#   of commands, a truncated download runs half an install. Everything here is therefore inside a
#   function, and `main "$@"` on the last line is the only statement that executes anything: a
#   truncated script defines some functions and exits.

set -eu

REPO="MakePrisms/maxplayerai"
# The name of the executable, which is deliberately not the crate name (`[[bin]] maxplayer` inside
# package `mobee`). It names the asset, the directory inside the asset, the installed file, and the
# first word this binary prints when asked for its version — asserted below, not assumed.
BIN_NAME="maxplayer"

say()  { printf '%s\n' "$*"; }
warn() { printf 'install.sh: %s\n' "$*" >&2; }
die()  { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# An explicit mktemp template, because a bare `mktemp -d` is not portable in the direction that
# matters here. GNU mktemp defaults the template; BSD mktemp — which is what macOS ships — documents
# one as required, and this script now runs on macOS. Naming the template makes the call mean the
# same thing on GNU, BSD and busybox, and it is cheaper than being wrong on the platform that cannot
# be tested from linux. `$TMPDIR` is honoured because macOS sets it to a per-user directory.
tmpl() { printf '%s/maxplayer-install-%s.XXXXXX\n' "${TMPDIR:-/tmp}" "$1"; }

# ── Platform ────────────────────────────────────────────────────────────────────────────────────
# Sets PLATFORM to a release platform name (`linux-x64`, `linux-arm64`, `darwin-arm64`). Those are
# NOT rust target triples — the asset filenames are what this has to agree with.
#
# The OS and the architecture are resolved TOGETHER, in one nested case, because the supported set is
# not a product of the two: linux ships both architectures, macOS ships only arm64. Checking them in
# sequence — an OS gate, then an arch gate — cannot express that, and the arch arm would have to
# either accept x86_64 on macOS (an asset no release publishes) or refuse it on linux.
#
# Everything unlisted is refused BY NAME, with the reason. Silence would be the worse failure: the
# alternative to refusing is downloading some other platform's tarball, which installs a file that
# cannot exec.
detect_platform() {
    _os="$(uname -s)"
    _arch="$(uname -m)"

    case "$_os" in
        Linux)
            case "$_arch" in
                x86_64 | amd64)  PLATFORM="linux-x64" ;;
                aarch64 | arm64) PLATFORM="linux-arm64" ;;
                *) die "unsupported architecture '$_arch' on linux — the released linux platforms are x86_64 and aarch64" ;;
            esac
            ;;
        Darwin)
            case "$_arch" in
                arm64 | aarch64) PLATFORM="darwin-arm64" ;;
                x86_64)
                    # ★ `uname -m` reports the architecture of THIS PROCESS, not of the machine.
                    # Under Rosetta 2 an Apple Silicon mac answers `x86_64`, so refusing on `uname`
                    # alone would turn away a machine we do support — and it is easy to be in a
                    # translated shell without knowing (an x86_64 Homebrew, a terminal opened with
                    # "Open using Rosetta"). The discriminator is one layer out and has to be asked
                    # for: `hw.optional.arm64` is a property of the HARDWARE.
                    #
                    # Fails closed toward the refusal — if sysctl is unavailable we refuse rather
                    # than assume. Either direction is safe (the prove-step below rejects a binary
                    # that cannot exec), but a named refusal beats a confusing "did not run".
                    if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
                        PLATFORM="darwin-arm64"
                        warn "this shell reports x86_64 because it is running under Rosetta; the machine is Apple Silicon, so installing the native arm64 build"
                    else
                        die "Intel macs are not supported — the release builds an Apple Silicon (arm64) mac asset only. Build from source, or use nix: nix run --refresh github:$REPO -- mcp"
                    fi
                    ;;
                *) die "unsupported architecture '$_arch' on macOS — the released mac platform is Apple Silicon (arm64)" ;;
            esac
            ;;
        *)
            die "unsupported operating system '$_os' — this installer covers linux (x86_64, aarch64) and macOS (arm64)"
            ;;
    esac
}

# ── Transport ───────────────────────────────────────────────────────────────────────────────────
# Sets DOWNLOADER. curl or wget, whichever is present: neither is guaranteed — debian-slim ships no
# downloader at all, alpine's is busybox wget, and macOS ships curl but no wget — so committing to
# either one alone would fail on a platform this installer supports.
pick_downloader() {
    if command -v curl >/dev/null 2>&1; then
        DOWNLOADER=curl
    elif command -v wget >/dev/null 2>&1; then
        DOWNLOADER=wget
    else
        die "neither curl nor wget is installed — one of them is needed to download the release"
    fi
}

# Fetch $1 into the file $2. Returns non-zero on failure so the caller can name what it was
# fetching. A failure must not leave a partial file that a later step reads as a complete download,
# so the destination is removed on any error.
fetch() {
    _url="$1"
    _dest="$2"
    case "$DOWNLOADER" in
        curl)
            # `--proto =https` refuses a plaintext redirect: without it a redirect to http:// is
            # followed silently, and the bytes this script is about to trust would arrive over a
            # channel anyone on the path can rewrite.
            curl -fsSL --proto '=https' --tlsv1.2 -o "$_dest" "$_url" \
                || { rm -f "$_dest"; return 1; }
            ;;
        wget)
            wget -q -O "$_dest" "$_url" \
                || { rm -f "$_dest"; return 1; }
            ;;
    esac
}

# ── Version ─────────────────────────────────────────────────────────────────────────────────────
# Sets VERSION from `/releases/latest`, which is the API's own definition of latest and excludes
# drafts and pre-releases. "Latest stable" is answered by the server rather than reconstructed here
# from a list of tags — sorting tags locally is how an installer starts handing users an -rc build.
#
# It 404s when every release so far is a pre-release, which is a state this repo has really been in.
# That is reported as the specific thing it is, with the way out, rather than as a download failure.
resolve_latest_version() {
    _api="https://api.github.com/repos/$REPO/releases/latest"
    _body="$(mktemp "$(tmpl api)")" || die "cannot create a temporary file"
    if ! fetch "$_api" "$_body"; then
        rm -f "$_body"
        die "could not ask GitHub for the latest release of $REPO. If every release is still a pre-release, or the API is rate-limiting this host, name a version instead: MAXPLAYER_VERSION=x.y.z"
    fi

    # `tr ',' '\n'` first so this works whether the API pretty-prints or returns one long line.
    _tag="$(tr ',' '\n' < "$_body" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n 1)"
    rm -f "$_body"

    [ -n "$_tag" ] \
        || die "GitHub's reply carried no tag_name — cannot tell which version is latest. Name one instead: MAXPLAYER_VERSION=x.y.z"

    VERSION="${_tag#v}"
}

# ── Checksum ────────────────────────────────────────────────────────────────────────────────────
# Sets HASHER, failing closed when there is none. `command -v sha256sum >/dev/null && verify` is the
# shape that turns verification into a coin flip decided by the host's package list; a host that
# cannot hash must not get an install.
#
# The fallback chain is load-bearing on macOS, not decoration: macOS ships NO `sha256sum`. It has
# `shasum` (perl) and `openssl`, which is why both are here — on darwin the second arm is the one
# that runs, so a chain that stopped at `sha256sum` would refuse every mac.
pick_hasher() {
    if command -v sha256sum >/dev/null 2>&1; then
        HASHER=sha256sum
    elif command -v shasum >/dev/null 2>&1; then
        HASHER=shasum
    elif command -v openssl >/dev/null 2>&1; then
        HASHER=openssl
    else
        die "no sha256 tool found (looked for sha256sum, shasum, openssl) — refusing to install an unverified download"
    fi
}

sha256_of() {
    case "$HASHER" in
        sha256sum) sha256sum "$1"             | cut -d' ' -f1 ;;
        shasum)    shasum -a 256 "$1"         | cut -d' ' -f1 ;;
        openssl)   openssl dgst -sha256 "$1"  | sed 's/.*= *//' ;;
    esac
}

# Compare the asset's actual digest against the line for ITS OWN filename in SHA256SUMS.
#
# Deliberately not `sha256sum -c SHA256SUMS`: the release's sums file lists every platform's asset
# and only one of them is on disk here, so `-c` fails on the ones that are simply absent. Its exit
# code cannot separate "the file we downloaded is wrong" from "a file we never wanted is missing",
# and the usual repair reintroduces the real hazard — with `--ignore-missing`, a sums file that does
# not mention our asset at all checks zero files and exits 0.
#
# So: look the expected digest up by name, require that the lookup found something, require it to be
# a well-formed digest, and compare. Each of those is a way this check could otherwise report a pass
# without having compared anything.
verify_sha256() {
    _file="$1"
    _name="$2"
    _sums="$3"

    # `sub(/^\*/…)` because sha256sum's binary-mode output prefixes the name with `*`. The release
    # writes text mode today; a reader of this line should not have to know that to trust it.
    _expected="$(awk -v want="$_name" '
        { n = $2; sub(/^\*/, "", n); if (n == want) { print $1; hit = 1 } }
        END { exit !hit }
    ' "$_sums")" \
        || die "SHA256SUMS for this release does not list $_name — refusing to install an artifact the release does not vouch for"

    # A digest is 64 hex characters. Anything else means the sums file is not what we think it is —
    # an HTML error page saved as SHA256SUMS parses as text and yields junk, and comparing junk to
    # junk is how a mismatch becomes a match.
    _hexonly="$(printf '%s' "$_expected" | tr -d '0-9a-fA-F')"
    if [ -n "$_hexonly" ] || [ "${#_expected}" -ne 64 ]; then
        die "the SHA256SUMS entry for $_name is not a sha256 digest ('$_expected') — refusing to install"
    fi

    _actual="$(sha256_of "$_file")"
    [ -n "$_actual" ] || die "could not compute a sha256 of the download with $HASHER — refusing to install"

    if [ "$_actual" != "$_expected" ]; then
        warn "CHECKSUM MISMATCH for $_name"
        warn "  expected (from the release's SHA256SUMS): $_expected"
        warn "  actual   (of the file downloaded here):   $_actual"
        die "the download does not match the release — nothing has been installed"
    fi

    say "verified sha256 $_actual ($_name)"
}

# ── PATH ────────────────────────────────────────────────────────────────────────────────────────
# `case` over PATH rather than asking `command -v`: this answers a question about the DIRECTORY, not
# about whether some other maxplayer already exists somewhere.
dir_on_path() {
    case ":$PATH:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

usage() {
    say "usage: install.sh [--version <x.y.z>] [--bin-dir <dir>]"
    say "  through a pipe:  curl -fsSL <url> | sh -s -- --version 0.1.0"
    say "  environment:     MAXPLAYER_VERSION, MAXPLAYER_BIN_DIR"
}

main() {
    VERSION="${MAXPLAYER_VERSION:-}"
    PLATFORM=""
    DOWNLOADER=""
    HASHER=""
    bin_dir="${MAXPLAYER_BIN_DIR:-}"

    while [ $# -gt 0 ]; do
        case "$1" in
            --version)   shift; [ $# -gt 0 ] || die "--version needs a value"; VERSION="$1" ;;
            --version=*) VERSION="${1#--version=}" ;;
            --bin-dir)   shift; [ $# -gt 0 ] || die "--bin-dir needs a value"; bin_dir="$1" ;;
            --bin-dir=*) bin_dir="${1#--bin-dir=}" ;;
            -h | --help) usage; return 0 ;;
            *) usage >&2; die "unknown option '$1'" ;;
        esac
        shift
    done

    # A leading `v` is what a user copies out of a tag name, and `v0.1.0` would build an asset name
    # no release has. Accept it and normalise rather than 404 later on something avoidable.
    VERSION="${VERSION#v}"

    # ~/.local/bin is the default because it needs no privilege: an installer that wants root to put
    # one user's CLI somewhere is an installer people run under sudo out of habit. It is also already
    # on PATH under systemd's user environment and most distro shell profiles; when it is not, the
    # hint at the end says so.
    if [ -z "$bin_dir" ]; then
        [ -n "${HOME:-}" ] || die "HOME is not set, so there is no default install directory — pass --bin-dir <dir>"
        bin_dir="$HOME/.local/bin"
    fi

    detect_platform
    pick_downloader
    pick_hasher

    command -v tar >/dev/null 2>&1 || die "tar not found — needed to unpack the release asset"
    command -v awk >/dev/null 2>&1 || die "awk not found — needed to read the release's SHA256SUMS"

    if [ -n "$VERSION" ]; then
        say "installing $BIN_NAME $VERSION for $PLATFORM"
    else
        resolve_latest_version
        say "latest release is $VERSION; installing for $PLATFORM"
    fi

    asset="$BIN_NAME-$VERSION-$PLATFORM.tar.gz"
    base="https://github.com/$REPO/releases/download/v$VERSION"

    tmp="$(mktemp -d "$(tmpl dl)")" || die "cannot create a temporary directory"
    # `staged` is cleaned by the trap too: it is a file inside the destination directory, so leaving
    # it behind on a failure would litter a directory that is on the user's PATH.
    staged=""
    trap 'rm -rf "$tmp"; [ -z "$staged" ] || rm -f "$staged"' EXIT HUP INT TERM

    fetch "$base/$asset" "$tmp/$asset" \
        || die "could not download $base/$asset — check that release v$VERSION exists and publishes a $PLATFORM asset"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" \
        || die "could not download $base/SHA256SUMS — refusing to install an unverified download"

    verify_sha256 "$tmp/$asset" "$asset" "$tmp/SHA256SUMS"

    tar -xzf "$tmp/$asset" -C "$tmp" || die "could not unpack $asset"

    unpacked="$tmp/$BIN_NAME-$VERSION-$PLATFORM/$BIN_NAME"
    [ -f "$unpacked" ] \
        || die "$asset does not contain $BIN_NAME-$VERSION-$PLATFORM/$BIN_NAME — this is not the asset layout this installer expects"
    chmod 755 "$unpacked"

    # ── Prove it before installing it ───────────────────────────────────────────────────────────
    # Run the binary from the temporary directory rather than after moving it into place: a binary
    # for the wrong architecture, or an asset carried over from another version, must leave nothing
    # behind. Doing it here is what makes "refuses and installs nothing" true of this check too.
    #
    # The version string is also the only check that can catch a correct-looking asset that is the
    # wrong build: the download matched a checksum, so every layer below this one already agrees.
    reported="$("$unpacked" version 2>/dev/null)" \
        || die "the downloaded $BIN_NAME did not run on this machine — nothing has been installed"
    [ "$reported" = "$BIN_NAME $VERSION" ] \
        || die "the downloaded binary reports '$reported' but this is meant to be $BIN_NAME $VERSION — nothing has been installed"

    mkdir -p "$bin_dir" || die "could not create $bin_dir"
    [ -w "$bin_dir" ] || die "$bin_dir is not writable — pass --bin-dir <dir>, or make it writable"

    # Install by rename, from a staging file in the SAME directory. Two reasons, and the second is
    # the one a plain copy gets wrong: rename is atomic, so a concurrent `maxplayer` never sees a
    # half-written file; and writing over a running executable in place fails with ETXTBSY, whereas
    # rename replaces the directory entry and leaves the running process on the old inode. That is
    # what makes re-running this an upgrade in place rather than a conflict.
    staged="$bin_dir/.$BIN_NAME.install.$$"
    cp "$unpacked" "$staged" || die "could not write to $bin_dir"
    chmod 755 "$staged"
    mv -f "$staged" "$bin_dir/$BIN_NAME" || die "could not install into $bin_dir"
    staged=""

    say "installed $reported -> $bin_dir/$BIN_NAME"

    # ── What the user's shell will actually run ─────────────────────────────────────────────────
    # The file being in place is not the same property as the name resolving to it. Reported
    # separately because the remedies differ: one is a missing PATH entry, the other is an older copy
    # of maxplayer earlier in PATH that will keep winning until it is removed.
    if ! dir_on_path "$bin_dir"; then
        say ""
        say "$bin_dir is not on your PATH. Add it, then re-open your shell:"
        say "  echo 'export PATH=\"\$PATH:$bin_dir\"' >> ~/.profile"
        say "Until then, run it by path: $bin_dir/$BIN_NAME version"
    else
        resolved="$(command -v "$BIN_NAME" 2>/dev/null || true)"
        if [ -n "$resolved" ] && [ "$resolved" != "$bin_dir/$BIN_NAME" ]; then
            say ""
            warn "'$BIN_NAME' on your PATH resolves to $resolved, which is not the copy just installed."
            warn "That one shadows $bin_dir/$BIN_NAME; remove it, or put $bin_dir earlier in PATH."
        else
            say "run: $BIN_NAME version"
        fi
    fi
}

main "$@"
