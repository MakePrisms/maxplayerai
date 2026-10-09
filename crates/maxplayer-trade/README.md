# maxplayer-trade — standalone real-money Cashu trades

A fixed-lot CLI, independent of jobs and the maxplayer daemon. Market traffic never uses
`relay.maxplayer.ai`; `nostr://` mint traffic (kinds 23410/23411) may, by Bob's decision of 2026-10-09.
**A live 32-for-24 trade completed on 2026-10-08**, including both claims, NUT-07 witness
recovery, fee-inclusive net delivery, and a signed `sold` status. Real-money trading is a normal supported use; this is not a production safety certification
or an unconditional atomicity guarantee.

Current operating policy is in [Round 5](#round-5-final-review-policy).
Earlier dated verification sections are historical snapshots, not current coverage totals.

## Round 5 final-review policy

- Sender-funded claim fees must equal the pinned quote, not the receiver’s outgoing budget.
  Incoming fees exceeding the admission cap are refused before a lock is accepted.
- CDK wallet calls have 15-second budgets; message handling has 60 seconds. Own-mint
  refunds run before counterparty claims. Missing mint info time does not block refunds;
  mint-enforced locktime remains authoritative. Known-preimage claims survive refund quarantine.
- Withdrawal reserve: max(32 sats, ceil(invoice sats × 2%)); 100,000 sats permits 2,000.
  `--max-debit` enforces invoice + input fee + reserve. Invalid/lost/refused quotes, and
  never-submitted quotes that expired, are terminal `refused`, retryable explicitly with the same invoice. No passive recovery submits an
  unsent quote; an explicitly failed pre-POST withdrawal cannot pay later on a serve tick.
  Submitted ambiguity retains exact authorization and replays it until definitive.
- Read-only `status` and `balance` use SQLite snapshots without the writer lock. Keep
  serve alive from second lock through settlement. Exits: 0 success, 1 command error,
  pre-submission refusal, or definitive terminal `unpaid_released`, 2 usage, 3 unresolved work, 4 terminal manual recovery. Non-final
  withdrawals never exit 0; a submitted (past `quote_created`) non-terminal withdrawal always
  exits 3, even when the mint fails after the POST. Exit 1 is never proof that a submitted
  withdrawal was refused: check `status` before paying the obligation another way.
- Funding preflight adds enabled NUT-04/20; withdrawal adds NUT-05; trading adds NUT-11.
  Paid funding issuance retains keyset/DLEQ verification without new admission gates.
  Expired fresh-UNPAID funding becomes terminal after 60 seconds grace, still charged
  against the cap and explicitly resumable for late payment.
- Relay exhaustion is `publication_abandoned`, separate from unresolved money. Retry
  scans prune completed/expired entries while retaining signed receipts. Inbox peer bursts
  are limited without a shared 256-message drop queue.

Verification is fake-mint-only, author-led; final counts, mutation results, detached
serve refund and current-head CI links are in the PR’s round-five section. Historical
sections below describe their dated runs, not current coverage totals.

## Implemented

- `list`, `discover`, `cancel`, `serve`, `take`, `recover`, `preflight`, `fund`, `balance`, `status`, `withdraw`.
- Immutable signed 3410 lots (24 hours), hash-chained 3411 statuses, signature/content/tag
  validation, fork/gap quarantine, no terminal reopening. Every configured relay is queried;
  discovery unions their results. No EOSE is an error, not an empty market; partial relay
  failures are reported separately. Every publication is journaled and attempted on eligible configured relays;
  positive receipts, cooldowns and bounded retry budgets survive restart.
- NIP-44 v2 encrypted 23412 negotiation, NIP-42 authentication, raw relay-message reception
  (retries are not suppressed by SDK event dedup), persisted signed outbox and request digests.
  Conflicting request IDs are rejected; quote rejection outcomes and completed swaps are retained.
- Quote binds immutable price/assets, net/gross/debits/fees/keysets, both identities, unique
  per-swap receive/refund keys, hash, and deadlines. Hold: 60 seconds; one open quote per taker,
  four per maker. `--max-give` caps total outgoing debit; `--min-receive` caps net receipt;
  `--max-fees` defaults to 16 in the relevant asset. No platform/trade fees.
- Taker generates the secret and locks **first**, for 60 minutes. Maker verifies the first
  lock before locking **second**, for 15 minutes. Both parties preflight both mints (reachable
  advertised NUT-07/09/11/12/14, reported clock within 60 seconds), keysets and fees before locking. Each received
  lock must have exact hash/keys/thresholds/SIG_INPUTS/deadline, unique proofs, DLEQ, exact
  net-after-input-fee value, and fresh UNSPENT state. Asset identity is **mint URL + unit**;
  both assets can be `sat`. Taker refuses to initiate a claim with less than three minutes left.
- Maker settlement reads the preimage from NUT-07 witnesses on **its own outgoing proofs**;
  the taker's encrypted notice supplements this. Without the notice, maker safety
  relies on the chosen mint reporting the HTLC witness. Mint choice is the buyer’s/seller’s
  responsibility; there is no witness-verification/rating gate. Maker publishes sold after claiming
  the incoming leg. Locked proofs are never counted as ordinary wallet balance.
- Private SQLite journal using CDK's KV API; exact inputs, blinded outputs, secrets/blinding
  factors, signed witnesses, results and send deadlines are persisted before mint effects.
  Recovery restores the same outputs and never creates a replacement swap on a timeout alone.
  Concrete proofs are reserved in the CDK wallet, not just subtracted in an order-book counter.
  A home-wide process lock plus durable reservation intents cover interrupted row-by-row CDK
  reservations. Partial/missing restore evidence remains held, not guessed safe.
- Refunds are real signed swaps with an explicit HTLC witness `preimage: ""`, using the refund
  key after strict locktime plus clock margin. Maker recovery polls every three seconds and
  refunds after its short deadline +60 seconds when the mint is reachable. A claim that wins
  the refund race is reconciled through its witness. Refund outputs survive a process exit.
- `recover` makes one bounded pass over existing authorizations; it does not admit new requests or wait for deadlines. Exit **3** and status `recovery_incomplete` mean retained items need a later pass; exit 0 means all money authorizations are terminal, exit 1 is a command error (never proof that a submitted withdrawal was refused). Each funding/withdrawal/swap attempt has a 120-second budget, independent of other items. HTTP requests may already have landed at timeout: the exact journal remains authoritative; no timeout releases funds.
  `serve` continuously accepts quotes and recovers swaps. **Keep a watcher running while funds
  are locked.** Use read-only `status`/`balance` without stopping it; never stop between `second_locked` and `settling`. Do not delete a trade home.

Any canonical HTTPS mint (or a `nostr://<npub>` mint, see below) can be used if automatic preflight passes: advertised NUT-07/09/11/12/14,
an active sat keyset, and clock skew at most 60 seconds. Preflight runs before each `list`,
`take`, funding quotes and lock submission; successful earlier preflight is not cached
as permission for a later lock. Test mints and loopback HTTP(S) fixtures remain supported.
HTTP is refused except on loopback, and redirects are disabled. There is no real-money
opt-in environment variable or allow-list flag. Each lock's gross amount and cumulative
funding per mint/home are capped at **100,000 sats**, including pending/lost funding intents.
Each withdrawal invoice is also capped at **100,000 sats**, plus at most max(32 sats, ceil(invoice amount × 2%)) of
quoted fee reserve, bounded together with input fees by `--max-debit`; spendable funds must cover the invoice, input fees and required change.

**Risks:** a mint can cheat: an issuer-run mint can steal its users' funds. Capability
advertisements are not evidence of honest operation. If the counterparty disappears,
funds remain locked until their deadlines and then require a successful refund transaction;
expiry does not return funds automatically. **Keep `serve` running while funds are locked**,
monitor its errors, and preserve the home. Never trade beyond the user's confirmed limits.
Nutshell/0.21.0 at cashu.cz completed a real timed refund in the dated round-four live test
(two SPENT proofs, empty-preimage HTLC refund witnesses, one signature each); this is not
a guarantee for other deployments or versions.

Default relays: `wss://nos.lol`, `wss://relay.primal.net`, `wss://offchain.pub`; repeat
`--relay URL` before the command to configure alternatives. The production relay is
explicitly forbidden. No production relay writes were made.

### `nostr://` mints (Maxplayer credits sidecar)

A mint may also be `nostr://<npub>`, e.g. a `crates/maxplayer-mint` sidecar. Lots and peer
messages carry exactly `nostr://<lowercase bech32 npub>` (no slash, path, query or fragment);
CLI arguments may use a 64-hex key or uppercase and are canonicalized. All mint traffic goes
through one dispatch (`src/transport.rs`): HTTP(S) mints keep the raw requests used before;
`nostr://` mints use the reviewed core connector (`maxplayer_core::nostr_mint`, one kind-23410
request / 23411 reply per call, identical re-send on a lost reply, request `exp` enforced by
the mint). CDK wallets for such mints are built with that connector and poll subscriptions.

- Relays: repeat `--mint-relay URL` (max 8, `wss://`, `ws://` only on loopback) before the
  command; default `wss://relay.maxplayer.ai`, `wss://relay.ditto.pub`,
  `wss://nostr-pub.wellorder.net` (`transport::default_mint_relays`), the same list and order a
  default credits sidecar listens on. They carry only kinds 23410/23411; market relays never carry
  mint traffic. `relay.maxplayer.ai` is allowed for mint traffic only (Bob, 2026-10-09); the market
  fence still refuses it for 3410/3411/23412 (`--relay`). Both sides must reach a relay
  the mint listens on; preflight fails (nothing locked) otherwise.
- Timing: a `nostr://` CDK wallet call is bounded at 40 s (30 s connector window + 10 s margin)
  instead of 15 s; a raw request is bounded by the connector, not the 20 s HTTP timeout. A
  deadline-bound swap (lock/claim) is sent with `exp <= send_before`, and not at all with under
  2 s left. Abandonment grace is 170 s (window 30 + margin 10 + clock skew 60 + 20) instead of
  60 s. A recovery item touching a `nostr://` leg gets max(120 s, advance deadline 100 s + 40 s).
  A call that outlives its bound is ambiguous: the journaled attempt is restored, never resent
  with new outputs and never treated as failed.
- Size: the sidecar takes at most 128 inputs/outputs and a 65,535-byte NIP-44 plaintext. A
  `nostr://` lock whose HTLC claim (preimage + signature per input) could exceed either is
  refused at admission before anything is locked or revealed; own locks are checked the same way.
- `fund` and `withdraw` refuse `nostr://` mints before touching the home: the sidecar serves no
  NUT-04/05/20. Credits come from the operator's `maxplayer-mint issue` token file.
- The sidecar has no configurable input fee (ppk 0).

## Agent skill

The standalone skill is [skills/maxplayer-trade/SKILL.md](skills/maxplayer-trade/SKILL.md).
It is not installed in the Maxplayer website skill index. The CLI regression
`skill_commands_and_flags_match_binary_help` checks its commands and flags against the built binary.

## Third review recovery policy (base ebc6ef5)

- Unforwardable-lock settlement is bookkeeping, not a prerequisite for refund. Errors
  (including persistently invalid DLEQ or already-spent owned change) are logged and do
  not skip the timed refund branches. The lock is never forwarded.
- `refund_quarantined` and `claim_quarantined` are terminal **manual-recovery** states,
  not success or wallet balance. Exact attempts, blinded outputs, secrets and returned
  proofs stay in the private journal; `recover` prints swap ID, state and
  `manual_recovery: true`. No automatic execution is retried after quarantine.
- Quarantine requires a specific cryptographic DLEQ verification failure on an owned-only
  attempt, a full NUT-09 restore bound to every exact blinded output (unique outputs and
  matching signature amounts/keysets), and every input reported SPENT by NUT-07.
  SPENT alone is insufficient: it could be a competing claim/refund. Empty/partial restore,
  PENDING, key-loading or RPC failures cannot establish this terminal state. Attempts
  without that evidence stay retryable/reconcilable. This is mint-reported commit evidence
  under the existing honest-issuer assumption, not cryptographic proof against a lying mint.
- Invalid outputs are never credited. Preserve the whole home and escalate quarantine to
  a human for verified restoration; do not import unverified proofs, delete the attempt,
  generate replacement outputs or reinterpret a quarantined claim as refund permission.
  An unforwardable lock's invalid change may still require manual recovery even when its
  separately verified refund reaches `refunded`. Terminal does not mean all journaled
  material became spendable. A quarantined maker listing stays held for human resolution.
- **NUT-07 witnesses:** `/v1/info` advertises NUT-07 but does not promise witness emission.
  There is no stateless cheap probe: `checkstate` on an unknown or UNSPENT Y has no spend
  witness. Proving emission needs a known spent HTLC and its expected witness, ordinarily
  requiring funded creation and redemption (and mint fees); preflight has neither a known
  spent Y nor authority to spend. It explicitly reports `nut07_witnesses: unverified...`.
  At refund time a missing/ambiguous SPENT witness fails closed: no taker refund that tick,
  even if the maker actually refunded. Keep recovery running; persistent omission needs
  human investigation. NUT-07 advertisement must never be reported as compatibility proof.
- Residual late-lock window: a mint that holds a delivered POST beyond the 60-second
  abandonment grace and starts it after the final restore/state/release can create a lock
  on an already-expired swap; automatic recovery cannot close that server-side window.

## Build and automated tests (historical round-three counts below)

This crate is its **own Cargo workspace**, with pinned CDK/Cashu 0.17.2 and Nostr SDK 0.44.1.
Do not add it to the root workspace. From the repository root:

```sh
# Tests/examples enable the real CDK mint dev-dependencies; protoc is required.
export PROTOC="$(find /nix/store -maxdepth 3 -path '*/bin/protoc' -print -quit)"
cargo build --manifest-path crates/maxplayer-trade/Cargo.toml --locked --bins --examples
cargo test --manifest-path crates/maxplayer-trade/Cargo.toml --locked -- --test-threads=1
TRADE_LAB_SECONDS=1 cargo test --manifest-path crates/maxplayer-trade/Cargo.toml \
  --locked --features lab -- --test-threads=1
# Rebuild the production executable after the lab build.
cargo build --manifest-path crates/maxplayer-trade/Cargo.toml --locked --bin maxplayer-trade
```

Long runs should be backgrounded once, redirected to a log and awaited through that process;
do not pipe a build through tail/grep or start duplicate watchers. If cargo is unavailable,
wrap the command with `nix develop --extra-experimental-features 'nix-command flakes' --command`.

`lab` is compile-time-only test instrumentation. `TRADE_LAB_SECONDS=1` uses 48/16-second locks,
2-second claim cutoff and 1-second refund margin, **only for 127.0.0.1 mints**. Production builds
ignore these test settings and retain 3600/900/180/60. The original 24/8-second fixture left only six usable claim seconds; under shared-host
load a single maker-lock step took 2.8 seconds and setup consumed the claim window.
Only lab locks are scaled 2×, preserving their 3:1 ratio; the two-second cutoff and
one-second margin are unchanged. Tests use two Tokio workers instead of one per visible
CPU, avoiding per-test oversubscription. Existing deadline and balance assertions remain unchanged.
Crash injection exits the subprocess at
explicit pre-effect or post-mint/pre-wallet-commit boundaries. Public mints never enable lab timings.

| Suite | Default passed/failed | Lab passed/failed |
|---|---:|---:|
| Protocol primitives | 21 / 0 | 21 / 0 |
| CLI privacy, lock, hard fence, skill help | 5 / 0 | 5 / 0 |
| Two-mint integration/recovery | 6 / 0 | 11 / 0 |
| Pinned-CDK orphan-quote reproducer | 1 / 0 | 1 / 0 |
| Relay union/unreachable/production fence | 3 / 0 | 3 / 0 |
| Review adversarial regressions | 12 / 0 | 39 / 0 |

**Final third-pass totals: 48 default / 80 lab passed, zero failures.**
Touched-file rustfmt check and the production-default binary/examples build passed.
Clippy --all-targets --no-deps in both default and lab modes exited 0 with non-blocking style warnings
(collapsible conditionals, unwrap-after-is_some, existing argument-count/import/clone lints).

In earlier rounds, the fee-bearing sell-back fixture recovered after every inbox step, generating a
request/quote replay storm inside the eight-second lab lock. It now mirrors the real CLI's
three-second recovery cadence; no deadline was widened in those rounds. **20/20 earlier repeated lab runs passed**
(each run trades in both directions with exact fee assertions). Command on the compiled lab
e2e binary: TRADE_LAB_SECONDS=1 <lab-e2e> --exact
fee_bearing_trade_and_sell_back_exact_balances --test-threads=1 --nocapture.
The initial H2 early-return mutation hit a fixture unwrap; setup now tolerates the deliberately
injected error, and the rerun fails at the actual refund safety assertion.

Integration tests run two in-process **CDK 0.17.2 FakeWallet mints with distinct mint keys**,
both unit `sat`, SQLite wallets, and a local Nostr relay (same relay-builder used by
`crates/maxplayer-mint/examples/local_relay.rs`). They exercise real mint HTTP operations and
NIP-44 negotiation, not mocked successful swaps:

- list → discover → take → both claims → sold, zero and nonzero `input_fee_ppk`, exact balances;
- sell-back direction with exact fee-inclusive balances;
- maker never locks → taker refund; taker never claims → prompt maker refund;
- maker offline at claim → NUT-07-only preimage recovery;
- subprocess exit after a committed mint swap → journal recovery, no double debit;
- subprocess exit after refund → exact output recovery;
- late claim wins against a prepared refund → maker obtains witness and completes;
- wrong hash/key/deadline/net, forged/missing DLEQ, duplicate proofs rejected;
- interrupted quote-expiry index update recovers without releasing listing backing;
- overlisting and cancelled lots rejected; primitive tests also reject expired/tampered lots;
- lot and status split across two relays still discovered through the union; unreachable is
  distinguished from a real empty EOSE response.

Root CI does not run this independent workspace. These local counts are separate from root CI.

## Rerunnable public-relay trade

Use **fresh homes**. These commands use fake auto-paid testnut quotes; they never pay invoices.
The only public data is signed listing/status metadata and encrypted negotiation events.

```sh
set -eu
umask 077
TRADE="$PWD/crates/maxplayer-trade/target/debug/maxplayer-trade"
RUN="$(mktemp -d /tmp/maxplayer-trade-live.XXXXXX)"
A=https://testnut.cashudevkit.org
B=https://testnut.cashu.space
"$TRADE" --home "$RUN/maker" fund "$A" --amount 128
"$TRADE" --home "$RUN/taker" fund "$B" --amount 128
# Record balances on both mints before trading.
for role in maker taker; do
  "$TRADE" --home "$RUN/$role" balance "$A"
  "$TRADE" --home "$RUN/$role" balance "$B"
done
"$TRADE" --home "$RUN/maker" list --give-mint "$A" --give 32 \
  --want-mint "$B" --want 24 > "$RUN/list.jsonl"
LOT="$(python3 -c 'import json,sys; print([json.loads(l)["lot_id"] for l in sys.stdin if "lot_id" in json.loads(l)][-1])' < "$RUN/list.jsonl")"
"$TRADE" --home "$RUN/maker" serve > "$RUN/maker.log" 2>&1 &
MAKER_PID=$!
"$TRADE" --home "$RUN/taker" discover
"$TRADE" --home "$RUN/taker" take "$LOT" --max-give 40 --min-receive 32
# Only stop the maker after both sides are complete; otherwise keep recovery running.
kill "$MAKER_PID"
wait "$MAKER_PID" || true
for role in maker taker; do
  "$TRADE" --home "$RUN/$role" balance "$A"
  "$TRADE" --home "$RUN/$role" balance "$B"
done
cat "$RUN/maker.log"
# Independent public readback: uses neither the coordinator nor wallet/journal.
cargo run --manifest-path crates/maxplayer-trade/Cargo.toml --locked --example read_trade -- "$LOT"
```

After interruption, use the same home and relays: `maxplayer-trade --home <home> recover`.
`serve` also resumes journaled work. A refund is not available until its deadline; a watcher may
remain running for the full lock duration. An inconclusive mint result is retained for recovery.
To cancel an unused listing: `maxplayer-trade --home <maker-home> cancel <lot-id>`.
Cancellation does not revoke an already authorized HTLC.

## Third-pass verification and fresh live evidence — 2026-10-08

Author-led fixes to paid review #3 at `ebc6ef59f78f41b6f9a31cb8180a6648f7756f36`;
not an independent reviewer sign-off. Only this crate changes. The round-2
`fresh_not_landed`/`claim_not_landed` helpers and taker refund branch were compared
byte-for-byte with that head and are unchanged. Production timings and mint fences
are unchanged. No real-money mint, other worktree, Maxplayer job or merge is involved.

| Review item | Regression/evidence |
|---|---|
| §2.1 persistent invalid lock | `probe_{maker,taker}_persistent_invalid_dleq`: terminal refund quarantine, outputs retained, no credit or repeated execution. |
| §2.1 independently valid refund | `probe_{maker,taker}_invalid_lock_valid_refund`: bad lock restore remains enabled, verified refund reaches refunded and exact balance. |
| §2.1 spent change | `probe_owned_change_spent_does_not_block_refund`: both roles, credited change spent through native CDK swap between ticks; settlement fails but refund succeeds. |
| §2.1 owned claims | `probe_claim_quarantine_and_witness_normalization`, `probe_maker_claim_quarantined`, `probe_unlanded_and_ambiguous_claims_stay_retryable`: both roles, no unverified balance, no refund after claim, rejected swap and restore outage remain retryable. |
| §2.2 | `preflight_reports_witness_emission_unverified`; the claim/witness probe also asserts SPENT without a witness fails closed. No stateless emission probe exists; limitation is explicit above. |
| §2.3 | Residual late-lock window documented above; no deadline-logic change. Existing N2 late-lock regression retained. |
| Witness normalization | Uppercase NUT-07 preimages are injected and observed, then normalized before coordinator storage. |
| Timing | `m1`, `m3`, notice normalization and both zero/positive-fee roundtrips: **5/5 passed** with two CPU burners and tests sharing CPUs 0–3; two Tokio workers, one test thread. |
| Agent skill | Frontmatter validator passed; CLI help test passed. All nine commands and every documented flag separately checked on the exact live default binary. |

**Mutation evidence:** three isolated copies under
`.openclaw/tmp/review-1107-pass3/final-mutations/`, changing only one guard per copy:
restore fatal settlement (`settle-fatal`), disable owned-output quarantine (`no-quarantine`),
and reject SPENT refund witnesses (`n1-spent`). Both Appendix A probes fail at the
"persistent invalid DLEQ must not strand the lock" safety assertion in each of the first
two copies; the earlier N1 regression fails at "N1 maker refund must not strand taker".
**Three mutations, five intended safety-assertion failures**, not build/setup errors.
The source worktree was never mutated.

Checkpoint failures are retained: the initial spent-change test incorrectly used the HTLC
redemption helper for ordinary change (mint correctly rejected it); the corrected fixture
uses native CDK swap. The original eight-second lab claim window also failed under shared
load, then passed alone. The lab-only adjustment above addresses measured fixture latency,
not weakened assertions. Full suites and repeat logs live in `target/third-pass/`.
The repetition runner executes all eight `probe_` tests, the zero-fee full trade/sell-back,
and the fee-bearing full trade/sell-back for twenty serialized rounds; final results are
reported on PR #1107. These checks are separate from root-workspace CI.

### Live default-feature binary

- Fresh homes: `/home/openclaw/.openclaw/workspace/.openclaw/tmp/credit-trade-round3-20261008-c9_08k84/{maker,taker}`; retained, private; old homes untouched.
- Binary SHA-256: `86c3d9be7cf1f0cb20f0df66a15701007ff0ccd1b84fecfc0a4fcb73d8af9586`. The later final CLI-test binary is byte-identical.
- Maker mint: testnut.cashudevkit.org (`cdk-mintd/0.18.0`); taker mint:
  testnut.cashu.space (`cdk-mintd/0.17.0-rc.3`), both 100 ppk.
- Lot: `b04f2990f887dcd5955bffdfd5ffbca21521bb9db7513063c3cd7c08f1b5dad1`.
- Available event: `ff01eb54c13e83e863f0fea0ccb5b72503fc2ea182f200b49d8bc5b53e162847`.
- Sold event: `63a835aab201f1f54e8e1a3de4cd77559aee446e3888db4a4bf97e88bf476eef`.
- Swap: `0e4c96cc-ecb3-4d79-874f-0b8cbd985f6c`; both roles **complete**,
  all four lock/claim attempts done with retained results, zero proof reservations,
  all remaining wallet proofs UNSPENT.

| Home | cashudevkit before → after | cashu.space before → after |
|---|---:|---:|
| Maker | 128 → **93** | 0 → **24** |
| Taker | 0 → **32** | 128 → **101** |

The independent `read_trade` reader verified all three signed public events on
**relay.ditto.pub**, including the contiguous chain ending in sold. Damus returned the lot
and available event but not sold in that read. No republication or retry was needed.
Each mint's combined final balance is **125 = 128 − 3 mint fees**. No real invoices,
platform fees, live refunds or live reverse trade were involved; refund and reverse-trade
coverage is from the actual local CDK mints. Issuer honesty, missing witnesses and the
late-server-processing residual remain explicit limitations, not atomicity guarantees.

## Second fix pass — reviewed base 05b7f03 (2026-10-08)

This is an **author-led fix/verification pass**, not another independent reviewer sign-off.
The paid re-review at `05b7f0334ac69918c6938eff650614f04b3c89a2` identified N1–N4;
all four were accepted; the third review found the remaining N3 settlement blocker described above. Only this standalone crate changes. No mint/NUT changes,
unlocked transfers, validation bypass, new Maxplayer jobs, production rollout, or merge.

Every regression below uses the two real in-process **CDK 0.17.2 FakeWallet mints and local
relay**. The HTTP middleware can hold a real swap in flight beyond its client timeout and
release it later; it never fabricates successful signatures. Late-arrival tests advance the
fixture's reported mint clock past grace while the real request is held. Public mints cannot
use lab timing. Existing assertions are retained; old abandonment waits now follow the
60-second grace constant instead of assuming 20 seconds.

| Finding | Resolution and regression tests | Mutation evidence |
|---|---|---|
| N1 | Fixed: `n1_maker_refund_before_taker_observes_abandonment`; `n1_restore_after_nut07_catches_landing_claim`. Refund-spent inputs no longer strand the taker; a second restore catches the NUT-07 race. Exact fee-bearing refund balance asserted. | Rejecting SPENT refund witnesses strands the taker; removing post-NUT-07 restore wrongly marks a landed claim abandoned. Both fail safety assertions. |
| N2 | Fixed: `n2_late_claim_after_abandonment_restore_outage_never_refunds`; `n2_grace_covers_swap_timeout_and_deadline_follows_checkstate`; `n2_late_lock_after_abandonment_is_restored_and_refunded`. Fresh refund evidence, deadline immediately before POST, derived 60-second grace, post-abandonment lock reconciliation. | Four independent mutations: persisted-flag refund authorization, 20-second grace, pre-NUT-07 deadline gate, and missing post-abandonment lock check. All fail their distinct safety assertions. |
| N3 | Round-2 partial (superseded by the third-pass fix above): `n3_maker_missing_dleq_refunds`, `n3_maker_invalid_dleq_refunds`, `n3_taker_missing_dleq_refunds`, `n3_taker_invalid_dleq_refunds`. Explicit unforwardable state, timed refund, exact net-of-fee balance, no reserved spent inputs. | Disabling the state transition makes all four coordinator tests fail at the explicit recovery-state assertion. |
| N4 | Fixed: `n4_partial_claim_wins_nut07_swap_race_reselects_refund`; `n4_lost_refund_reply_reconciles_outputs_before_complete`. Retire a freshly proven rejected refund, retain its exact outputs, select fresh UNSPENT inputs, and claim using the learned preimage. | Disabling reselection or same-step claim leaves the maker nonterminal; removing the refund-output reconciliation gate terminates with uncredited outputs. |
| Dead cancel guard | Removed; `l4_active_quote_cannot_cancel` retains active-quote refusal. | No money-path mutation needed for dead code removal. |
| Notice preimage | Lowercase normalization before persistence; `nit_notice_preimage_normalized_before_storage`. | No mutation required. |
| Taker lock release | Explicit `lock_reconciling`; covered by the late-lock test and `h2_unsent_lock_expires_and_releases_reservation`. Maker's active index also survives a raced release failure. | Late-lock mutation expires without the mandatory restore and fails before releasing the held request. |

The original `h2_landed_claim_lost_reply_never_refunds` was mutation-checked again:
removing its refund fairness guard produces the forbidden refund and fails its safety assertion.
**Eleven disposable mutations compiled, with 14 expected test failures, all at safety assertions**
(not build/setup failures). Copies and logs are retained under
`.openclaw/tmp/review-1107-pass2/release-mutations/`; the PR source was byte-for-byte unchanged during the run.
Mutation builds used a separate warm target from the PR's test/live binary.

### Historical round-two final-tree verification

- Default: **46 passed / 0 failed**; lab: **70 passed / 0 failed**. Per-suite counts are above.
- **20/20** zero-fee full trade + sell-back runs and **20/20** fee-bearing full trade + sell-back
  runs passed, each with exact balances in both directions. The **five N1/N2 tests each passed
  20/20 times** (100 regression executions). Tests ran on the compiled lab binaries.
- Touched-file rustfmt check, default/lab clippy (`--all-targets --no-deps`) and final default
  binary/examples build exited 0. Clippy retains non-blocking style warnings and the explicit
  constant-invariant assertion warning. The rebuilt default binary matched the live binary.
- Earlier overlapping checkpoint runs missed the short lab claim window (zero-fee sell-back
  and H2 setup); those failure logs are retained. Their isolated checks and final serialized
  suites passed without widening any deadline or loosening an assertion. This is not a claim
  of load-independent wall-clock test timing. The outer runner was terminated (SIGTERM)
  during round 10; only unfinished repetitions were resumed on the same binaries, and the
  incomplete attempt is not counted as a pass or an assertion failure.
- Evidence logs: `target/second-pass/`; disposable mutations: path above. Root CI is separate
  because this crate is an independent Cargo workspace. No new paid review was commissioned.

### Fresh-home live evidence — final default-feature binary

- Homes: `/home/openclaw/.openclaw/workspace/.openclaw/tmp/review-1107-pass2/live-20261008-_jdz9jay/{maker,taker}`; old orphan homes untouched.
- Binary SHA-256: `5021b70fbc91646e5f8d7a503b2060ebcc89f583d836d4c59ef301b98410578e`.
- Maker mint: `https://testnut.cashudevkit.org`, `cdk-mintd/0.18.0`.
- Taker mint: `https://testnut.cashu.space`, `cdk-mintd/0.17.0-rc.3`.
- Lot: `7c3e2b8cd0df45b11092f73ab0ab84db1c19d0066f9a6a17495975f7b86a84f9`.
- Available: `f25dbc10ebcd3f3a6435e206f848bf59254ac8ad4d2cda38d4d4c635f2372e6b`.
- Sold: `7017f8c44c83d677aa50533cca29a8081cb85260785dfd8957f7dd2abfb0afce`.
- Swap: `cdd45454-b24e-4b42-9909-0f96a13c553f`; **both roles complete**, all four lock/claim attempts done,
  exact outputs saved, all wallet proofs UNSPENT, zero remaining reservations.

| Home | cashudevkit before → after | cashu.space before → after |
|---|---:|---:|
| Maker | 128 → **93** | 0 → **24** |
| Taker | 0 → **32** | 128 → **101** |

Both mints charge **100 ppk**. Maker locks 33 gross for 32 net, pays 2 locking fee + 1 claim
fee, and debits 35. Taker locks 25 gross for 24 net, pays 2 + 1, and debits 27. Each mint's
combined ending balances are **125 = 128 − 3 mint fees**, exactly. No platform/trade fee.

The independent `read_trade` reader verified all three events from **relay.ditto.pub**, including
signatures, author binding and the contiguous status chain ending in sold. Independent read
attempts: **1**. No event or trade was republished. ACKs alone are
not counted as retained evidence. No private proof/preimage is included here.

Live refunds and live sell-back were not run; short refunds and both trade directions are covered
by the real local mints. No public-mint locktime fence was shortened for this run.

## Live review-fix evidence — 2026-10-07

Fresh homes:
`/home/openclaw/.openclaw/workspace/.openclaw/tmp/credit-trade-review-20261007-i084gidm/{maker,taker}`.
The flow above ran on a frozen **default-feature** binary after both updated suites passed.
Binary SHA-256: 87e489e69b36ad1626a1592af8e7ccbc84e0f28fa95ea550ed4581ded31448d2.
After mutation builds, the worktree default binary was rebuilt and compared byte-for-byte.
Original orphan homes were left untouched. Both swaps are `complete`, all four lock/claim
attempts are done with saved outputs, all wallet proofs are UNSPENT, and reservations are zero.

- Maker: `https://testnut.cashudevkit.org`, `cdk-mintd/0.18.0`.
- Taker: `https://testnut.cashu.space`, `cdk-mintd/0.17.0-rc.3`.
- Lot: `c9b171bbab62792bf93bc0cfe316485fa69b529248ea324ce81f3383aad5fd81`.
- Available: `4f03dd274ad5b619dbd00afdb781a4f4a0a2a79e99fee9cc1b76bdf0925abc0c`.
- Sold: `1fb82c244c187d71adfe0e2d0e54ca3bd397d891b49f42827c529e77a7b81a2b`.
- Swap: `613087b4-6e59-45d8-a18c-cf9f607bfe07`.

| Home | cashudevkit before → after | cashu.space before → after |
|---|---:|---:|
| Maker | 128 → **93** | 0 → **24** |
| Taker | 0 → **32** | 128 → **101** |

Both mints charge 100 ppk. Maker: 33 gross, 32 net, 2 locking fee + 1 claim fee, debit 35.
Taker: 25 gross, 24 net, 2 locking fee + 1 claim fee, debit 27. Per mint, final balances sum
to 125 = 128 − 3 mint fees. No platform fee or unsecured send.

Independent `read_trade` fetched both public relays and verified signatures, author binding,
contiguous sequence/previous-id chain and sold status. Ditto returned all three events;
Damus returned no events in that read. The union proves readback;
neither relay's ACK alone is used as evidence of retention. Initially discovery was empty
after publication ACK, and take refused **before any lock**. Independent readback subsequently
found the signed lot/initial status; repeating the unchanged discover/take commands completed.
No validation was bypassed and no replacement swap was fabricated.

**Live refund not run:** the existing lab configuration permits short locks only against
127.0.0.1, not either public test mint. That fence was preserved. Both local refund paths and
the failed-claim/refund path passed against the two actual in-process CDK mints.

## Earlier baseline live evidence — 2026-10-07

Actual fresh homes on this machine:
`/home/openclaw/.openclaw/workspace/.openclaw/tmp/credit-trade-final-20261007/{maker,taker}`.
The commands above were executed using these homes and a copied production-default binary.
Neither the original `credit-trade-live` homes nor their reservations were changed.

- Relays: **wss://relay.ditto.pub + wss://relay.damus.io** (both queried/published).
- Maker mint: **https://testnut.cashudevkit.org**, `cdk-mintd/0.18.0`.
- Taker mint: **https://testnut.cashu.space**, `cdk-mintd/0.17.0-rc.3`.
- Both advertised NUT-07/14, reported clock skew 0–1 seconds and input fee **100 ppk**.
- Lot: `d659b210246a41cef13c3edfe790ba01d0a60b15ed0ee2d74856eaea52a1c445`.
- Initial available event: `f5e981a34c5fbf4f94853fa1c78e4e008aa77c1acce54b60edfcc95890595dee`.
- Sold event: `a85ca93f54f8c5c922593996c7584945dc7fb3daa8510e86fa8e503facb96d58`.
- Swap: `4f312a95-19aa-4b8a-b871-86bc6731751a`; **maker complete, taker complete**.

| Home | cashudevkit before → after | cashu.space before → after |
|---|---:|---:|
| Maker | 128 → **93** | 0 → **24** |
| Taker | 0 → **32** | 128 → **101** |

Maker: net 32, gross lock 33 (2 proofs), preparation fee 2, claim fee 1, debit 35.
Taker: net 24, gross lock 25 (3 proofs), preparation fee 2, claim fee 1, debit 27.
Each mint's combined final balances are 125, exactly 128 minus its three units of mint fees.
There were **no trade/platform fees, no unsecured sends, and no live protocol blocker**.
An earlier fresh-home run also completed with the same final balances (lot
`85e59585ca6462b7dbf5b459d7e8d5a1077a8742641e3fb541895a2a9c909b3c`).
Private proofs/preimages are not included in this document or logs.

### Relay observations

The earlier independent-reader probe found inconsistent persistent readback at Ditto; hence
publish-to-all and read-the-union. Damus returned stored **23412** events despite their ephemeral
kind. This is acceptable here because their contents are NIP-44 encrypted; confidentiality does
not rely on deletion. Neither observation guarantees 24-hour retention or relay completeness.
The diagnostic remains rerunnable as `target/debug/examples/relay_probe` from this crate.

### Preserved CDK 0.17.2 orphan-quote bug

The old CLI attempted issuance before testnut auto-pay, producing `Amount undefined`. Two old
quotes subsequently remained `PAID` with a reservation but no saga:
`01a1181c-9e87-7ff1-b9d4-176ec59adfda` and `01a1181c-acbf-7a40-817a-8c37a9ca04bf`.
`inner_check_mint_quote_status` releases the orphan reservation, then writes the still-reserved
in-memory quote back through `add_mint_quote`, resurrecting `used_by_operation`. The next mint
returns **`Quote already in use by another operation`**.

`tests/pinned_quote_recovery.rs` retains the exact public-database-API reproducer. It proves the
bug, **not successful recovery**. The corrected `fund` waits for PAID and works on fresh quotes,
as demonstrated by this live run. The original homes are preserved; no reservation was cleared,
CDK was not patched/upgraded, and this old-quote bug is **not a blocker for fresh-home trading**.

## First fix pass — historical review and regression evidence

The paid cashu-rust-dev review read code only at 15b15ec; this author-led fix pass independently
built and exercised the consequences. It is not a new independent reviewer sign-off.

Before editing production code, the C1 premise test passed on the unfixed adapter in **both
directions**: hostile witness passed validation, the mint rejected the claim, and inputs stayed
UNSPENT. After fixing it, admission rejects the witness and the redemption defense independently
clears it. Mutation of that defense reproduces HTTP 400 "malformed signature".

All named tests are in tests/review_regressions.rs and instantiate two distinct-key CDK 0.17.2
FakeWallet mints plus a local relay. Faults are HTTP middleware around the real mint router,
not mock successful swaps. Lab-gated tests use only the existing loopback timing configuration.

| Finding | Resolution and test | Mutation evidence |
|---|---|---|
| C1 | Fixed — c1_sender_witness_rejected_and_sanitized_both_directions | Removing admission fails its rejection assertion; removing sanitization fails the mint-claim safety assertion. |
| C2 | Fixed — c2_partial_claim_recovers_preimage_and_refunds_only_unspent | Restoring all-or-nothing witness extraction leaves maker receipt 0 instead of 24. |
| H1 | Fixed — h1_maker_offline_past_long_still_claims | Restoring the long deadline leaves maker receipt 0 instead of 24. |
| H2 | Fixed — h2_failed_claim_abandoned_then_taker_refunds; h2_unsent_lock_expires_and_releases_reservation; h2_maker_abandoned_lock_releases_listing; h2_pending_inputs_prevent_abandonment; h2_landed_claim_lost_reply_never_refunds | Six mutations: disable abandonment (claim/taker lock/maker lock), allow PENDING, restore early error return, remove refund fairness guard. Each fails its safety assertion. |
| H3 | Fixed — h3_uppercase_hash_request_rejected_at_admission | Restoring permissive hex admission accepts the uppercase request and fails rejection. |
| M1 | Fixed — m1_notice_recovers_without_nut07_witness | Not required; actual mint responses have witnesses stripped, encrypted notice still completes maker claim. |
| M2 | Fixed — m2_missing_dleq_persists_result_and_credits_change; m2_invalid_present_dleq_persisted_but_not_credited; m2_l3_preflight_requires_restore_and_dleq | Not required. |
| M3 | Fixed — m3_claimed_terminal_without_refund | Not required; terminal recovery returns, and no refund attempt exists. |
| L1 | Fixed — l1_taker_lock_deadline_leaves_twenty_seconds_for_delivery | Not required; inspects the saved send deadline. |
| L2 | Fixed — l2_own_fee_mismatch_not_journalled | Not required; mismatched fee is refused before any attempt record. |
| L3 | Fixed — m2_l3_preflight_requires_restore_and_dleq | Not required. |
| L4 | Fixed — l4_active_quote_cannot_cancel | Not required; listing remains uncancelled. |
| L5 | Fixed — l5_mint_clock_controls_claim_cutoff | Not required; fast mint clock prevents starting a claim. |

H3 representation correction: the hash is 64 lowercase hex characters. Pinned Cashu HTLC public
keys serialize as **33-byte compressed SEC1 / 66 lowercase hex characters**. Requiring 64-hex
Cashu keys would reject the valid keys produced by this crate; canonical serialized keys are
required instead. Nostr identity keys remain 64-hex.

Mutation procedure: copy only this standalone crate (excluding target) into separate disposable
directories under .openclaw/tmp/review-1107-fix/mutation-*. Remove one safety condition per copy,
then run its named regression with:

```sh
PROTOC=<protoc> TRADE_LAB_SECONDS=1 CARGO_TARGET_DIR=<idle-warm-target> \
  cargo test --manifest-path <disposable-copy>/Cargo.toml --locked --features lab \
  --test review_regressions <test-name> -- --test-threads=1
```

All **11 mutations compiled and failed at the intended safety assertion** (exit 101), not at fixture setup. The PR source tree was not used for mutant edits. Logs and full copies are retained under the path above.

## Review-fix recovery policy

- Received proofs must have **no witness**. Redemption also clears any supplied witness before
  adding our preimage/signature. Hashes must be lowercase 64-hex. Cashu HTLC public keys must
  use their canonical lowercase **66-hex compressed** representation (not Nostr's 64-hex keys).
- Preimage discovery scans individual SPENT proofs, skipping UNSPENT/PENDING states and
  refund/malformed witnesses. Valid preimages are compared as bytes and persisted monotonically.
  The encrypted `claimed` notice now carries the preimage; NUT-07 remains the fallback.
- The maker claims immediately once it knows the preimage, with **no claim send deadline**.
  A partial taker claim cannot prevent payment recovery. After short + margin, the maker selects
  only freshly confirmed UNSPENT proofs for refund, never the full original set. It remains
  `settling` until its outgoing proofs are reconciled. Unknown SPENT proofs are not silently
  treated as our refund.
- **Fresh-evidence fairness (N1/N2/H2):** a persisted `abandoned` flag is never refund
  authority. At the taker's refund decision, an existing claim attempt requires its deadline
  plus **60 seconds** of grace to have passed, an empty restore of its exact outputs, a fresh
  NUT-07 check of the bound incoming proofs, and another empty restore **after** NUT-07.
  PENDING, RPC errors, partial/nonempty restore, a SPENT matching-preimage witness, or an
  ambiguous/missing SPENT witness means **no refund this tick**. Explicit HTLC refund witnesses
  (empty or nonmatching preimages) are accepted: a maker refund must not strand the taker.
  When there is no claim attempt, there are no claim outputs to restore; any stored incoming
  proofs still require fresh, unambiguous NUT-07 evidence before refunding.
- **Send/abandonment discipline (N2):** the send deadline is checked immediately before the
  swap POST, after the last NUT-07 and mint-clock RPC, with no intervening RPC. Grace is derived
  as `3 * RPC_TIMEOUT_SECONDS` (20 seconds per RPC), covering the swap timeout plus 40 seconds
  of margin. Abandonment never discards exact outputs or resubmits them. A lock cannot expire
  or release its backing on the flag alone: a post-abandonment restore → NUT-07 → restore
  recheck is mandatory. `lock_reconciling` keeps late results reachable when evidence or
  release fails; a restored late lock becomes outgoing and follows the normal timed refund.
- **Unforwardable own locks (N3):** missing/invalid DLEQ records `lock_unforwardable` and the
  locked proofs as outgoing, never sends them to the peer, and follows refund-after-locktime.
  Owned change and spent funding reservations are settled through the wallet; invalid present
  DLEQ is never credited. Recovery can repair DLEQ metadata by restoring the exact outputs
  and verifying again. Even repaired locks remain unforwardable. An issuer that continues to
  supply invalid evidence can cause terminal manual-recovery quarantine, while refusal of restoration can still block recovery; this is not a
  validation bypass or a guarantee against a dishonest mint.
- **Refund races (N4):** a rejected maker refund is retired only on fresh empty restore,
  NUT-07 evidence of a matching-preimage competing claim (without PENDING/ambiguous states),
  and a second empty restore. Its exact outputs stay journaled. A durable refund generation
  permits a fresh UNSPENT-only selection. Matching preimages are persisted independently of
  negative evidence (including before a final restore can fail) and drive the maker's claim in
  the same recovery step. A crash cannot force another outgoing-mint witness read to reclaim
  that already learned preimage. SPENT refund inputs alone are not completion: the maker
  remains `settling` until its exact refund outputs have been restored and credited.
- **M3 policy:** after receiving the maker's funds, a taker **never refunds its payment**.
  If the maker has not claimed by long + margin, the taker becomes terminal
  `complete_unclaimed`, ending retries (even if mint info is offline once the local deadline has passed) and allowing future takes/`recover` to finish.
  The maker remains entitled to the original lock and can still claim under NUT-14.
- Claim/refund decisions read fresh mint info and use `max(local time, mint time)`.
  Taker locks use `send_before = min(exp − 20 s, short − cutoff)`; production locks remain
  60/15 minutes, quote hold 60 seconds, and claim cutoff 3 minutes.
- Both mints must advertise NUT-09 and NUT-12 as well as NUT-07/14. Swap results are journalled
  before DLEQ refusal. Missing DLEQ prevents forwarding, but owned change is credited;
  present invalid DLEQ is quarantined for verified restoration and timed recovery. Own-lock claim fees are checked before journalling.
  Cancellation is refused while a quote/swap is active.

These are client recovery policies, not mint/NUT changes. Already-delivered HTTPS requests have
no server-enforced expiry; the grace rule assumes an honest mint's current restore/state evidence.
A mint can still lie, withhold evidence, or process an already-delivered request late.

## Boundaries, deviations and uncovered cases

- Authorized deviation from the integration spec: standalone crate, home/seed/CDK wallets and
  trade SQLite journal; HTTP(S) test mints and public relays; no jobs, budget/ledger, MCP,
  sidecar changes, or production-relay deployment (`nostr://` mints use the core connector, see above). Same-repository PR #1107 stays draft.
- Trade swaps use pinned CDK **public lower-level primitives** with explicitly journaled outputs,
  rather than opaque high-level send/receive sagas. Claims and refunds use the same durable
  adapter; refunds include the empty-preimage witness required by pinned CDK. No dependency fork.
- Negotiation carries bounded proof arrays whose mint/unit are bound by the quote and verified
  against that mint's keys, rather than an additional bearer-token wrapper. Denominations are
  binary; fees use one ceil after summing input ppk. Limits: 1,000,000 net per lot, 128 proofs,
  48 KiB encrypted plaintext, 256 status revisions, 4,096 events per relay query, eight relays.
- Two SQLite domains (trade journal and per-mint wallet) are coordinated by durable intents and
  exclusive home ownership, not a cross-database atomic transaction. Other copies of a wallet
  seed remain outside the single-owner guarantee. Money records and receipts are retained; a pruned pending-publication index bounds recurring scans.
- A persisted client send deadline prevents **resubmitting** an expired mint request; HTTPS
  mints do not offer an enforceable server-side request expiry. Already-delivered requests may
  race refunds. Ambiguous/partial restoration remains held; no automatic unsafe compensation.
- NUT-14 receiver claims remain possible after locktime. Honest/available mints, intact keys,
  clock bounds, and a running recovery watcher are required. An offline maker can lose its
  recovery window; an issuer can lie or refuse service. This is not trustlessness against mints.
- Not covered: every crash boundary, OS/power-loss durability, arbitrary long outages/clock skew,
  pre-fix journal migration, malformed live witnesses, mint key rotation,
  adversarial fee changes, exhaustive peer flood/quote-limit concurrency, 24-hour relay retention,
  root workspace suites locally, or production deployment. Round-four live coverage is recorded separately below. Tests cover selected
  process-exit boundaries and one claim/refund race, not a full model checker.


## Receive (import a Cashu token)

`maxplayer-trade --home <home> receive <mint> --token-file <path>` (or the token on stdin;
never argv) imports one token, e.g. one written by `maxplayer-mint issue`. Code: `src/receive.rs`.

- Refused before any journal write: token mint not exactly `<mint>` after canonicalization,
  multi-mint, non-sat, NUT-10 locked (P2PK/HTLC), > 128 or duplicate proofs, invalid incoming
  DLEQ (when present), > 100,000 sats, or cumulative funding + receives for that mint above
  100,000 sats. Common preflight and NUT-07 UNSPENT are required before the journal write.
- One swap into fresh home-owned outputs; token, inputs, blinded outputs and secrets are
  journaled before the POST. Ambiguity → NUT-09 restore of the same outputs and identical
  replay only. A NUT error with inputs UNSPENT is terminal `refused`; SPENT inputs with nothing
  of ours restorable is terminal `already_spent`. Only DLEQ-verified proofs are credited;
  missing/invalid DLEQ on our outputs is terminal `quarantined` (exit 4).
- Idempotent per Y-set (sha256 of the sorted proof Ys): repeats resume the existing attempt.
- The mint input fee is deducted; output reports `amount`, `fee`, `net`, `credited`, state and
  attempt id only. `recover`/`serve` resume receives under the same 120 s item budget.
- `nostr://` mints: cdk 0.17.2 `MintUrl`/`Token` parse them (unit test
  `cdk_token_parsing_accepts_nostr_mint_url`), but this crate's `Asset::new` admits only
  http/https, so a nostr:// receive is refused until the nostr transport branch lands.

## Funding and withdrawals

This path is **not a production safety certification**. Public-mint results, if any, belong in
its separate run report; the historical test counts above predate this change.

- Each lock gross is capped at **100,000 sats**, at both planning and submission.
- Funding on every mint is capped at **100,000 sats cumulatively per mint per home**. The journal charges
  an intent before creating the mint quote; pending, failed, issued and lost-reply intents
  stay charged across restarts. A lost quote response is deliberately not auto-replaced.
- `fund` locks the quote to a private NUT-20 key and requires the mint to echo that
  binding before printing the invoice and quote. Issuance is signed with that key. It prints
  the invoice and quote only after recording them. It never pays an
  invoice. It waits up to 30 seconds; `fund --quote <id>` or `recover` resumes it. Issuance
  records its exact outputs before POST and restores those outputs after an ambiguous reply.
- `withdraw <mint> --invoice <bolt11>` uses raw NUT-05, **not CDK's melt saga**. Its private
  `trade.sqlite` record binds the invoice, quote, amount, fee reserve, input fee, exact input
  proofs and blank change outputs including secrets/blinding factors before submission.
  Same-invoice invocations resume the original authorization rather than paying again.
- Explicit non-terminal states: `quote_created`, `request_sent`, `pending`, and
  `paid_change_unreconciled`. A no-reply timeout replays only the identical journaled melt POST. `PAID` alone does
  not mean `done`: the mint may commit payment before change signatures. Change is restored
  by the recorded blinded messages, matched against the quote, unblinded, DLEQ-verified,
  fee-bounded and credited before settlement. Empty restore is not proof of zero change.
  Zero change is terminal only when the input/amount/input-fee arithmetic proves it.
- Definitive UNPAID response or well-formed numeric NUT error on HTTP 4xx, or a previously observed PENDING
  followed by UNPAID, additionally requires fresh UNSPENT inputs before release. An UNPAID
  snapshot after an ambiguous send is not sufficient: the original POST might still arrive.
- New proofs are credited **reserved**, then the journal is marked done, then they are
  released. Recovery finishes an interrupted release. Never open a home concurrently or
  spend its proofs outside this tool. The CLI holds the home lock; library callers must too.
- Withdrawals have a 100,000-sat invoice cap and a max(32 sats, ceil(invoice sats × 2%)) reserve cap. Input selection requires change to remain positive even
  if the whole reserve is consumed, unless arithmetic proves zero change. Sweep conservatively; do not interpret a `paid_change_unreconciled` result as dust
  or fees. Preserve every funded home and stop for inspection on unresolved state.
  Reserves are mint-specific (live: Minibits/Macadamia 2, cashu.cz 5 sats), not a global
  constant. An insufficient-funds refusal is terminal `refused` and cannot pay later via recovery.
  Only explicit same-invoice withdrawal advances a crash-retained `quote_created` record;
  recovery only expires/releases it. Never edit the journal or replace an ambiguous submitted payment.
- NUT-07 advertisement is insufficient for HTLC refund safety. Check actual spent-proof
  witness emission and parsing on each mint before running the timed refund scenario.
  Never log proofs, tokens, wallet keys, blinding secrets, or HTLC/payment preimages.

`recover` attempts each funding, withdrawal and swap once and reports public IDs and states.
An unreachable mint or stuck authorization does not stop other items or keep the command
running indefinitely. Pending items remain non-terminal; exit 3 requests a later pass.
Use `serve` for continuous recovery while locks are live. Historical round-four harness
results below predate this bounded-pass fix.

The tool's per-home caps do not constrain the external wallet paying a funding invoice.
A hard source-wallet budget needs a quote-bound total-debit ceiling at the payment's
confirmation boundary, including input/swap fees. A separately checked estimate is not
such a ceiling if the payer creates a fresh quote when paying. Do not use an unbounded
payer for a run that requires a hard spending ceiling.

### Secret-free home audit

Stop the home's running process first. The audit helper takes its exclusive owner lock,
reports balances/funding/withdrawal accounting, and queries NUT-07 for every recorded
outgoing HTLC proof. It counts matching claim witnesses, empty-preimage refund witnesses,
missing/invalid witnesses, PENDING and UNSPENT proofs; it never prints proof material or
preimages. Missing/invalid spent witnesses make it fail. It does not recover or submit
payments. Supply every mint needed by that home's recorded trades:

```sh
cargo run --manifest-path crates/maxplayer-trade/Cargo.toml \
  --locked --example real_audit -- /absolute/preserved/trade-home \
  https://mint.minibits.cash/Bitcoin https://mint.macadamia.cash
```

### Historical real-money-path implementation validation (2026-10-08, before live funding)

| Suite | Default | Lab |
|---|---:|---:|
| Protocol primitives and gross cap | 22 | 22 |
| Existing CLI checks | 4 | 4 |
| Two-mint integration/recovery | 6 | 11 |
| New money-path/opt-in/recovery checks | 12 | 13 |
| Pinned quote reproducer | 1 | 1 |
| Relay checks | 3 | 3 |
| Existing review regressions | 11 | 30 |
| **Passed / failed** | **59 / 0** | **84 / 0** |

All suites used `--test-threads=1`; no timing failures needed reruns. A disposable mutation
that swallowed the change-restore error and marked the melt `Done` failed the explicit
`SAFETY: restore failure must not mark done` assertion. Restoring the original implementation
passed the same test. Touched-file fmt and whitespace checks passed. Default/lab clippy
`--all-targets --no-deps` exited 0 with non-blocking style warnings, including the new CLI's
nested funding `if`. Binaries/examples were rebuilt without `lab` afterward.

**No public real-money trades were run for this revision.** The specified installed payer,
`maxplayer wallet melt` at `89eca2d`, creates a fresh payment quote and confirms with no
spend ceiling. The live run stopped before funding because that cannot enforce the requested
hard source-wallet budget. No source-wallet CLI/core edits or budget waiver were made.
The inherited round-3 review work on the base trade coordinator is separate; its parallel
worktree and PR were not changed by this implementation.

## Round 4 relay policy and real-money integration (author-led verification)

Current measured results, per-suite counts, mutation assertions, relay probes, both live
refunds and real-sat accounting are in the [round-four evidence report](evidence/round4-20261008/REPORT.md).
Round four passed **66 default / 100 lab tests**, **100 under CPU contention**, and **860
repeated executions (20 full rounds)**, with zero test failures. Three safety mutations
failed at their intended assertions; restored controls passed. The final default binary
completed a fake trade/sell-back and both fake/real production-timing refunds; Nutshell
refund witnesses and withdrawals are verified. Minibits returned, the old authorization
reconciled without payment, and five sweeps returned 257 sats. Final run accounting:
**9169 = 9125 in the designated wallet + 22 retained dust + 22 fees**. All real-home authorizations
are terminal and retained spendable proofs were remotely confirmed UNSPENT. The report
preserves both the real recovery-harness timeout and the unsent cashu.cz reserve refusal.

The counts and live results above describe head `9dad8e62669468492f997b4cb44fabe2b26ba8e1`,
before the subsequently delivered scope change. That historical head had testing-only
mint admission and 500-sat lock/funding caps. The current design removes mint opt-in,
requires automatic preflight and raises lock, funding and withdrawal invoice caps to 100,000 sats. Historical live
results are not a test of this new binary. The withdrawal fee-reserve limit stays 32 sats.
An invoice at the cap still requires enough balance for fees and verified change; the
cap is not a promise that a 100,000-sat balance can pay a 100,000-sat invoice plus fees. No maxplayer CLI/core changes.

### Relay traffic and failure behavior

The serialized healthy-trade regression publishes **10 unique events per relay**: lot, available, sold, plus
request, quote, first, second, claimed and one done from each party. The trade/sell-back
regression asserts this count and exactly one attempt per event on a healthy relay.
A concurrent live trade can omit the claimed notice if mint evidence already makes the
taker complete before that notice is sent, giving nine rather than ten unique events.
Before this fix, the three-second serve/recover tick resent unchanged state messages
(up to 20 sends/minute per waiting role/relay, plus handling-triggered sends); replayed
requests caused more quote replies. Listing completion resent the available status.
A disposable original-head two-relay reproduction completed one trade in 7.60 seconds
with **38 publication attempts for 10 unique events** (3410: 2, 3411: 6, 23412: 30);
individual events were sent up to ten times. The fixed healthy-path budget for the same
two relays is 20 attempts (30 for the three new defaults). This measures avoidable
amplification rather than guessing the historical relay threshold. The Damus ban reported repeated
rate-limit violations, but its private threshold and exact historical send count are unknown.

The publisher journals the signed event, retry charge and positive per-relay receipts.
Success requires at least one recorded positive ACK; a missing/negative ACK is never
success. Attempts run in parallel with a 12-second bound. Failed copies are revisited by
serve/recover ticks, with exponential backoff (6 seconds initially, capped at 300 seconds,
plus 0–5 seconds jitter) and at most 12 attempts per event/relay. ACKed copies are never
republished, including after restart. `rate-limited:` pauses the entire relay for at least
five minutes; `blocked:`/`banned:` disables writes to it for that home. No blind restart
resets those limits. Exhausted/blocked delivery needs another explicitly configured relay;
no automatic retry-budget reset exists. A 24-hour event-age bound stops historical retries.

Admission of a quote/first/second leg additionally requires a receipt for the preceding
local message. This cannot undo a lock already committed at a mint: its state must still
be recorded even if publishing the resulting lock message fails. Such locks follow their
normal timed refund path. Relay outage never authorizes a new spend or substitutes for
mint evidence. Active swaps are recovered before listing publication; listing-delivery
errors cannot skip other swaps' refund branches. Network attempts can delay a tick, so
"three seconds" is a nominal cadence, not a real-time deadline guarantee.

ACKs are not storage evidence. Two independent fresh-key probe rounds checked all three
kinds, live subscription delivery, then fetch-by-id at least 60 seconds later. The second
round used a new readback client that had not seen the live events. Selected defaults
passed all checks; Ditto and Wellorder did not retain 23412 in these probes, and Nostr Band
failed. This dated snapshot is not a promise of future public-relay retention. Encrypted
23412 messages may be retained despite their ephemeral kind; do not promise deletion.

Withdrawal states remain `quote_created`, `request_sent`, `pending`, and
`paid_change_unreconciled` until reconciled. `done` requires exact restored, verified,
accounted-for change and SPENT inputs, not PAID alone; `unpaid_released` requires safe
absence/termination evidence. Ambiguous sends are not resubmitted. Preserve every funded
home and exact authorization. Quarantined trade outputs never become withdrawal balance.
The skill documents these states and the CLI help regression includes `withdraw --invoice`.
