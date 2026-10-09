/** Display helpers for /tokens. Pure; no DOM. */
import { npubEncode } from "./nip19.js";

/**
 * A mint as a reader needs it. `https://` is the norm and is dropped; `http://`
 * stays visible because it is unencrypted; `nostr://npub1…` is shortened.
 * The full URL always goes in the title attribute.
 */
export function mintLabel(url: string): string {
  if (url.startsWith("nostr://")) {
    const npub = url.slice("nostr://".length);
    return `nostr:${npub.slice(0, 9)}…${npub.slice(-4)}`;
  }
  return url.startsWith("https://") ? url.slice("https://".length) : url;
}

/** Ratio with four significant digits, no exponent for any lot the rules allow (1e-6 … 1e6). */
export function rate(x: number): string {
  if (!Number.isFinite(x) || x <= 0) return "—";
  const r = Number(x.toPrecision(4));
  return r >= 1e-6 ? r.toLocaleString("en-US", { maximumFractionDigits: 6, useGrouping: false }) : r.toString();
}

/** Time to expiry: "23h 05m", "12m", "45s", or "—" once gone. */
export function timeLeft(seconds: number): string {
  if (seconds <= 0) return "—";
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}

/** npub, and a short form for narrow columns. */
export function seller(pubkey: string): { npub: string; short: string } {
  try {
    const npub = npubEncode(pubkey);
    return { npub, short: `${npub.slice(0, 10)}…${npub.slice(-4)}` };
  } catch {
    return { npub: pubkey, short: pubkey.slice(0, 8) };
  }
}

/**
 * Which edge the lot popup docks to (bob: never the middle): open lots are
 * the left column, so they dock left; Recent is the right column.
 */
export function dockSide(list: string): "left" | "right" {
  return list === "lots" ? "left" : "right";
}
