# Seller-credits end to end (spec stage 3)

Seller A runs `maxplayer-mint`. Seller B accepts A's mint after its Lightning mint. A buyer holds
only A's credits and hires B. B is paid in A's credits and remits its platform fee in real sats
from its Lightning mint.

Pieces:

- `examples/local_relay.rs`: a local NIP-42 relay. It challenges on connect, like
  relay.maxplayer.ai; core's receipt publisher needs that. `nostr-relay-builder` alone
  challenges only on the first read or write.
- `stub-acp-agent.mjs`: a stub ACP agent. It passes the seller's harness probe and answers every
  job inline, so no model and no git remote are needed.
- `mcp-call.mjs`: calls one buyer MCP tool (`post_job`, `get_job`, `collect`).

Binaries: `cargo build -p maxplayer --release --no-default-features --features wallet,acp` and,
in this crate, `cargo build --example local_relay` plus the `maxplayer-mint` bin.

## Run

```sh
local_relay 47810                                  # prints ws://127.0.0.1:47810
MAXPLAYER_HOME=$A maxplayer-mint init              # then mint.toml: relays = ["ws://127.0.0.1:47810"]
MAXPLAYER_HOME=$A maxplayer-mint run &
MAXPLAYER_HOME=$BUYER maxplayer whoami             # bootstraps; set relay_url, accepted_mints = [A]
MAXPLAYER_HOME=$A maxplayer-mint issue 300
MAXPLAYER_HOME=$BUYER maxplayer wallet receive "$(cat $A/mint/issued/<id>.token)"
MAXPLAYER_HOME=$B maxplayer whoami                 # relay_url; accepted_mints = [<lightning mint>, A]
MAXPLAYER_HOME=$B maxplayer wallet mints add <A's nostr:// URL>   # so B's wallet can spend what it earns at A
MAXPLAYER_HOME=$B maxplayer seller --agent-argv node --agent-argv $PWD/stub-acp-agent.mjs \
  --rate-sats 100 --accept-open-targeted --unsafe-no-sandbox --skip-doctor --non-interactive \
  --git-remote https://example.invalid/b.git &
MAXPLAYER_HOME=$BUYER node mcp-call.mjs maxplayer post_job \
  '{"task":"...","output":"text/plain","amount_sats":100,"max_sats":100,"seller_pubkey":"<B hex>"}'
MAXPLAYER_HOME=$BUYER maxplayer collect <job_id> --out e2e
MAXPLAYER_HOME=$B maxplayer seller fees            # 10 sats accrued; remitted once B holds sats
```

`--unsafe-no-sandbox` is safe here only because the stub agent runs no code. Against a public
relay, use `[seller] accept_offers_only_from = [<buyer hex>]` instead of `--accept-open-targeted`.

## Result (2026-09-24, local relay)

The job was claimed, awarded and delivered inline. B's node received the 100 credits through A's
mint (`mint_fee=0 fee_sats=10 kept=90`). The buyer's payment journal went intent → locked → sent
→ receipt_published → closed. B's fee remit was refused before spending while B held 0 sats at
its Lightning mint. After B was funded, B's retry paid 8 sats to `maxplayer@strike.me` plus a
1-sat melt fee.

The wallet only treats `accepted_mints[0]` plus `extra_mints` as its own mints. So a seller that
accepts a credit mint also runs `wallet mints add <credit mint>` (step above). Without it, B's
node still receives A's credits, but `maxplayer wallet` lists them as `role=unconfigured` and
`send`/`melt` refuse them. This run skipped that step, which is how the gap showed up. Checked
afterwards on this branch's binary: before `mints add`, `wallet send 5 --mint <A>` exited 2 with
"is not configured"; after it, the row read `role=extra` and the send went through.

## Result (2026-09-24, live on relay.maxplayer.ai)

Same homes, `relay_url = "wss://relay.maxplayer.ai"`, A's `mint.toml` `relays = []` (default +
fallbacks). B restricted to `accept_offers_only_from = [<buyer>]`, no open surfaces. Every process
ran under `strace -f -e trace=connect`.

- Job `683f3ef7…` (100 credits): claimed 05:10:57, awarded and delivered inline 05:10:58, B
  collect ok 05:11:02 (`mint_fee=0 fee_sats=10 kept=90`), fee remitted 05:11:06 (8 sats to
  `maxplayer@strike.me` + 1-sat melt fee, real sats from minibits). Buyer collect → `Closed`.
- Outbound connects, by process:
  - A's sidecar: relay.maxplayer.ai, relay.ditto.pub, nostr-pub.wellorder.net (443). Nothing else.
  - Buyer (MCP + daemon): the same three relays only. It never contacted any mint over HTTP.
  - B: the three relays, mint.minibits.cash (its Lightning mint), strike.me (LNURL). Never A over
    HTTP (A has no HTTP surface).
- No process listened on a TCP port.

Outbound HTTP was observed, not denied: this box has unprivileged user namespaces disabled, so no
network policy could be enforced. The connect trace counts as the stage 3 check (Bob, 24 Sep). Paths
this one job didn't exercise are covered by tests instead: all relays down is a clean error with no
HTTP fallback, and mint/melt are refused on a local-issue mint.
