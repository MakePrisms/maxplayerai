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
function installed({ withPayload, payloadVersion = manifest.version }) {
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
    fs.writeFileSync(path.join(bin, "..", "package.json"), JSON.stringify({ name: payload, version: payloadVersion }));
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

test("a payload of another version on the resolution path is not run (RED ON REVERT: the stale binary runs)", { skip: !supported }, () => {
  const tree = installed({ withPayload: true, payloadVersion: "0.0.1" });
  const check = postinstall(tree);
  assert.equal(check.status, 1, check.stderr);
  assert.ok(check.stderr.includes(payload + "@0.0.1 instead"), check.stderr);
  const result = run(tree, ["--version"]);
  assert.equal(result.status, 1);
  assert.ok(!fs.existsSync(tree.marker), "the stale binary must not run");
});

// ── scripts/wait-npm-published.sh with a fake curl ──────────────────────────────────────────────
// The fake registry answers ONLY the install packument (the Accept header npm install sends); a
// script that read the full packument (npm view) would see nothing and time out.
function fakes({ listedAfter, downloads }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "wait-npm-"));
  const count = path.join(dir, "count");
  fs.writeFileSync(count, "0");
  const listed = JSON.stringify({ versions: { "9.9.9": { dist: { tarball: "https://registry.example/pkg.tgz" } } } });
  fs.writeFileSync(
    path.join(dir, "curl"),
    "#!/bin/sh\ncase \"$*\" in\n" +
      "*vnd.npm.install-v1+json*registry.npmjs.org/@maxplayerai%2flinux-x64*)\n" +
      "  n=$(($(cat '" + count + "') + 1)); echo $n > '" + count + "'\n" +
      "  if [ $n -gt " + listedAfter + " ]; then echo '" + listed + "'; else echo '{\"versions\":{}}'; fi; exit 0;;\n" +
      "*registry.example/pkg.tgz*) exit " + (downloads ? 0 : 22) + ";;\n" +
      "esac\nexit 22\n",
    { mode: 0o755 },
  );
  return dir;
}
function wait(dir, timeout) {
  return spawnSync("bash", [path.join(repo, "scripts", "wait-npm-published.sh"), "@maxplayerai/linux-x64", "9.9.9"], {
    env: { ...process.env, CURL: path.join(dir, "curl"), WAIT_NPM_TIMEOUT: String(timeout), WAIT_NPM_INTERVAL: "0.1" },
    encoding: "utf8",
  });
}

test("wait-npm-published waits until the registry serves the tarball", () => {
  const result = wait(fakes({ listedAfter: 2, downloads: true }), 30);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /waiting: @maxplayerai\/linux-x64@9\.9\.9 — the registry's install metadata does not list it yet/);
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
  const payloadPublish = job.indexOf('./scripts/npm-publish-once.sh "npm/cli-$platform"');
  const waitAll = job.indexOf('./scripts/wait-npm-published.sh "@maxplayerai/$platform" "$VERSION"');
  const launcherPublish = job.indexOf("./scripts/npm-publish-once.sh npm/maxplayer");
  assert.ok(payloadPublish > 0, "payload publish not found");
  assert.ok(launcherPublish > 0, "launcher publish not found");
  assert.ok(waitAll > payloadPublish && waitAll < launcherPublish, "the launcher must be published only after the payload wait");
  // Every publish goes through the conflict-tolerant script, so a re-run never dies on E403.
  const commands = job.replace(/^\s*(#|-?\s*name:).*$/gm, "");
  assert.ok(!/npm publish/.test(commands), "a bare npm publish is left in the publish job");
  // The wait loops over every release platform, not one hard-coded package.
  assert.match(job.slice(payloadPublish, waitAll), /for platform in \$RELEASE_PLATFORMS; do\s*$/);
});

// ── scripts/npm-publish-once.sh with a fake npm ─────────────────────────────────────────────────
// A re-run whose `npm view` read a stale cache reaches `npm publish` for a version the registry
// already has. npm's E403 "cannot publish over" refusal means published, not failed.
function publishOnce(npmScript) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "publish-once-"));
  const pkgDir = path.join(dir, "pkg");
  fs.mkdirSync(pkgDir);
  fs.writeFileSync(path.join(pkgDir, "package.json"), JSON.stringify({ name: "@maxplayerai/linux-x64", version: "9.9.9" }));
  fs.writeFileSync(path.join(dir, "npm"), "#!/bin/sh\n" + npmScript + "\n", { mode: 0o755 });
  const result = spawnSync("bash", [path.join(repo, "scripts", "npm-publish-once.sh"), pkgDir, "--access", "public"], {
    env: { ...process.env, NPM: path.join(dir, "npm") },
    encoding: "utf8",
  });
  fs.rmSync(dir, { recursive: true, force: true });
  return result;
}

test("npm-publish-once treats npm's already-published refusal as done (RED ON REVERT: E403 fails the re-run)", () => {
  const conflict = publishOnce(
    "echo 'npm error code E403' >&2; echo 'npm error 403 403 Forbidden - PUT https://registry.npmjs.org/@maxplayerai%2flinux-x64 - You cannot publish over the previously published versions: 9.9.9.' >&2; exit 1",
  );
  assert.equal(conflict.status, 0, conflict.stderr);
  assert.match(conflict.stdout, /@maxplayerai\/linux-x64@9\.9\.9 is already on the registry/);
  const legacy = publishOnce("echo 'npm ERR! code EPUBLISHCONFLICT' >&2; exit 1");
  assert.equal(legacy.status, 0, legacy.stdout);
});

test("npm-publish-once passes the publish arguments and fails on any other error", () => {
  const ok = publishOnce('[ "$*" = "publish --access public" ] || { echo "args: $*"; exit 9; }; echo published');
  assert.equal(ok.status, 0, ok.stdout);
  assert.match(ok.stdout, /published/);
  const denied = publishOnce("echo 'npm error code E403' >&2; echo 'npm error 403 Forbidden - You do not have permission to publish' >&2; exit 1");
  assert.equal(denied.status, 1);
  assert.match(denied.stdout, /do not have permission/);
  assert.match(denied.stderr, /publishing @maxplayerai\/linux-x64@9\.9\.9 failed/);
});

test("wait-npm-published never fetches a tarball location that is not an http(s) URL", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "wait-npm-"));
  const listed = JSON.stringify({ versions: { "9.9.9": { dist: { tarball: "-o/tmp/x file:///etc/passwd" } } } });
  const fetched = path.join(dir, "fetched");
  fs.writeFileSync(
    path.join(dir, "curl"),
    "#!/bin/sh\ncase \"$*\" in\n*vnd.npm.install-v1+json*) echo '" + listed + "'; exit 0;;\nesac\ntouch '" + fetched + "'; exit 0\n",
    { mode: 0o755 },
  );
  const result = wait(dir, 1);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /does not list it yet/);
  assert.ok(!fs.existsSync(fetched), "a non-URL tarball was passed to curl");
  fs.rmSync(dir, { recursive: true, force: true });
});
