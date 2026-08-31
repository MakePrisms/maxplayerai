/**
 * Watch the runner lamps in a real browser, with `prefers-reduced-motion`
 * emulated both ways.
 *
 * `npm test` has no browser and no CSS engine, so test/motion.test.mjs can
 * only assert the SHAPE of the stylesheet — that no reduced-motion rule
 * suppresses the lamp. It cannot answer the question bob actually asked:
 * with the preference on, does the sweep move? Only a CSS engine can.
 *
 * NOT part of `npm test`. It needs a browser binary:
 *
 *   node scripts/motion-check.mjs
 *   CHROME_PATH=/path/to/chrome node scripts/motion-check.mjs
 *
 * The lamp markup comes from the real `statusDot()`, bundled here with
 * esbuild, so this fixture cannot drift from what the app ships. The
 * stylesheet is the real public/styles.css, served as-is.
 *
 * ## The control that makes the `reduce` arm mean anything
 *
 * An emulation call that silently did nothing would make every reduce-arm
 * reading identical to the no-preference arm — and "the lamp animates under
 * reduce" would then be a false pass produced by the preference never being
 * set. Two independent controls run in the same pass:
 *
 *   1. `matchMedia("(prefers-reduced-motion: reduce)").matches` flips.
 *   2. `.role`'s transition-duration goes 0.12s -> 0s, which is a DIFFERENT
 *      rule in the same media block. That proves the emulated query reaches
 *      this stylesheet, not merely the matchMedia API.
 *
 * A reduce arm whose controls do not both flip is a failed run, not a pass.
 */
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const CHROME = process.env.CHROME_PATH
  ?? join(process.env.HOME ?? "", "Library/Caches/ms-playwright/chromium_headless_shell-1234/chrome-headless-shell-mac-arm64/chrome-headless-shell");

const failures = [];
const check = (ok, label) => {
  console.log(`${ok ? "  ok  " : "  FAIL"} ${label}`);
  if (!ok) failures.push(label);
};

/* --- the fixture: real markup, real stylesheet -------------------------- */

const scratch = mkdtempSync(join(tmpdir(), "motion-check-"));
const { build } = await import(pathToFileURL(join(root, "node_modules", "esbuild", "lib", "main.js")).href);
await build({
  entryPoints: [join(root, "src", "ui", "indicators.ts")],
  bundle: true,
  format: "esm",
  outfile: join(scratch, "indicators.mjs"),
  logLevel: "silent",
});
const { statusDot, RACE_LIGHT_CYCLE_SECONDS } = await import(
  pathToFileURL(join(scratch, "indicators.mjs")).href
);

// One working runner and one idle runner, straight out of the shipped
// function. `statusDot` only reads jobs.length, so a single opaque job is a
// faithful "working" input.
const workingMarkup = statusDot(true, [{}]);
const idleMarkup = statusDot(true, []);
const styles = readFileSync(join(root, "public", "styles.css"), "utf8");

const fixture = `<!doctype html><html><head><meta charset="utf-8">
<style>${styles}</style></head><body>
<span id="role-probe" class="role racer"><span>probe</span></span>
<span id="working">${workingMarkup}</span>
<span id="idle">${idleMarkup}</span>
</body></html>`;

const server = createServer((_req, res) => {
  res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
  res.end(fixture);
}).listen(0, "127.0.0.1");
await new Promise((r) => server.once("listening", r));
const origin = `http://127.0.0.1:${server.address().port}/`;

/* --- browser + a minimal CDP client ------------------------------------- */

const profile = mkdtempSync(join(tmpdir(), "motion-profile-"));
const browser = spawn(CHROME, [
  "--headless", "--disable-gpu", "--no-first-run",
  `--user-data-dir=${profile}`, "--remote-debugging-port=0",
], { stdio: ["ignore", "pipe", "pipe"] });

const wsUrl = await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("browser never printed a devtools url")), 20000);
  let buffered = "";
  browser.stderr.on("data", (chunk) => {
    buffered += chunk;
    const match = buffered.match(/ws:\/\/[^\s]+/);
    if (match) { clearTimeout(timer); resolve(match[0]); }
  });
});

let nextId = 1;
const pending = new Map();
const socket = new WebSocket(wsUrl);
await new Promise((resolve) => socket.addEventListener("open", resolve));
socket.addEventListener("message", (event) => {
  const message = JSON.parse(event.data);
  if (message.id === undefined) return;
  const waiter = pending.get(message.id);
  if (!waiter) return;
  pending.delete(message.id);
  message.error ? waiter.reject(new Error(JSON.stringify(message.error))) : waiter.resolve(message.result);
});
const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
  const id = nextId++;
  pending.set(id, { resolve, reject });
  socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
});

const { targetId } = await send("Target.createTarget", { url: "about:blank" });
const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
await send("Runtime.enable", {}, sessionId);
await send("Page.enable", {}, sessionId);

async function evaluate(expression) {
  const { result, exceptionDetails } = await send(
    "Runtime.evaluate",
    { expression, awaitPromise: true, returnByValue: true },
    sessionId,
  );
  if (exceptionDetails) throw new Error(exceptionDetails.exception?.description ?? exceptionDetails.text);
  return result.value;
}

/**
 * Sample each lamp bar's computed background across slightly more than one
 * full cycle. A suppressed animation yields ONE distinct colour per bar; a
 * running sweep yields several.
 */
const PROBE = `(async () => {
  const bars = [...document.querySelectorAll("#working .dot.working i")];
  const idle = [...document.querySelectorAll("#idle .dot i")];
  const cs = (el) => getComputedStyle(el);
  const seen = bars.map(() => new Set());
  const cycleMs = ${RACE_LIGHT_CYCLE_SECONDS} * 1000;
  const started = performance.now();
  while (performance.now() - started < cycleMs * 1.1) {
    bars.forEach((bar, i) => seen[i].add(cs(bar).backgroundColor));
    await new Promise((r) => requestAnimationFrame(r));
  }
  return {
    reduceMatches: matchMedia("(prefers-reduced-motion: reduce)").matches,
    roleTransition: cs(document.getElementById("role-probe")).transitionDuration,
    barCount: bars.length,
    names: bars.map((b) => cs(b).animationName),
    durations: bars.map((b) => cs(b).animationDuration),
    playStates: bars.map((b) => cs(b).animationPlayState),
    distinctColours: seen.map((s) => s.size),
    colours: seen.map((s) => [...s]),
    idleNames: idle.map((b) => cs(b).animationName),
    idleNose: idle.length ? cs(idle[0]).backgroundColor : null,
  };
})()`;

async function arm(label, features) {
  await send("Emulation.setEmulatedMedia", { features }, sessionId);
  await send("Page.navigate", { url: origin }, sessionId);
  // Re-navigating guarantees a fresh document under the new media state.
  await new Promise((r) => setTimeout(r, 400));
  const reading = await evaluate(PROBE);
  console.log(`\n[${label}]`);
  console.log(`  matchMedia reduce = ${reading.reduceMatches} · .role transition = ${reading.roleTransition}`);
  console.log(`  animationName     = ${reading.names.join(", ")}`);
  console.log(`  animationDuration = ${reading.durations.join(", ")}`);
  console.log(`  playState         = ${reading.playStates.join(", ")}`);
  console.log(`  distinct colours  = ${reading.distinctColours.join(", ")}`);
  return reading;
}

try {
  const normal = await arm("no-preference", []);
  const reduced = await arm("reduce", [{ name: "prefers-reduced-motion", value: "reduce" }]);

  console.log("\n--- controls: the emulation actually reached the stylesheet ---");
  check(normal.reduceMatches === false, "no-preference arm: matchMedia(reduce) is false");
  check(reduced.reduceMatches === true, "reduce arm: matchMedia(reduce) is true");
  check(
    normal.roleTransition !== reduced.roleTransition && reduced.roleTransition === "0s",
    `.role transition-duration flips ${normal.roleTransition} -> ${reduced.roleTransition} (a different rule in the same block)`,
  );

  console.log("\n--- the fix: the lamps sweep with the preference ON ---");
  check(reduced.barCount === 3, `reduce arm: found ${reduced.barCount} lamp bars`);
  check(
    reduced.names.join(",") === "lamp-a,lamp-b,lamp-c",
    `reduce arm: animation names are ${reduced.names.join(",")}`,
  );
  check(
    reduced.durations.every((d) => d === `${RACE_LIGHT_CYCLE_SECONDS}s`),
    `reduce arm: every bar runs at ${RACE_LIGHT_CYCLE_SECONDS}s`,
  );
  check(
    reduced.playStates.every((s) => s === "running"),
    `reduce arm: every bar's animation-play-state is running`,
  );
  check(
    reduced.distinctColours.every((n) => n >= 2),
    `reduce arm: every bar actually CHANGES colour over one cycle (${reduced.distinctColours.join(",")} distinct)`,
  );

  console.log("\n--- no regression: normal rendering is unchanged ---");
  check(
    normal.names.join(",") === reduced.names.join(","),
    "animation names identical in both arms",
  );
  check(
    normal.durations.join(",") === reduced.durations.join(","),
    "animation durations identical in both arms",
  );
  check(
    JSON.stringify(normal.distinctColours) === JSON.stringify(reduced.distinctColours) ||
      normal.distinctColours.every((n) => n >= 2),
    `no-preference arm still sweeps (${normal.distinctColours.join(",")} distinct)`,
  );
  check(
    normal.idleNames.every((n) => n === "none") && reduced.idleNames.every((n) => n === "none"),
    "an IDLE runner's bars animate in neither arm",
  );
  check(
    normal.idleNose === reduced.idleNose,
    `an idle runner's lit nose is the same colour in both arms (${normal.idleNose})`,
  );
} finally {
  socket.close();
  server.close();
  // Wait for the browser to actually exit before removing its profile.
  // `kill()` only delivers the signal, and Chromium keeps writing the profile
  // on the way down: removing it too early throws ENOTEMPTY and turns a run
  // whose every check passed into a non-zero exit. `force` does not cover
  // that — it suppresses ENOENT only — so the wait and the retries are both
  // load-bearing. Measured here at roughly 1 run in 8 before the wait.
  const exited = new Promise((resolve) => browser.once("exit", resolve));
  browser.kill();
  await Promise.race([exited, new Promise((r) => setTimeout(r, 5000))]);
  for (const dir of [scratch, profile]) {
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}

console.log(
  failures.length
    ? `\nFAILED — ${failures.length} check(s):\n  - ${failures.join("\n  - ")}`
    : "\nAll checks passed.",
);
process.exit(failures.length ? 1 : 0);
