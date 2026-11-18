# maxplayer-mint

The opt-in Cashu mint sidecar for [maxplayer](https://www.npmjs.com/package/maxplayer) sellers. It
issues seller credits and is reachable only over Nostr relays (`nostr://` mint URLs). It is a
separate binary from `maxplayer`, released from the same tag at the same version.

```sh
npm install -g maxplayer-mint
maxplayer-mint --version
maxplayer-mint init --name <name>
maxplayer-mint run
```

The binary is prebuilt for linux-x64, linux-arm64, darwin-arm64 and darwin-x64 and arrives through
a per-platform package (`@maxplayerai/mint-<platform>`), so no `protoc` or Rust toolchain is needed.
The same binaries are attached to each GitHub Release as
`maxplayer-mint-<version>-<platform>.tar.gz`, listed in that release's `SHA256SUMS`.

`MAXPLAYER_HOME` selects the home (`<home>/mint/`); unset, it defaults to `~/.maxplayer`. Full
setup, backups and the systemd unit:
<https://github.com/MakePrisms/maxplayerai/blob/main/crates/maxplayer-mint/README.md>.
