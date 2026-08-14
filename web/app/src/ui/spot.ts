/**
 * BTC-USD spot. Sats stay the settlement unit on the wire; dollars are the
 * display unit, converted at the live Coinbase rate. Until the first quote
 * lands the amounts render as "…", never a made-up rate.
 */

let btcUsd: number | null = null;
const listeners = new Set<() => void>();

export function usd(sats: number | null | undefined): string {
  if (sats == null) return "—";
  if (btcUsd == null) return "…";
  const v = (sats / 1e8) * btcUsd;
  if (v === 0) return "$0";
  if (v < 0.01) return "<1¢";
  if (v < 1) return `${Math.round(v * 100)}¢`;
  return `$${v.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

export const spotUsd = (): number | null => btcUsd;

export function onSpotChange(listener: () => void): void {
  listeners.add(listener);
}

/**
 * The plausibility band for a BTC-USD quote.
 *
 * Deliberately enormous — this is not a forecast, it is a garbage filter. The
 * failures worth catching are shaped like a wrong field, a missing decimal, or
 * cents read as dollars, and those land orders of magnitude out. Rejecting a
 * quote costs us a stale display, which is its own kind of wrong, so the band
 * errs heavily towards accepting anything a real market could produce.
 */
const RATE_MIN = 1_000;
const RATE_MAX = 10_000_000;

/**
 * `> 0` is not enough. It admits 0.00001 and 1e12, and the number reaches the
 * page as a fact — every dollar figure on the board derives from it, with
 * nothing to tell a reader the rate was nonsense.
 */
export function isPlausibleRate(rate: unknown): rate is number {
  return typeof rate === "number" && Number.isFinite(rate) && rate >= RATE_MIN && rate <= RATE_MAX;
}

export async function fetchSpot(fetchImpl: typeof fetch = fetch): Promise<void> {
  try {
    const res = await fetchImpl("https://api.coinbase.com/v2/prices/BTC-USD/spot");
    const rate = Number((await res.json())?.data?.amount);
    if (!isPlausibleRate(rate)) {
      // Never silent: a refused quote means the page keeps showing the last
      // known rate (or "…"), and that is worth being able to see.
      console.warn(`[spot] refused an implausible BTC-USD quote: ${rate}`);
      return;
    }
    if (rate !== btcUsd) {
      btcUsd = rate;
      for (const listener of listeners) listener();
    }
  } catch { /* keep the last known rate; the next interval retries */ }
}

export function startSpot(): void {
  void fetchSpot();
  setInterval(fetchSpot, 300000);
}
