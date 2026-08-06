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
