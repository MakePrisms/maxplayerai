#!/usr/bin/env bash
# The maxplayer-trade binary users install has no plain-HTTP mint and no ws:// mint relay, on
# loopback or anywhere (Petar, 2026-11-06). Loopback fixtures exist only in builds with the
# test-only `local-test-mints` feature, which every `cargo test` gets through a self
# dev-dependency. This check is the one place that runs the binary built WITHOUT it.
#
# Usage (repo root): scripts/check-user-build-mint-fence.sh [path/to/maxplayer-trade]
# Without an argument it builds the binary the way a user does: no features, `--locked`, no
# dev-dependencies (`cargo install --locked --path crates/maxplayer-trade` resolves the same
# features; this uses the debug profile to stay cheap in CI).
#
# Every refusal must happen before any mint or relay is contacted (a local listener counts
# connections) and before the --home directory is created. https:// and nostr:// mints must
# still pass the fence (checked through `balance` on an absent home: no network, prints 0).
set -euo pipefail

MANIFEST=crates/maxplayer-trade/Cargo.toml
HTTP_REFUSED='http:// mints are not supported; use https:// or nostr://'
WS_REFUSED='ws:// mint relays are not supported; use wss://'
NPUB=npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d

if [ $# -ge 1 ]; then
  BIN=$1
else
  cargo build --manifest-path "$MANIFEST" --locked --bin maxplayer-trade
  target=$(cargo metadata --manifest-path "$MANIFEST" --format-version 1 --no-deps |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
  BIN=$target/debug/maxplayer-trade
fi
[ -x "$BIN" ] || { echo "FAIL: no binary at $BIN"; exit 1; }

work=$(mktemp -d)
trap 'kill "$listener" 2>/dev/null || true; rm -rf "$work"' EXIT
: >"$work/connections"
# A loopback listener standing in for a fake mint / ws relay: every accepted connection is logged.
python3 - "$work" <<'PY' &
import socket, sys, os
work = sys.argv[1]
s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen(16)
open(os.path.join(work, "port.tmp"), "w").write(str(s.getsockname()[1]))
os.rename(os.path.join(work, "port.tmp"), os.path.join(work, "port"))
while True:
    c, _ = s.accept()
    with open(os.path.join(work, "connections"), "a") as f:
        f.write("x\n")
    c.close()
PY
listener=$!
for _ in $(seq 50); do [ -f "$work/port" ] && break; sleep 0.1; done
PORT=$(cat "$work/port")
HTTP_MINT=http://127.0.0.1:$PORT
WS_RELAY=ws://127.0.0.1:$PORT

fail=0
n=0
# run <expected exit> <expected text or ''> <args...>: runs with an absent --home.
run() {
  local want_code=$1 want_text=$2
  shift 2
  n=$((n + 1))
  local home=$work/home-$n code=0
  env -u MAXPLAYER_HOME "$BIN" --home "$home" "$@" >"$work/out" 2>"$work/err" || code=$?
  local all
  all=$(cat "$work/out" "$work/err")
  if [ "$code" != "$want_code" ]; then
    echo "FAIL: maxplayer-trade $*: exit $code, expected $want_code"; echo "$all"; fail=1; return
  fi
  if [ -n "$want_text" ] && ! grep -qF -- "$want_text" <<<"$all"; then
    echo "FAIL: maxplayer-trade $*: output lacks '$want_text'"; echo "$all"; fail=1; return
  fi
  if [ -e "$home" ]; then
    echo "FAIL: maxplayer-trade $*: created --home $home"; fail=1; return
  fi
  echo "ok: maxplayer-trade $* -> exit $code: $(grep -F -- "${want_text:-\"balance\"}" <<<"$all" | head -1)"
}

# Refused: plain HTTP mints on loopback, in every command that takes a mint.
run 1 "$HTTP_REFUSED" preflight "$HTTP_MINT"
run 1 "$HTTP_REFUSED" balance "$HTTP_MINT"
run 1 "$HTTP_REFUSED" fund "$HTTP_MINT" --amount 1
run 1 "$HTTP_REFUSED" withdraw "$HTTP_MINT" --invoice lnbc1
run 1 "$HTTP_REFUSED" receive "$HTTP_MINT" --token-file /dev/null
run 1 "$HTTP_REFUSED" send "$HTTP_MINT" --amount 1 --out "$work/token"
run 1 "$HTTP_REFUSED" list --give-mint "$HTTP_MINT" --give 1 --want-mint https://testnut.cashudevkit.org --want 1
run 1 "$HTTP_REFUSED" list --give-mint https://testnut.cashudevkit.org --give 1 --want-mint "$HTTP_MINT" --want 1 --dry-run
run 1 "$HTTP_REFUSED" preflight http://localhost:$PORT
run 1 "$HTTP_REFUSED" preflight "http://[::1]:$PORT"
# doctor reports configuration failures as its diagnostic exit 2, naming the refusal.
run 2 "$HTTP_REFUSED" doctor "$HTTP_MINT" --mint-relay wss://relay.maxplayer.ai --json
# Refused: ws:// mint relays on loopback, globally and for doctor.
run 1 "$WS_REFUSED" --mint-relay "$WS_RELAY" status
run 1 "$WS_REFUSED" --mint-relay "$WS_RELAY" preflight "nostr://$NPUB"
run 2 "$WS_REFUSED" doctor "nostr://$NPUB" --mint-relay "$WS_RELAY" --json
# Still accepted by the fence (no network: balance of an absent home is 0).
run 0 '"balance":0' balance https://testnut.cashudevkit.org
run 0 '"balance":0' balance "nostr://$NPUB"
run 0 '"balance":0' --mint-relay wss://relay.maxplayer.ai balance "nostr://$NPUB"

connections=$(wc -l <"$work/connections")
if [ "$connections" != 0 ]; then
  echo "FAIL: $connections connection(s) reached the loopback mint/relay listener"
  fail=1
fi
if [ "$fail" != 0 ]; then
  echo "FAIL: the user build of maxplayer-trade must refuse http:// mints and ws:// mint relays"
  exit 1
fi
echo "PASS: user build refuses http:// mints and ws:// mint relays ($n commands, 0 connections)"
