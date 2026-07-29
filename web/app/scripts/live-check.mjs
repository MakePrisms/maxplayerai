/**
 * Live check — run the real core against the real relay and print what it sees.
 *
 * Read-only. This is the proof that the client, cache and trade join work
 * against the live market rather than only against fixtures.
 *
 *   node scripts/live-check.mjs
 */
import { RELAY_URL } from "../config.js";
import { createCache } from "../js/cache.js";
import { createRelayClient } from "../js/relay.js";
import { marketMetrics, conversionRates } from "../js/trades.js";

const cache = createCache();
const started = Date.now();

const client = createRelayClient({
  url: RELAY_URL,
  onEvent: (event) => cache.ingest(event),
  onStatus: ({ state, detail }) => {
    process.stderr.write(`  [${state}]${detail ? " " + detail : ""}\n`);
    if (state === "failed") process.exitCode = 1;
  },
  onHistoryComplete: ({ pages }) => {
    const events = cache.all();
    const m = marketMetrics(events);
    const rates = conversionRates(m.funnel);
    const pct = (x) => `${Math.round(x * 100)}%`;

    console.log(`\nrelay      ${RELAY_URL}`);
    console.log(`read       ${events.length} events over ${pages} page(s) in ${((Date.now() - started) / 1000).toFixed(1)}s`);
    console.log(`window     ${m.daysActive} days, ${new Date(m.firstEventAt * 1000).toISOString().slice(0, 10)} → ${new Date(m.lastEventAt * 1000).toISOString().slice(0, 10)}`);
    console.log(`\nfunnel (current protocol, trades whose offer we saw)`);
    console.log(`  posted     ${m.funnel.posted}`);
    console.log(`  claimed    ${m.funnel.claimed}  (${pct(rates.claimed)} of posted)`);
    console.log(`  awarded    ${m.funnel.awarded}  (${pct(rates.awarded)} of claimed)`);
    console.log(`  delivered  ${m.funnel.delivered}  (${pct(rates.delivered)} of awarded)`);
    console.log(`  receipted  ${m.funnel.receipted}  (${pct(rates.receipted)} of delivered)`);
    console.log(`\nsettlement (FLOOR — a trade can settle with no receipt published)`);
    console.log(`  receipts on record   ${m.receiptsOnRecord}`);
    console.log(`  sats in receipts     ${m.satsInReceipts.toLocaleString("en-US")}`);
    console.log(`\nparticipants  ${m.buyers} buyers · ${m.sellers} sellers`);
    console.log(`trades        ${m.tradesTracked} tracked · ${m.rootedElsewhere} rooted outside this view`);

    client.stop();
    process.exit(process.exitCode || 0);
  },
});

setTimeout(() => { console.error("timed out before history completed"); process.exit(1); }, 120000);
client.connect();
