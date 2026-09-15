# Cashu/CDK specialist seat — evidence

Build completion and live readiness are reported **separately**, because they are not the same
thing and only one of them is finished.

## A. Build — COMPLETE, verified

### Acceptance gate: PASS, 22 assertions across 8 legs, exit 0

Full transcript: `acceptance-run-20260909.txt`. Counts, not adjectives:

| leg | asserted |
|---|---|
| 1 mint info (NUT-06) | version `cdk-mintd/0.17.2`; 3 NUT-04 methods advertised |
| 2 mint quote + issue | 64 sats minted; wallet balance 64 |
| 3 send + receive | token 954 chars; sender 64 → 43 (sent 21, fee 0); receiver credited 21, balance 21 |
| 4 double spend | second receive of the same token rejected: `Token Already Spent`; double-spender balance 0 |
| 5 melt | melt amount 5; balance 21 → 15 (melt 5, fee 1) |
| 6 failed payment | injection payload round-tripped through the BOLT11 description; wallet errored `Payment failed`; balance 15 → 15 — nothing lost |
| 7 restart recovery | `test-mint restart`; sender 43 → 43, receiver 15 → 15; a spend AFTER the restart succeeded (1 sat received); the pre-restart spent token still rejected — no duplicate credit |
| 8 loopback only | `/proc/net/tcp` `local_address` for the 8085 listener = `0100007F`; TCP connect to the container's own bridge address `172.17.0.2:8085` REFUSED |

No leg is a no-op and none was skipped. Every failure mode named in the order was simulated; if one
could not have been, the harness `bail!`s rather than printing a pass (see the `container_ip()`
branch).

### Four defects found by running rather than reading

1. **`cdk-mintd` needs `protoc` at build time even with `--no-default-features`.** It depends on
   `cdk-signatory` unconditionally and that crate's `build.rs:14` compiles its proto regardless of
   the `grpc` feature. First build failed exactly there.
2. **`[ln]` min/max mint+melt are REQUIRED**, though `cdk-mintd`'s own `example.config.toml`
   comments them out. `struct Ln` (`src/config.rs:170-179`) has no `#[serde(default)]` for them;
   omitting one kills the whole file with `data did not match any variant of untagged enum
   LnOneOrMany for key 'ln'`, which names neither the field nor the cause.
3. **The internal roster note's mint config is wrong at 0.17.2.** It describes
   `[payment_backend] backend = "fakewallet"`; the real schema is `[ln] ln_backend = "fakewallet"`
   plus a `[fake_wallet]` table. Its loopback `127.0.0.1:8085` claim was correct.
4. **`pay_err: true` alone does NOT simulate a failed payment.** `cdk-fake-wallet`
   (`src/lib.rs:706-714`) inserts `check_payment_state` into its payment-states map *before* it
   honours `pay_err`, so with the struct's `Paid` default the melt finalises `state=Paid,
   amount=5, fee_paid=0` for an invoice the backend refused. `check_payment_state: Unpaid` is
   required too. A harness asserting only "the call returned" would have reported false coverage
   here.

### Containment

- Mint binds loopback and `test-mint` refuses any other bind (enforced in the script, not merely
  documented).
- The mint binary is built without cln/lnd/lnbits/bdk/ldk-node compiled in, so it cannot be pointed
  at a real payment backend by any configuration.
- Seat job containment: dedicated docker network `maxplayer-cashu-jobs`; doctor reports the policy
  renders **23 rules**, installed in each job's own netns before the job starts.
- The test mint appears in **no** `accepted_mints`. The seat's accepted set is the shipped default,
  unchanged.
- No secret value appears in any artifact, log, command line or report. The mint seed is generated
  per reset into a `0600` file and passed by `--seed-file`.

## B. Live readiness — PARTIAL, and blocked on a human decision

`doctor-20260909.txt`: **17 checks, exit 0.** PASS on nix, seller key, relay reachability
(NIP-42 authenticated), mint reachability, sandbox launcher, sandbox image, sandbox egress
(23 rules), engine floor, containment probe, home permissions (0700).

The seat starts, authenticates to the relay, and publishes its relay-git announce
(`seller-boot-20260909.log`). It then **refuses to advertise**:

```
04:53:05Z pre-advertise probe FAILED claude: this is an AUTHENTICATION failure
  (ACP request 3 failed: {"code":-32000,"message":"Authentication required"})
prove-before-advertise: none of 1 configured harness(es) produced a probe artifact;
  refusing to advertise
```

**This is correct fail-closed behaviour, not a defect.** The node proves a harness can actually run
before it advertises a capability. The daemon's environment holds none of the four contained
credentials (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`,
`OPENAI_API_KEY` — `seller_exec.rs:2763-2786`), so the #647 proxy has no real value to substitute
for the per-job placeholder and the agent inside the container cannot authenticate.

The file-based route does **not** cover this harness at 0.5.8: `[sandbox] file_credentials` reads a
single **top-level** JSON key (`read_file_credential`, `seller_exec.rs:2904-2920`, a plain
`.get(&cred.field)` with no JSON-pointer support), while claude-code's OAuth file nests its token.
Verified in source, not assumed.

**Not attempted by design.** Provisioning a model credential for a stranger-facing seat is a human
decision and a protected-credential operation. No credential was requested, handled, echoed or
stored by this lane.

### Therefore

- Discovery evidence — kind-0 profile and the kind-30340 capability heartbeat — **cannot be
  produced yet**, because the node withholds them until a harness proves out. Saying otherwise
  would be claiming a marketplace surface that does not exist.
- **No marketplace readiness is claimed.** A process starting is not readiness, and this process is
  explicitly declining to advertise.

### The one remaining step, for whoever holds the decision

Provision one contained model credential into the seller daemon's environment through supported
protected setup, then restart the seat with the same command in the runbook. The pre-advertise
probe will then either pass — at which point kind-0 and kind-30340 appear and discovery evidence can
be captured — or fail for a different, reportable reason.
