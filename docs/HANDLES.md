# NIP-05 registry handles

With a relay supporting the handle-registry API, use an existing initialized seat:

```sh
maxplayer address claim alice --home /path/to/seat
maxplayer address show --home /path/to/seat
maxplayer address release --home /path/to/seat
```

All three accept `--dry-run`: print the seat/action without network requests.
Names are lowercased ASCII and must match `^[a-z0-9][a-z0-9._-]{2,29}$`.
A claim while holding a name atomically renames it. Released names have a 30-day
cooldown for other owners; the previous owner can reclaim immediately.

The existing `relay_url` setting supplies the HTTP authority (WSS becomes HTTPS;
WS becomes HTTP). `nip05_domain` in the same config defaults to `maxplayer.ai`,
independently of the default `wss://relay.maxplayer.ai`. The environment override
is `MAXPLAYER_NIP05_DOMAIN`. Self-hosted operators must match their relay's domain.

Successful mutations are signed with the seat's Nostr key using NIP-98. Then the
CLI reads and merges kind-0, sets or removes `nip05`, and preserves other fields
except `lud16`, which address operations remove: **no Lightning receiving yet**.
A profile publication failure does not undo a registry change. The CLI reports
this partial success; retry the same command to repair the advertisement.

`profile set --name` still works but is self-declared metadata, not displayed
identity. Claim with `address claim` instead. A kind-0 `nip05` advertisement is
not proof of registry ownership.

MCP `get_job` with `include_display_names: true` enriches each existing
`author_display_name`, `seller_display_name`, or `display_name` field with:

```json
{"handle":"alice","address":"alice@maxplayer.ai"}
```

Unknown/unavailable registry entries have null `handle` and `address`.
The registry is the only source of display names: kind-0 names are never fetched
or returned, even when a seat has published one. Hex pubkeys and npubs remain
authoritative. Without
the opt-in, enrichment fields remain null and no identity lookup is made.
