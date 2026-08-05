# Flag-day cutover runbook (protocol v0 → v1)

Operational guide for the one-way wire cutover shipped in this release. The previous protocol
(`t=mobee`, `v=0`, `d=mobee-seller`, home `~/.mobee`) and the current one (`t=maxplayer`, `v=1`,
`d=maxplayer-seller`, home `~/.maxplayer`) **do not interoperate** — this is a clean cut, by design.

## What flips

| Surface | Before | After |
|---|---|---|
| Namespace tag `t` | `mobee` | `maxplayer` |
| Protocol version `v` | `0` | `1` |
| Seller heartbeat / NIP-89 `d` | `mobee-seller` | `maxplayer-seller` |
| Signing domains | `mobee/v1/*` | `maxplayer/v1/*` |
| Relay write-policy namespace | accepts `t=mobee` | accepts `t=maxplayer` only |
| Default home dir | `~/.mobee` (`MOBEE_HOME`) | `~/.maxplayer` (`MAXPLAYER_HOME`) |

## The partition (accepted, by design)

A v1 parser rejects a pre-cut event two independent ways: `t=mobee` is out of namespace
(`MissingMaxplayerTag`) and `v=0` is an unsupported version (`UnsupportedVersion`). So a pre-cut
seat and a post-cut seat **cannot trade**, and pre-cut seats disappear from the v1 board until they
upgrade and re-announce. There is no dual-speak build and no backcompat shim — the cut is
forward-only.

## Order of operations

1. **Relay.** Deploy the release relay config; the write-policy's `namespaceTag` is `maxplayer`, so
   new `t=mobee` writes are rejected. Events already stored under `t=mobee` remain readable (the
   write-policy gates writes, not reads; the DB keeps its history), but the public board queries
   `#t=maxplayer` and so shows only post-cut trades.
2. **Seats (sellers).** Upgrade each seat.
   - *Home migration.* The default home moved to `~/.maxplayer`. On first boot with an existing
     `~/.mobee` and no `~/.maxplayer`, the seat **refuses to boot** and prints the exact fix:

     ```
     mv ~/.mobee ~/.maxplayer
     ```

     Run it (this moves the wallet, keys, and state), or set `MAXPLAYER_HOME` to a chosen path. A
     fresh box (no `~/.mobee`) boots normally and bootstraps `~/.maxplayer`.
   - After migration the seat re-announces (kind-0, NIP-89 handler, heartbeat with
     `d=maxplayer-seller`) and emits trade events with `t=maxplayer` / `v=1`.
3. **Buyers.** Upgrade; the MCP/buyer surface emits `t=maxplayer` / `v=1` offers and filters the
   relay on `#t=maxplayer`.

## Drain — observed, not gated

Do **not** block the cut on in-flight v0 trades. Jobs already in flight between not-yet-upgraded
participants complete on the v0 wire among themselves. Watch the board / relay for the tail of v0
activity; it drains as seats upgrade. There is no drain gate and no gate-keeper — draining is an
observation, not a precondition.

## Money safety

The home-migration boot-guard is fail-closed: an upgraded seat cannot silently start on an empty
`~/.maxplayer` while funds sit in `~/.mobee`. It refuses to boot and names the `mv`. No award, pay,
or budget path changes in the cut itself — only the wire values and the home path.

## Verify after the cut

- A real offer with `t=maxplayer` / `v=1` round-trips through `parse_offer`; a `t=mobee` or `v=0`
  event is rejected (covered by `legacy_mobee_v0_offer_is_rejected_under_v1`).
- The relay write-policy logs `namespace tag = "maxplayer"` at startup.
- The board shows post-cut trades under `#t=maxplayer`.

## No rollback

Cleancut: there is no dual-speak fallback. Reverting means redeploying the pre-cut build with the
relay `namespaceTag` back to `mobee` and moving `~/.maxplayer` → `~/.mobee` on each seat — an
unsupported, manual path. Treat the cut as forward-only.
