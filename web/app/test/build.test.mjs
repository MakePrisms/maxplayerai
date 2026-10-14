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
  for (const page of ["index.html", "market.html", "sell.html", "chat.html", "mint.html", "tokens.html"]) {
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

test("all site pages share navigation and footer links, with a current page and no inline scripts", () => {
  const pages = ["index.html", "sell.html", "chat.html", "mint.html", "market.html", "tokens.html"];
  let expectedNav;
  let expectedFooter;
  for (const page of pages) {
    const html = readFileSync(join(root, "dist", page), "utf8");
    const nav = html.match(/<nav class="nav-links"[^>]*>([\s\S]*?)<\/nav>/)?.[1];
    const footer = html.match(/<nav class="foot-nav"[^>]*>([\s\S]*?)<\/nav>/)?.[1];
    assert.ok(nav && footer, `${page} has shared chrome`);
    const links = (markup) =>
      [...markup.matchAll(/<a href="([^"]+)"[^>]*>([\s\S]*?)<\/a>/g)].map((m) => [m[1], m[2].replace(/<[^>]+>/g, "")]);
    expectedNav ??= links(nav);
    expectedFooter ??= links(footer);
    assert.deepEqual(links(nav), expectedNav, `${page} nav matches`);
    assert.deepEqual(links(footer), expectedFooter, `${page} footer matches`);
    // bob: Chat is linked from the footer only (marked Lab, like Tokens), never the top nav.
    assert.ok(!nav.includes('href="/chat"'), `${page}: /chat is not in the top nav`);
    assert.match(footer, /href="\/market">Market<\/a>\s*<div class="foot-labs">\s*<a href="\/chat">Chat<sup class="lab-sup">Lab<\/sup><\/a>/, `${page}: /chat is in the footer after Market, marked Lab`);
    // Mint is a Lab page too: footer only, right after Chat.
    assert.ok(!nav.includes('href="/mint"'), `${page}: /mint is not in the top nav`);
    assert.match(footer, /<a href="\/chat">Chat<sup class="lab-sup">Lab<\/sup><\/a>\s*<a href="\/mint">Mint<sup class="lab-sup">Lab<\/sup><\/a>/, `${page}: /mint is in the footer after Chat, marked Lab`);
    // Tokens is a Lab page too: footer only, right after Mint.
    assert.ok(!nav.includes('href="/tokens"'), `${page}: /tokens is not in the top nav`);
    assert.match(footer, /<a href="\/mint">Mint<sup class="lab-sup">Lab<\/sup><\/a>\s*<a href="\/tokens">Tokens<sup class="lab-sup">Lab<\/sup><\/a>/, `${page}: /tokens is in the footer after Mint, marked Lab`);
    // bob: Skill and GitHub leave the top bar; they sit with @maxplayerai on the
    // footer's second, dimmer row.
    assert.deepEqual(links(nav).map(([href]) => href), ["/", "/sell", "/market"], `${page}: top nav is Buy, Sell, Market`);
    // bob: the Lab pages are grouped, so phones can put them on their own row.
    const labs = footer.match(/<div class="foot-labs">([\s\S]*?)<\/div>/)?.[1] ?? "";
    assert.deepEqual(links(labs).map(([href]) => href), ["/chat", "/mint", "/tokens"], `${page}: Lab links grouped`);
    const sub = footer.match(/<div class="foot-row foot-sub">([\s\S]*?)<\/div>/)?.[1] ?? "";
    assert.deepEqual(links(sub).map(([, text]) => text), ["Skill", "GitHub", "@maxplayerai"], `${page}: footer second row`);
    const current = [...nav.matchAll(/aria-current="page"/g)].length;
    if (page === "chat.html" || page === "mint.html" || page === "tokens.html") {
      assert.equal(current, 0, `${page} is not in the top nav, so nothing there is current`);
    } else {
      const route = page === "index.html" ? "/" : `/${page.replace(".html", "")}`;
      assert.ok(nav.includes(`href="${route}" aria-current="page"`), `${page} marks itself current`);
      assert.equal(current, 1);
    }
    for (const script of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g)) {
      assert.match(script[1], /\bsrc="/);
      assert.equal(script[2].trim(), "", `${page} has no inline script`);
    }
  }
});

test("/chat carries the /tokens experimental notice under the nav", () => {
  const chat = readFileSync(join(root, "dist", "chat.html"), "utf8");
  assert.match(chat, /<\/header>\s*<!--[^>]*-->\s*<p class="labs-strip" role="note"><span class="labs-pill">Experimental<\/span>/, "experimental strip under the nav, like /tokens");
});

test("/tokens ships its own read-only bundle and is linked from the footer", () => {
  const tokens = readFileSync(join(root, "dist", "tokens.html"), "utf8");
  for (const id of ["tokens", "lots", "recent", "windows", "statgrid", "conn", "utc-clock", "trade-detail"]) {
    assert.ok(tokens.includes(`id="${id}"`), `tokens.html carries #${id}`);
  }
  assert.ok(!tokens.includes("terminal.js"), "the jobs bundle is not loaded on /tokens");
  assert.ok(!readFileSync(join(root, "dist", "terminal.js"), "utf8").includes("offchain.pub"), "the jobs bundle carries no trade reader");
  assert.match(tokens, /<meta name="robots" content="noindex, nofollow">/);
  // bob: the skeleton is the real frame. Inline styles are dropped by the
  // /tokens CSP (style-src 'self'), so none may carry layout; the stat labels
  // and period buttons are static, and only the values shimmer.
  assert.ok(!/\sstyle="/.test(tokens), "no inline style attributes (CSP drops them)");
  assert.deepEqual([...tokens.matchAll(/data-w="([^"]+)"/g)].map((m) => m[1]), ["24h", "7d", "all"]);
  assert.match(tokens, /data-w="24h" aria-pressed="true"/);
  assert.deepEqual([...tokens.matchAll(/data-stat="([^"]+)"/g)].map((m) => m[1]), ["trades", "give", "want", "sellers", "pairs", "fill"]);
  assert.equal((tokens.match(/class="skel skel-stat"/g) ?? []).length, 6);
  // Lot details open in /market's popup window, not inline under the board.
  assert.match(tokens, /<aside class="dock dock-(left|right) pinned" id="trade-detail"[^>]*hidden>/);
  assert.ok(!/dock-mid/.test(tokens), "the lot popup never opens in the middle");
  assert.ok(!tokens.includes('id="done"'), "no completed-trades table under the board (bob)");
  assert.ok(!tokens.includes('id="stats-note"') && !tokens.includes("Mints can fail"), "no footnotes (bob)");
  assert.ok(tokens.indexOf('id="trade-detail"') > tokens.indexOf("</footer>"), "the popup lives outside the page flow");
  // bob: linked from the footer (asserted with the shared chrome); the page
  // carries the experimental notice right under the nav, like /chat and /mint.
  assert.match(tokens, /<\/header>\s*<!--[^>]*-->\s*<p class="labs-strip" role="note"><span class="labs-pill">Experimental<\/span>/);
  // Agent-facing docs stay about the jobs market.
  for (const page of ["llms.txt", "skill.md"]) {
    const text = readFileSync(join(root, "dist", page), "utf8");
    assert.ok(!/\]\(\/(tokens|trades)\)|maxplayer\.ai\/tokens/.test(text), `${page} does not link the token page`);
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

test("/mint explains the seller mint with the shared page layout", () => {
  const mint = readFileSync(join(root, "dist", "mint.html"), "utf8");
  assert.match(mint, /<body class="home mint-page">/);
  assert.match(mint, /<h1 class="h-title-short h-title-mint">Mint your<br \/><em>own tokens\.<\/em><\/h1>/);
  assert.equal([...mint.matchAll(/<ol class="h-steps">[\s\S]*?<\/ol>/g)].length, 1);
  assert.equal([...mint.matchAll(/<li>\s*<span class="n">0\d<\/span>/g)].length, 3, "three steps, like Chat");
  assert.ok(mint.includes('id="start"') && mint.includes('class="h-manual"') && mint.includes('class="h-section h-faq"'));
  // Every command shown is a real one from the maxplayer-mint README / CLI usage.
  for (const cmd of ["cargo install --path crates/maxplayer-mint --locked", "maxplayer-mint init", "maxplayer-mint run", "maxplayer-mint issue 1000", "maxplayer wallet mints add nostr://"]) {
    assert.ok(mint.includes(cmd), `shows ${cmd}`);
  }
  assert.match(mint, /<\/header>\s*<!--[^>]*-->\s*<p class="labs-strip" role="note"><span class="labs-pill">Experimental<\/span>/, "experimental strip under the nav, like /tokens");
  assert.match(mint, /Are my tokens bitcoin\?<\/summary><p>No\./, "seller tokens are distinguished from bitcoin");
});

test("Mint's one-line instruction resolves to a shipped, discoverable combined skill", () => {
  const mint = readFileSync(join(root, "dist", "mint.html"), "utf8");
  const instruction = /<code id="cmd-guide">([^<]+)<\/code>/.exec(mint)?.[1];
  assert.ok(instruction, "the copy button has an instruction to copy");
  assert.match(mint, /data-copy="cmd-guide"/);
  const urls = instruction.match(/https:\/\/\S+/g) ?? [];
  assert.equal(urls.length, 1, "one entry point, not separate mint and trade prompts");
  const url = new URL(urls[0]);
  assert.equal(url.origin, "https://www.maxplayer.ai");
  assert.ok(discoveryIndex.skills.some((skill) => skill.url === url.href));
  const skill = readFileSync(join(root, "dist", url.pathname), "utf8");
  // The published entry point must hand off to both actual operating manuals.
  for (const path of ["crates/maxplayer-mint/README.md", "crates/maxplayer-trade/skills/maxplayer-trade/SKILL.md"]) {
    assert.ok(skill.includes(`https://github.com/MakePrisms/maxplayerai/blob/main/${path}`));
    assert.ok(existsSync(join(root, "../..", path)), `${path} exists in this revision`);
  }
  const mintReadme = readFileSync(join(root, "../../crates/maxplayer-mint/README.md"), "utf8");
  assert.ok(mintReadme.includes(instruction), "README and website give the same entry instruction");
});

test("FAQ sections pin their heading so opening an answer never moves it", () => {
  const css = readFileSync(join(root, "dist", "styles.css"), "utf8");
  assert.match(css, /\.h-section\.h-faq\.faq-pinned \{ justify-content: flex-start; \}/);
  assert.match(css, /\.h-faq\.faq-pinned > \.h-in \{ margin-top: var\(--faq-lift, 0px\); \}/);
  const js = readFileSync(join(root, "dist", "terminal.js"), "utf8");
  assert.ok(js.includes("faq-pinned") && js.includes("--faq-lift"), "the shared bundle pins FAQs");
  for (const page of ["index.html", "sell.html", "chat.html", "mint.html"]) {
    const html = readFileSync(join(root, "dist", page), "utf8");
    assert.match(html, /class="h-section h-faq[" ]/, `${page} has an FAQ`);
    assert.match(html, /src="\.\/terminal\.js/, `${page} loads the bundle that pins it`);
  }
});
