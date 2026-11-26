#!/usr/bin/env node
// Launcher for the maxplayer binary. Node finds the executable and hands off — no bindings, no wasm,
// no FFI. The binary is a statically linked ELF carried in a per-platform package that npm installs
// only when its os/cpu match, so an install pulls one platform rather than all of them.
"use strict";

const { spawnSync } = require("node:child_process");
const os = require("node:os");

// Scoped under @maxplayerai so the payload names cannot collide with anything unscoped. The value
// is the package name exactly as `require.resolve` needs it — a scope is part of the specifier, so
// these are joined with the in-package path below rather than assembled from the platform key.
const PLATFORM_PACKAGES = {
  "linux-x64": "@maxplayerai/linux-x64",
  "linux-arm64": "@maxplayerai/linux-arm64",
  // Apple Silicon. Node under Rosetta reports darwin-x64, and gets the Intel build below, which
  // runs there too (translated) — install.sh instead picks the native arm64 build for that case.
  "darwin-arm64": "@maxplayerai/darwin-arm64",
  // Intel macs, built natively on an Intel runner (.github/release-platforms.json).
  "darwin-x64": "@maxplayerai/darwin-x64",
};

const key = `${process.platform}-${process.arch}`;
const pkg = PLATFORM_PACKAGES[key];
// The exact payload version the launcher pins (package.json optionalDependencies), for messages.
let version = null;
try {
  version = require("../package.json").version;
} catch {
  // Messages fall back to the bare package name.
}
// npm runs this file as the package's postinstall: then it only CHECKS that the binary resolves.
// npm skips a missing optional dependency silently — a payload the registry was not serving yet
// (0.8.2-rc2's linux-x64 answered 404 for ~23 minutes after its publish) left installs and
// reinstalls with a launcher and no binary, and nothing said so. Failing the postinstall makes the
// install itself fail, naming the package; for an upgrade, npm then keeps the previous version.
// With --ignore-scripts this never runs, and the launcher's own message below still applies.
// Both conditions, so a user's own postinstall that runs `maxplayer …` still runs the binary.
const INSTALL_CHECK_ARG = "--npm-postinstall-check";
const installCheck =
  process.env.npm_lifecycle_event === "postinstall" &&
  process.argv.length === 3 &&
  process.argv[2] === INSTALL_CHECK_ARG;

let binary = null;
if (pkg) {
  try {
    binary = require.resolve(`${pkg}/bin/maxplayer`);
  } catch {
    // Absent because npm skipped it on an os/cpu mismatch, or because of --no-optional.
  }
}

if (!binary) {
  if (installCheck && !pkg) {
    // Unchanged behaviour for a platform with no payload at all: install, then explain at run time.
    process.exit(0);
  }
  const wanted = version ? `${pkg}@${version}` : pkg;
  console.error(
    `maxplayer: no binary for ${key}. Available: ${Object.keys(PLATFORM_PACKAGES).join(", ")}.\n` +
      (pkg
        ? `Expected ${wanted} to be installed, and it is not. If you installed with --no-optional or ` +
          `--omit=optional, reinstall without it. Otherwise the registry may not be serving ${wanted} ` +
          `yet (check: npm view ${wanted}); reinstall once it is: npm install -g maxplayer@${version ?? "<version>"}`
        : `That platform is not published yet.`)
  );
  process.exit(1);
}

if (installCheck) {
  // The binary resolves: the install is complete. Nothing is executed during install.
  process.exit(0);
}

// stdio "inherit" is load-bearing: the buyer MCP server speaks JSON-RPC over stdin/stdout, so the
// streams must be the real ones rather than pipes this process would have to shuttle.
const { status, signal, error } = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (error) {
  console.error(`maxplayer: could not execute ${binary}: ${error.message}`);
  process.exit(1);
}
// A child killed by a signal has no exit code; report it the way a shell does.
process.exit(signal ? 128 + (os.constants.signals[signal] ?? 0) : (status ?? 1));
