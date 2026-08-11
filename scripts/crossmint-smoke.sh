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
#   --self-test              parsers only. No network, no home, no money.
#   --dry-run                testnut. Proves the gates, home discipline, env config, balance math,
#                            and that a failed melt halts the script. Cannot prove a SUCCESSFUL
#                            melt (see above).
#   --fund                   REAL SATS. Raises a mint quote at the SOURCE and prints a bolt11 for a
#                            human to pay. Nothing is at risk until that invoice is paid.
#   --fund-complete <quote>  issues the ecash once the invoice is paid.
#   --stage-a                REAL SATS. Routing + fee probe: the hop performed by hand in three
#                            wallet commands — no daemon, no relay, no seller, no hop code. Answers
#                            the only question hermetic tests cannot: does LN route source ->
#                            target, and what does it cost? Exits 3 if the fee will not fit.
#   --stage-b                refuses; the two-sided trade rig is its own slice.
#
# Stage A before Stage B is deliberate. The unknowns in A are EXTERNAL (two mints, LN routing,
# fees); the unknowns in B are OURS. Run together, a failure tells you neither. Run in order, an
# A-failure means the mint pair and a B-failure means our code.
#
# Usage — fund and probe are separate invocations against the SAME throwaway wallet, so RUN_DIR must
# be the same for both (it defaults to a stable path):
#
#   export SOURCE_MINT=... TARGET_MINT=...
#   export CROSSMINT_SMOKE_AUTH="<fund-sats>:<source-mint>:<target-mint>"
#   ./scripts/crossmint-smoke.sh --fund
#   # pay the printed invoice, then:
#   ./scripts/crossmint-smoke.sh --fund-complete <quote_id>
#   ./scripts/crossmint-smoke.sh --stage-a
#
# The token names the TOTAL FUNDED EXPOSURE and must match exactly. Change the values and the token
# stops matching — authorization is for one specific spend, not for the script.

set -euo pipefail

# ── The authorized spend ────────────────────────────────────────────────────────────────────────
DELIVERY_SATS="${DELIVERY_SATS:-21}"   # what the seller receives; the buyer pays this plus hop fees
FUND_SATS="${FUND_SATS:-50}"      # total real sats put at risk; the auth token names THIS number
PROBE_SATS="${PROBE_SATS:-5}"          # Stage A probe. Small: its job is to measure, not to deliver
EXPOSURE_CAP_SATS="${EXPOSURE_CAP_SATS:-50}"
SOURCE_MINT="${SOURCE_MINT:-}"         # the mint the buyer holds sats at
TARGET_MINT="${TARGET_MINT:-}"         # the mint the seller accepts — MUST differ from SOURCE_MINT

MAXPLAYER="${MAXPLAYER:-/srv/forge/workspaces/.crossmint-target/release/maxplayer}"
RUN_DIR="${RUN_DIR:-/tmp/crossmint-smoke-run}"

# The marker that makes a directory a legitimate target. Bootstrap WRITES into MAXPLAYER_HOME, and an
# unset MAXPLAYER_HOME falls back to ~/.maxplayer — a real home with a real key and wallet. So no maxplayer
# command in this script is reachable except through maxplayer_at(), which refuses any directory this
# run did not itself create and mark.
MARKER=".crossmint-smoke-throwaway"

die() { echo "crossmint-smoke: $*" >&2; exit 1; }
say() { echo "$*"; }
rule() { echo "--- $* ---"; }

# ── Parsers ─────────────────────────────────────────────────────────────────────────────────────
# Factored out and self-tested because the live formats they consume are the ones the dry run
# CANNOT produce: testnut auto-settles, so `status=needs_payment` never appears against a test
# mint. Proving them against fixtures is the only honest coverage available before real sats.

# `mint=<url> role=<default|extra|unconfigured> balance_sats=<n>` lines, then `total_sats=<n>`
# (preceded by `configured_total_sats=<n>` only when the two differ — never for this script's
# configured-only mints). The parser keys on `mint=<url> ` and reads `balance_sats`, so the role
# vocabulary is informational here.
parse_balance_for_mint() { # <text> <mint-url>
    printf '%s\n' "$1" | command grep -F "mint=$2 " | command sed -n 's/.*balance_sats=\([0-9]*\).*/\1/p' | command head -1
}
# `status=needs_payment amount_sats=<n> mint=<url> quote_id=<id> (...)`
#
# Match everything up to whitespace rather than an allow-list of characters. A previous version
# used [A-Za-z0-9-], which silently TRUNCATED real quote ids at the first underscore
# (lcLc0JwHHCIG_UIwQ8... -> lcLc0JwHHCIG) and cost two live mint-complete failures. Quote ids are
# opaque: testnut issues UUIDs, the real mints issue base64url with _ and -. Never enumerate the
# characters of an identifier you do not define.
#
# The failure surfaced as `quote <id> has no stored amount; pass --amount to complete it`, which
# is why it took two hops to place. That message means the id was NOT FOUND in this home — the
# lookup missed, and the code guesses you need --amount. It has no way to know your id is a
# truncated prefix. Confirmed both ways: `complete_mint_by_id` only reaches that error when the
# quote lookup returns None, and the live leg-1 recovery succeeded with the FULL id and NO
# --amount. An error's suggested remedy is a hypothesis, not a diagnosis.
parse_quote_id() { printf '%s\n' "$1" | command sed -n 's/.*quote_id=\([^ ]*\).*/\1/p' | command head -1; }
# `paid_sats=<n> fee_sats=<n> balance_sats=<n> mint=<url>`
parse_field()   { printf '%s\n' "$1" | command sed -n "s/.*$2=\([0-9]*\).*/\1/p" | command head -1; }

# A mint that auto-funds (i.e. a TEST mint) prints a `minted_sats=...` line where a real mint
# prints a bolt11. Both are non-empty, so a bare emptiness check would happily feed that line into
# `wallet melt`. Check the shape, so the failure names the real cause.
is_bolt11() { case "${1:-}" in ln[bt]c*) return 0 ;; *) return 1 ;; esac; }

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
    #
    # ★ BOTH id shapes, because the first version of this fixture used only the UUID and that is
    # exactly why the truncation shipped. The UUID came from testnut — the mint the dry run CAN
    # reach — so the fixture inherited the id format of the reachable world while standing in for
    # the unreachable one. The real mints issue base64url. A fixture for a format you cannot
    # observe must be drawn from that format, not from the one in front of you.
    local uuid='019fa5cd-b27c-7803-b89e-51d8f45c3c05'
    local needs="status=needs_payment amount_sats=21 mint=https://b.test quote_id=${uuid} (pay the invoice, then \`maxplayer wallet mint-complete ${uuid}\`)"
    check "quote id, uuid form"  "$uuid" "$(parse_quote_id "$needs")"
    check "amount from needs_payment" "21" "$(parse_field "$needs" amount_sats)"

    # The id that actually broke it live, verbatim: underscore AND hyphen.
    local b64='lcLc0JwHHCIG_UIwQ8cvPB-LXgiVfSxE6ZO6Tq1b'
    local needs64="status=needs_payment amount_sats=5 mint=https://b.test quote_id=${b64} (pay the invoice, then \`maxplayer wallet mint-complete ${b64}\`)"
    check "quote id, base64url form" "$b64" "$(parse_quote_id "$needs64")"
    # Pin the exact regression: an allow-list class truncates here, and a truncated id is a
    # PREFIX of the real one — it looks like an id, which is why it failed downstream, not here.
    check "id is not truncated at _" "$b64" "$(parse_quote_id "quote_id=${b64} (trailing)")"

    local melt='paid_sats=21 fee_sats=3 balance_sats=26 mint=https://a.test'
    check "paid_sats from melt" "21" "$(parse_field "$melt" paid_sats)"
    check "fee_sats from melt"  "3"  "$(parse_field "$melt" fee_sats)"

    # A zero fee must read as "0", never as empty — the fee-fit verdict does arithmetic on it.
    check "zero fee is 0 not empty" "0" "$(parse_field 'paid_sats=5 fee_sats=0 balance_sats=1 mint=x' fee_sats)"

    # The auto-fund line a TEST mint emits where a real mint emits a bolt11. It is non-empty, so
    # only a shape check keeps it out of `wallet melt`.
    is_bolt11 "lnbc20n1p4x0cksdqqpp5yvc5c6" && check "real bolt11 accepted" "y" "y" || check "real bolt11 accepted" "y" "n"
    is_bolt11 "minted_sats=5 balance_sats=5 mint=https://testnut.cashudevkit.org" \
        && check "auto-fund line rejected" "y" "n" || check "auto-fund line rejected" "y" "y"
    is_bolt11 "" && check "empty rejected" "y" "n" || check "empty rejected" "y" "y"

    [ "$fail" -eq 0 ] || die "parser self-test FAILED"
    say "parser self-test passed"
}

# ── Home discipline ─────────────────────────────────────────────────────────────────────────────
# Create the home, or reuse one THIS tooling marked. Funding and probing are separate invocations
# against the same wallet — the operator pays a real invoice out of band between them — so reuse
# has to be allowed. What must never be allowed is touching a directory we did not mark, and that
# is what the marker enforces; reuse of our own marked home is safe.
ensure_home() { # <path>
    [ -n "$1" ] || die "ensure_home: empty path"
    if [ -e "$1" ]; then
        [ -f "$1/$MARKER" ] || die "refusing to reuse $1 as a throwaway home: it exists but carries
no $MARKER, so this tooling did not create it."
        say "  reusing throwaway home $1"
        return
    fi
    mkdir -p "$1"
    : > "$1/$MARKER"
    say "  created throwaway home $1"
}

assert_throwaway() { # <path>
    [ -n "${1:-}" ] || die "MAXPLAYER_HOME is empty — refusing (an unset home falls back to ~/.maxplayer)"
    [ -d "$1" ]     || die "not a directory: $1"
    [ -f "$1/$MARKER" ] || die "refusing to touch $1: no $MARKER. This run did not create it, so it
may be a real funded home. Every maxplayer invocation is gated on this marker precisely because an
unset or wrong MAXPLAYER_HOME resolves to ~/.maxplayer, which holds a real key and wallet."
    case "$(cd "$1" && pwd -P)" in
        "$HOME/.maxplayer"|"$HOME/.maxplayer/"*) die "refusing: $1 resolves into ~/.maxplayer" ;;
    esac
}

# The ONLY way this script invokes maxplayer. Asserts the marker, then exports MAXPLAYER_HOME explicitly so
# the fallback path is never reachable even if a caller forgets.
maxplayer_at() { # <home> <args...>
    local home="$1"; shift
    assert_throwaway "$home"
    MAXPLAYER_HOME="$home" "$MAXPLAYER" "$@" --home "$home"
}
# Same, for the non-wallet commands that take no --home flag and read MAXPLAYER_HOME from the env.
maxplayer_env() { # <home> <args...>
    local home="$1"; shift
    assert_throwaway "$home"
    MAXPLAYER_HOME="$home" "$MAXPLAYER" "$@"
}

# ── Authorization ───────────────────────────────────────────────────────────────────────────────
# The token names the TOTAL FUNDED EXPOSURE, not each step's amount. One authorization covers the
# whole envelope (fund -> probe), which is the shape the spend was actually approved in: a single
# human decision about how many real sats are at risk, not a per-command negotiation.
require_auth() {
    for var in SOURCE_MINT TARGET_MINT; do
        [ -n "${!var}" ] || die "$var is not set — fill in the authorized values first"
    done
    [ "$SOURCE_MINT" != "$TARGET_MINT" ] \
        || die "source and target mints are identical; that is a direct payment, not a hop"

    local expected="${FUND_SATS}:${SOURCE_MINT}:${TARGET_MINT}"
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
# booted with MAXPLAYER_ALLOW_REAL_MINTS=true still has `allow_real_mints = false` in its config.toml.
# So the real-mint fence is process-scoped and evaporates on exit. There is no fence to restore and
# no EXIT trap that could fail to restore it.
#
# These are EXPORTED rather than prefixed onto each call: maxplayer_at is a shell function, and `env
# VAR=x maxplayer_at ...` cannot work — env execs a binary and would never see the function. Exporting
# also means the fence and the cap apply to every invocation in the stage, not just the ones a
# caller remembered to prefix.
arm_real_mints() {
    export MAXPLAYER_ALLOW_REAL_MINTS=true
    export MAXPLAYER_ACCEPTED_MINTS="$SOURCE_MINT"
    export MAXPLAYER_EXTRA_MINTS="$TARGET_MINT"
    # The exposure cap as a RUNTIME gate rather than a matter of discipline: the budget gate
    # charges the hop's full cost (delivery + fee reserve + input fee), so this bounds real
    # exposure independently of anything this script asserts afterwards.
    export MAXPLAYER_TOTAL_BUDGET_SATS="$EXPOSURE_CAP_SATS"
}

# ── Funding ─────────────────────────────────────────────────────────────────────────────────────
# Two steps, because paying the invoice happens out of band and by a human. A real (non-test) mint
# returns a bolt11 rather than auto-funding, so this is also the first place the live
# `status=needs_payment` branch is exercised — the branch no test mint can produce.
fund() {
    require_auth
    arm_real_mints
    local home="$RUN_DIR/probe"
    rule "FUND — raise a ${FUND_SATS} sat mint quote at the SOURCE (${SOURCE_MINT})"
    ensure_home "$home"

    local invoice quote_out quote_id
    invoice=$(maxplayer_at "$home" wallet mint "$FUND_SATS" --mint "$SOURCE_MINT" 2>"$RUN_DIR/fund.err") \
        || die "mint quote at the source failed: $(command cat "$RUN_DIR/fund.err" 2>/dev/null)"
    quote_out=$(command cat "$RUN_DIR/fund.err" 2>/dev/null || true)
    say "$quote_out"
    quote_id=$(parse_quote_id "$quote_out")
    [ -n "$quote_id" ] || die "no quote_id from the source mint: $quote_out"
    is_bolt11 "$invoice" || die "the source did not return a bolt11 (got: ${invoice:0:40}).
If it auto-funded instead, it is a TEST mint and this is not the real-sats path."

    say ""
    rule "PAY THIS INVOICE (${FUND_SATS} sats), then run --fund-complete"
    say "$invoice"
    say ""
    say "  quote_id : $quote_id"
    say "  next     : RUN_DIR=$RUN_DIR ... $0 --fund-complete $quote_id"
    say ""
    say "Nothing is at risk until the invoice is paid. If you stop here, no sats have moved."
}

fund_complete() { # <quote_id>
    require_auth
    arm_real_mints
    local home="$RUN_DIR/probe" quote_id="${1:-}"
    [ -n "$quote_id" ] || die "usage: $0 --fund-complete <quote_id>"
    rule "FUND-COMPLETE — issue the ecash at the source"
    ensure_home "$home"
    # --amount here is a CROSS-CHECK, not a fix. When the quote is found, mint-complete refuses if
    # the passed amount differs from the stored one, so this pins that we are completing the quote
    # we raised. It is NOT what cured the live failure — the truncated id was (see parse_quote_id).
    maxplayer_at "$home" wallet mint-complete "$quote_id" --amount "$FUND_SATS" --mint "$SOURCE_MINT" 2>&1 \
        || die "mint-complete failed for quote ${quote_id}. If the invoice IS paid, the sats are at
the mint and recoverable — re-run this exact command. Do not re-pay the invoice."
    rule "balance at the source"
    maxplayer_at "$home" wallet balance 2>&1
    say ""
    say "Funded. Next: $0 --stage-a"
}

# ── Stage A: routing + fee probe ────────────────────────────────────────────────────────────────
stage_a() {
    require_auth
    arm_real_mints
    local home="$RUN_DIR/probe"
    rule "STAGE A — routing + fee probe: ${PROBE_SATS} sats ${SOURCE_MINT} -> ${TARGET_MINT}"
    ensure_home "$home"

    # One wallet holds BOTH mints: source as default, target as extra. Verified: `wallet mints
    # list` reports role=default and role=extra respectively under the two env vars armed above.

    rule "balances before"
    local before; before=$(maxplayer_at "$home" wallet balance 2>&1) || die "balance failed: $before"
    say "$before"
    local src_before tgt_before
    src_before=$(parse_balance_for_mint "$before" "$SOURCE_MINT"); src_before="${src_before:-0}"
    tgt_before=$(parse_balance_for_mint "$before" "$TARGET_MINT"); tgt_before="${tgt_before:-0}"
    [ "$src_before" -gt "$PROBE_SATS" ] \
        || die "buyer holds ${src_before} sats at ${SOURCE_MINT}; the probe needs more than ${PROBE_SATS}"

    # 1. Raise a mint quote at the TARGET — a real bolt11 the source must pay.
    rule "1/3 mint quote at the target"
    local quote_out invoice quote_id
    invoice=$(maxplayer_at "$home" wallet mint "$PROBE_SATS" --mint "$TARGET_MINT" 2>"$RUN_DIR/quote.err") \
        || die "mint quote at target failed: $(command cat "$RUN_DIR/quote.err" 2>/dev/null)"
    quote_out=$(command cat "$RUN_DIR/quote.err" 2>/dev/null || true)
    say "$quote_out"
    quote_id=$(parse_quote_id "$quote_out")
    [ -n "$quote_id" ] || die "no quote_id in the target's response — cannot complete the hop:
$quote_out"
    is_bolt11 "$invoice" || die "the target did not return a bolt11 (got: ${invoice:0:40}).
If it auto-funded instead, the target is a TEST mint and this probe proves nothing about routing."
    say "  quote_id=$quote_id  invoice=${invoice:0:32}..."

    # 2. Melt at the SOURCE to pay it. THIS is the leg no hermetic test can reach.
    rule "2/3 melt at the source to pay that invoice"
    local melt_out melt_rc=0
    melt_out=$(maxplayer_at "$home" wallet melt "$invoice" --mint "$SOURCE_MINT" 2>&1) || melt_rc=$?
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
    # As in fund(): a cross-check that refuses a mismatched completion, not the cure for the
    # truncation bug.
    maxplayer_at "$home" wallet mint-complete "$quote_id" --amount "$PROBE_SATS" --mint "$TARGET_MINT" 2>&1 \
        || die "THE STRAND: the melt at ${SOURCE_MINT} PAID (${paid} sats, fee ${fee}) but issuing at
${TARGET_MINT} failed for quote ${quote_id}. The money left the source and is not yet ecash at the
target. It is RECOVERABLE and no sats are lost — re-run exactly:

  MAXPLAYER_HOME=${home} MAXPLAYER_ALLOW_REAL_MINTS=true MAXPLAYER_ACCEPTED_MINTS=${SOURCE_MINT} \\
  MAXPLAYER_EXTRA_MINTS=${TARGET_MINT} ${MAXPLAYER} wallet mint-complete ${quote_id} \\
  --amount ${PROBE_SATS} --mint ${TARGET_MINT} --home ${home}

Copy the quote id from THIS line, not from any earlier output. Report before retrying."

    rule "balances after"
    local after; after=$(maxplayer_at "$home" wallet balance 2>&1)
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
    require_auth
    die "STAGE B IS NOT WIRED YET — refusing rather than pretending.

Stage A is complete and proven end to end. Stage B needs a two-sided trade rig that this script
does not yet contain, and shipping an unrun Stage B is exactly the defect this rewrite exists to
remove: the previous version of this file invoked eleven commands that do not exist, and every one
of them returned rc=1 against the real binary.

What Stage B requires, all verified against the merged tree:
  - buyer:  post_job over the daemon socket (\$MAXPLAYER_HOME/buyer.sock, newline-delimited JSON;
            methods status|post_job|get_job|award|collect). There is NO 'maxplayer job' CLI.
            'maxplayer collect <job_id>' auto-spawns the daemon, so no manual daemon step is needed.
  - seller: a throwaway seller home with MAXPLAYER_SELLER__AGENT_COMMAND / __RATE_SATS / __GIT_REMOTE,
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
    ensure_home "$home"

    rule "home discipline refuses an unmarked directory"
    local unmarked="$RUN_DIR/unmarked"; mkdir -p "$unmarked"
    if ( maxplayer_at "$unmarked" wallet balance >/dev/null 2>&1 ); then
        die "GATE BROKEN: an unmarked directory was accepted"
    fi
    say "  refused an unmarked home"
    if ( assert_throwaway "" 2>/dev/null ); then die "GATE BROKEN: empty home accepted"; fi
    say "  refused an empty MAXPLAYER_HOME (the ~/.maxplayer fallback path)"

    rule "fund on testnut"
    maxplayer_at "$home" wallet mint 5 2>&1 | command head -2

    rule "balance parses"
    local bal; bal=$(maxplayer_at "$home" wallet balance 2>&1); say "$bal"
    local got; got=$(parse_balance_for_mint "$bal" "https://testnut.cashudevkit.org")
    [ -n "$got" ] && [ "$got" -ge 5 ] || die "expected >=5 sats parsed from the balance, got '${got:-}'"
    say "  parsed balance_sats=$got"

    rule "a FAILED melt halts the script (the safety-critical behaviour)"
    local inv rc=0
    inv=$(maxplayer_at "$home" wallet invoice 2 2>/dev/null) || die "invoice failed"
    [ -n "$inv" ] || die "no bolt11 from wallet invoice"
    say "  got a real bolt11: ${inv:0:32}..."
    maxplayer_at "$home" wallet melt "$inv" >/dev/null 2>&1 || rc=$?
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
        --dry-run|--fund|--fund-complete|--stage-a|--stage-b) ;;
        *) die "usage: $0 --self-test | --dry-run | --fund | --fund-complete <quote_id> | --stage-a | --stage-b" ;;
    esac

    [ -x "$MAXPLAYER" ] || die "no maxplayer binary at $MAXPLAYER (set MAXPLAYER=...)"
    self_test   # parsers are proven before anything else runs, on every mode

    mkdir -p "$RUN_DIR"
    say "run dir: $RUN_DIR"

    case "$mode" in
        --dry-run)        dry_run ;;
        --fund)           fund ;;
        --fund-complete)  fund_complete "${2:-}" ;;
        --stage-a)        stage_a ;;
        --stage-b)        stage_b ;;
    esac
}

main "$@"
