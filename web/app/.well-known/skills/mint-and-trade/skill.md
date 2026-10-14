---
name: maxplayer-mint-and-trade
description: Set up a seller's own token mint and standalone token trading together. Use for “set up my mint and token trading” or “issue my own tokens and sell them”. Preserves existing identity and custody, installs both binaries, connects the mint and trading home, and prepares an approved issuance and listing.
---

# Set up a mint and token trading

Complete both parts of setup from this one instruction. `maxplayer-mint` issues
and services the human's tokens; `maxplayer-trade` exchanges them for another
mint's tokens. Installing ordinary `maxplayer` does not install either binary.
These are service credits counted in sats, **not automatically bitcoin-backed
sats**. The credit mint has no Lightning cash-out. A listing sells only if a buyer
accepts the price; it is not guaranteed redemption.

## 1. Read the operating instructions and inspect existing state

Read these before installing or operating either tool:

- [Mint setup and operations](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-mint/README.md).
- [Trading skill](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-trade/skills/maxplayer-trade/SKILL.md), including its linked command, recovery and verification references.

Use the documents at the same repository revision as the binaries you build.
Check existing binaries, services, mint identity, seller configuration and trading
homes first. Reuse them. Never reinitialize a mint, replace its keys, delete a
database or start a second writer to fix a setup failure. Record the chosen
revision and paths without displaying secrets.

The mint lives under `<MAXPLAYER_HOME>/mint` (default `~/.maxplayer/mint`).
Use a **separate, private, persistent absolute trading home**, passed with
`maxplayer-trade --home <path>`. It is not the seller's ordinary Maxplayer wallet.
Do not invent a trading-home environment variable. Preserve all state across
restarts; do not move an existing funded home without a recovery plan.

## 2. Install and connect both tools

From one checkout of the chosen revision, install missing binaries using the mint
README and [trading build instructions](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-trade/README.md).
Both are separate Cargo workspaces; check Rust and
`protoc` prerequisites. From the repository root, the source installs are:

```sh
cargo install --locked --path crates/maxplayer-mint
cargo install --locked --path crates/maxplayer-trade
```

Check the installed commands' `--help`; do not guess flags or overwrite a running
installation without checking its current version and obligations.

- Initialize a mint **only if none exists**. Follow the README to run it under a
  persistent service appropriate to this host. Inspect and preserve existing
  service configuration. The mint's documented graceful stop is SIGINT; do not
  assume it uses the trading server's shutdown signal.
- Keep exactly one live mint. Establish a protected backup of the whole mint
  directory while the mint is stopped, and a procedure to keep that backup
  current. An old database is not a safe substitute for the latest spent-proof
  record. Never print seeds, keys, proofs, token contents or raw databases.
- Configure the trading client to reach the mint using compatible **mint relays**.
  These are distinct from **market relays**, which carry lots and trade messages.
  Follow the trading references; do not use `relay.maxplayer.ai` as a market relay.
- Run the trading skill's read-only readiness checks against the mint's public
  `nostr://` URL. Verify an actual mint reply, not just a running process or relay
  acceptance. Record the trading home and relay settings for future commands.
- If a Maxplayer seller already exists, follow the mint README to add this mint
  to its accepted mints and wallet. Preserve existing accepted mints and the first
  Lightning mint used for fees; restart only when safe for its existing work.
  If there is no seller, explain that issuing tokens does not create a seller or
  make anyone accept them. Use [seller-operate](../seller-operate/skill.md) if the
  human also wants to set one up; do not start an unconfigured seller.

**Setup is complete** when both binaries are usable, the mint service is running,
the trading client can reach it, and the persistent homes and backup procedure
are recorded. Report any host limitation instead of claiming a temporary process
will survive logout or reboot.

## 3. Prepare the first issuance and sale

Ask only for missing terms: how many tokens to issue and import, how many to sell,
which exact mint's tokens to receive, the net price, and the maximum fees/debit.
Do not invent a price or assume that “set up” authorizes issuance or trading.

1. Obtain approval for the exact issuance and import amount. Issue using the mint
   README. Keep the resulting token file private; do not paste it into chat or
   command arguments. If issuance is interrupted, resume that operation as
   documented rather than issuing another batch.
2. Import that file into the **trading home**, using the trading skill's `receive`
   workflow, not the ordinary seller wallet. Verify the receive state and balance.
   An ambiguous result needs recovery of the existing attempt, not new issuance.
3. Show the listing's `--dry-run` using the agreed mint pair, amounts and fee cap.
   Explain the debits, receipts and lock/recovery obligations from the trading
   skill, then obtain approval for publishing those exact terms. No automatic
   listing, purchase, funding payment, send or withdrawal during setup.
4. Publish only the approved listing and keep `serve` running as documented.
   Keep the mint available while its tokens remain outstanding, and trading
   recovery running while a swap is unresolved. Do not stop services merely
   because the setup conversation ends.

Report public mint URL, service status, chosen revision, persistent home paths,
readiness results and what awaits approval. After an authorized sale, additionally
report the lot ID and actual state; distinguish a published lot from a completed
trade. Never claim tokens sold merely because setup or listing succeeded.
