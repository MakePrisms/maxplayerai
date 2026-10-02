import { inject, type BeforeSend } from "@vercel/analytics";

// Page views only. Seller filters and any future sensitive query/hash values
// must not become analytics dimensions. Do not read forms or browser storage.
export const beforeSend: BeforeSend = (event) => {
  if (event.type !== "pageview") return null;
  try {
    const url = new URL(event.url);
    url.search = "";
    url.hash = "";
    return { ...event, url: url.origin + url.pathname };
  } catch {
    return null;
  }
};

export function startAnalytics(): void {
  inject({ mode: "production", beforeSend });
}
