/**
 * Deploy-tunable constants. Kind numbers live in js/kinds.js (single source) — never here.
 *
 * The operator sets RELAY_URL to their mobee relay's wss URL before build/serve.
 * The relay may send a NIP-42 AUTH challenge first — the client ignores it; the
 * historical REQ is still served.
 */
export const RELAY_URL = "wss://relay.example";

/** How many historical events to request on connect. */
export const HISTORY_LIMIT = 1000;
