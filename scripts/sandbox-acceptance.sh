#!/usr/bin/env bash
#
# The ONE offline acceptance command for job-local veth containment (#996).
#
# It exists because the obvious command lies. The live containment matrix lives in
# `crates/maxplayer-core/tests/sandbox_netns_live.rs`, every case `#[ignore]`d because each one needs
# a docker daemon, a gVisor runtime and a built sidecar image. So `cargo test -p maxplayer-core
# --features acp,wallet` prints `0 passed; N ignored` and exits 0 on a machine that has never run a
# single containment case — the same green as a machine where every case passed. Accepting on that
# output is accepting nothing.
#
# This entrypoint splits the two claims apart and makes both of them fail-able:
#
#   1. SOURCE — the offline suite (unit + non-live integration) compiles and passes here, now.
#   2. MATRIX — a SAVED record of a live run is validated by `maxplayer_core::sandbox_evidence`:
#      every required case present exactly once with the outcome the matrix requires, no unscored
#      outcome, no unknown id, and identity headers naming the commit, binary and host that produced
#      it. That is the leg `cargo test` alone cannot make.
#
# ★ Replay is NOT here, on purpose. Re-running the live cases needs the VM, the runtime and the
#   image; a validator that shelled out to docker would be the live gate wearing a false beard, and
#   would turn "is this record complete" into "is the daemon up today". Live replay is the separate
#   command documented in the PR body.
#
# ★ A missing record is a FAILURE, never a skip. Refusing to run without one is the whole point: the
#   silent skip is the defect this script was written against.
#
# Usage:
#
#   scripts/sandbox-acceptance.sh path/to/live-matrix.txt
#   MAXPLAYER_LIVE_EVIDENCE=path/to/live-matrix.txt scripts/sandbox-acceptance.sh
#
# Exit 0 means: this source passed its offline suite, and the named record is a complete live matrix
# attributable to a commit contained in this branch. It does NOT mean the live run happened today,
# and it does not re-measure one packet.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

RECORD="${1:-${MAXPLAYER_LIVE_EVIDENCE:-}}"

if [[ -z "${RECORD}" ]]; then
  cat >&2 <<'EOF'
sandbox-acceptance: no saved live matrix named.

Pass one as the first argument, or set MAXPLAYER_LIVE_EVIDENCE. This is deliberately fatal: the
offline suite alone reports the live containment cases as *ignored*, so running without a record
would report success for a matrix nobody measured.

  scripts/sandbox-acceptance.sh evidence/<run>/live-matrix.txt
EOF
  exit 2
fi

if [[ ! -r "${RECORD}" ]]; then
  echo "sandbox-acceptance: saved live matrix '${RECORD}' is not readable." >&2
  echo "A named record that is not there is a missing gate, not an absent one." >&2
  exit 2
fi

RECORD_ABS="$(cd "$(dirname "${RECORD}")" && pwd)/$(basename "${RECORD}")"

echo "== sandbox acceptance =="
echo "repo HEAD : $(git rev-parse HEAD)"
echo "record    : ${RECORD_ABS}"
echo "record sha: $(shasum -a 256 "${RECORD_ABS}" | awk '{print $1}')"

# --- leg 0: the record's source identity belongs to this branch -------------------------------
#
# `sandbox_evidence::validate` checks that the record NAMES a commit; it cannot check that the
# commit is one of ours. A complete matrix measured on someone else's source is a complete matrix
# about someone else's source, and it would pass the module's checks unchallenged.
SOURCE_HEAD="$(awk -F= '/^source_head=/{print $2; exit}' "${RECORD_ABS}" | tr -d '[:space:]')"
if [[ -z "${SOURCE_HEAD}" ]]; then
  echo "sandbox-acceptance: record names no source_head." >&2
  exit 1
fi
if ! git cat-file -e "${SOURCE_HEAD}^{commit}" 2>/dev/null; then
  echo "sandbox-acceptance: record's source_head ${SOURCE_HEAD} is not a commit in this repo." >&2
  exit 1
fi
if ! git merge-base --is-ancestor "${SOURCE_HEAD}" HEAD; then
  echo "sandbox-acceptance: record's source_head ${SOURCE_HEAD} is NOT an ancestor of HEAD." >&2
  echo "The matrix describes source this branch does not contain; re-run the live matrix." >&2
  exit 1
fi
echo "source_head ${SOURCE_HEAD} is contained in HEAD ($(git rev-list --count "${SOURCE_HEAD}..HEAD") commit(s) since)."

# --- leg 1: the offline suite, on this source -------------------------------------------------
#
# `--all-targets` so the live test file is type-checked even though it will not run, and the full
# `acp,wallet` set because `seller_exec` — the caller that hands a job to the contained runtime — is
# wallet-gated and is otherwise compiled out of the run.
echo
echo "-- cargo check (acp,wallet, all targets) --"
cargo check -p maxplayer-core --features acp,wallet --all-targets

echo
echo "-- cargo test (acp,wallet) --"
cargo test -p maxplayer-core --features acp,wallet

# --- leg 2: the saved matrix ------------------------------------------------------------------
#
# The validating test returns early when the variable is unset, so a filter typo would make it
# "pass" having checked nothing. Hence the explicit `1 passed` assertion below: the gate has to prove
# it executed, not merely that nothing complained.
echo
echo "-- saved live matrix --"
MATRIX_LOG="$(mktemp -t sandbox-acceptance-matrix)"
trap 'rm -f "${MATRIX_LOG}"' EXIT
set +e
MAXPLAYER_LIVE_EVIDENCE="${RECORD_ABS}" cargo test -p maxplayer-core --features acp,wallet --lib -- \
  --exact sandbox_evidence::tests::the_named_saved_matrix_validates --nocapture 2>&1 | tee "${MATRIX_LOG}"
MATRIX_STATUS="${PIPESTATUS[0]}"
set -e
if [[ "${MATRIX_STATUS}" -ne 0 ]]; then
  echo "sandbox-acceptance: the saved live matrix did not validate." >&2
  exit 1
fi
if ! grep -qE 'test result: ok\. 1 passed' "${MATRIX_LOG}"; then
  echo "sandbox-acceptance: the matrix test did not run (expected exactly 1 passed)." >&2
  echo "A filter that selects no test exits 0 and proves nothing." >&2
  exit 1
fi

echo
echo "ACCEPTED (offline): source suite green, saved matrix complete for ${SOURCE_HEAD}."
echo "NOT claimed: any live packet was measured by this command, or that the record is recent."
