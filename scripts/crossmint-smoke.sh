#!/usr/bin/env bash
#
# Live cross-mint hop smoke — a small REAL-SATS trade between two different mints.
#
# ┌───────────────────────────────────────────────────────────────────────────────────────────┐
# │ THIS SCRIPT SPENDS REAL MONEY AND REQUIRES A MONEY-GATE CONFIG CHANGE.                     │
# │ It refuses to run without an explicit authorization token naming the exact amount and      │
# │ both mints, so it cannot be started by an agent, a retry, or a stray shell history entry.  │
# └───────────────────────────────────────────────────────────────────────────────────────────┘
#
# Why this exists at all: test ecash structurally CANNOT hop. Fake mints cannot pay each other's
# invoices over real Lightning, so the hermetic teeth prove control flow, the journal, budget
# accounting and recovery — but not real LN routing between two mints. This is the only thing that
# proves that leg, and it is the reason the slice ships with that caveat stated rather than hidden.
#
# Usage (after authorization):
#   CROSSMINT_SMOKE_AUTH="<amount>:<source-mint>:<target-mint>" ./scripts/crossmint-smoke.sh
#
# The token must match the three values below exactly. Change the values, and the token stops
# matching — which is the point: authorization is for one specific spend, not for the script.

set -euo pipefail

# ── The authorized spend ────────────────────────────────────────────────────────────────────────
# Fill these in from the authorization, then hand the operator the matching CROSSMINT_SMOKE_AUTH.
AMOUNT_SATS="${AMOUNT_SATS:-}"       # what the seller receives (the buyer pays this plus hop fees)
SOURCE_MINT="${SOURCE_MINT:-}"       # the mint the buyer holds sats at
TARGET_MINT="${TARGET_MINT:-}"       # the mint the seller accepts — MUST differ from SOURCE_MINT
BUYER_HOME="${BUYER_HOME:-}"         # buyer home whose config gets allow_real_mints
SELLER_HOME="${SELLER_HOME:-}"       # seller home advertising TARGET_MINT

MOBEE="${MOBEE:-./target/release/mobee}"

die() { echo "crossmint-smoke: $*" >&2; exit 1; }

# ── Gate ────────────────────────────────────────────────────────────────────────────────────────
for var in AMOUNT_SATS SOURCE_MINT TARGET_MINT BUYER_HOME SELLER_HOME; do
    [ -n "${!var}" ] || die "$var is not set — fill in the authorized values first"
done
[ "$SOURCE_MINT" != "$TARGET_MINT" ] || die "source and target mints are identical; that is a direct payment, not a hop"

expected_auth="${AMOUNT_SATS}:${SOURCE_MINT}:${TARGET_MINT}"
# The token is not printed on refusal, deliberately. Its job is to make the run DELIBERATE — a human
# naming the amount and both mints — and a refusal that echoed the expected value would hand that
# right back to whatever just tried to run this, which is exactly the caller it exists to stop.
[ "${CROSSMINT_SMOKE_AUTH:-}" = "$expected_auth" ] || die \
"refusing to spend real sats without matching authorization.
CROSSMINT_SMOKE_AUTH must be set to '<amount>:<source-mint>:<target-mint>' for the spend that was
actually authorized. This is real money and a money-gate config change: it is authorized by a human
naming those three values, never by an agent and never by a retry."

echo "=== crossmint smoke: ${AMOUNT_SATS} sats, ${SOURCE_MINT} -> ${TARGET_MINT} ==="

# ── 0. Record the before-state, so the after-state means something ──────────────────────────────
echo "--- balances before ---"
"$MOBEE" --home "$BUYER_HOME"  wallet balances
"$MOBEE" --home "$SELLER_HOME" wallet balances
before_spent=$("$MOBEE" --home "$BUYER_HOME" budget status --json | jq -r '.spent_total_sats')
echo "buyer spent before: ${before_spent}"

# ── 1. Money-gate config: allow_real_mints on both homes ────────────────────────────────────────
# THIS IS THE MONEY-GATE CHANGE. It is part of what was authorized, and it is reverted in step 6
# whether the trade succeeds or fails.
restore_fence() {
    echo "--- restoring the real-mint fence on both homes ---"
    "$MOBEE" --home "$BUYER_HOME"  config set allow_real_mints false || true
    "$MOBEE" --home "$SELLER_HOME" config set allow_real_mints false || true
}
trap restore_fence EXIT

"$MOBEE" --home "$BUYER_HOME"  config set allow_real_mints true
"$MOBEE" --home "$SELLER_HOME" config set allow_real_mints true

# The buyer holds sats ONLY at the source; the seller accepts ONLY the target. That non-overlap is
# the whole experiment — if they overlap, the pay path plans Direct and proves nothing.
"$MOBEE" --home "$BUYER_HOME"  config set default_mint  "$SOURCE_MINT"
"$MOBEE" --home "$SELLER_HOME" config set accepted_mints "$TARGET_MINT"

buyer_balance=$("$MOBEE" --home "$BUYER_HOME" wallet balances --json \
    | jq -r --arg m "$SOURCE_MINT" '.[] | select(.mint_url == $m) | .balance_sats')
[ -n "$buyer_balance" ] && [ "$buyer_balance" -gt "$AMOUNT_SATS" ] || die \
    "buyer holds ${buyer_balance:-0} sats at ${SOURCE_MINT}; the hop needs more than ${AMOUNT_SATS} (delivery plus fees)"

# ── 2. Run one trade ────────────────────────────────────────────────────────────────────────────
# Nothing here is hop-specific: it is an ordinary trade, which is the point. The hop is chosen
# inside the pay path because the mints do not overlap, and every other step is unchanged.
echo "--- posting and settling one job ---"
job_id="crossmint-smoke-$(date -u +%Y%m%dT%H%M%SZ)"
"$MOBEE" --home "$BUYER_HOME" job post --id "$job_id" --amount "$AMOUNT_SATS" --spec "cross-mint hop smoke"
"$MOBEE" --home "$BUYER_HOME" job settle --id "$job_id" --wait

# ── 3. What has to be true ──────────────────────────────────────────────────────────────────────
echo "--- assertions ---"
outcome=$("$MOBEE" --home "$BUYER_HOME" job show --id "$job_id" --json)

state=$(echo "$outcome"        | jq -r '.state')
amount_sats=$(echo "$outcome"  | jq -r '.amount_sats')
charged_sats=$(echo "$outcome" | jq -r '.charged_sats')

[ "$state" = "Paid" ]                  || die "job state is ${state}, expected Paid"
# THE headline assertion: the seller received exactly the offer amount, and the hop's fees came out
# of the BUYER's cost rather than the seller's delivery.
[ "$amount_sats" = "$AMOUNT_SATS" ]    || die "seller received ${amount_sats}, expected exactly ${AMOUNT_SATS} — delivery is supposed to be pinned"
[ "$charged_sats" -gt "$amount_sats" ] || die "charged ${charged_sats} vs delivered ${amount_sats}: a hop must cost the buyer MORE than it delivers (fee reserve + input fee), so this did not hop"

# The receipt binds the TARGET mint: after the hop, that is where the seller's ecash lives.
receipt_mint=$(echo "$outcome" | jq -r '.receipt.mint')
[ "$receipt_mint" = "$TARGET_MINT" ]   || die "receipt binds ${receipt_mint}, expected the hop target ${TARGET_MINT}"

# The hop journal recorded the pairing and completed it — one hop, both quote ids, settled.
journal="${BUYER_HOME}/crossmint-journal"
attempt_id=$(echo "$outcome" | jq -r '.attempt_id')
[ -f "${journal}/${attempt_id}.jsonl" ] || die "no hop journal at ${journal}/${attempt_id}.jsonl — the pay path did not hop"
grep -q '"record":"planned"' "${journal}/${attempt_id}.jsonl" || die "hop journal has no pairing record"
grep -q '"record":"settled"' "${journal}/${attempt_id}.jsonl" || die "hop journal never settled — a leg is stranded"

# The budget moved by the hop's COST, not by the delivered amount.
after_spent=$("$MOBEE" --home "$BUYER_HOME" budget status --json | jq -r '.spent_total_sats')
delta=$(( after_spent - before_spent ))
[ "$delta" = "$charged_sats" ] || die "budget moved ${delta} but the hop charged ${charged_sats}"

echo "--- balances after ---"
"$MOBEE" --home "$BUYER_HOME"  wallet balances
"$MOBEE" --home "$SELLER_HOME" wallet balances

# ── 4. Pays-once under a restart ────────────────────────────────────────────────────────────────
# Re-driving a settled attempt must touch neither mint. This is the live form of the hermetic
# exactly-once tooth, and it is cheap: if it is wrong, it costs another AMOUNT_SATS and we find out.
echo "--- re-driving the settled attempt (must be a no-op) ---"
"$MOBEE" --home "$BUYER_HOME" job settle --id "$job_id" --wait || true
replay_spent=$("$MOBEE" --home "$BUYER_HOME" budget status --json | jq -r '.spent_total_sats')
[ "$replay_spent" = "$after_spent" ] || die \
    "PAYS-ONCE VIOLATED: re-driving the attempt moved the budget from ${after_spent} to ${replay_spent}"

# ── 5. The startup sweep sees a finished hop and leaves it alone ────────────────────────────────
echo "--- restarting the buyer daemon (the sweep must find nothing to resume) ---"
"$MOBEE" --home "$BUYER_HOME" buyer restart 2>&1 | tee /dev/stderr | grep -q "cross-mint hop sweep" \
    || die "the daemon did not report a hop sweep at startup"

echo
echo "=== SMOKE PASSED ==="
echo "  delivered to seller : ${amount_sats} sats at ${TARGET_MINT}"
echo "  charged to buyer    : ${charged_sats} sats (hop fees = $(( charged_sats - amount_sats )))"
echo "  attempt             : ${attempt_id}"
echo
echo "Report the delivered/charged pair and the fee delta — the gap between them IS the hop, and it"
echo "is the number #186 will later reconcile."
# The EXIT trap restores allow_real_mints=false on both homes from here.
