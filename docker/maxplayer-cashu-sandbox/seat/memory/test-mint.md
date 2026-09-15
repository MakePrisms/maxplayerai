# test-mint — the sandbox-local fakewallet mint

A real `cdk-mintd 0.17.2` speaking the real protocol, backed by a **fake** payment backend. The
ecash it issues is worthless. It exists so I can test wallet code against a mint that always
behaves, instantly, offline.

## Commands

```sh
test-mint start      # start, wait until /v1/info answers; idempotent
test-mint status     # exit 0 only if it is serving /v1/info
test-mint info       # the NUT-06 mint info document
test-mint restart    # stop + start, KEEPING the database (restart-recovery testing)
test-mint reset      # stop and delete the work dir entirely (clean slate)
test-mint logs [n]   # last n lines
test-mint url        # http://127.0.0.1:8085/
```

State lives in `$TEST_MINT_WORK_DIR`, default **`/var/lib/cashu-test-state/mint`** — container-local
and deliberately **outside `/work`**. `/work` is the delivered workdir: it is bind-mounted from the
host and everything in it is handed to the buyer, so mint databases, logs, config and the seed file
must never land there. `test-mint` refuses to start if its state root resolves inside the delivery
directory, and `test-mint isolation` is the command that shows this (state root, whether it is
inside the delivered dir, state file names and sizes, seed mode, and any delivered-dir entry
matching mint state).

The state root is container-local, so it dies with the container either way. `reset` between test
runs; `restart` when I am specifically testing that balances survive a mint bounce (it keeps the
database and the seed).

The seed is written to a `0600` file under that root and passed to `cdk-mintd` as `--seed-file`.
Never echo it, never pass it as an argument value, never copy it into `/work`.

## Acceptance harness

`mint-acceptance` is baked in and needs no build step:

```sh
test-mint start && mint-acceptance
```

It runs eight legs — mint info, issue, send/receive, double-spend rejection, melt, failed payment,
restart recovery, loopback-only reachability — and asserts amounts, not statuses. It exits non-zero
on any failure and prints an assertion count. Read `/usr/local/bin/mint-acceptance`'s source in the
repo (`docker/maxplayer-cashu-sandbox/acceptance/src/main.rs`) for worked examples of every one of
those flows at CDK 0.17.2.

## Rules I do not break

- **This mint never goes in a seller's `accepted_mints`.** That set is the seller's real-money
  surface; `payment_wallet.rs` checks it twice (realized mint, and the NUT-18 payload mint). Test
  ecash in that set means the seat would accept worthless tokens as payment.
- **Loopback only.** It binds `127.0.0.1:8085`; `test-mint` refuses any non-loopback bind. Verified
  from `/proc/net/tcp` (`local_address` `0100007F`), and the container's own bridge address refuses
  the connection.
- **No real funds, ever.** The binary is built `--no-default-features --features fakewallet,sqlite`,
  so cln/lnd/lnbits/bdk/ldk-node are not compiled in. It *cannot* be pointed at a real backend.
- The seed is a throwaway BIP-39 mnemonic generated per `reset` into a `0600` file. Never print it,
  never put it in a deliverable, never pass it on a command line.

## Simulating failures

`cdk-fake-wallet` reads the BOLT11 **description** as a `FakeInvoiceDescription` JSON blob:
`pay_invoice_state`, `check_payment_state`, `pay_err`, `check_err`. Build the invoice with
`cdk_fake_wallet::create_fake_invoice(amount_msat, description_json)`.

To make a payment genuinely fail I must set **both** `pay_err: true` **and**
`check_payment_state: Unpaid` — see [[known-drift]]. Setting only `pay_err` yields a melt that
finalises as `Paid`.
