---
name: maxplayer-mint-and-trade
description: Set up a seller's own token mint and token trading together. Use for “set up my mint and token trading” or “issue my own tokens and sell them”. Preserves existing identity and custody, installs the mint, connects the mint and the maxplayer home, and prepares an approved issuance and listing.
---

# Set up a mint and token trading

Complete both parts of setup from this one instruction. `maxplayer-mint` issues
and services the human's tokens; `maxplayer trade` (part of the `maxplayer` CLI) exchanges
them for another mint's tokens. Installing ordinary `maxplayer` does not install the mint.
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
Trading uses the **same maxplayer home**: `maxplayer trade` signs with its `key`, pays from
its wallet, and keeps its journal in `<MAXPLAYER_HOME>/trade` (pass `--home <path>` only to
pick a home other than `$MAXPLAYER_HOME`/`~/.maxplayer`). Only the home's mints can be traded: the
default, `extra_mints` (`maxplayer wallet mints add`) and, on a seller, `accepted_mints`. A seller
can trade from its own home: job payments land in the same wallet. An older standalone trading
home (with `wallet.seed`/`trade.key`) is drain-only: move its funds with its `send`, then
`maxplayer wallet receive --token-file`. Money in and out (fund, receive, send, pay an
invoice) is `maxplayer wallet`; `maxplayer trade` lists, takes and recovers. Preserve all state
across restarts.

## 2. Install and connect both tools

Install the mint with `maxplayer` itself. It downloads the released `maxplayer-mint` for **this**
`maxplayer`'s version (GitHub release archive, checked against the release's `SHA256SUMS` and the
version it reports; no npm package, no `protoc`), installs it into `~/.local/bin` and runs
`maxplayer-mint init` only when `<home>/mint/` does not exist yet:

```sh
maxplayer mint                       # read-only: installed?, version, mint URL, running?, linked?, updates
maxplayer mint install --name <name>
maxplayer account link --mint --email <your account email>   # emails a code
maxplayer account link --mint --code <6 digits from the email>
```

Use the email the human's Maxplayer account already uses. The mint is version-locked to
`maxplayer`: after updating `maxplayer` (the way it was installed), run `maxplayer mint upgrade`
(it replaces only an older released mint, keeps `<home>/mint/`, never runs `init`), then restart
the mint service; a running mint keeps the old version until restarted. `maxplayer mint` and
`maxplayer doctor` say when a newer mint is available. `maxplayer mint uninstall` removes only the
binary (refused while the mint is serving) and never `<home>/mint/`; never delete that folder, it
is the mint and every credit it issued. To build a chosen revision from source instead, use the
mint README: the mint is its own Cargo workspace, and building it needs `protoc`. From the
repository root:

```sh
cargo install --locked --path crates/maxplayer-mint --root "$HOME/.local"
```

Trading needs no install: it is `maxplayer trade`. If `maxplayer trade --help` fails, the
installed `maxplayer` predates it; upgrade it (see the
[trading build instructions](https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-trade/README.md)).
Delete any old standalone `maxplayer-trade` binary (`maxplayer doctor` warns about one).

This explicitly installs the mint in `~/.local/bin`, matching the mint
README's `ExecStart=%h/.local/bin/maxplayer-mint run`. Use those absolute executable
paths for services and checks; add `~/.local/bin` to the interactive PATH if needed.
For an existing installation elsewhere, preserve it and use its verified absolute
path in the service instead. Verify the service's executable exists and is the
chosen build before starting it; do not rely on the service inheriting shell PATH.
Check the installed commands' usage; do not guess flags or overwrite a running
installation without checking its current version and obligations. The mint prints
usage and exits 2 for `--help`; that alone is not an installation failure. An
existing `maxplayer-mint` whose usage has no `restore` predates automatic backups:
upgrade it (`maxplayer mint upgrade` for an older release, or from the chosen revision at the
same path), then restart its service, and drop any stopped-backup procedure recorded for it
earlier.

- Initialize a mint **only if none exists**, with `maxplayer-mint init --name <name>`: wallets
  show the name next to the mint URL. Use the seller's own name unless the operator chose another.
  For an existing unnamed mint, suggest `maxplayer-mint name <name>` and a service restart. Follow the README to run it under a
  persistent service appropriate to this host. Inspect and preserve existing
  service configuration. The mint's documented graceful stop is SIGINT; do not
  assume it uses the trading server's shutdown signal.
- Keep exactly one live mint. **Backups are automatic:** `maxplayer-mint run`
  snapshots the database online after every change and keeps the key, seed,
  config and issued token files in its backup directory (`<home>/mint/backups/`,
  or `backup_dir` in `mint.toml`). The only backup step is **one-time, at setup**:
  ask the human to get that directory off this machine, either by pointing
  `backup_dir` at off-box storage (then restart the mint) or by adding it to an
  existing off-box backup or sync. Record which. **Never ask the human to back up
  or stop the mint after a trade, issue or receive**, and never stop it to copy
  files: the backups are already current. Restore only with `maxplayer-mint
  restore` (newest backup only, mint stopped). Never print seeds, keys, proofs,
  token contents or raw databases.
- Configure the trading client to reach the mint using compatible **mint relays**.
  These are distinct from **market relays**, which carry lots and trade messages.
  Follow the trading references. `relay.maxplayer.ai` is the first default market relay
  (it stores lots only from relay members); public relays are the fallbacks. To use it, link
  the mint key (`maxplayer account link --mint`) and the home key (`maxplayer account link`;
  trading signs with it, so no separate trading key exists), and run `doctor` with
  `--home <home>`. Add the mint to the wallet: `maxplayer wallet mints add nostr://<npub>`.
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
the trading client can reach it, and the persistent homes and the backup
directory's off-box location are recorded. Report any host limitation instead of claiming a temporary process
will survive logout or reboot.

## 3. Prepare the first issuance and sale

**Check capacity before issuing.** `maxplayer-mint issue` does not enforce the
trading limits. Explain these limits before asking for the amount, then validate
the proposed terms against the chosen mint and existing trading home:

- Each trade lock is capped at **100,000 sats gross**, including its required
  fee allowance, not just the advertised net sale amount. Check both legs using
  the trading references and current mint fees; a 100,000-sat net lot may not fit.
- A receive permits at most **128 input proofs** (and at most 128 fresh output
  proofs). For `nostr://`, its swap must also fit one **NIP-44 request**. Check the
  chosen revision's issuance split and receive sizing rules before issuing;
  a face amount under the cap alone is not proof that an arbitrary token fits.

If fee-inclusive locks or proof/payload fit cannot be established, stop before
issuance and report the uncertainty. Do not test capacity by issuing spendable
tokens, silently lower amounts, split batches to evade a cap, or create a new home
to bypass it. Propose supported terms for
fresh approval. Recheck capacity immediately before issuance if the home changed.

Ask only for missing terms: how many tokens to issue and import, how many to sell,
which exact mint's tokens to receive, the net price, and the maximum fees/debit.
Do not invent a price or assume that “set up” authorizes issuance or trading.

1. After the capacity check, obtain approval for the exact issuance and import amount. Issue using the mint
   README. Keep the resulting token file private; do not paste it into chat or
   command arguments. If issuance is interrupted, resume that operation as
   documented rather than issuing another batch.
   No backup step follows `issue`, `receive` or a trade: `run` already has it.
2. Import that file into the home's wallet with `maxplayer wallet receive --token-file <file>
   --add-mint` (it lands in the same wallet `maxplayer wallet balance` shows). Verify the
   balance. An ambiguous result needs the same file received again, not new issuance.
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
