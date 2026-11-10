#!/usr/bin/env bash
# Drive THIS client's real ACP `initialize` (AcpDriver::ready) against real adapter binaries (#1289).
#
# The v0.7.3-rc7 release check confirmed each adapter "answers ACP initialize" with a generic client.
# Maxplayer's own params were never sent, and the image shipped with claude-agent-acp 0.104.1 and
# codex-acp 2.6.0, both of which refused them. Every stock-image claude/codex seat restart-looped.
# This script closes that gap:
#
#   * the claude/codex adapter versions are READ FROM docker/maxplayer-sandbox/Dockerfile, so a pin
#     bump is checked against the client that will ship with it, in the same pull request;
#   * the previous pins (claude-agent-acp 0.85.0, codex-acp 2.1.1) stay in the matrix because seats
#     still run them on derived images (#1289's operator workaround);
#   * cursor-agent is the exact tarball the Dockerfile pins, digest-checked (skip: --no-cursor).
#
# Usage: scripts/check-acp-initialize.sh [--no-cursor]
# Needs: node + npm, curl, sha256sum, cargo. Spends no model turn: initialize needs no credentials.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dockerfile="$root/docker/maxplayer-sandbox/Dockerfile"
with_cursor=1
[ "${1:-}" = "--no-cursor" ] && with_cursor=0

pin() {
  local version
  version="$(grep -oE "@agentclientprotocol/$1@[0-9][0-9A-Za-z.+-]*" "$dockerfile" | head -1 | sed 's/.*@//' || true)"
  [ -n "$version" ] || { echo "no $1 pin found in $dockerfile" >&2; exit 1; }
  printf '%s' "$version"
}
claude_pin="$(pin claude-agent-acp)"
codex_pin="$(pin codex-acp)"
# The adapters v0.7.3-rc4's image shipped. Keep until no seat runs them.
claude_prev="0.85.0"
codex_prev="2.1.1"

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

install() { # <dir> <package@version>
  mkdir -p "$scratch/$1"
  npm install --prefix "$scratch/$1" --no-audit --no-fund --silent "$2" >&2
}
install claude-pin "@agentclientprotocol/claude-agent-acp@$claude_pin"
install codex-pin "@agentclientprotocol/codex-acp@$codex_pin"
adapters="claude-$claude_pin|claude|$scratch/claude-pin/node_modules/.bin/claude-agent-acp"
adapters+=";codex-$codex_pin|codex|$scratch/codex-pin/node_modules/.bin/codex-acp"
if [ "$claude_prev" != "$claude_pin" ]; then
  install claude-prev "@agentclientprotocol/claude-agent-acp@$claude_prev"
  adapters+=";claude-$claude_prev|claude|$scratch/claude-prev/node_modules/.bin/claude-agent-acp"
fi
if [ "$codex_prev" != "$codex_pin" ]; then
  install codex-prev "@agentclientprotocol/codex-acp@$codex_prev"
  adapters+=";codex-$codex_prev|codex|$scratch/codex-prev/node_modules/.bin/codex-acp"
fi

if [ "$with_cursor" = 1 ]; then
  case "$(uname -m)" in
    x86_64) arch=x64; dockerarch=amd64 ;;
    aarch64 | arm64) arch=arm64; dockerarch=arm64 ;;
    *) echo "cursor: unsupported arch $(uname -m); rerun with --no-cursor" >&2; exit 1 ;;
  esac
  # `|| true`: under `set -e` + `pipefail` a non-matching grep would exit here SILENTLY, before
  # the explicit "pin not found" error below could say which pin moved.
  url="$(grep -oE 'https://downloads\.cursor\.com/[^"]*/linux/\$\{arch\}/agent-cli-package\.tar\.gz' "$dockerfile" | head -1 || true)"
  # The Dockerfile line is `amd64) arch=x64;   sha=<64 hex> ;;` — the case label, then the sha.
  sha="$(grep -E "^[[:space:]]*$dockerarch\)" "$dockerfile" | grep -oE 'sha=[0-9a-f]{64}' | head -1 | cut -d= -f2 || true)"
  [ -n "$url" ] && [ -n "$sha" ] || { echo "cursor pin not found in $dockerfile" >&2; exit 1; }
  url="${url/\$\{arch\}/$arch}"
  curl -sSfL -o "$scratch/cursor.tgz" "$url"
  echo "$sha  $scratch/cursor.tgz" | sha256sum -c - >&2
  mkdir -p "$scratch/cursor"
  tar -xzf "$scratch/cursor.tgz" -C "$scratch/cursor"
  cursor_bin="$(find "$scratch/cursor" -type f -name cursor-agent | head -1)"
  [ -n "$cursor_bin" ] || { echo "cursor-agent not in the pinned tarball" >&2; exit 1; }
  adapters+=";cursor-agent|generic|$cursor_bin|acp"
fi

echo "checking initialize against: $(printf '%s' "$adapters" | tr ';' '\n' | cut -d'|' -f1 | paste -sd' ' -)" >&2
cd "$root"
MAXPLAYER_ACP_ADAPTERS="$adapters" cargo test -p maxplayer-core --features acp --locked --lib \
  driver::acp_driver::tests::real_adapters_accept_this_clients_initialize -- --ignored --exact --nocapture
