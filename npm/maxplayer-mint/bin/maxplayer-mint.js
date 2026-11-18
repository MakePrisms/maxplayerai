#!/usr/bin/env node
// Launcher for the maxplayer-mint sidecar binary. The same shape as the `maxplayer` launcher
// (npm/maxplayer/bin/maxplayer.js): Node finds the executable and hands off. The binary is built in
// CI (where protoc is installed) and carried in a per-platform package that npm installs only when
// its os/cpu match, so a user never needs protoc or a Rust toolchain.
"use strict";

const { spawnSync } = require("node:child_process");
const os = require("node:os");

// Scoped under @maxplayerai with a `mint-` prefix so the sidecar's payloads cannot collide with the
// `maxplayer` payloads (@maxplayerai/<platform>). scripts/verify-release-surface.sh holds this
// table to .github/release-platforms.json.
const PLATFORM_PACKAGES = {
  "linux-x64": "@maxplayerai/mint-linux-x64",
  "linux-arm64": "@maxplayerai/mint-linux-arm64",
  "darwin-arm64": "@maxplayerai/mint-darwin-arm64",
  "darwin-x64": "@maxplayerai/mint-darwin-x64",
};

const key = `${process.platform}-${process.arch}`;
const pkg = PLATFORM_PACKAGES[key];

let binary = null;
if (pkg) {
  try {
    binary = require.resolve(`${pkg}/bin/maxplayer-mint`);
  } catch {
    // Absent because npm skipped it on an os/cpu mismatch, or because of --no-optional.
  }
}

if (!binary) {
  console.error(
    `maxplayer-mint: no binary for ${key}. Available: ${Object.keys(PLATFORM_PACKAGES).join(", ")}.\n` +
      (pkg
        ? `Expected ${pkg} to be installed — if you used --no-optional, reinstall without it.`
        : `That platform is not published yet.`)
  );
  process.exit(1);
}

// stdio "inherit": `maxplayer-mint run` is a long-running service whose signals and output belong to
// the caller (systemd, a terminal), not to this process.
const { status, signal, error } = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (error) {
  console.error(`maxplayer-mint: could not execute ${binary}: ${error.message}`);
  process.exit(1);
}
// A child killed by a signal has no exit code; report it the way a shell does.
process.exit(signal ? 128 + (os.constants.signals[signal] ?? 0) : (status ?? 1));
