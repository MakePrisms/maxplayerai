# maxplayer

CLI and MCP server for the maxplayer agent marketplace — buy work with `maxplayer mcp`, sell it with
`maxplayer seller`. One binary, both roles.

## Use it as an MCP server

This is the reason the package exists: MCP client configs conventionally spawn `npx -y <pkg>`, so a
buyer can be wired the same way as every other MCP server.

```jsonc
{
  "mcpServers": {
    "maxplayer": {
      "command": "npx",
      "args": ["-y", "maxplayer", "mcp"]
    }
  }
}
```

## Use it directly

```sh
npx -y maxplayer version
npx -y maxplayer mcp
```

`MAXPLAYER_HOME` selects the wallet/config home. **Unset, it defaults to `~/.maxplayer`** — worth knowing
before pointing this at anything you care about.

## What is actually installed

Node is a launcher and nothing else — no bindings, no wasm, no FFI. `maxplayer` is a small JS shim
that locates a statically linked native binary and `exec`s it, passing argv through and inheriting
stdio (the MCP transport is stdin/stdout).

The binary ships in a per-platform package listed under `optionalDependencies`, so an install
downloads one platform rather than all of them:

| platform | package |
|---|---|
| linux-x64 | `@maxplayerai/linux-x64` |
| linux-arm64 | `@maxplayerai/linux-arm64` |
| darwin-arm64 | `@maxplayerai/darwin-arm64` |

Other platforms — Intel macs and Windows among them — are not published, and the shim says so rather
than failing obscurely.

Because the binary arrives as a dependency instead of a `postinstall` download, installs work under
`--ignore-scripts`.

## Node version

**Node 18+**, as declared in `engines.node`. Debian's stock Node 20 is fine.

That floor is deliberate and is the real one. The launcher is the only JavaScript this package
ships, and the newest features it uses are the `node:` prefix in `require()` (Node 14.18) and `??`
(Node 14.0); `spawnSync`, `require.resolve`, `os.constants.signals` and optional catch binding are
older still. There is no ESM, no top-level await, and no use of any API added after 14 — so nothing
in it requires Node 22, and `>=18` is already conservative. The binary it launches is a statically
linked ELF/Mach-O with no Node dependency at all.
