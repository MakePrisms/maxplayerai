#!/usr/bin/env node
// Launcher for the mobee binary. Node finds the executable and hands off — no bindings, no wasm,
// no FFI. The binary is a statically linked ELF carried in a per-platform package that npm installs
// only when its os/cpu match, so an install pulls one platform rather than all of them.
"use strict";

const { spawnSync } = require("node:child_process");
const os = require("node:os");

const PLATFORM_PACKAGES = {
  "linux-x64": "@mobee/cli-linux-x64",
};

const key = `${process.platform}-${process.arch}`;
const pkg = PLATFORM_PACKAGES[key];

let binary = null;
if (pkg) {
  try {
    binary = require.resolve(`${pkg}/bin/mobee`);
  } catch {
    // Absent because npm skipped it on an os/cpu mismatch, or because of --no-optional.
  }
}

if (!binary) {
  console.error(
    `mobee: no binary for ${key}. Available: ${Object.keys(PLATFORM_PACKAGES).join(", ")}.\n` +
      (pkg
        ? `Expected ${pkg} to be installed — if you used --no-optional, reinstall without it.`
        : `That platform is not published yet.`)
  );
  process.exit(1);
}

// stdio "inherit" is load-bearing: the buyer MCP server speaks JSON-RPC over stdin/stdout, so the
// streams must be the real ones rather than pipes this process would have to shuttle.
const { status, signal, error } = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (error) {
  console.error(`mobee: could not execute ${binary}: ${error.message}`);
  process.exit(1);
}
// A child killed by a signal has no exit code; report it the way a shell does.
process.exit(signal ? 128 + (os.constants.signals[signal] ?? 0) : (status ?? 1));
