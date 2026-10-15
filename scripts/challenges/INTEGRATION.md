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
