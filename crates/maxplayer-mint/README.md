# maxplayer-mint — run a credit mint

`maxplayer-mint` lets a seller issue its own credits: Cashu ecash in the `sat` unit, one credit =
one sat, that buyers spend on jobs. It is a separate, opt-in binary. The default `maxplayer`
release has no mint code in it. Design and decisions: [`docs/specs/seller-credits.md`](../../docs/specs/seller-credits.md).

The mint has no HTTP server and opens no port. It only connects out to Nostr relays, and wallets
reach it there through its `nostr://npub1…` URL.

## Agent setup: mint + trading

For an agent to set up both tools in one flow, give it this instruction:

```text
Read https://www.maxplayer.ai/.well-known/skills/mint-and-trade/skill.md and set up my mint and token trading.
```

The [combined skill](../../web/app/.well-known/skills/mint-and-trade/skill.md) covers
installation, existing mint state, persistent services, trading from the same maxplayer home
and readiness checks. The agent asks for issuance and sale terms before issuing or
listing tokens. Mint-only setup is documented below.

## Install

These commands need a `maxplayer` that has `maxplayer mint` (check `maxplayer mint --help`).
`maxplayer-mint` is released with `maxplayer`, from the same tag and at the same version, for
linux-x64, linux-arm64, darwin-arm64 and darwin-x64, as GitHub release archives
(`maxplayer-mint-<version>-<platform>.tar.gz`, listed in the release's `SHA256SUMS`). There is no npm
package. No `protoc` or Rust toolchain is needed. Install it with `maxplayer` itself:

```bash
maxplayer mint install --name <name>   # downloads the mint for THIS maxplayer's version, checks
                                       # SHA256SUMS and its version line, installs it into
                                       # ~/.local/bin (--bin-dir to choose), then runs
                                       # `maxplayer-mint init` (an existing <home>/mint/ is kept)
maxplayer mint                         # what is installed, the mint URL, running?, linked?, updates
```

Every `maxplayer-mint` command below is also reachable as `maxplayer mint <command>`:
`maxplayer mint [--home <dir>] [--bin-dir <dir>] <init|name|run|issue|restore|version> [args...]`
runs the same `maxplayer-mint` that `maxplayer mint` reports (`--bin-dir`, else the first on PATH,
else `~/.local/bin`) with `MAXPLAYER_HOME` set to that home, so it can never pick up another copy
of the mint on PATH or serve `~/.maxplayer` by mistake. Arguments after the command go to the mint
unchanged (`maxplayer mint restore --from <dir>`, `maxplayer mint init --name <name>`); put
`--home`/`--bin-dir` before it. `maxplayer` replaces itself with the mint process, so Ctrl-C and
`kill` reach `run` directly and the exit code is the mint's. One alias then covers a mint kept
outside the defaults, for every command including `install`/`upgrade`/`uninstall`:

```bash
alias seller-mint='maxplayer mint --home ~/sellers/acme/.maxplayer --bin-dir ~/sellers/acme/bin'
seller-mint install --name acme   # the first time
seller-mint run                   # serve it (foreground)
seller-mint issue 1000
```

Then link the mint key to your Maxplayer account, with the email your account already uses. It
emails a code; run the same command again with the code:

```bash
maxplayer account link --mint --email <your account email>
maxplayer account link --mint --code <6 digits from the email>
```

### Upgrade

The mint is version-locked to `maxplayer`. After updating `maxplayer` (the way you installed it:
the install script, or `npm install -g maxplayer@latest`), upgrade the mint to match:

```bash
maxplayer mint upgrade                    # replaces an OLDER released maxplayer-mint; keeps <home>/mint/
systemctl --user restart maxplayer-mint   # a running mint keeps the old version until restarted
```

`upgrade` downloads and checks the release exactly like `install`, and swaps the binary in one
rename, so a running mint is never left with a half-written file. It does nothing when the mint is
already at this version, and refuses a newer version, a source build or an unrecognised binary
(`maxplayer mint install --force` replaces anything). It never touches `<home>/mint/` and never runs
`init`. `maxplayer mint` and `maxplayer doctor` carry two separate notices. One says the installed
mint is older than `maxplayer` and to run `maxplayer mint upgrade` (a local check). The other says a
newer `maxplayer` is released and to update it the way you installed it; it shows with or without a
mint and does not mention the mint (checked against GitHub's latest release, a few seconds at most;
`--offline` skips it on both commands, and a failed check shows "could not check"). After you update
`maxplayer`, the first notice is what tells you to upgrade the mint.

### Uninstall

```bash
maxplayer mint uninstall   # removes the maxplayer-mint binary only
```

It refuses while `maxplayer-mint run` is serving the home (stop it first:
`systemctl --user disable --now maxplayer-mint`). A symlink is
removed, not the file it points to (the link's target is left as it was). It **never** deletes `<home>/mint/`: the key, seed and ecash
database stay, and `maxplayer mint install` picks the mint up again. Deleting that folder destroys
the mint and every credit it issued; back it up first if you really mean to delete it.

### Build from source

The mint needs `protoc` to build (Debian/Ubuntu: `sudo apt-get install -y protobuf-compiler`).
It is its own cargo workspace, so build it by manifest path from the repo root:

```bash
cargo build --release --locked --manifest-path crates/maxplayer-mint/Cargo.toml
install -m 0755 crates/maxplayer-mint/target/release/maxplayer-mint ~/.local/bin/
```

## 1. Create the mint

`maxplayer mint install --name <name>` already did this step if `<home>/mint/` did not exist yet.
To create the mint by hand (for example after building from source):

```bash
maxplayer-mint init --name <name>      # or: maxplayer mint [--home <dir>] init --name <name>
```

**Give your mint a name.** It is what wallets show next to your mint's URL
(`maxplayer wallet balance` prints `name="<name>"`), and it is how holders tell your credits from
everyone else's: a `nostr://npub1…` URL alone means nothing to them. Use a name people already
know you by, ideally your seller name. Like a seller's profile name it is self-chosen, so the
`nostr://` URL stays the mint's identity.

Without `--name` the mint is called `maxplayer-mint-<first 8 hex of its key>`, the same way an
unnamed seller is `maxplayer-seller-<8 hex>`. That works, but nobody will recognise it. Name or
rename an existing mint at any time with `maxplayer-mint name <name>`, then restart `run`; holders
see the new name after their wallet next talks to the mint, or right away after
`maxplayer wallet mints add <url>`. Names are up to 64 characters, without `=`, `"` or `\`, and
without leading or trailing spaces.

This creates `<home>/mint/`, where `<home>` is `$MAXPLAYER_HOME` or `~/.maxplayer` (the same home
your `maxplayer` seller uses). It prints the mint's `nostr://` URL and the two setup lines from
step 4. It refuses if `<home>/mint/` already exists.

`<home>/mint/` holds everything the mint is:

- `nostr.key` and `seed`: the mint itself. Whoever holds them can issue credits under your mint.
- `mint.sqlite`: the keysets and every proof already spent.
- `mint.toml`: relays, rate limit, name and `backup_dir`.
- `issued/`: one token file per `issue`. A file nobody has received yet is spendable credits.
- `backups/`: the automatic backups `run` keeps (below).

The directory is created `0700`, and that mode is what keeps it private. `nostr.key`, `seed`,
`mint.toml` and the token files are `0600`, but cdk creates `mint.sqlite` under your umask, so
never loosen the directory mode.

**Backups are automatic.** While `run` serves, it keeps the backup directory
(`<home>/mint/backups/`, or `backup_dir` in `mint.toml`) current by itself:

- A consistent snapshot of `mint.sqlite`, taken online with SQLite `VACUUM INTO` (no stopping
  the mint): once at start, within about 30 seconds of any change (every trade, receive and
  `issue`), and once more on a clean stop. The newest 10 are kept as
  `mint-<sequence>-<UTC time>.sqlite`, each written atomically and `0600` in a `0700` directory.
- Copies of `nostr.key`, `seed`, `mint.toml` and every `issued/*.token`, so the backup directory
  alone is a complete restore set.
- A snapshot that fails is logged as `maxplayer-mint: BACKUP FAILED …` and retried; the mint keeps
  serving.

**The one manual step, once at setup: get the backup directory off this machine.** A backup on
the same disk does not survive losing that disk. Either set `backup_dir` in `mint.toml` to a
mounted off-box location (network share, a second disk, a synced folder) and restart `run`, or
have your existing backup or sync tool copy `<home>/mint/backups/` off the box on its own
schedule. Nothing else is needed after trades: never stop the mint to back it up.

- Losing `nostr.key` or `seed` makes every credit the mint issued worthless. There is no
  platform backup.
- An **old or missing** `mint.sqlite` can let credits that were already spent be spent again:
  with the seed still there, the mint comes back with the same keys and no record of what was
  spent. Never delete `mint.sqlite` to fix a problem, and never run two copies. `run` refuses to
  start when `mint.sqlite` is missing or older than the newest backup.
- Protect the backup directory like the mint: it can issue credits and holds every unreceived
  token.

**Restore** only the NEWEST backup, with the service stopped:

```bash
systemctl --user stop maxplayer-mint
maxplayer-mint restore                 # from backup_dir; or: maxplayer-mint restore --from <dir>
                                       # (same as: maxplayer mint [--home <dir>] restore [--from <dir>])
systemctl --user start maxplayer-mint
```

`restore` takes the newest snapshot and nothing older, moves the replaced `mint.sqlite` aside
(`<home>/mint/replaced-<time>/`, never deleted), and recreates `<home>/mint/` from the backup
directory if it is gone, including missing `issued/*.token` files. It refuses another mint's
backups (checked against the seed even when only snapshots are there), a database that already
holds everything in the newest snapshot, and one that has diverged from it (each holds spends the
other lacks). `run` likewise refuses an unreadable `mint.sqlite` while backups exist.

## 2. Configure (optional)

`<home>/mint/mint.toml`:

```toml
relays = []       # empty = wss://relay.maxplayer.ai + wss://relay.ditto.pub + wss://nostr-pub.wellorder.net
rate_limit = 20   # new requests per second, across all clients (replays of old requests don't count)
name = "io-seller" # shown by wallets next to the mint URL; unset = maxplayer-mint-<8 hex>
backup_dir = "/mnt/offbox/maxplayer-mint"  # automatic backups; unset = <home>/mint/backups (relative = from <home>/mint)
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
NoNewPrivileges=yes
# `run` shuts down cleanly (taking a last backup) on SIGINT, SIGTERM or SIGHUP.
KillSignal=SIGINT
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
```

The unit runs as your user, so the mint can read the rest of `~/.maxplayer`, including the
seller wallet. Don't count on systemd's filesystem options (`ProtectSystem=`, `ReadWritePaths=`,
`PrivateTmp=`) to fence it in: in a user unit they need unprivileged user namespaces, and where
those are restricted they can have no effect without any error. On our Ubuntu 24.04 host
(systemd 255) they didn't stop the service writing outside `mint/`. To keep the mint away from
the seller wallet, run it as a separate Unix user with its own `MAXPLAYER_HOME`.

```bash
systemctl --user daemon-reload
systemctl --user enable --now maxplayer-mint
loginctl enable-linger "$USER"    # without this the mint stops when you log out
journalctl --user -u maxplayer-mint -f
```

The unit runs the binary directly, not `maxplayer mint run`: systemd already sets
`MAXPLAYER_HOME` and the absolute `ExecStart` path, and this way the service keeps working whatever
`maxplayer` is (or isn't) on the service's PATH and does not depend on the two versions matching.

### Without systemd (macOS, or any OS)

`run` stays in the foreground until it is stopped, so keep it in something that outlives your
terminal (tmux, screen, a launchd agent on macOS):

```bash
maxplayer mint --home ~/.maxplayer run                        # Ctrl-C stops it cleanly, with a last backup
tmux new-session -d -s maxplayer-mint 'maxplayer mint run'    # detached, default home
```

Add `--bin-dir <dir>` when the mint is not the one on PATH. This is the same as
`MAXPLAYER_HOME=<home> <bin-dir>/maxplayer-mint run`, without the risk of starting another copy of
the mint or the wrong home. A mint whose version differs from `maxplayer` still starts, after a
warning with the fix `maxplayer mint` prints (usually `maxplayer mint upgrade`).

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
them as `role=unconfigured` and `send` refuses them. With it, your wallet can send, receive and
pay jobs directly with the credits. It never melts them: `wallet melt` and cross-mint hops refuse
any `nostr://` mint, configured or not, and the mint itself serves no mint or melt. Restart
`maxplayer seller` after editing `config.toml`.

Any other seller who wants to accept your credits does the same two steps with your URL.

## 5. Issue credits

```bash
maxplayer-mint issue 1000
```

This writes `<home>/mint/issued/<id>.token` and prints its path. **That file is the credits:**
anyone holding it can spend them. Send it to the buyer privately. `issue` can run while the
service is running. If an issue is interrupted, the next `issue` (or `issue` with no amount)
finishes it with the same outputs instead of issuing again.

The buyer receives the token and adds your mint in the same step:

```bash
maxplayer wallet receive --add-mint --token-file <id>.token
```

Without `--add-mint`, `receive` refuses a token from a mint the wallet does not have, and the
token stays unredeemed. On a terminal it asks first (`Add it and receive? [y/N]`). Running
`maxplayer wallet mints add nostr://npub1…` before `receive` works too.

`--token-file` (or `-` with the token piped on stdin) keeps the token out of shell history and the
process list; avoid passing it as a command-line argument.

The buyer can then hire any seller that accepts your mint. Credits count against the buyer's
budget like sats, and your mint charges no fee.

## What isn't there yet

- No Lightning backend: credits are only issued with `issue`, and can't be bought or cashed out
  through the mint. `maxplayer wallet melt` refuses them too.
- No expiry, revocation or retirement of credits.

Live `info` overlays `version: maxplayer-mint/<package version>` and `maxplayer_mint`
(schema 1, implementation/version/build, envelope versions, holder operations, durable
replay and expiry enforcement) on stored metadata. Operator name/contact/other fields
are preserved. `MINT_SIDECAR_BUILD` may pin build provenance when building outside Git;
otherwise the build embeds the checkout revision. Capabilities are advertisements, not
proof of spent-witness emission or future availability.
