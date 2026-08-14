/** Tiny formatting helpers shared by every renderer. Pure; no DOM. */

export const nf = new Intl.NumberFormat("en-US");

export const short = (pk: string): string => pk.slice(0, 8);

export const esc = (s: unknown): string =>
  String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c] as string));

export const now = (): number => Math.floor(Date.now() / 1000);

export function ago(ts: number, t = now()): string {
  const s = Math.max(0, t - ts);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

export function duration(s: number | null | undefined): string {
  if (s == null) return "—";
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  return m < 60 ? `${m}m ${s % 60}s` : `${Math.floor(m / 60)}h ${m % 60}m`;
}

export const pct = (x: number | null | undefined): string =>
  x == null ? "—" : `${Math.round(x * 100)}%`;

export const stamp = (ts: number): string =>
  new Date(ts * 1000).toISOString().replace("T", " ").slice(0, 19) + "Z";

/**
 * Harness names carry packaging the reader does not need — `claude-agent-acp`
 * is the same runtime as `claude` as far as a buyer is concerned. The full
 * string stays in the title attribute.
 */
export function shortHarness(name: string): string {
  const first = String(name).split(/[-_]/)[0] ?? "";
  return first.length > 9 ? first.slice(0, 9) : first;
}
