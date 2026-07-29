/**
 * Deploy-tunable constants. Kind numbers live in js/kinds.js (single source) — never here.
 *
 * The mobee relay, baked in. Read-only: no key is ever loaded, so there is nothing to configure per-deploy.
 * The relay may send a NIP-42 AUTH challenge first — the client ignores it; the
 * historical REQ is still served.
 */
export const RELAY_URL = "wss://mobee-relay.orveth.dev";

/** How many historical events to request on connect. */
export const HISTORY_LIMIT = 1000;
