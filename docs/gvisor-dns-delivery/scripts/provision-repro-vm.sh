#!/usr/bin/env bash
# Provision a DISPOSABLE Linux VM for the gVisor DNS reproduction.
# Runs INSIDE the throwaway lima VM `gvisor-repro`. Never run on the forge host,
# and never against the shared colima VM: other lanes depend on that daemon.
#
# Installs: docker engine, gVisor runsc, registered as the `runsc` docker runtime.
# Everything it writes lives inside the VM and dies with `limactl delete gvisor-repro`.
set -euo pipefail

RUNSC_RELEASE="${RUNSC_RELEASE:-20260831.0}"
ARCH="$(uname -m)"
LOG=/var/log/gvisor-provision.log

log() { echo "[provision] $*"; }

log "arch=${ARCH} kernel=$(uname -r) os=$(. /etc/os-release && echo "${PRETTY_NAME}")"

export DEBIAN_FRONTEND=noninteractive
sudo -E apt-get update -qq
sudo -E apt-get install -y -qq docker.io curl ca-certificates git iproute2 dnsutils >/dev/null
sudo systemctl enable --now docker

log "docker: $(docker --version)"

# gVisor. Try the release named in the brief first; fall back to the current
# release only if that exact one has no artifact for this arch, and say so loudly
# so no report can silently claim the briefed version.
install_runsc() {
  local rel="$1" base url tmp
  base="https://storage.googleapis.com/gvisor/releases/release/${rel}/${ARCH}"
  tmp="$(mktemp -d)"
  for f in runsc containerd-shim-runsc-v1; do
    url="${base}/${f}"
    if ! curl -fsSL "${url}" -o "${tmp}/${f}"; then
      echo "MISS ${url}" >&2
      rm -rf "${tmp}"
      return 1
    fi
    curl -fsSL "${url}.sha512" -o "${tmp}/${f}.sha512" || true
  done
  ( cd "${tmp}" && sha512sum -c ./*.sha512 ) || { echo "checksum failed for ${rel}" >&2; rm -rf "${tmp}"; return 1; }
  sudo install -m 0755 -t /usr/local/bin "${tmp}/runsc" "${tmp}/containerd-shim-runsc-v1"
  rm -rf "${tmp}"
  echo "${rel}" | sudo tee /etc/gvisor-installed-release >/dev/null
  return 0
}

if install_runsc "${RUNSC_RELEASE}"; then
  log "runsc installed from briefed release ${RUNSC_RELEASE}"
else
  log "WARNING: briefed release ${RUNSC_RELEASE} has no ${ARCH} artifact; falling back to 'latest'"
  install_runsc latest
  log "runsc installed from 'latest' — every result must record this substitution"
fi

log "runsc: $(runsc --version | tr '\n' ' ')"

sudo runsc install
sudo systemctl restart docker
sleep 3
docker info --format 'runtimes={{.Runtimes}}' | tee -a "${LOG}" 2>/dev/null || docker info | grep -i runtime

log "provision complete"
