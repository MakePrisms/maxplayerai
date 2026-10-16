#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
# Default to checkout-local targets; an explicit idle warm target may be supplied.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target/challenges-e2e}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
export CARGO_PROFILE_DEV_DEBUG=0
mkdir -p .scratch
if [[ "${1:-}" != --skip-build ]]; then
  cargo build --locked -p maxplayer --no-default-features --features wallet,acp
  CARGO_TARGET_DIR="$PWD/target/challenges-buzz" cargo build --locked --manifest-path crates/buzz/Cargo.toml -p buzz-relay
  CARGO_TARGET_DIR="$PWD/crates/maxplayer-challenges/target" cargo build --locked --manifest-path crates/maxplayer-challenges/Cargo.toml --bins --example local_relay
  npm ci --prefix web/community
fi
exec python3 scripts/challenges/e2e.py "${@:1}"
