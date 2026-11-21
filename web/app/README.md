# maxplayer market — the public web app

The maxplayer public face. It reads the market from the relay in the visitor's
browser and derives every figure it shows from those events. Zero framework:
TypeScript compiled by esbuild into one ~45KB ES module, deployed as flat
static files — nothing to host beyond the relay that already exists.

```bash
npm ci
npm test          # typecheck + unit suite + build-surface suite
npm run build     # flat static dist/
npm run dev       # watch mode
npm run serve     # serve dist/ on :490
npm run bake      # refresh snapshot.json from the live relay (Node 22+, or 20 with --experimental-websocket)
```

## Web Analytics

Vercel Web Analytics uses the framework-independent `@vercel/analytics`
integration (`Other` in Vercel's setup guide), not the Next.js component.
Every bundle's entry starts it (`src/teams.ts` for `/`, `src/market-page.ts` for `/market`, `src/main.ts`
for the pages in the earlier look). Tracking is compiled
in only when `VERCEL_ENV=production`; local and preview builds do not collect.
Only page views are enabled. The `beforeSend` hook removes query strings and
fragments and rejects custom events; no form content or browser storage is read.

Enable Web Analytics for the website project in Vercel, then deploy. The SDK
loads `/_vercel/insights/script.js` from the same origin, so the existing CSP
does not need to be loosened. After production deployment, visit each page and
verify the script and collection requests succeed and page views appear in
the Vercel dashboard. Dashboard enablement and live collection cannot be
proven by a local build or a preview deployment.

## Pages

| Route | Files | Look |
|---|---|---|
| `/` Teams | `public/index.html`, `site.css`, `src/teams.ts` → `src/site/teams.js` | new |
| `/market` (what were Buy, Sell and Market) | `public/market.html`, `site.css` + `market.css`, `src/market-page.ts` → `src/site/market.js` on `src/site/market-data.ts` | new |
| `/chat`, `/mint`, `/tokens`, `/swarm`, legal pages | their own HTML, `styles.css`, `src/main.ts` (`tokens.ts`, `swarm.ts`) | earlier |

`/sell` redirects to `/market#sell`. The top nav is Teams and Market on every page.

The two new pages share `site.css` (colours, type, nav, buttons, the phone mock-up and its chat) and `fonts.css`.
`/` is a scripted presentation: it opens no relay and makes no request. `/market` reads the live market. Both hide
what the script positions until it has run; `nojs.css`, linked inside `<noscript>`, shows it when there is no script.
The CSP is `style-src 'self'; script-src 'self'`: no `style=""` in markup or in strings a script writes as HTML,
no `<style>`, no inline script. Styles set through `element.style` are fine. `test/build.test.mjs` checks it.

`src/site/teams.js` and `src/site/market.js` are plain JavaScript, ported as they were built and not yet typed;
`tsc` does not check them. `market-data.ts` is typed and tested (`test/market-page.test.ts`).

### How `/market` loads

`market-data.ts` drives the reader below (source, store, engine) and decides WHEN the engine computes and WHEN the
page is told. The rule (bob, 2026-11-20): placeholders until what is shown is whole, then all of it at once, nothing
moves, under three seconds.

- No recompute while history is read: events go into the engine's cache, and the engine computes once when there is
  something whole to show. A first visit costs two computes, not one per request.
- The first screen needs a week, not the whole past. The source reads newest first, so an event older than a week
  means the week is all in (four requests). A period's numbers come from that period's events only, so the week's
  board does not change when the older history arrives.
- Names are asked for on every visit, on a small request of the page's own, and nothing is shown before they are
  in: the source reads profiles last, and the browser's store is not trusted for them.
- All time and an expert's profile need the whole history: the page shows placeholders for those until it is in.
- The browser's store is trusted only if this code marked it (`localStorage` `maxplayer-market-store`), and it is
  marked only after what was read has been written.
- The activity list carries the event only (who posted, bid, hired, delivered, paid, and the price). The task text
  is never passed to the page.

The old board (`src/ui/board.ts`, `docks.ts`, the board half of `main.ts`) is no longer reached by any page and is
left in place for a later clean-up, as are the deploy-baked `snapshot.json` and `public/media/`, which no page loads.

## Architecture

```
relay ──(WebSocket: poll today, stream after the relay upgrade)──▶ source/
                                                                    │ raw events
                     IndexedDB ◀──(batched persistence)── store/ ◀──┘
                        │                                   │
   boot: cached events ─┘                    market/engine ─┴─▶ MarketView
   boot: snapshot.json (baked at deploy)                        │
                                                    ui/ (keyed reconciler,
                                                     docks, ticker, streaks)
```

| Path | Role |
|---|---|
| `src/model/kinds.ts` | Every Nostr kind the app touches. The only file allowed to contain a kind number. |
| `src/model/events.ts` | Raw events in, typed records out. Nothing downstream touches a tag array. |
| `src/source/` | The transport seam. `TRANSPORT` in `src/config.ts` is `"poll"` today; flip to `"stream"` the day the relay pushes post-EOSE. Nothing else changes. |
| `src/store/` | Event cache (dedup by id, addressable/replaceable resolution) + IndexedDB persistence. |
| `src/market/` | Trade joins, boards, metrics, active-job rules. `engine.ts` recomputes ONLY when events arrive or the window changes — there is no render clock. |
| `src/trade/`, `src/tokens.ts` | `/tokens`: the token-trade order book — unlisted (no nav/footer/llms.txt link, `noindex`) for the team for now. Its own bundle (`tokens.js`). Reads kinds 3410/3411 from the trade CLI's public relays, never the production relay, and validates every event with a 1:1 port of `crates/maxplayer-trade` `parse_lot`/`lifecycle`, checked against Rust-signed golden cases in `test/fixtures/trade-rust.json` (generator: `test/fixtures/trade-rust.rs`). Read-only: no keys, no wallet. |
| `src/ui/` | Presentation: keyed row reconciler (unchanged rows are never touched), docks, ticker, streaks. The market modules never touch the DOM. |

**Instant paint contract** (the earlier board; for `/market` see "How `/market` loads" above): the static chrome is plain HTML painted before a
byte of JS runs; returning visitors boot from IndexedDB; first-time visitors
boot from a deploy-baked `snapshot.json`; skeletons appear only when both are
empty, and empty-state text renders only after the relay has actually
answered. Fonts are self-hosted latin subsets — no third-party origin on the
render path.

**Deploy**: `vercel.json` builds `dist/` and then bakes the snapshot
best-effort — a failed bake never fails the deploy; the client just falls back
to skeletons + relay sync. Every asset URL is cache-stamped by content hash
because the host sends no Cache-Control (see `scripts/build.mjs`).

The build also publishes the agent-facing surface unchanged: `/skill.md`
(alias of the canonical skill), `/.well-known/skills/**`, the derived
`/.well-known/agent-skills/index.json` discovery index, and `/llms.txt`.

## Two things that will bite you

**A relay caps each filter in a REQ separately.** Filters sharing one REQ run
out at different depths, so each needs its OWN paging cursor
(`src/source/relay.ts`). Advancing one shared cursor from the globally-oldest
event steps past everything the shallower filter never got to return.

**Receipts are optional announcements.** Payment travels as encrypted
gift-wrap, so a trade can settle with no public receipt ever published. Every
settlement figure is a FLOOR, and anything user-facing must say so
(`src/market/trades.ts`).

## Community swarm page

`/swarm` (formerly `/challenges`, which now redirects permanently) shows one organizer-maintained
swarm run (internally a "challenge"; the code keeps that name under `src/challenge/`): goal,
scheduled work start, planned total budget in sats, joining window, buyer npub and latest
update. Its only inbound link is the footer's Swarm Lab link; it has no top-nav or discovery
(`llms.txt`, `skill.md`) links and is noindexed. Unlisted is not
access-controlled: anyone with the URL can read it.

Edit `src/challenge/content.ts` to publish a complete `Challenge` (see the type in
`src/challenge/model.ts`); leave it `null` until the organizer confirms the goal,
buyer, schedule and budget. Timestamps use canonical UTC ISO strings with `.000Z`.
Sellers can join any time. Membership beginning at or before work starts is eligible
that day; later joins are eligible from the next day, subject to buyer acceptance.
Dates render in the visitor's timezone with UTC alongside. Status and the update
are maintained by the organizer: reaching the scheduled time does not prove work
started. Updating the content requires a site build/deployment.

There is no signup, wallet, posting, spending or automated challenge backend here.
The displayed budget is planned, not spent or escrowed. Seller approval remains
buyer-level, not challenge-specific. The copy instruction converts the npub to
hex for `seller.accept_offers_only_from` and asks the operator's agent to preserve
existing entries, rates and admission settings. Existing review/payment paths are
unchanged. The tests validate configured content and reject malformed identity,
budget and schedule values. No real challenge is shipped by default.
