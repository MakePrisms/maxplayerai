# NIP-05 handle registry (Maxplayer)

The relay owns handles in Postgres, independent of kind-0 advertisements and
`users.nip05_handle`. Existing profile handles are **not** migrated into claims.
Names are ASCII-lowercased and must match `^[a-z0-9][a-z0-9._-]{2,29}$`.

## Configuration and host binding

Set `NIP05_DOMAIN=maxplayer.ai` independently of `RELAY_URL=wss://relay.maxplayer.ai`.
For handle endpoints only, this exact normalized domain is an explicit alias of
the community whose `communities.host` matches the configured relay URL authority.
Both `Host: maxplayer.ai` and `Host: relay.maxplayer.ai` therefore read/write that
same registry. Other mapped tenant hosts keep their own registry; unknown or
missing hosts never fall back to the deployment community. This is a single
configured alias, not a wildcard or a cross-tenant search.

An external rewrite of `maxplayer.ai/.well-known/nostr.json` to the relay can
preserve the apex Host or replace it with the destination relay Host: both work.
We do not trust `X-Forwarded-Host` for community selection. The NIP-05 relays map
advertises the bound relay host, not the alias. The actual Vercel rewrite/redirect
configuration remains to be checked at deployment; no Vercel changes are included.
NIP-98 clients sign the actual request Host, path and query, with HTTPS when
RELAY_URL uses WSS (HTTP for WS development). A proxy rewriting authenticated
requests must preserve that authority; the planned public NIP-05 rewrite needs no
authentication.

## API

- `GET /api/handles/available?name=alice`: `{available: true, reason: null}` or
  `{available: false, reason: "taken" | "reserved" | "invalid" | "cooldown"}`.
  Anonymous availability conservatively reports cooldown even to the old owner;
  an authenticated claim by that owner can reclaim immediately.
- `POST /api/handles/claim`, JSON `{ "name": "alice" }`: claims or atomically
  renames; returns `{name, pubkey}`. Conflicts return 409 and `{available:false,reason}`.
- `POST /api/handles/release`: releases only the signing pubkey's handle;
  returns `{released:true}`, including when no claim exists. Body may be empty or `{}`.
- Both mutations require `Authorization: Nostr <base64 event>` (kind 27235), exact
  URL/method, ±60-second freshness, and a SHA-256 payload tag covering the raw body
  (including empty bodies). Shared Redis replay protection fails closed. No client
  supplied pubkey can nominate an owner. NIP-FI/membership is not required.
- Mutations use shared atomic Redis fixed windows: 10/minute per community/pubkey,
  60/minute per socket peer IP, with separate handle counters. IP is the transport
  peer, not arbitrary forwarding headers; proxy deployments share the proxy's IP
  budget unless the transport is configured upstream to provide trusted client IPs.
  Redis failure returns 503; exceeding a budget returns 429.
- `GET /api/handles?pubkeys=<64-char-hex,...>`: `{ "<hex>": "alice" }`, maximum
  500 entries, omitting unknown pubkeys. Empty input returns `{}`; malformed input
  returns 400. Public, `Access-Control-Allow-Origin: *`, `Cache-Control: no-store`.
- `GET /.well-known/nostr.json?name=ALIce`: case-insensitive lookup, echoing `ALIce`
  as the response key. Always `names` and `relays`, CORS `*`, and `no-store`. Missing
  name returns empty maps; never lists the registry.

Released names remain unavailable to other pubkeys for 30 days. Renames release
the old name in the same transaction. Partial unique indexes enforce one active
name per pubkey and one active owner per name in each community. Community-row
locking serializes claim/release mutations, including absent-name races.

`reserved_handles` blocks both claiming and discovery. Admin release/block API
integration is deferred: it must use the admin plane's audited/idempotent action
framework, not an unaudited shortcut. Until that follow-up, operator-managed
reservations must coordinate with registry writes (call
`assert_community_write_allowed(community_id)` before locking the community row
in the same transaction, preserving deletion-fence lock order). No reserved names are silently seeded by this change.

## Migration and verification

`migrations/100027_handles.sql` is embedded by buzz-db's existing sqlx migrator;
`BUZZ_AUTO_MIGRATE=1` (already set by `nix/relay.nix`) applies it on startup. Desired
state files under `schema/` mirror the migration. Both tables have community write
fences and are in the deletion inventory/purge order. No deployment is part of
this PR.

Integration tests require disposable Postgres and Redis (no real money):

```sh
BUZZ_TEST_DATABASE_URL=postgresql://... HANDLE_TEST_REDIS_URL=redis://... \
  cargo test --manifest-path crates/buzz/Cargo.toml -p buzz-relay --lib \
  api::handles -- --include-ignored --nocapture --test-threads=1
```

These tests run embedded migrations and fail if services are absent, rather than
silently passing. They cover closed relay/NIP-FI-deny mode, NIP-98 failure/replay,
ownership, rename, cooldown, concurrency, blocklist, batch limits, kind-0
isolation, both Host forms, and actual routed discovery headers.
