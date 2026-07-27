# Seller recovery: order the resubscribe after NIP-42, and give the open-pool re-arm an owned schedule

Closes the client half of #189 and #190.

Three behavioural changes. The first two are the charter's; the third is a defect found while
instrumenting the first, and it is called out separately below because it rides a two-fix PR.

---

## 1. #189 — the recovery path let p-gated REQs out before NIP-42

**Mechanism, read out of the SDK source rather than inferred.** `RelayInner::post_connection` calls
`resubscribe()` as its first act on every socket-up (`relay/inner.rs:748-752`), before that
connection has any NIP-42 state; auth happens later, in the ingester (`:936`), which then
resubscribes a second time (`:941`). mobee-relay evaluates its p-gate against the empty authed
pubkey of that unauthenticated session and answers `restricted:` — the permanent prefix — where the
truth is the retryable `auth-required:`.

That misclassification is not cosmetic, and the SDK's CLOSED taxonomy is why:

| relay says | nostr-sdk does | consequence |
|---|---|---|
| `auth-required:` | `MarkAsClosed` (`:1023-1027`) | stays in the registry; the post-auth `resubscribe()` at `:941` **restores it automatically** |
| `restricted:` | `Remove` (`:1028`) | **deleted**; `:941` cannot see it and never restores it |
| *(empty reason)* | `Remove` (`:1029-1034`) | same permanent deletion |

So carrying subscription registrations across a reconnect killed the kind-1059 money leg on every
recovery. (The relay half is owned elsewhere and is being patched independently — the
misclassification lives in the fork's open-read path, where a pending-auth connection collapses to
an anonymous empty pubkey before the p-gate runs. No relay code is touched here, and this fix does
not depend on that one landing.) **The fix is ordering:** the registrations are now dropped *before* the new socket comes
up, so that first resubscribe has nothing to send and the REQs go out after auth — the order boot
has always had. A failed reconnect re-registers them, because the SDK's own background reconnect is
a real recovery path in the field (the run loop distinguishes it in the `RESTORED` line) and can
only restore what it still knows about.

**Plus the belt, which is not redundancy.** `RelayOptions::reconnect(true)` means the SDK also
reconnects on its own, entirely inside the SDK, with no hook — and that path will always resubscribe
pre-auth. So: a `restricted:` CLOSED of a subscription whose filters *all* pin `#p` to our own
pubkey, on a session that has authenticated, is re-issued once per authenticated session.

**The CLOSED-prefix taxonomy is not softened.** `restricted:` stays permanent-class. A genuine
wrong-`#p` refusal cannot reach the branch — we author these filters from our own pubkey, so the
only way the relay can refuse one is by having no authenticated pubkey to compare against. A
subscription carrying any un-pinned filter is excluded, because there the refusal may genuinely be
about the un-pinned half. A second refusal falls through to the paced recovery rather than looping.
`subscription_pins_only_our_pubkey` carries the argument in a doc comment.

NIP-42 state is tracked from a new `relay.notifications()` arm, because `Authenticated` never
becomes a pool notification (`relay/inner.rs:418` maps it to `None`) — the pool stream the run loop
already watches structurally cannot see it. Both stale readings of that flag are bounded and safe:
stale-false only declines a cheap retry and falls through to the paced recovery; stale-true spends
the one retry the session allows and then does the same.

## 2. #190 — the open-pool re-arm waited on a trigger nothing guarantees

The degrade re-armed only via `open_pool_degraded = false` in the recovery-success arm
(`run.rs:859`), so a seat that degrades and then stays healthy has no path back.

The re-arm now rides the **wrap-backfill tick**. Not the heartbeat: the heartbeat is disableable by
config, and a repair must not depend on a tick that may never fire; the backfill tick is
unconditional. Acceptance is the relay's **EOSE on the offer subscription** — a response NIP-01 owes
us — never the fact that our send succeeded, because a REQ that left the socket proves nothing about
whether the relay took it. A refusal doubles a capped backoff, so a permanently refusing relay costs
one REQ per cap interval rather than one per tick, and an attempt that draws *no* verdict at all is
treated as a refusal so there is no timer-less park.

`run.rs:859` stays — a full resubscribe genuinely does restore the grouped REQ. It was only ever the
*sole* path; this adds an owned one alongside it.

**Scope, stated honestly.** This half is defence in depth, not a repair for an observed seat. The
reported stuck specimen was withdrawn: every seat seen degraded in the field was flapping on the
#189 sawtooth, not stuck. The quiet-seat case follows from `run.rs:859` but nobody has observed it.
The gap is structural, and it survives the #189 fix — which is why the owned schedule stays.

## 3. An unknown-id CLOSED no longer forces a recovery

Field seats open every cycle with a CLOSED naming a subscription id the client never registered. It
was escalated to a full recovery, and the recovery then re-closed the 1059 leg: **escalating an
unknown-id CLOSED cost a reconnect per cycle on a socket that was never broken.** A subscription id
we never registered cannot be a leg of ours going deaf, so it is now logged and nothing else.
Genuine deafness stays covered by the EOSE liveness probe, which is unchanged.

**This is only safe because §2 exists, and they must land together.** Rocky's field data shows the
unknown-close-triggered recovery was also what re-armed the open-pool half — by accident. Removing
the escalation removes that accidental rescue, and the owned re-arm in §2 is what covers it now.
The no-reconnect tooth is what proves the replacement works without the reconnect.

**A candidate for what the unknown id is — a lead, not a claim.** Our own periodic wrap backfill
calls `client.fetch_events`, which *generates* its subscription id (`pool/mod.rs:815`) and runs on
exactly the 300s cadence these closes appear on. The relay owner has since enumerated every periodic
mechanism on the relay side and none fits, which places the source outside the relay — client-side
or a fronting proxy's idle timeout — and makes our own transient REQ the leading candidate rather
than merely a plausible one. It is still not asserted here. The new log line carries the age of the
last backfill *and* the age of the last successful NIP-42 auth, and is the instrument that settles
it: a small backfill age implicates our own REQ, and the auth age bounds anything session-scoped.

---

## The fixture, and a trap worth naming

The teeth needed a relay this repo did not have. `nostr-relay-builder`'s NIP-42 read gate answers an
unauthenticated REQ with **`auth-required:`** (`local/inner.rs:961-989`) — which nostr-sdk keeps and
restores by itself. **Against that fixture every ordering passes and the tooth is decorative.** The
next person to test anything in this area will hit the same wall.

`crates/mobee-core/src/seller_node/p_gate_relay_fixture.rs` speaks the deployed rule directly — a
`#p` filter is refused `restricted:` unless the session is authenticated as that very pubkey — which
covers both cases the teeth must tell apart: the pre-auth race (right `#p`, no auth yet) and a
genuine violation (someone else's `#p`, fully authed). It records every REQ with the session's auth
state *at arrival*, and counts sockets, so "no reconnect was required" is an observable rather than
an inference.

## Teeth

Nine new tests, all against the real paths.

| tooth | what it pins |
|---|---|
| `recovery_puts_no_p_gated_req_on_the_wire_before_nip42_completes` | #189(a): AUTH held 400ms past the socket; all four subscriptions end live, zero permanent removals |
| `a_genuine_wrong_p_restricted_stays_removed` | #189(b): the taxonomy does not soften — wrong-`#p` is refused, deleted, never retried |
| `wraps_subscription_survives_ten_consecutive_reconnects` | #189(c): the money leg survives repeated recoveries, not just the first |
| `open_pool_rearms_on_an_owned_tick_without_any_reconnect` | #190(a)+(b): re-arm within one owned tick, targeted half never disturbed |
| `repeated_open_pool_rejection_backs_off_and_never_hot_loops` | #190(c): ≤5 attempts over ~12 owned ticks against a relay refusing every one |
| `open_pool_rearm_backoff_doubles_and_stays_capped` | the backoff arithmetic; never zero after a refusal |
| `a_rearm_attempt_with_no_verdict_is_treated_as_a_refusal` | silence advances the backoff instead of parking |
| `an_unknown_id_closed_costs_no_reconnect_and_no_resubscribe` | §3: inert about the close, not inert about liveness |
| `the_unknown_close_diagnostic_carries_both_ages_and_the_auth_state` | the field-facing line keeps what the relay owner needs |

### Red-on-revert, strong form, `rc=101` each

```
1(a)  move clear_subscription_registrations back after reconnect_and_authenticate
      panicked: a p-gated REQ reached the relay before NIP-42 completed — that is #189:
      [ReqRecord { subscription_id: "mobee-awards", authenticated: false, p_pinned: true,
        verdict: Closed("restricted: p-gated events require #p matching your pubkey") }, ...]

2(a)  disable the open_pool block in the wrap-backfill arm (the hookup only)
      panicked: the open-pool half was never re-armed: a healthy seat that degrades has no
      recovery to wait for, which is #190

3     drop the !is_our_subscription early return
      panicked: a CLOSED for an id we never registered forced a reconnect — that is a reconnect
      per cycle on a socket that was never broken
```

**One of these teeth did not bite on the first try, and the fix is worth recording.** The unknown-id
tooth originally slept 4s and then asserted no reconnect had happened. A reconnect against this
fixture takes ~6s, so under revert the recovery was still in flight and the tooth passed — a
decorative tooth. It now waits for the socket count to move with a 20s ceiling, returning early on
the red path. Any "X did not happen" assertion has to outlast the time X takes to happen.

## Gates

- `cargo test --workspace` — **rc=0, 607 tests** (555 in the `mobee-core` suite). Baseline on the
  untouched tip: rc=0 / **598 tests**, run on a frozen checkout of `origin/dev`. The delta is exactly
  the nine teeth above.
- **Two of those teeth failed their first full-suite run, and both were the test's fault.** The
  ten-reconnect tooth asserted "zero pre-auth p-gated REQs across all ten cycles" — which contradicts
  what this fix claims, since the SDK's background reconnect and the deliberate re-register on a
  failed recovery both put pre-auth REQs on the wire by design. That assertion was asserting the fix
  away; it is gone, the per-cycle claim stands, and the bite was re-verified afterwards. The same
  tooth also raced its own boot REQ against the first clear. Recorded because a module-filtered green
  is not the same evidence as a full-suite-parallel green — both passed the former.
- The #169-arc teeth in this region all re-ran green and are unmodified by this diff:
  `liveness_probe_answers_only_on_an_authenticated_session`,
  `reconnect_reauthenticates_and_delivery_resumes_in_process`,
  `wrap_backfill_cursor_clamps_to_the_oldest_unsettled_delivery_and_fails_closed`.
- **Per-file `rustfmt`, with the whole-tree skew measured rather than asserted.** Under this
  toolchain the *untouched* `run.rs` from `origin/dev` already produces **83** `rustfmt --edition
  2024 --check` diffs, almost all import-ordering from the 2024 style edition. The set-difference of
  normalised diff hunks between the pristine file and mine is **empty** — this diff contributes zero
  formatting deviations. `p_gate_relay_fixture.rs` is `rc=0` outright, being new. CI runs neither
  `fmt` nor `clippy` (`.github/workflows/ci.yml` is build+test), and whole-tree formatting is a
  fleet-level call, not this slice's. Precedent: the crossmint slice's `PR-BODY.md`.
- **`clippy`** (`--no-default-features --features gateway,git-delivery,wallet --all-targets -D
  warnings`): 57 pre-existing errors across the workspace. Two name `run.rs` — `:2377` and `:2447` —
  both in test code this diff does not touch. Zero findings attributable to this change.

## Field validation

Pre-registered by the rocky fleet *before* the fix, which is the right order.

**The contract is the invariant set, not cumulative counts.** Cumulative totals grow with uptime and
say nothing across a rebuild, so the BEFORE state is characterised by the shape of each cycle,
scoped per-run (the after-script asserts `boots=1`):

| invariant (BEFORE, flapping) | expected AFTER |
|---|---|
| degrades == recoveries, 1:1 | **`deg = 0` across ≥3 consecutive 300s windows (~15 min) per seat** |
| inter-event interval **exactly 300s** | no periodic degrade/recovery cycle |
| `attempts=1`, outage ~10s per cycle | recoveries 0, or genuine and non-periodic |
| offer state **TARGETED-ONLY** | state **FULL** — corroborating only, see below |

That 300s regularity is external confirmation of the `run.rs:859` mechanism: the heartbeat tick
servicing a queued `forced_recovery` is what paces the flap.

**`state` is the weak signal; `deg` over a window is the strong one.** On the known-bad build a seat
sampled `FULL` — caught inside the few-second armed window between the resubscribe and the next
close. A point sample can therefore read `FULL` against a 100% degrade rate. `deg` counted over a
window cannot be caught mid-cycle, which is why the pass criterion hangs on it and not on `state`.

**The pass signal is a zero, so the parser needs a positive control.** `deg = 0` is
indistinguishable from a regex that matches nothing, so the after-script requires a
must-be-present line (`wrap backfill`, the node's unconditional periodic line) to parse non-zero
before it will read any zero as meaningful — measured controls 41/35/43 on the current fleet. This
is the denominator: a checker that only reports failures cannot tell "nothing broken" from "nothing
examined". The binary-literal guard in step 0 protects the *vocabulary*; this protects the *parsing*.
Both are needed, and neither substitutes for the other.

### Step 0 — prove the lines can still appear, before reading their absence

The success signal is the *absence* of `RELAY-CLOSED` lines, and an absence is only evidence if the
line could still have been emitted. The after-script therefore checks the literals in the shipped
binary first and aborts if any is missing. **Grep the stable PREFIX, not the full sentence**, and
here is why that is not pedantry:

```
$ strings <binary> | grep -cF 'seller node RELAY-CLOSED DEGRADE:'          # 1  — anchor present
$ strings <binary> | grep -cF 're-armed on the next successful recovery'   # 0  — GONE, on purpose
```

The `DEGRADE` line's trailing parenthetical **changed by design** — #190 requires it to state the
real re-arm schedule instead of the old "re-armed on the next successful recovery". A step 0
matching that old sentence aborts on a correct build. The prefix is the contract; the tail is not.

For the same reason, expected literal counts must be calibrated against a binary, not remembered.
Measured on this branch (`cargo build -p mobee --features wallet`, `strings | grep -cF`):

```
1  seller node RELAY-CLOSED:              1  seller node RELAY-CLOSED DEGRADE:
1  seller node RELAY-CLOSED degrade failed
1  RELAY-RECOVERY triggered               1  recovery SUCCEEDED
1  RESTORED via MANUAL                    2  wrap backfill (periodic)
```

Note these are counts of `strings` lines, and `eprintln!` splits a message at each `{}` placeholder —
so a count is a property of where the placeholders fall, not of how many call sites exist. If the
pre-registered expectations were taken from a differently-built binary they will not match this one,
and the mismatch is not a rename.

New literals this PR adds, all following the existing shape, none replacing anything:
`RELAY-CLOSED RETRY:`, `RELAY-CLOSED RE-ARM`, `RELAY-CLOSED RE-ARMED:`, `RELAY-CLOSED UNKNOWN-ID:`.
