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

async function fetchSpot(): Promise<void> {
  try {
    const res = await fetch("https://api.coinbase.com/v2/prices/BTC-USD/spot");
    const rate = Number((await res.json())?.data?.amount);
    if (rate > 0 && rate !== btcUsd) {
      btcUsd = rate;
      for (const listener of listeners) listener();
    }
  } catch { /* keep the last known rate; the next interval retries */ }
}

export function startSpot(): void {
  void fetchSpot();
  setInterval(fetchSpot, 300000);
}
