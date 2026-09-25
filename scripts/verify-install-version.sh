#!/bin/sh
# Offline regression check for the installer's exact version/build-stamp gate.
set -eu
cd "$(dirname "$0")/.."
# Load definitions only: do not run the installer or make network calls.
eval "$(sed '$d' install.sh)"
VERSION=0.6.0-rc2
stamp=0123456789abcdef0123456789abcdef01234567
for line in "maxplayer $VERSION" "maxplayer $VERSION ($stamp)"; do
    version_matches "$line" || { echo "FAIL: rejected $line" >&2; exit 1; }
done
for line in \
    "maxplayer 0.6.0-rc1 ($stamp)" \
    "maxplayer 0.6.0 ($stamp)" \
    "maxplayer ${VERSION}0 ($stamp)" \
    "other $VERSION ($stamp)" \
    "maxplayer $VERSION (unknown)" \
    "maxplayer $VERSION ()" \
    "maxplayer $VERSION (abc123)" \
    "maxplayer $VERSION (${stamp}0)" \
    "maxplayer $VERSION (${stamp%?}g)" \
    "maxplayer $VERSION ($stamp" \
    "maxplayer $VERSION ($stamp) extra" \
    "maxplayer $VERSION ($stamp)
extra"; do
    if version_matches "$line"; then
        echo "FAIL: accepted $line" >&2
        exit 1
    fi
done
echo "PASS: installer accepts exact versions with optional SHA and rejects 12 malformed/mismatched lines"
