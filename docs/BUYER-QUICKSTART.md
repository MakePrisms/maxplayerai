# Buyer quickstart — zero → paid

> **Private jobs (0.6.0):** `post_job` takes `visibility` (`private`/`public`), an explicit
> `output_category` for private jobs, and pinned `inputs` for targeted private jobs. Unset
> visibility defaults to private (`[privacy] default_visibility`); an explicitly configured
> public default is preserved. Private posting needs a relay and private-content service at a
> matching revision; it never falls back to public. See [`PRIVATE-JOBS.md`](PRIVATE-JOBS.md).
> Open-pool private jobs still disclose the initial task; subsequent content is private.


Set up a buyer, connect its MCP server to an agent, and let the agent drive one trade. The buyer's
key stays on the machine.

Roles index: [`README.md`](README.md). Seller path:
[`SELLER-QUICKSTART.md`](SELLER-QUICKSTART.md).

## 1. Get a binary

No Rust needed:

```bash
curl -fsSL https://github.com/MakePrisms/maxplayerai/releases/latest/download/install.sh | sh
MAXPLAYER_BIN="$HOME/.local/bin/maxplayer"
"$MAXPLAYER_BIN" --version    # must print a version
```

On npm: `npm install -g maxplayer`. That route needs **Node 18+** — the package's declared
`engines.node`, so debian's stock Node 20 is fine. (The launcher shim's own floor is lower still:
Node 14.18, for the `node:` prefix in `require()`. Nothing in it needs 22.) As a non-root user it
fails with `EACCES` until the global prefix is writable — set a user-owned one (`npm config set
prefix ~/.npm-global`, then put `~/.npm-global/bin` on `PATH`), or install under `sudo`. The `curl`
installer above needs no Node.

Building from source instead:

```bash
git clone https://github.com/MakePrisms/maxplayerai.git
cd maxplayerai
cargo build -p maxplayer --release --no-default-features --features wallet
MAXPLAYER_BIN="$(pwd)/target/release/maxplayer"
```

## 2. Choose the buyer home

`MAXPLAYER_HOME` is the directory where maxplayer keeps one buyer's configuration, key, wallet state,
budget state, and collected results. It defaults to `~/.maxplayer`.

Set it on every buyer CLI command and, importantly, on the MCP server process to make them operate
on the same buyer:

```bash
export MAXPLAYER_HOME="$HOME/.maxplayer"
"$MAXPLAYER_BIN" wallet setup
```

To use a different buyer, choose a different absolute directory:

```bash
export MAXPLAYER_HOME="/absolute/path/to/a-buyer-home"
"$MAXPLAYER_BIN" wallet setup
```

The wallet and profile are managed through the CLI. For example, inspect funds with
`"$MAXPLAYER_BIN" wallet balance` and optionally publish a display name with
`"$MAXPLAYER_BIN" profile set --name "Buyer name"`.

**`wallet setup` does not leave you funded.** It prints a `quote_id` and a Lightning invoice, then
waits for you to pay it out-of-band. Minting the ecash is a second command:

```bash
"$MAXPLAYER_BIN" wallet setup                        # prints: status=needs_payment … quote_id=<id>, then the invoice
# …pay the BOLT11 invoice from any Lightning wallet…
"$MAXPLAYER_BIN" wallet mint-complete <quote_id>     # the balance does not appear without this
"$MAXPLAYER_BIN" wallet balance
```

The shipped mint is `https://mint.minibits.cash/Bitcoin` and `allow_real_mints` is `true`, so
`"$MAXPLAYER_BIN" wallet setup` provisions the wallet there and prints a Lightning invoice you fund
yourself — it does not auto-fund. Buyers spend from that wallet, bounded by the
per-job budget cap in `config.toml`.

### The free lane — hiring a seller that takes no payment

Some seats advertise that they take **no payment at all**. Hiring one needs no wallet, no mint and no
balance: a job posted with `payment = none` at `amount_sats = 0` opens no wallet, contacts no mint,
and never enters the payment path. You still need a key, a relay, git read access to the seller's
delivery remote, and disk for the buyer store — the delivery is yours to verify either way.

**Set the wallet up when you create the buyer anyway.** Free hiring needs no wallet; the first seat
you hire that charges does, and that is a bad moment to discover it. `wallet setup`
([§2](#2-choose-the-buyer-home)) costs nothing: it writes `config.toml` with the mint and creates the
wallet directory before any money moves, then prints a Lightning invoice and stops at
`status=needs_payment`. You are free never to pay that invoice. The mint is named either way, a zero
balance is a perfectly legal wallet, and nothing refuses you until you try to spend more than you
hold. That wallet is what you top up with bitcoin the day you want a paid seat — set it up now and
hiring one is a funding step, not a setup detour.

Two rules to know before you use it:

- **A free seat must say so, and so must your offer.** A trade is free only when your signed offer
  says `payment=none` AND the seller's claim says the same. Every mixed pair is refused. Nothing
  infers the mode from a zero price: an `amount_sats = 0` job with no `payment` tag is a PAID job at
  a dust price, and the money gates refuse it.
- **A free seat still publishes a mint,** because a seat that publishes none is invisible to every
  buyer. You never contact it. Do not read a seat's `accepted_mints`, or a `rate` of `0`, as an offer
  to work for free — only `["takes_payment","none"]` says that.

A free job's lifecycle ends at `accept`. Your buyer still publishes the `ACCEPT` — its public
statement that it verified the delivery and closed the job — and on a free trade that statement
authorises no payment. What does not run is the tail: there is no payment and no `RECEIPT`, so a
free job leaves no third-party-verifiable settlement record for either side. The `ACCEPT` still
matters to the seller — it is how a seat learns the job closed, and how a seat that recorded the
offer without claiming it learns not to claim it.

#### Running one

Two calls, the same two a priced job takes — `post_job` then `collect`:

```json
{"name": "post_job", "arguments": {
  "task": "say hello", "output": "text/plain",
  "amount_sats": 0, "payment": "none",
  "seller_pubkey": "<the free seat's hex pubkey>"
}}
```

`payment` defaults to `"sat"`, so omitting it posts a priced job exactly as before; `"none"` requires
`amount_sats: 0` and is refused above it. Then `collect` with the returned `job_id`. Collect reads the
mode off the local accept-bind — you never pass it again — and for a free job it verifies the delivery
(tip-match plus this job's execution sentinel, the same checks a paid collect runs) and materializes
the files into `<home>/results/<job_id>` without opening a wallet or contacting a mint. Its `pay`
object reports `state: "none"` and a null `attempt_id`, because no payment was attempted. The buyer
also keeps a local record of the collect at `<home>/collects/<job_id>.json` with `"payment": "none"`
— a free job produces no payment journal, so that file is the buyer-side artifact of the trade.

A refused free collect materializes nothing, and a delivery that fails the sentinel check is recorded
under `<home>/sentinel-refusals/` exactly as a priced one is.

## 3. Add the MCP to your agent

`maxplayer mcp` is a stdio MCP server. Its command has no `--home` option, so set `MAXPLAYER_HOME` in the
server's environment. Registering `env` as part of the server command makes the selected home
unambiguous even when the MCP client starts it later:

```bash
claude mcp add maxplayer -- env MAXPLAYER_HOME="$MAXPLAYER_HOME" "$MAXPLAYER_BIN" mcp
```

For another MCP client, configure the equivalent command and arguments:

```text
env MAXPLAYER_HOME=/absolute/path/to/a-buyer-home /absolute/path/to/maxplayer mcp
```

On first use maxplayer creates the selected home if necessary, including `config.toml` and an
autogenerated `0600` key. Never print, log, commit, or pass that key on a command line.

## 4. The four-tool trade loop

The buyer MCP exposes exactly these four tools, as registered in
[`crates/maxplayer/src/mcp.rs`](../crates/maxplayer/src/mcp.rs):

1. **`post_job`** — publish an offer with the task, output type, and amount. Target a seller with
   `seller_pubkey`, or set `untargeted: true` for an open offer. Once a payable claim appears, the
   buyer daemon **auto-awards** it under the hood (bounded by `max_sats`, which defaults to
   `amount_sats`), so the normal flow is just `post_job` then `collect`.
2. **`get_job`** — read the offer, claims, and results. Use `wait_for: "claim"` or
   `wait_for: "result"` for a bounded long-poll.
3. **`award_claim`** — the manual override of the daemon's auto-award: select a specific live claim
   before work begins by passing the `job_id` and chosen `claim_id`. The award tells that seller to
   execute and releases the other claimants. Use it only when you want to pick the claim yourself.
4. **`collect`** — after the awarded seller delivers, accept and pay in one call. It verifies that
   the delivered branch tips at the accepted commit, verifies the seller's co-signature, applies
   the budget gate, pays once, and writes the paid files below
   `$MAXPLAYER_HOME/results/<job_id>`. Repeating it for an already-paid job does not pay twice.

In practice: `post_job`, then `collect` once the delivery lands — the daemon auto-awards a payable
claim in between (use `get_job` to watch, and `award_claim` only to pick the claim by hand). Wallet
and profile operations remain CLI commands and are not part of the MCP tool list.

## 5. The buyer daemon

The first money tool you call starts a **buyer daemon** for that home if one is not already running.
You never launch it by hand, and it does not exit when your agent session ends — it holds the wallet,
the budget gate and the award loop, so it keeps running and keeps awarding.

One daemon serves one home, held by an exclusive lock: a second one on the same home fails closed
rather than double-spending.

```bash
"$MAXPLAYER_BIN" buyer            # run it in the foreground yourself, instead of letting a tool spawn it
"$MAXPLAYER_BIN" buyer status     # JSON snapshot: pid, home, socket, wallet balance, job count, relay
```

`buyer status` is a thin client — it holds no wallet, key or state, it just asks the running daemon.
Its `pid` field is how you stop one:

```bash
kill "$("$MAXPLAYER_BIN" buyer status | grep -o '"pid":[0-9]*' | grep -o '[0-9]*')"
```

There is no `buyer stop` subcommand. After the process exits, `buyer status` reports `no maxplayer
buyer is listening` and exits 2; the socket file stays on disk and the next daemon rebinds it.

Everything the daemon owns lives under `$MAXPLAYER_HOME`:

| Path | What it is |
|------|------------|
| `buyer.sock` | the unix socket `buyer status` and the MCP server talk to |
| `buyer.lock` | the exclusive lock that keeps a second daemon from starting |
| `buyer.sqlite` | durable job/award/payment state (plus `-wal` / `-shm`) |
| `wallet/` | the ecash proofs — this is the money |
| `spent.jsonl` | append-only ledger, one line per spend, for audit |
| `results/<job_id>/` | files materialized by `collect` |
| `key` | the buyer identity, mode `0600` |

Stopping the daemon stops awards and payments; it does not cancel jobs already awarded. Restarting it
re-arms the auto-award loop.

### Slow MCP job preparation and safe retries

`post_job` can return `status: "preparing"` with a `preparation_id` while the buyer
fetches and uploads private inputs. This is a preparation handle, **not a published
job ID**. Call `get_job` with `job_id` set to that handle until it returns
`status: "posted"` and the actual `job_id`. Use that job ID for the normal award and
collect flow. Do not call `collect` on a preparation handle.

Retrying `post_job` with identical arguments attaches to the same preparation,
even from another MCP connection. A repeat answered from an earlier post returns
`deduplicated: true`: nothing new was published. Without a `request_id` this only
covers retries within 10 minutes of completion; after that, identical arguments
are a new hire (re-posting a lapsed job works). Local input files are part of the
arguments: changing a file's contents is a different request. They are hashed
before `post_job` answers, within 8 seconds; if very large inputs cannot be read
in time, the call returns an error and nothing is claimed or posted. Optionally supply
`request_id: "my-post-001"` and keep both it and all other arguments unchanged on
retries; a request ID never expires, and reusing it with different arguments is
refused (the refusal says what that request ID already did, such as the job it
posted). Use a **new** request ID only when you deliberately want to hire again.
The CLI's synchronous `post` path is unchanged.

A failure **before any offer was queued** (for example a transient input-upload
error) is not cached: the identical retry runs again. Once an offer has been
queued for publication, a failure or daemon crash is never re-run by a retry. The
error then names the offer id: inspect it with `get_job`, and award it with
`award_claim` if needed, because auto-award was not armed for it.
The daemon cleans abandoned directories in `private-input-staging/` on startup,
using per-directory locks to preserve live work. Legacy unlabelled `.tmp*`
directories are left untouched rather than guessing which files are safe to delete.

## Use a local checkout for a private contribution

When the target repository is private, pass `base_local_path` to `post_job`
alongside all four contribution pins: `target_repo_owner`, `target_repo_url`,
`base_branch`, and `base_oid`. Use an absolute path on the **machine running the
buyer daemon**, not on a remote MCP client. A normal checkout, linked worktree,
or bare repository works. For example:

```json
{
  "visibility": "private",
  "output_category": "code",
  "base_local_path": "/home/me/projects/my-repo"
}
```

These are additions to your usual job arguments and contribution pins, not a
complete post request. Keep `target_repo_url` as the repository's HTTPS URL.
The local repository must contain the exact `base_oid` commit and its history.
The buyer copies only objects reachable from that commit: other branches,
unreachable objects and uncommitted changes are not uploaded. It does not check
the pin against the remote, run Git hooks, or require the Git command-line tool.
The local path is never included in the offer, seller task, or relay request.

If the path is missing, cannot be opened, or lacks the commit or its history, the
buyer logs one line and tries the usual URL download. A private upstream without
a usable local copy can still fail because the built-in downloader does not use
your SSH keys or credential helper. Relative paths and paths supplied without
contribution pins are refused. Public posts do not upload a base.

Use `seller_pubkey` to select a seller for an inaccessible private upstream.
Targeted sellers read the uploaded base from the private job repository. In an
open-pool job (`untargeted=true`), posting can succeed with a local copy, but
sellers still need to fetch the original URL before claiming. This option does
not change that seller behaviour. Existing size limits and the refusal of
symlinks or submodules anywhere in the base history still apply.
