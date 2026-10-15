#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
# Both targets belong only to this checkout; never borrow another agent's target.
export CARGO_TARGET_DIR="$PWD/target/challenges-e2e"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
export CARGO_PROFILE_DEV_DEBUG=0
mkdir -p .scratch
if [[ "${1:-}" != --skip-build ]]; then
  cargo build --locked -p maxplayer --no-default-features --features wallet,acp
  CARGO_TARGET_DIR="$PWD/target/challenges-buzz" cargo build --locked --manifest-path crates/buzz/Cargo.toml -p buzz-relay
  npm ci --prefix web/community
fi
exec python3 scripts/challenges/e2e.py "${@:1}"
