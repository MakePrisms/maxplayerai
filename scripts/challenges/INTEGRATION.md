# Auto-award regression follow-up — 2026-10-15

The harness now **never calls `award_claim`**: it requires the buyer daemon's
award within 60 seconds after observing the claim. Historical evidence below
predates this change.

A full unmodified-transport run and five repeated baseline runs passed, confirming
that ordinary scheduling does not reproduce the original failure consistently.
The loopback-only `--duplicate-history` fixture deterministically models Buzz's
live/history overlap: `req.rs` registers live fan-out before querying historical
rows, allowing the same signed claim to arrive twice before EOSE.

Reproduce with the base buyer (2b434ad6 plus count-only diagnostics):

```sh
bash scripts/challenges/run-e2e.sh --skip-build --baseline --duplicate-history
```

Captured on the base buyer, without a manual award:

```text
lifecycle wire history incomplete: rows=2 accepted=1 ended=true failed=false
22:12:47Z buyer: auto-award for 7ec203017a136da343e5fe9f80a420023674625caaf76e93df87e8b69496127f did not complete (job lifecycle relay: lifecycle wire history incomplete); left pending for re-arm
```

The wire guard counts frames; `history::collect` formerly deduplicated IDs before
that comparison. It now retains validated frames until completeness/saturation
checks, and `paginate` deduplicates the final history. Invalid events, missing
EOSE, notification loss, partial auth restarts and saturated pages still fail
closed. Original deployment frames were not retained, so this proves a matching
failure mechanism, not that every historical incident had the same cause.

Transient driver errors now retry after 1/2/4/8/16/30 seconds (30-second cap), bounded
by the locally retained signed offer deadline. Retries use the same reserve-first
and pinned-award path. An unavailable local cache is retried without inventing an expiry; the deadline is
re-read on every failure. Unknown history never releases reservations or falsely
marks an offer absent.

---

# Local integration evidence

## Existing buyer auto-award: intermittent incomplete history

Source: `feat/challenges-e2e` at `fe5121dd`, incorporating app `c1c9df12` on base
`76f991fa`; challenge-core had not landed. All relay writes were to a disposable local
Buzz instance; job was public, targeted, `payment=none`, amount/max=0.

The first baseline completed automatically. The second run of
`bash scripts/challenges/run-e2e.sh --baseline` observed a valid claim but no committed award:

```text
19:26:30Z seller node claimed job_id=1d922e7ec434f367fc5373a8b1f90da2f41d27edf97f37b2df6eff79ad26df77 buyer=bb3fd71643dc66f005561b9c948fa22cfb403b36cd1141f0b6b55230ded01db0 amount=0 deadline=1792092510 slot-reserved (awaiting award; 2 slot(s) free)
19:26:30Z buyer: auto-award for 1d922e7ec434f367fc5373a8b1f90da2f41d27edf97f37b2df6eff79ad26df77 did not complete (job lifecycle relay: lifecycle wire history incomplete); left pending for re-arm
```

`get_job` returned claim `3d51d6974eb9c39375941646b896e9669b1bd565d16d9953ead338e354b9501f`,
`live:true`, `payment_mode:"none"`, the intended seller
`572177f9f1dec4cc2be05a3d9c8a54e360bebf0c22cf0e0869c7f65ccccdb3e1`, `read_confirmed:true`,
`results:[]`, and no `awarded` field. Private raw artifacts remain under
`.scratch/ce-cc8tqz2o/` (do not publish homes/keys).

The run ended with `FAIL seller delivered result: timeout; last=None` after its 90-second
delivery wait. A subsequent run passed all baseline assertions using the manual override.

No buyer/relay implementation was changed. The harness now explicitly observes the claim
and invokes the documented `award_claim` override when no award is committed. A successful
run with that override is not evidence that auto-award recovery is fixed. Root cause beyond
the logged incomplete-history condition is not established.

## Fixture startup correction (not a challenge bug)

The first Buzz boot failed at its default Git/S3 conformance probe against localhost:9000.
Inline-only harness now sets `BUZZ_GIT_CONFORMANCE_PROBE=false` and points S3 at loopback
port 1. Real Git/S3 delivery is out of scope; Postgres, Redis and the real Buzz event pipeline
remain in use. No production relay setting was changed.

## Core/app skill-discovery mismatch (small integration fix)

After merging core `ceb29d53` and app `c1c9df12` at integration head `1fe0260b`,
`npm test --prefix web/app` reported **329 passed, 1 failed**:

```text
test at test/build.test.mjs:58:1
AssertionError [ERR_ASSERTION]: maxplayer-challenge-join has the published skill path shape, got /.well-known/skills/challenge-join/SKILL.md
```

The existing discovery test requires lowercase `skill.md` and a companion link from the
homepage skill. Small fixes only: rename the new challenge-join artifact to `skill.md`,
update its index path and the two CLI-test path literals, and add the missing companion link.
No behavior in the core CLI, protocol, app service or page was rewritten.

## App drops actual public-v2 jobs (small integration fix)

First full run (`.scratch/ce-dgcd293m`, integration source `1fe0260b`) passed challenge
publication, applications, decisions and work-window gating, then failed:

```text
PASS accepted seller live claim observed
PASS existing manual award override committed
FAIL claim and award yield running tally: timeout; last=None
```

Persisted Buzz rows prove the public trade existed:

- OFFER `d92e448f3374e13a62343f79d60be748c2a3432573dfb448eca693a53fadfde6` (3401)
- CLAIM `0db062c7d957b4dfa2e23d933d8940382858de04d3dcd425a06cd93e88b9be5b` (3402)
- AWARD `d1c0fdf96dbcb93e3da4cff8beba77844bfd331b126d7032fa4469ee2855d8e1` (3405)

All three carry `t=maxplayer`, `v=2`, `visibility=public`, and the same `job` coordinate.
The offer has `amount=0 sat`, `param payment=none`, and deadline `1792093043`. The award's
root points at the offer and unmarked `e` points at the claim. Seller log at 19:35:24Z says
`awarded ... — executing`; app was authenticated/history-complete but had `jobs:[]`.
`web/community/src/model.mjs` filtered trades using the v1-only `namespace` predicate.

Small fix: a separate trade predicate admits existing public-v2 envelopes (known trade
kinds, one version/public-visibility/hex job tag); challenge validation stays strictly v1.
Two signed-event regression tests cover v2 running/result/accept tally and reject private,
malformed, unknown-version envelopes and v2 challenges. No app tally algorithm was rewritten.

### Incomplete history also reproduced after core integration

The next full run at `570e254b` (`.scratch/ce-v4jz09it`) reached the public post, then
`get_job` returned MCP `isError:true`, text `job lifecycle relay: lifecycle wire history incomplete`.
Offer: `c0cc0965a1996e0132f1c776e87466ada6d8253dcdc0c249f29698fbc7bb69fa`.
The harness now allows at most three identical attempts of the existing idempotent
get/award/collect operations for **only** that exact failure, logging each retry. Other
errors and exhaustion still fail. The underlying client/relay behavior remains unfixed.
