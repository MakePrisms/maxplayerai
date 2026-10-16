# Community challenge reader (v1)

Node 24 ESM, one direct dependency (`@noble/curves`, matching web/app). Uses Node's
WebSocket, HTTP, crypto and test runner; reuses the app's pure NIP-19 codec through
Node 24 type stripping. Run from this repository (not a standalone copied folder).
No buyer wallet, coordinator, payment execution, or event-publishing API.

```sh
npm ci --prefix web/community
npm start --prefix web/community
curl http://localhost:4910/api/challenges/current
# Open http://localhost:4910/review
npm test --prefix web/community
```

| Environment | Default / meaning |
| --- | --- |
| `RELAY_URL` | `wss://relay.maxplayer.ai` |
| `CHALLENGE_BUYER_PUBKEY` | `94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5`; npub or lowercase hex |
| `APP_AUTH_KEY_FILE` | `web/community/.state/auth-key`; app-only hex key, generated locally with mode 0600 |
| `PORT` | `4910` |
| `HOST` | unset = all interfaces; set e.g. `127.0.0.1` to bind one address |
| `CORS_ORIGIN` | `https://www.maxplayer.ai`; one exact origin |

Never supply a buyer key. The only signing function constructs kind 22242 for NIP-42
AUTH; no EVENT frame is sent. The auth identity must differ from the configured buyer.
Keep `.state/` private and persistent; never commit it. Restart rebuilds the projection
from relay history. This is read-only public data; the review page is not an admin API.

`GET /api/challenges/current` returns `asOf` (Unix seconds), challenge (or null), sellers,
jobs, budget, updates and relay readiness. `GET /api/challenges/current/stream` sends the
same JSON in SSE `data` messages, and a comment heartbeat every 15 seconds. Snapshots
change on verified events and running-minute ticks. Consumers must show incomplete or
stale status if relay authentication/history is not ready. The page extrapolates elapsed
seconds locally using a monotonic clock and caps each job at its deadline.

`/review` is plain read-only HTML with escaped member names/notes, advertised heartbeat
capabilities (with timestamp), minutes, completed jobs across known challenges, and
selectable accept/deny CLI commands. Run these in the **buyer's CLI home**. This service
cannot execute them. No external content is interpreted as HTML or executable markdown.

## Page configuration

Build `web/app` with `COMMUNITY_APP_URL=https://community.maxplayer.ai` for a separately
hosted service. Empty/unset uses same-origin `/api/challenges/...` (requires a reverse
proxy; none is deployed here). `/challenges` CSP permits only self and that HTTPS origin;
if choosing a different origin, change its route's CSP in `web/app/vercel.json` too.
Set the service's CORS origin to the actual frontend origin, including preview origins
when testing. Stream proxies must disable buffering. Nothing links this page from the
homepage/navigation; no deployment or DNS change is included.

## Verification and limits

```sh
npm test --prefix web/community
npm test --prefix web/app
npm run build --prefix web/app
node web/community/scripts/preview.mjs
```

The last command requires `google-chrome`, serves a local fixture (no relay), and writes
full-page desktop/390px screenshots plus overflow/target measurements under `.state/`.
It checks rendered data, not just a static shell. Build with the default same-origin URL.

Every ingested event's canonical NIP-01 id and Schnorr signature is verified locally.
Addressable replacements use timestamp then lowest id. Validation and tally are in
`src/events.mjs` / `src/model.mjs`; the buyer side is implemented separately and must
run parity fixtures (it cannot literally import this JavaScript module).

History uses per-kind 500-event pages, overlapping the boundary second; live subscription
is installed first, and reconnect replays history. A 500-event single-second boundary is
explicitly marked incomplete rather than silently skipped. Requests are paced. Kind-0
and 30340 are subscribed only for observed members. The in-memory history is not a
bounded archival database; a large buyer history increases startup time and memory.

In `crates/buzz/crates/buzz-relay/src/handlers/req.rs`, `handle_req` explicitly grants
anonymous open-read history **without live fan-out**. Authenticated global REQs register
live subscriptions. `apply_channel_scope_to_query` includes global data even with an
empty accessible-channel list; explicit channel filters require membership. Kind gates
still apply (p-gated, author-only, shared/result visibility). These challenge and public
job kinds are not those gated kinds. An app-only non-member identity can read their public
history/live feed, not membership-only or otherwise private data. Live verification must
not be confused with testing live publication: this service never publishes test events.

Budget is an estimate: 3405 has no amount; we use the fixed-price offer selected by the
award. A buyer-authored 3406 with matching offer root and unmarked claim moves it from
committed to paid estimate. That is authorisation, not proof of Cashu settlement. Fees,
refunds and private money state cannot be inferred. Missing amounts are counted explicitly.

## Persistent membership

30342 is one membership per seller/buyer (`d=p=buyer`, no `a`), with daily minutes,
`joined|left`, and `since`. 30343 is one persistent decision per buyer/seller
(`d=p=seller`, no `a`). Accepted membership carries into every daily challenge without
rejoining. Eligibility requires joined + accepted + `since <= starts`.
Leaving immediately blocks new assignments but preserves awarded work and settlement.
Minutes used include only jobs in the selected day's work window, so allowances reset
daily. The API includes `membershipStatus` and `since`; states include joined, accepted,
working, done, left and denied. Review commands are per seller, not per challenge.

Sellers can join at any time. Joining after work starts takes effect the next day. Use
`maxplayer-challenges members [--challenge <d>]` to compare CLI eligibility with the reader.
