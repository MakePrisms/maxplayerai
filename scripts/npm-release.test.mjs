// Release-path tests for the npm launcher and the publish ordering (run by CI's release-gates job):
//   node --test scripts/npm-release.test.mjs
//
// 0.8.2-rc2: @maxplayerai/linux-x64 answered 404 for ~23 minutes after its publish while the
// launcher pinning it was already live; npm skipped the missing optional dependency silently, so
// installs and reinstalls got a launcher with no binary and said nothing. These hold the two fixes:
// the release publishes the launcher only after every payload is served, and an install whose payload
// is missing fails loudly.
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const launcherDir = path.join(repo, "npm", "maxplayer");
const manifest = JSON.parse(fs.readFileSync(path.join(launcherDir, "package.json"), "utf8"));
const key = process.platform + "-" + process.arch;
const payload = "@maxplayerai/" + key;
const supported = Object.keys(manifest.optionalDependencies).includes(payload);

// An installed tree: node_modules/maxplayer (the real launcher) and, optionally, the payload whose
// "binary" is a script that records that it ran.
function installed({ withPayload }) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "maxplayer-launcher-"));
  const nm = path.join(root, "node_modules");
  fs.mkdirSync(path.join(nm, "maxplayer", "bin"), { recursive: true });
  fs.copyFileSync(path.join(launcherDir, "bin", "maxplayer.js"), path.join(nm, "maxplayer", "bin", "maxplayer.js"));
  fs.copyFileSync(path.join(launcherDir, "package.json"), path.join(nm, "maxplayer", "package.json"));
  const marker = path.join(root, "ran");
  if (withPayload) {
    const bin = path.join(nm, "@maxplayerai", key, "bin");
    fs.mkdirSync(bin, { recursive: true });
    fs.writeFileSync(path.join(bin, "maxplayer"), "#!/bin/sh\nprintf '%s' \"$*\" > '" + marker + "'\n", { mode: 0o755 });
  }
  return { root, pkgDir: path.join(nm, "maxplayer"), marker };
}

function run(tree, args, env = {}) {
  const clean = { ...process.env };
  delete clean.npm_lifecycle_event;
  return spawnSync(process.execPath, [path.join(tree.pkgDir, "bin", "maxplayer.js"), ...args], {
    cwd: tree.pkgDir,
    env: { ...clean, ...env },
    encoding: "utf8",
  });
}

// What npm runs as the postinstall, word for word from package.json.
function postinstall(tree) {
  const [cmd, script, ...args] = manifest.scripts.postinstall.split(" ");
  assert.equal(cmd, "node");
  return spawnSync(process.execPath, [path.join(tree.pkgDir, script), ...args], {
    cwd: tree.pkgDir,
    env: { ...process.env, npm_lifecycle_event: "postinstall" },
    encoding: "utf8",
  });
}

test("the launcher package runs an install check (RED ON REVERT: no postinstall, install stays silent)", () => {
  assert.ok(manifest.scripts?.postinstall, "npm/maxplayer/package.json has no postinstall");
  assert.match(manifest.scripts.postinstall, /^node bin\/maxplayer\.js /);
});

test("an install whose payload is missing fails, naming the package and version", { skip: !supported }, () => {
  const tree = installed({ withPayload: false });
  const result = postinstall(tree);
  assert.equal(result.status, 1, result.stderr);
  assert.ok(result.stderr.includes(payload + "@" + manifest.version), result.stderr);
  assert.ok(result.stderr.includes("npm view"), result.stderr);
});

test("an install whose payload is present passes without executing the binary", { skip: !supported }, () => {
  const tree = installed({ withPayload: true });
  const result = postinstall(tree);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.existsSync(tree.marker), false, "the install check ran the binary");
});

test("a run with the payload hands off to the binary with its arguments", { skip: !supported }, () => {
  const tree = installed({ withPayload: true });
  const result = run(tree, ["version", "--json"]);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.readFileSync(tree.marker, "utf8"), "version --json");
});

test("the check flag outside npm's postinstall is passed through, not swallowed", { skip: !supported }, () => {
  const tree = installed({ withPayload: true });
  const result = run(tree, ["--npm-postinstall-check"]);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.readFileSync(tree.marker, "utf8"), "--npm-postinstall-check");
  // And a user's own postinstall that calls 'maxplayer <cmd>' still runs the command.
  const theirs = run(tree, ["wallet", "balance"], { npm_lifecycle_event: "postinstall" });
  assert.equal(theirs.status, 0, theirs.stderr);
  assert.equal(fs.readFileSync(tree.marker, "utf8"), "wallet balance");
});

test("a run without the payload fails loudly", { skip: !supported }, () => {
  const tree = installed({ withPayload: false });
  const result = run(tree, ["version"]);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /no binary for/);
  assert.ok(result.stderr.includes(payload + "@" + manifest.version), result.stderr);
});

// ── scripts/wait-npm-published.sh with a fake npm and curl ──────────────────────────────────────
function fakes({ listedAfter, downloads }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "wait-npm-"));
  const count = path.join(dir, "count");
  fs.writeFileSync(count, "0");
  fs.writeFileSync(
    path.join(dir, "npm"),
    "#!/bin/sh\nn=$(($(cat '" + count + "') + 1)); echo $n > '" + count + "'\n" +
      "[ $n -gt " + listedAfter + " ] && echo https://registry.example/pkg.tgz\nexit 0\n",
    { mode: 0o755 },
  );
  fs.writeFileSync(path.join(dir, "curl"), "#!/bin/sh\nexit " + (downloads ? 0 : 22) + "\n", { mode: 0o755 });
  return dir;
}
function wait(dir, timeout) {
  return spawnSync("bash", [path.join(repo, "scripts", "wait-npm-published.sh"), "@maxplayerai/linux-x64", "9.9.9"], {
    env: { ...process.env, NPM: path.join(dir, "npm"), CURL: path.join(dir, "curl"), WAIT_NPM_TIMEOUT: String(timeout), WAIT_NPM_INTERVAL: "0.1" },
    encoding: "utf8",
  });
}

test("wait-npm-published waits until the registry serves the tarball", () => {
  const result = wait(fakes({ listedAfter: 2, downloads: true }), 30);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /waiting: @maxplayerai\/linux-x64@9\.9\.9 — the registry does not list it yet/);
  assert.match(result.stdout, /ok: @maxplayerai\/linux-x64@9\.9\.9 is served/);
});

test("wait-npm-published fails, naming the package, when it never becomes installable", () => {
  const listedNotServed = wait(fakes({ listedAfter: 0, downloads: false }), 1);
  assert.equal(listedNotServed.status, 1);
  assert.match(listedNotServed.stderr, /@maxplayerai\/linux-x64@9\.9\.9 is still not installable/);
  assert.match(listedNotServed.stderr, /does not download yet/);
  const neverListed = wait(fakes({ listedAfter: 1e9, downloads: true }), 1);
  assert.equal(neverListed.status, 1);
  assert.match(neverListed.stderr, /does not list it yet/);
});

// ── release.yml: the launcher is published only after every payload is served ──────────────────
test("release.yml publishes the launcher only after waiting for every payload (RED ON REVERT)", () => {
  const workflow = fs.readFileSync(path.join(repo, ".github", "workflows", "release.yml"), "utf8");
  const job = workflow.slice(workflow.indexOf("\n  publish:\n"), workflow.indexOf("\n  npm-probe:\n"));
  assert.ok(job.length > 0, "no publish job found");
  const payloadPublish = job.indexOf('( cd "npm/cli-$platform" && npm publish');
  const waitAll = job.indexOf('./scripts/wait-npm-published.sh "@maxplayerai/$platform" "$VERSION"');
  const launcherPublish = job.indexOf("( cd npm/maxplayer && npm publish");
  assert.ok(payloadPublish > 0, "payload publish not found");
  assert.ok(launcherPublish > 0, "launcher publish not found");
  assert.ok(waitAll > payloadPublish && waitAll < launcherPublish, "the launcher must be published only after the payload wait");
  // The wait loops over every release platform, not one hard-coded package.
  assert.match(job.slice(payloadPublish, waitAll), /for platform in \$RELEASE_PLATFORMS; do\s*$/);
});
