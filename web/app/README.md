# maxplayer market — the public web app

The maxplayer public face. It reads the market from the relay in the visitor's browser
and derives every figure it shows from those events.

No server, no API, no bundler. The browser loads the same ES modules the tests
import, so what ships is what was tested.

```bash
npm test          # unit suite
npm run live-check   # run the core against the real relay and print what it sees
npm run build     # flat static dist/
```

## Layout

| Path | Role |
|---|---|
| `js/kinds.js` | Every Nostr kind the app touches. The only file allowed to contain a kind number — a test enforces it. |
| `js/relay.js` | Read-only relay client: pages history, then stays subscribed. |
| `js/cache.js` | Event store: dedup by id; addressable events resolved by author+kind+d, replaceable by author+kind. |
| `js/model.js` | Raw events in, typed records out. Nothing downstream touches a tag array. |
| `js/trades.js` | Joins events into trades and derives the market metrics. |
| `js/app.js` | Presentation. The core modules never touch the DOM. |

## Two things that will bite you

**A relay caps each filter in a REQ separately.** Filters sharing one REQ run out
at different depths, so each needs its OWN paging cursor. Advancing one shared
cursor from the globally-oldest event steps past everything the shallower filter
has not delivered — and the read still ends with a clean EOSE and plausible
numbers while missing half the market. `historyStreams()` exists for this reason.

**A `CLOSED` frame is not always a refusal.** The relay echoes
`["CLOSED", subid, ""]` to acknowledge a `CLOSE` we sent. Treating that as a
rejection ends history at the first page. A real refusal carries a reason:
`auth-required:` may work later, `restricted:` will not.

Both are covered by regression tests. Neither failed loudly when it was wrong,
which is exactly why they are tested.

## Counting settlements

A receipt is an **optional** announcement — payment travels as encrypted
gift-wrap and the wallet is the only complete record, so a trade can settle with
no receipt ever published. Every settlement figure here is therefore a **floor**,
never a total, and the names say so (`receiptsOnRecord`, `satsInReceipts`). Any
label that reaches a reader has to carry that too.

Figures describe the market on the **current protocol**. The market ran an earlier
protocol whose kinds this app deliberately does not read, so these counts are
narrower than the analytics pipeline's view of all history.

## Deploying

Static output, no runtime. `vercel.json` sets the build and the output
directory; any static host works the same way. Serve over HTTPS so the browser
can open the `wss://` relay connection — a `ws://` connection from an https page
is blocked as mixed content.

The relay is baked into `config.js`. The app is read-only: no key is ever loaded,
gift-wrap is never requested, and a test asserts the client sends nothing but
`REQ` and `CLOSE`.
