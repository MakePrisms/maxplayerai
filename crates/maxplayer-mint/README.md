# maxplayer-mint — run a credit mint

`maxplayer-mint` lets a seller issue its own credits: Cashu ecash in the `sat` unit, one credit =
one sat, that buyers spend on jobs. It is a separate, opt-in binary. The default `maxplayer`
release has no mint code in it. Design and decisions: [`docs/specs/seller-credits.md`](../../docs/specs/seller-credits.md).

The mint has no HTTP server and opens no port. It only connects out to Nostr relays, and wallets
reach it there through its `nostr://npub1…` URL.

## Build

There is no prebuilt release of `maxplayer-mint` yet; build it from source (Bob, 24 Sep: revisit
once operators outside the team want to run a mint).

The mint needs `protoc` to build (Debian/Ubuntu: `sudo apt-get install -y protobuf-compiler`).
It is its own cargo workspace, so build it by manifest path from the repo root:

```bash
cargo build --release --locked --manifest-path crates/maxplayer-mint/Cargo.toml
install -m 0755 crates/maxplayer-mint/target/release/maxplayer-mint ~/.local/bin/
```

## 1. Create the mint

```bash
maxplayer-mint init
```

This creates `<home>/mint/`, where `<home>` is `$MAXPLAYER_HOME` or `~/.maxplayer` (the same home
your `maxplayer` seller uses). It prints the mint's `nostr://` URL and the two setup lines from
step 4. It refuses if `<home>/mint/` already exists.

`<home>/mint/` holds everything the mint is: `nostr.key` (its identity), `seed`, `mint.sqlite`
(keysets and spent proofs), `mint.toml`, and `issued/`. The directory is `0700`, the files `0600`.

**Back it up now, and keep exactly one live copy.**

- Losing it makes every credit the mint issued worthless. There is no platform backup.
- Restoring an **old** copy can let credits that were already spent be spent again, because the
  old database doesn't know about them. Restore only the latest copy, and never run two copies.

## 2. Configure (optional)

`<home>/mint/mint.toml`:

```toml
relays = []       # empty = wss://relay.maxplayer.ai + wss://relay.ditto.pub + wss://nostr-pub.wellorder.net
rate_limit = 20   # new requests per second, across all clients (replays of old requests don't count)
```

Wallets use the same default and fallback relays, so leave `relays` empty unless you know why.

## 3. Run it as a service

Credits can only be spent or moved while the mint is running, so run it like the seller daemon,
as a systemd user service:

```ini
# ~/.config/systemd/user/maxplayer-mint.service
[Unit]
Description=Maxplayer credit mint
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=%h/.local/bin/maxplayer-mint run
Environment=MAXPLAYER_HOME=%h/.maxplayer
UMask=0077
# `run` shuts down cleanly on SIGINT.
KillSignal=SIGINT
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now maxplayer-mint
loginctl enable-linger "$USER"    # without this the mint stops when you log out
journalctl --user -u maxplayer-mint -f
```

On start it logs `maxplayer-mint: serving nostr://npub1… on [relays]`. Only one `run` can serve a
mint directory: a second one exits with `another maxplayer-mint run is already serving`. A mint
killed mid-request finishes or undoes that request on the next start.

## 4. Accept your own credits

The issuer accepts its own mint, like any other seller that takes its credits. In your seller's
`<home>/config.toml`, **append** the URL to `accepted_mints`, after your Lightning mint:

```toml
accepted_mints = ["https://mint.minibits.cash/Bitcoin", "nostr://npub1…"]
```

Keep the Lightning mint first. The platform fee is paid in real sats from `accepted_mints[0]`.

Then let your wallet spend what it earns at the mint:

```bash
maxplayer wallet mints add nostr://npub1…
```

Without this step your node still takes payment in credits, but `maxplayer wallet balance` shows
them as `role=unconfigured` and `send`/`melt` refuse them. Restart `maxplayer seller` after
editing `config.toml`.

Any other seller who wants to accept your credits does the same two steps with your URL.

## 5. Issue credits

```bash
maxplayer-mint issue 1000
```

This writes `<home>/mint/issued/<id>.token` and prints its path. **That file is the credits:**
anyone holding it can spend them. Send it to the buyer privately. `issue` can run while the
service is running. If an issue is interrupted, the next `issue` (or `issue` with no amount)
finishes it with the same outputs instead of issuing again.

The buyer adds your mint, then receives the token:

```bash
maxplayer wallet mints add nostr://npub1…
maxplayer wallet receive "$(cat <id>.token)"
```

The buyer can then hire any seller that accepts your mint. Credits count against the buyer's
budget like sats, and your mint charges no fee.

## What isn't there yet

- No Lightning backend: credits are only issued with `issue`, and can't be bought or cashed out
  through the mint.
- No expiry, revocation or retirement of credits.
