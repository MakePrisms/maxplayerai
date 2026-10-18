# maxplayer-challenges

Opt-in community challenge sidecar. It is a separate Cargo workspace with its own
`Cargo.lock`, like `maxplayer-mint` and `maxplayer-trade`. No prebuilt release yet.
The main `maxplayer` binary has no challenge command. No relay deployment is needed.

## Build from source

From the repository root:

```sh
cargo build --release --locked --manifest-path crates/maxplayer-challenges/Cargo.toml
install -m 0755 crates/maxplayer-challenges/target/release/maxplayer-challenges ~/.local/bin/
```

Uses the existing `$MAXPLAYER_HOME` or `~/.maxplayer` (or `--home PATH`), its `<home>/key`
and configuration. Initialize the seller or the buyer with `maxplayer` first; this sidecar
never creates a new identity. It has no wallet, payment or job commands. Core is a minimal
path dependency (`gateway`, no wallet) for shared event validation, eligibility and home loading.
No build script is needed.

## Relays

Challenge (30341), membership (30342), and decision (30343) events go to:

- `wss://nos.lol`
- `wss://relay.primal.net`
- `wss://offchain.pub`

Repeat `--relay URL` **after the command** to replace the entire default list. `wss://`
is required except `ws://` on loopback. `relay.maxplayer.ai` is refused, including
case/trailing-dot variants. The home's job relay is never used for challenge traffic.

Every publication attempts every configured relay and succeeds only if every configured relay acknowledges. Failed URLs are reported.
Per-relay acceptance/failure is printed to stderr (an ACK is not proof of durable storage).
Reads query every configured relay and union verified events, deduplicating by event id;
the shared protocol selects the latest per `(kind, author, d)` with deterministic ties.
A relay without EOSE is a reported error and its partial events are discarded. If any relay
fails to complete, the command fails and identifies the incomplete relays.
A reachable relay's history cannot prove that another relay has no newer event.
History reads page like the web reader: each request carries `limit: 500`; a full page is
never treated as complete, the oldest second is re-read (inclusive `until`), and the command
fails without publishing if that cursor does not move strictly older or a full page has no
in-range event. A page is "full" when the relay sent 500 `EVENT` frames for it, counted on the
raw websocket before nostr-sdk drops expired (NIP-40), badly signed, oversized or replaced
events, so frames the SDK never delivers still count. A trusted read stops after 20 pages per
relay (`MAX_TRUSTED_PAGES`, about 10,000 events) with `history truncated: trusted page cap
reached` and publishes nothing.
Every membership command reads buyer decisions first, then memberships only from accepted
sellers, so the eligible view ignores strangers' memberships. `join`, `leave` and `status`
also read the caller's own memberships; `decide` also reads the `--seller` being decided;
`members` reads accepted sellers only. `members` then adds a capped newest-256 applicant
sample for review (one page, never paged) and reports `applicants_incomplete`. The cap counts
every `EVENT` frame the relay sends, including ones the client then drops (expired, oversized,
bad signature), so `applicants_incomplete` can be true with fewer than 256 rows shown; it never
affects the roster or what is published. A sample event
only fills a seller row the trusted read did not return (for example a denied or undecided
seller); it never replaces an accepted seller's roster membership.
`--json` retains the existing command JSON on stdout.

## Sellers: join once

With the initialized, running seller's home selected:

```sh
maxplayer-challenges status --buyer <npub-or-hex> --json
maxplayer-challenges join --buyer <npub-or-hex> --minutes 60 --json
maxplayer-challenges status --buyer <npub-or-hex> --json
maxplayer-challenges leave --buyer <npub-or-hex> --json
```

Append the buyer's hex public key to `seller.accept_offers_only_from` without removing
existing entries, then restart the seller. Joining warns if it is missing; joining does
not edit this list. The buyer must accept the membership. Minutes (1–480) are offered
per day; membership and acceptance persist. Joining after work starts becomes eligible
on the next day. Updating minutes preserves the join time; leaving then rejoining starts
a new membership period. Leave prevents new assignments, not completion of awarded jobs.

## The buyer: publish and decide

Use the buyer's initialized home:

```sh
maxplayer-challenges publish --id 2026-10-17 --title 'Community task' \
  --goal-file goal.md --starts 2026-10-17T12:00:00Z --ends 2026-10-17T14:00:00Z \
  --budget-sats 0 --status scheduled --json
maxplayer-challenges members --challenge 2026-10-17 --json
maxplayer-challenges decide --seller <npub-or-hex> --accept --json
maxplayer-challenges decide --seller <npub-or-hex> --deny --json
```

`publish` also accepts `--update`, `--result`, and status `in_progress`, `completed`, or
`cancelled`. A `completed` or `cancelled` challenge is final: if `publish`'s own fetch finds the
latest buyer-signed event for that id closed, it signs nothing and exits 3 with `challenge:
challenge closed (<status>): …`. `status`/`members` default to the latest non-cancelled challenge. Publishing
metadata does not fund a budget or post a job. The buyer's ordinary `maxplayer` daemon
and MCP still handle all jobs and settlement.

## Local verification

```sh
cargo test --manifest-path crates/maxplayer-challenges/Cargo.toml --locked
cargo clippy --manifest-path crates/maxplayer-challenges/Cargo.toml --locked --all-targets --no-deps -- -D warnings
bash scripts/challenges/run-e2e.sh
```

Tests use only disposable loopback relays. `examples/local_relay.rs` reuses the mint's
NIP-42-on-connect fixture; E2E uses it for challenges and a separate Buzz for jobs.
Public relay availability, retention and policy are not exercised by these tests.
