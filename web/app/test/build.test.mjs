/**
 * The build's shipped-surface invariants: the agent-facing URLs (/skill.md,
 * the discovery index) and the cache-stamped asset references. These pin what
 * a deploy publishes, not how the app behaves — that's market.test.ts.
 */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
execFileSync(process.execPath, ["scripts/build.mjs"], { cwd: root });

const legacyIndex = JSON.parse(
  readFileSync(join(root, ".well-known", "skills", "index.json"), "utf8"),
);
const discoveryIndex = JSON.parse(
  readFileSync(join(root, "dist", ".well-known", "agent-skills", "index.json"), "utf8"),
);

test("build publishes the RFC v0.2.0 discovery index for every legacy skill", () => {
  assert.equal(
    discoveryIndex.$schema,
    "https://schemas.agentskills.io/discovery/0.2.0/schema.json",
  );
  assert.equal(discoveryIndex.skills.length, legacyIndex.skills.length);
  assert.deepEqual(
    discoveryIndex.skills.map(({ name, description }) => ({ name, description })),
    legacyIndex.skills.map(({ name, description }) => ({ name, description })),
  );

  for (const skill of discoveryIndex.skills) {
    const legacy = legacyIndex.skills.find(({ name }) => name === skill.name);
    assert.equal(skill.type, "skill-md");
    assert.equal(skill.url, `https://www.maxplayer.ai${legacy.path}`);
    assert.deepEqual(Object.keys(skill), ["name", "type", "description", "url", "digest"]);
  }
});

test("discovery digests are lowercase SHA-256 hashes of the raw artifacts", () => {
  for (const skill of discoveryIndex.skills) {
    const artifactPath = new URL(skill.url).pathname;
    const artifact = readFileSync(join(root, artifactPath.slice(1)));
    const digest = `sha256:${createHash("sha256").update(artifact).digest("hex")}`;
    assert.match(skill.digest, /^sha256:[0-9a-f]{64}$/);
    assert.equal(skill.digest, digest);
  }
});

// The expected list is DERIVED from the legacy index, never written out here. A literal
// roll-call passes unchanged the day a sixth skill is added and the homepage never links
// it — the skill then exists, ships, and is reachable only by an agent that already knows
// its URL, because discovery from this page is top-down. Deriving it means adding a skill
// to the index is what makes this test demand the link.
test("the homepage skill links every companion skill the index publishes", () => {
  const homepage = readFileSync(
    join(root, ".well-known", "skills", "default", "skill.md"),
    "utf8",
  );
  // The homepage IS the `default` entry; it does not link to itself.
  const companions = legacyIndex.skills
    .map(({ name, path }) => {
      const dir = /^\/\.well-known\/skills\/([^/]+)\/skill\.md$/.exec(path)?.[1];
      assert.ok(dir, `${name} has the published skill path shape, got ${path}`);
      return dir;
    })
    .filter((dir) => dir !== "default");

  assert.ok(companions.length > 0, "the index publishes at least one companion skill");
  for (const name of companions) {
    assert.match(
      homepage,
      new RegExp(`\\[${name}\\]\\(/\\.well-known/skills/${name}/skill\\.md\\)`),
      `the homepage links ${name}`,
    );
  }
});

test("the root skill alias is byte-identical to the canonical homepage skill", () => {
  assert.deepEqual(
    readFileSync(join(root, "dist", "skill.md")),
    readFileSync(join(root, ".well-known", "skills", "default", "skill.md")),
  );
});

test("every asset URL in shipped HTML and CSS carries the deploy stamp", () => {
  const { stamp } = JSON.parse(readFileSync(join(root, "dist", ".buildstamp"), "utf8"));
  assert.match(stamp, /^[0-9a-f]{12}$/);

  const html = readFileSync(join(root, "dist", "index.html"), "utf8");
  for (const page of ["index.html", "market.html", "sell.html", "tokens.html"]) {
    const pageHtml = readFileSync(join(root, "dist", page), "utf8");
    const script = page === "tokens.html" ? "tokens.js" : "terminal.js";
    for (const asset of ["styles.css", "fonts.css", script]) {
      assert.ok(pageHtml.includes(`./${asset}?v=${stamp}`), `${asset} is stamped in ${page}`);
      assert.ok(!pageHtml.includes(`"./${asset}"`), `no unstamped ${asset} reference remains in ${page}`);
    }
  }
  // Font preload URLs must match fonts.css URLs byte-for-byte or the browser
  // fetches every preloaded font twice.
  const fontsCss = readFileSync(join(root, "dist", "fonts.css"), "utf8");
  const preloads = [...html.matchAll(/href="\.\/(fonts\/[^"?]+)\?v=([0-9a-f]{12})"/g)];
  assert.ok(preloads.length >= 2, "font preloads exist and are stamped");
  for (const m of preloads) {
    assert.equal(m[2], stamp, `${m[1]} preload carries the stamp`);
    assert.ok(fontsCss.includes(`url('./${m[1]}?v=${stamp}')`), `${m[1]} stamped identically in fonts.css`);
  }
  assert.ok(!/url\((['"])\.\/fonts\/[^'"?]+\1\)/.test(fontsCss), "no unstamped font URL remains in fonts.css");
});

test("the Node floor the bake actually needs is declared", () => {
  // scripts/bake-snapshot.mjs needs a GLOBAL WebSocket, which arrived in Node
  // 22. Undeclared, the host picks its own version and the bake becomes a
  // permanent no-op that fails the deploy quietly — the client just never gets
  // a snapshot, which looks exactly like a first visit.
  const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
  const floor = Number(/(\d+)/.exec(pkg.engines?.node ?? "")?.[1]);
  assert.ok(floor >= 22, `engines.node must require >=22, got ${pkg.engines?.node}`);
});

test("llms.txt ships at the site root, byte-identical to the source", () => {
  // Part of the agent-facing surface and a live URL: maxplayer.ai/llms.txt.
  // The rebuild dropped it once already, and nothing else fails when it goes
  // missing: the build stays green and the URL just starts 404ing. Living
  // under public/ is what carries it today — this pins the OUTCOME, so moving
  // it back out without a copy step goes red instead of shipping a 404.
  assert.deepEqual(
    readFileSync(join(root, "dist", "llms.txt")),
    readFileSync(join(root, "public", "llms.txt")),
  );
});

test("the bundle ships as one module and the snapshot stays out of git", () => {
  assert.ok(existsSync(join(root, "dist", "terminal.js")));
  // A local bake writes public/snapshot.json (that's fine); git must ignore
  // it — 2.5MB of market data would rot in history and churn every refresh.
  const gitignore = readFileSync(join(root, ".gitignore"), "utf8");
  assert.match(gitignore, /^public\/snapshot\.json$/m, "snapshot.json is baked at deploy, never committed");
});

test("the live market ships at /market and the homepage links it", () => {
  // cleanUrls serves dist/market.html at /market. The board's static chrome
  // (the three lanes main.ts renders into) must live there, and the homepage
  // must carry no board — it boots no relay.
  const market = readFileSync(join(root, "dist", "market.html"), "utf8");
  for (const id of ["market", "buyers", "feed", "sellers", "statgrid", "windows", "conn", "utc-clock"]) {
    assert.ok(market.includes(`id="${id}"`), `market.html carries #${id}`);
  }
  const home = readFileSync(join(root, "dist", "index.html"), "utf8");
  assert.ok(!home.includes('id="market"'), "the homepage carries no board");
  assert.ok(home.includes('href="/market"'), "the homepage links the live market");
  assert.ok(home.includes('href="/sell"'), "the homepage links the seller page");
  const sell = readFileSync(join(root, "dist", "sell.html"), "utf8");
  assert.ok(!sell.includes('id="market"'), "the seller page carries no board");
  assert.ok(sell.includes("follow the seller instructions"), "the seller page hands out the seller line");
  // Old #market deep links (skill.md, llms.txt, shared URLs) still land on the board.
  assert.match(readFileSync(join(root, "src/main.ts"), "utf8"), /location\.hash === "#market".*location\.replace\("\/market"\)/);
  assert.ok(!home.includes("<script>"), "no inline scripts in the browser-key origin");
});

test("/tokens ships its own read-only bundle and stays unlisted", () => {
  const tokens = readFileSync(join(root, "dist", "tokens.html"), "utf8");
  for (const id of ["tokens", "lots", "recent", "done", "windows", "statgrid", "conn", "utc-clock", "trade-detail"]) {
    assert.ok(tokens.includes(`id="${id}"`), `tokens.html carries #${id}`);
  }
  assert.ok(!tokens.includes("terminal.js"), "the jobs bundle is not loaded on /tokens");
  assert.ok(!readFileSync(join(root, "dist", "terminal.js"), "utf8").includes("offchain.pub"), "the jobs bundle carries no trade reader");
  assert.match(tokens, /<meta name="robots" content="noindex, nofollow">/);
  // Lot details open in /market's popup window, not inline under the board.
  assert.match(tokens, /<aside class="dock dock-(left|right) pinned" id="trade-detail"[^>]*hidden>/);
  assert.ok(!/dock-mid/.test(tokens), "the lot popup never opens in the middle");
  assert.ok(tokens.indexOf('id="trade-detail"') > tokens.indexOf("</footer>"), "the popup lives outside the page flow");
  // Team link only (bob): nothing on the public site points at it.
  for (const page of ["index.html", "market.html", "sell.html", "tokens.html", "llms.txt", "skill.md"]) {
    const text = readFileSync(join(root, "dist", page), "utf8");
    assert.ok(!/href="\/(tokens|trades)"|\]\(\/(tokens|trades)\)/.test(text), `${page} does not link the token page`);
  }
});

test("the /tokens CSP opens only the trade relays, only on /tokens, and is noindex", () => {
  const { headers } = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  const matching = (path) => headers.filter((h) => new RegExp(`^${h.source.replace(/^\//, "\\/")}$`).test(path));
  const csp = (path) => matching(path).flatMap((h) => h.headers.filter((x) => x.key === "Content-Security-Policy").map((x) => x.value));
  for (const path of ["/", "/market", "/sell", "/terminal.js", "/tokens.js", "/tokensx"]) {
    const [v, ...rest] = csp(path);
    assert.equal(rest.length, 0, `${path} gets exactly one CSP`);
    assert.match(v, /connect-src 'self' wss:\/\/relay\.maxplayer\.ai https:\/\/api\.coinbase\.com;/, path);
    assert.ok(!v.includes("nos.lol"), `${path} does not open the trade relays`);
  }
  const [t, ...more] = csp("/tokens");
  assert.equal(more.length, 0, "/tokens gets exactly one CSP");
  assert.match(t, /connect-src 'self' wss:\/\/nos\.lol wss:\/\/relay\.primal\.net wss:\/\/offchain\.pub;/);
  assert.ok(!t.includes("relay.maxplayer.ai"), "/tokens cannot reach the production relay");
  assert.match(t, /script-src 'self';/);
  const robots = matching("/tokens").flatMap((h) => h.headers.filter((x) => x.key === "X-Robots-Tag").map((x) => x.value));
  assert.deepEqual(robots, ["noindex, nofollow"]);
});
