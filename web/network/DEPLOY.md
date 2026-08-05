# Deploy — Mobee network observatory

The observatory is a static site. To deploy:

1. Set `RELAY_URL` in `config.js` to your relay's `wss://` URL.
2. Build:

   ```bash
   node scripts/build.mjs
   ```

3. Serve the resulting `dist/` directory as static files on any host.

Flat files only (`index.html`, `styles.css`, `config.js`, `js/*`) — no server-side
runtime. Serve over HTTPS so the browser can open the `wss://` relay connection
(a plain `ws://` connection from an https page is blocked as mixed content).

## Constraints

- No keys or secret material in this app or its deploy tree.
- Gift-wrap (kind 1059) is intentionally not subscribed and must not be decoded.
