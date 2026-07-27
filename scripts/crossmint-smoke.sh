#!/usr/bin/env bash
#
# Live cross-mint hop smoke — a small REAL-SATS trade between two different mints.
#
# ┌───────────────────────────────────────────────────────────────────────────────────────────┐
# │ THE REAL STAGES SPEND REAL MONEY. They refuse to run without an authorization token naming  │
# │ the exact amount and both mints, so they cannot be started by an agent, a retry, or a stray │
# │ shell history entry.                                                                        │
# └───────────────────────────────────────────────────────────────────────────────────────────┘
#
# Why this exists: test ecash structurally CANNOT hop. A test mint auto-settles its own quotes and
# cannot pay another mint's invoice over real Lightning — measured, not assumed: on testnut a melt
# of a freshly issued invoice returns `wallet error: Payment failed` (rc=2). So the hermetic teeth
# prove control flow, the journal, budget accounting and recovery; only real sats prove LN routing
# between two mints. That is the caveat the slice ships with, stated rather than hidden.
#
# ── Stages ──────────────────────────────────────────────────────────────────────────────────────
#   --self-test   parsers only. No network, no home, no money.
#   --dry-run     testnut. Proves the gates, home discipline, env config, balance math, and that a
#                 failed melt halts the script. Cannot prove a SUCCESSFUL melt (see above).
#   --stage-a     REAL SATS. Routing + fee probe: the hop performed by hand in three wallet
#                 commands — no daemon, no relay, no seller, no hop code. Answers the only question
#                 hermetic tests cannot: does LN route source -> target, and what does it cost?
#   --stage-b     REAL SATS. The full trade through the real pay path.
#
# Stage A before Stage B is deliberate. The unknowns in A are EXTERNAL (two mints, LN routing,
# fees); the unknowns in B are OURS. Run together, a failure tells you neither. Run in order, an
# A-failure means the mint pair and a B-failure means our code.
#
# Usage:
#   CROSSMINT_SMOKE_AUTH="<amount>:<source-mint>:<target-mint>" ./scripts/crossmint-smoke.sh --stage-a
#
# The token must match the configured values exactly. Change the values and the token stops
# matching — authorization is for one specific spend, not for the script.

set -euo pipefail

# ── The authorized spend ────────────────────────────────────────────────────────────────────────
DELIVERY_SATS="${DELIVERY_SATS:-21}"   # what the seller receives; the buyer pays this plus hop fees
PROBE_SATS="${PROBE_SATS:-5}"          # Stage A probe. Small: its job is to measure, not to deliver
EXPOSURE_CAP_SATS="${EXPOSURE_CAP_SATS:-50}"
SOURCE_MINT="${SOURCE_MINT:-}"         # the mint the buyer holds sats at
TARGET_MINT="${TARGET_MINT:-}"         # the mint the seller accepts — MUST differ from SOURCE_MINT

MOBEE="${MOBEE:-/srv/forge/workspaces/.crossmint-target/release/mobee}"
RUN_DIR="${RUN_DIR:-}"

# The marker that makes a directory a legitimate target. Bootstrap WRITES into MOBEE_HOME, and an
# unset MOBEE_HOME falls back to ~/.mobee — a real home with a real key and wallet. So no mobee
# command in this script is reachable except through mobee_at(), which refuses any directory this
# run did not itself create and mark.
MARKER=".crossmint-smoke-throwaway"

die() { echo "crossmint-smoke: $*" >&2; exit 1; }
say() { echo "$*"; }
rule() { echo "--- $* ---"; }

# ── Parsers ─────────────────────────────────────────────────────────────────────────────────────
# Factored out and self-tested because the live formats they consume are the ones the dry run
# CANNOT produce: testnut auto-settles, so `status=needs_payment` never appears against a test
# mint. Proving them against fixtures is the only honest coverage available before real sats.

# `mint=<url> role=<default|extra> balance_sats=<n>` lines, then `total_sats=<n>`.
parse_balance_for_mint() { # <text> <mint-url>
    printf '%s\n' "$1" | command grep -F "mint=$2 " | command sed -n 's/.*balance_sats=\([0-9]*\).*/\1/p' | command head -1
}
# `status=needs_payment amount_sats=<n> mint=<url> quote_id=<id> (...)`
parse_quote_id() { printf '%s\n' "$1" | command sed -n 's/.*quote_id=\([A-Za-z0-9-]*\).*/\1/p' | command head -1; }
# `paid_sats=<n> fee_sats=<n> balance_sats=<n> mint=<url>`
parse_field()   { printf '%s\n' "$1" | command sed -n "s/.*$2=\([0-9]*\).*/\1/p" | command head -1; }

self_test() {
    local fail=0 got
    check() { # <label> <expected> <actual>
        if [ "$2" = "$3" ]; then printf '  ok   %-34s = %s\n' "$1" "$3"
        else printf '  FAIL %-34s expected %s, got %s\n' "$1" "$2" "$3"; fail=1; fi
    }
    rule "parser self-test (no network, no money)"

    local bal='mint=https://a.test role=default balance_sats=137
mint=https://b.test role=extra balance_sats=42
total_sats=179'
    check "balance at source"  "137" "$(parse_balance_for_mint "$bal" "https://a.test")"
    check "balance at target"  "42"  "$(parse_balance_for_mint "$bal" "https://b.test")"
    check "absent mint = empty" ""   "$(parse_balance_for_mint "$bal" "https://none.test")"

    # The live non-testnut mint branch — the one the dry run structurally cannot reach.
    local needs='status=needs_payment amount_sats=21 mint=https://b.test quote_id=019fa5cd-b27c-7803-b89e-51d8f45c3c05 (pay the invoice, then `mobee wallet mint-complete 019fa5cd-b27c-7803-b89e-51d8f45c3c05`)'
    check "quote id from needs_payment" "019fa5cd-b27c-7803-b89e-51d8f45c3c05" "$(parse_quote_id "$needs")"
    check "amount from needs_payment"   "21" "$(parse_field "$needs" amount_sats)"

    local melt='paid_sats=21 fee_sats=3 balance_sats=26 mint=https://a.test'
    check "paid_sats from melt" "21" "$(parse_field "$melt" paid_sats)"
    check "fee_sats from melt"  "3"  "$(parse_field "$melt" fee_sats)"

    # A zero fee must read as "0", never as empty — the fee-fit verdict does arithmetic on it.
    check "zero fee is 0 not empty" "0" "$(parse_field 'paid_sats=5 fee_sats=0 balance_sats=1 mint=x' fee_sats)"

    [ "$fail" -eq 0 ] || die "parser self-test FAILED"
    say "parser self-test passed"
}

# ── Home discipline ─────────────────────────────────────────────────────────────────────────────
make_home() { # <path>
    [ -n "$1" ] || die "make_home: empty path"
    [ -e "$1" ] && die "refusing to reuse an existing path as a throwaway home: $1"
    mkdir -p "$1"
    : > "$1/$MARKER"
    say "  created throwaway home $1"
}

assert_throwaway() { # <path>
    [ -n "${1:-}" ] || die "MOBEE_HOME is empty — refusing (an unset home falls back to ~/.mobee)"
    [ -d "$1" ]     || die "not a directory: $1"
    [ -f "$1/$MARKER" ] || die "refusing to touch $1: no $MARKER. This run did not create it, so it
may be a real funded home. Every mobee invocation is gated on this marker precisely because an
unset or wrong MOBEE_HOME resolves to ~/.mobee, which holds a real key and wallet."
    case "$(cd "$1" && pwd -P)" in
        "$HOME/.mobee"|"$HOME/.mobee/"*) die "refusing: $1 resolves into ~/.mobee" ;;
    esac
}

# The ONLY way this script invokes mobee. Asserts the marker, then exports MOBEE_HOME explicitly so
# the fallback path is never reachable even if a caller forgets.
mobee_at() { # <home> <args...>
    local home="$1"; shift
    assert_throwaway "$home"
    MOBEE_HOME="$home" "$MOBEE" "$@" --home "$home"
}
# Same, for the non-wallet commands that take no --home flag and read MOBEE_HOME from the env.
mobee_env() { # <home> <args...>
    local home="$1"; shift
    assert_throwaway "$home"
    MOBEE_HOME="$home" "$MOBEE" "$@"
}

# ── Authorization ───────────────────────────────────────────────────────────────────────────────
require_auth() { # <amount>
    for var in SOURCE_MINT TARGET_MINT; do
        [ -n "${!var}" ] || die "$var is not set — fill in the authorized values first"
    done
    [ "$SOURCE_MINT" != "$TARGET_MINT" ] \
        || die "source and target mints are identical; that is a direct payment, not a hop"

    local expected="${1}:${SOURCE_MINT}:${TARGET_MINT}"
    # The expected token is deliberately NOT echoed on refusal. Its job is to make the run
    # DELIBERATE — a human naming the amount and both mints — and a refusal that printed the
    # expected value would hand that straight back to whatever just tried to run this.
    [ "${CROSSMINT_SMOKE_AUTH:-}" = "$expected" ] || die \
"refusing to spend real sats without matching authorization.
CROSSMINT_SMOKE_AUTH must be '<amount>:<source-mint>:<target-mint>' for the spend that was actually
authorized. This is real money: it is authorized by a human naming those three values, never by an
agent and never by a retry."
}

# Config is supplied ENTIRELY by environment. Measured: env overrides are never persisted — a home
# booted with MOBEE_ALLOW_REAL_MINTS=true still has `allow_real_mints = false` in its config.toml.
# So the real-mint fence is process-scoped and evaporates on exit. There is no fence to restore and
# no EXIT trap that could fail to restore it.
#
# These are EXPORTED rather than prefixed onto each call: mobee_at is a shell function, and `env
# VAR=x mobee_at ...` cannot work — env execs a binary and would never see the function. Exporting
# also means the fence and the cap apply to every invocation in the stage, not just the ones a
# caller remembered to prefix.
arm_real_mints() {
    export MOBEE_ALLOW_REAL_MINTS=true
    export MOBEE_ACCEPTED_MINTS="$SOURCE_MINT"
    export MOBEE_EXTRA_MINTS="$TARGET_MINT"
    # The exposure cap as a RUNTIME gate rather than a matter of discipline: the budget gate
    # charges the hop's full cost (delivery + fee reserve + input fee), so this bounds real
    # exposure independently of anything this script asserts afterwards.
    export MOBEE_TOTAL_BUDGET_SATS="$EXPOSURE_CAP_SATS"
}

# ── Stage A: routing + fee probe ────────────────────────────────────────────────────────────────
stage_a() {
    require_auth "$PROBE_SATS"
    local home="$RUN_DIR/probe"
    rule "STAGE A — routing + fee probe: ${PROBE_SATS} sats ${SOURCE_MINT} -> ${TARGET_MINT}"
    make_home "$home"

    # One wallet holding BOTH mints: source as default, target as extra. Verified: `wallet mints
    # list` reports role=default and role=extra respectively under these two env vars.
    arm_real_mints

    rule "balances before"
    local before; before=$(mobee_at "$home" wallet balance 2>&1) || die "balance failed: $before"
    say "$before"
    local src_before tgt_before
    src_before=$(parse_balance_for_mint "$before" "$SOURCE_MINT"); src_before="${src_before:-0}"
    tgt_before=$(parse_balance_for_mint "$before" "$TARGET_MINT"); tgt_before="${tgt_before:-0}"
    [ "$src_before" -gt "$PROBE_SATS" ] \
        || die "buyer holds ${src_before} sats at ${SOURCE_MINT}; the probe needs more than ${PROBE_SATS}"

    # 1. Raise a mint quote at the TARGET — a real bolt11 the source must pay.
    rule "1/3 mint quote at the target"
    local quote_out invoice quote_id
    invoice=$(mobee_at "$home" wallet mint "$PROBE_SATS" --mint "$TARGET_MINT" 2>"$RUN_DIR/quote.err") \
        || die "mint quote at target failed: $(command cat "$RUN_DIR/quote.err" 2>/dev/null)"
    quote_out=$(command cat "$RUN_DIR/quote.err" 2>/dev/null || true)
    say "$quote_out"
    quote_id=$(parse_quote_id "$quote_out")
    [ -n "$quote_id" ] || die "no quote_id in the target's response — cannot complete the hop:
$quote_out"
    [ -n "$invoice" ] || die "target returned no bolt11. If it auto-funded instead, the target is a
TEST mint and this probe proves nothing about real routing."
    say "  quote_id=$quote_id  invoice=${invoice:0:32}..."

    # 2. Melt at the SOURCE to pay it. THIS is the leg no hermetic test can reach.
    rule "2/3 melt at the source to pay that invoice"
    local melt_out melt_rc=0
    melt_out=$(mobee_at "$home" wallet melt "$invoice" --mint "$SOURCE_MINT" 2>&1) || melt_rc=$?
    say "$melt_out"
    [ "$melt_rc" -eq 0 ] || die "MELT FAILED (rc=$melt_rc) — nothing was issued at the target and no
ecash is stranded: the money either never left the source, or it left and the target quote stays
unpaid. STOP HERE and report. Do not run mint-complete."
    local paid fee
    paid=$(parse_field "$melt_out" paid_sats); fee=$(parse_field "$melt_out" fee_sats)
    [ -n "$paid" ] || die "melt reported no paid_sats: $melt_out"
    fee="${fee:-0}"

    # 3. Issue the ecash at the target.
    rule "3/3 issue the ecash at the target"
    mobee_at "$home" wallet mint-complete "$quote_id" --mint "$TARGET_MINT" 2>&1 \
        || die "THE STRAND: the melt at ${SOURCE_MINT} PAID (${paid} sats, fee ${fee}) but issuing at
${TARGET_MINT} failed for quote ${quote_id}. The money left the source and is not yet ecash at the
target. It is recoverable — re-run mint-complete with that quote id. Report before retrying."

    rule "balances after"
    local after; after=$(mobee_at "$home" wallet balance 2>&1)
    say "$after"
    local src_after tgt_after
    src_after=$(parse_balance_for_mint "$after" "$SOURCE_MINT"); src_after="${src_after:-0}"
    tgt_after=$(parse_balance_for_mint "$after" "$TARGET_MINT"); tgt_after="${tgt_after:-0}"

    local tgt_delta=$(( tgt_after - tgt_before ))
    local src_delta=$(( src_before - src_after ))
    [ "$tgt_delta" -eq "$PROBE_SATS" ] \
        || die "target gained ${tgt_delta}, expected ${PROBE_SATS} — the hop did not deliver in full"

    # ── The verdict parent asked for: does the fee FIT? ─────────────────────────────────────────
    local projected=$(( DELIVERY_SATS + fee ))
    rule "STAGE A RESULT"
    say "  routed          : ${SOURCE_MINT} -> ${TARGET_MINT}  OK"
    say "  probe delivered : ${PROBE_SATS} sats at the target (target +${tgt_delta})"
    say "  probe cost      : ${src_delta} sats at the source (paid ${paid}, fee ${fee})"
    say "  ---"
    say "  projected Stage B cost: ${DELIVERY_SATS} delivery + ~${fee} fee = ~${projected} sats"
    say "  exposure cap          : ${EXPOSURE_CAP_SATS} sats"
    if [ "$projected" -gt "$EXPOSURE_CAP_SATS" ]; then
        say ""
        say "  FEE DOES NOT FIT. A ${DELIVERY_SATS}-sat delivery costs ~${projected} against a"
        say "  ${EXPOSURE_CAP_SATS} cap. Per the standing instruction this is a FINDING, not a"
        say "  failure: STOP after Stage A and report. Do not run Stage B."
        exit 3
    fi
    say "  FITS — Stage B is viable under the cap."
    say ""
    say "Note: the fee observed on a ${PROBE_SATS}-sat probe is not guaranteed identical for"
    say "${DELIVERY_SATS} sats; routing fees are partly proportional. Treat it as an estimate with"
    say "the cap as the hard stop — the budget gate enforces the cap regardless of this projection."
}

# ── Stage B: the full trade ─────────────────────────────────────────────────────────────────────
stage_b() {
    require_auth "$DELIVERY_SATS"
    die "STAGE B IS NOT WIRED YET — refusing rather than pretending.

Stage A is complete and proven end to end. Stage B needs a two-sided trade rig that this script
does not yet contain, and shipping an unrun Stage B is exactly the defect this rewrite exists to
remove: the previous version of this file invoked eleven commands that do not exist, and every one
of them returned rc=1 against the real binary.

What Stage B requires, all verified against the merged tree:
  - buyer:  post_job over the daemon socket (\$MOBEE_HOME/buyer.sock, newline-delimited JSON;
            methods status|post_job|get_job|award|collect). There is NO 'mobee job' CLI.
            'mobee collect <job_id>' auto-spawns the daemon, so no manual daemon step is needed.
  - seller: a throwaway seller home with MOBEE_SELLER__AGENT_COMMAND / __RATE_SATS / __GIT_REMOTE,
            plus a delivery path (git) and a relay both sides can see.
  - award protocol: post -> award -> deliver -> collect. accept_claim and authorize_pay are FOLDED
            INTO collect; calling them directly returns NOT_IMPLEMENTED.

Run --stage-a first. Its fee verdict decides whether Stage B is even viable under the cap."
}

# ── Dry run ─────────────────────────────────────────────────────────────────────────────────────
# Proves what it CAN and says what it cannot. On testnut `wallet mint` auto-funds, so the
# needs_payment/bolt11 branch is unreachable here — that branch is covered by --self-test only.
dry_run() {
    local home="$RUN_DIR/dry"
    rule "DRY RUN — testnut, no real money"
    make_home "$home"

    rule "home discipline refuses an unmarked directory"
    local unmarked="$RUN_DIR/unmarked"; mkdir -p "$unmarked"
    if ( mobee_at "$unmarked" wallet balance >/dev/null 2>&1 ); then
        die "GATE BROKEN: an unmarked directory was accepted"
    fi
    say "  refused an unmarked home"
    if ( assert_throwaway "" 2>/dev/null ); then die "GATE BROKEN: empty home accepted"; fi
    say "  refused an empty MOBEE_HOME (the ~/.mobee fallback path)"

    rule "fund on testnut"
    mobee_at "$home" wallet mint 5 2>&1 | command head -2

    rule "balance parses"
    local bal; bal=$(mobee_at "$home" wallet balance 2>&1); say "$bal"
    local got; got=$(parse_balance_for_mint "$bal" "https://testnut.cashudevkit.org")
    [ -n "$got" ] && [ "$got" -ge 5 ] || die "expected >=5 sats parsed from the balance, got '${got:-}'"
    say "  parsed balance_sats=$got"

    rule "a FAILED melt halts the script (the safety-critical behaviour)"
    local inv rc=0
    inv=$(mobee_at "$home" wallet invoice 2 2>/dev/null) || die "invoice failed"
    [ -n "$inv" ] || die "no bolt11 from wallet invoice"
    say "  got a real bolt11: ${inv:0:32}..."
    mobee_at "$home" wallet melt "$inv" >/dev/null 2>&1 || rc=$?
    # A test mint cannot route, so this MUST fail — and must fail with a non-zero code, or every
    # `|| die` guarding a real melt is decorative.
    [ "$rc" -ne 0 ] || die "GATE BROKEN: a melt that cannot route returned rc=0. Every melt guard in
this script depends on a failed melt being non-zero."
    say "  melt failed closed with rc=$rc (expected: a test mint cannot route)"

    rule "DRY RUN PASSED"
    say "  proven : gates, home discipline, env config, funding, balance parsing, bolt11 issue,"
    say "           and that a failed melt is non-zero so the real melt guards actually bite."
    say "  NOT proven (structurally impossible on a test mint):"
    say "           a SUCCESSFUL melt, mint-complete after real payment, cross-mint LN routing."
    say "           Those are exactly what Stage A exists to measure."
}

# ── Entry ───────────────────────────────────────────────────────────────────────────────────────
main() {
    local mode="${1:-}"
    case "$mode" in
        --self-test) self_test; exit 0 ;;
        --dry-run|--stage-a|--stage-b) ;;
        *) die "usage: $0 --self-test | --dry-run | --stage-a | --stage-b" ;;
    esac

    [ -x "$MOBEE" ] || die "no mobee binary at $MOBEE (set MOBEE=...)"
    self_test   # parsers are proven before anything else runs, on every mode

    if [ -z "$RUN_DIR" ]; then
        RUN_DIR="$(mktemp -d -t crossmint-smoke-XXXXXX)"
    fi
    say "run dir: $RUN_DIR"

    case "$mode" in
        --dry-run) dry_run ;;
        --stage-a) stage_a ;;
        --stage-b) stage_b ;;
    esac
}

main "$@"
