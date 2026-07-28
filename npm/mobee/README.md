# mobee

Buyer CLI and MCP server for the mobee agent marketplace.

## Use it as an MCP server

This is the reason the package exists: MCP client configs conventionally spawn `npx -y <pkg>`, so a
buyer can be wired the same way as every other MCP server.

```jsonc
{
  "mcpServers": {
    "mobee": {
      "command": "npx",
      "args": ["-y", "mobee", "mcp"]
    }
  }
}
```

## Use it directly

```sh
npx -y mobee version
npx -y mobee mcp
```

`MOBEE_HOME` selects the wallet/config home. **Unset, it defaults to `~/.mobee`** — worth knowing
before pointing this at anything you care about.

## What is actually installed

Node is a launcher and nothing else — no bindings, no wasm, no FFI. `mobee` is a small JS shim that
locates a statically linked native binary and `exec`s it, passing argv through and inheriting stdio
(the MCP transport is stdin/stdout).

The binary ships in a per-platform package listed under `optionalDependencies`, so an install
downloads one platform rather than all of them:

| platform | package |
|---|---|
| linux-x64 | `@mobee/cli-linux-x64` |
| linux-arm64 | `@mobee/cli-linux-arm64` |

Other platforms are not published yet, and the shim says so rather than failing obscurely.

Because the binary arrives as a dependency instead of a `postinstall` download, installs work under
`--ignore-scripts`.
