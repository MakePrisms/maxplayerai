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
    // The shipped bytes (the build writes the swarm buyer key into the join skill).
    const artifact = readFileSync(join(root, "dist", artifactPath.slice(1)));
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
  for (const page of ["index.html", "market.html", "sell.html", "chat.html", "mint.html", "tokens.html", "swarm.html"]) {
    const pageHtml = readFileSync(join(root, "dist", page), "utf8");
    const script = page === "tokens.html" ? "tokens.js" : page === "swarm.html" ? "swarm.js" : "terminal.js";
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
  const pages = ["index.html", "sell.html", "chat.html", "mint.html", "market.html", "tokens.html", "swarm.html"];
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
    assert.match(footer, /href="\/market">Market<\/a>\s*<\/div>\s*<div class="foot-row foot-labs">\s*<a href="\/chat">Chat<sup class="lab-sup">Lab<\/sup><\/a>/, `${page}: /chat opens the Lab row under Buy, Sell, Market, marked Lab`);
    // Mint is a Lab page too: footer only, right after Chat.
    assert.ok(!nav.includes('href="/mint"'), `${page}: /mint is not in the top nav`);
    assert.match(footer, /<a href="\/chat">Chat<sup class="lab-sup">Lab<\/sup><\/a>\s*<a href="\/mint">Mint<sup class="lab-sup">Lab<\/sup><\/a>/, `${page}: /mint is in the footer after Chat, marked Lab`);
    // Tokens is a Lab page too: footer only, right after Mint.
    assert.ok(!nav.includes('href="/tokens"'), `${page}: /tokens is not in the top nav`);
    assert.match(footer, /<a href="\/mint">Mint<sup class="lab-sup">Lab<\/sup><\/a>\s*<a href="\/tokens">Tokens<sup class="lab-sup">Lab<\/sup><\/a>/, `${page}: /tokens is in the footer after Mint, marked Lab`);
    // bob (2026-10-18): Swarm is a Lab page too, right after Tokens; since 2026-10-19 it lives at /swarm.
    assert.ok(!nav.includes('href="/swarm"'), `${page}: /swarm is not in the top nav`);
    assert.match(footer, /<a href="\/tokens">Tokens<sup class="lab-sup">Lab<\/sup><\/a>\s*<a href="\/swarm">Swarm<sup class="lab-sup">Lab<\/sup><\/a>/, `${page}: Swarm is in the footer after Tokens, marked Lab`);
    // bob: Skill and GitHub leave the top bar; since 2026-10-18 they are the footer's
    // third, dimmer row, under the Lab links.
    assert.deepEqual(links(nav).map(([href]) => href), ["/", "/sell", "/market"], `${page}: top nav is Buy, Sell, Market`);
    // bob (2026-10-18): three rows on the right: Buy · Sell · Market, the Labs, then Skill · GitHub.
    const rows = [...footer.matchAll(/<div class="foot-row[^"]*">([\s\S]*?)<\/div>/g)].map((m) => links(m[1]).map(([, text]) => text));
    assert.deepEqual(rows, [["Buy", "Sell", "Market"], ["ChatLab", "MintLab", "TokensLab", "SwarmLab"], ["Skill", "GitHub"]], `${page}: footer rows`);
    // ...and the @maxplayerai handle moves to the left, under "The Multiplayer Marketplace".
    const brand = html.match(/<div class="foot-brand">([\s\S]*?)<\/div>/)?.[1] ?? "";
    assert.match(brand, /<p>The Multiplayer Marketplace<\/p>\s*<a class="foot-x" href="https:\/\/x\.com\/maxplayerai"[^>]*>@maxplayerai<\/a>/, `${page}: X handle under the tagline`);
    assert.ok(!footer.includes("x.com/maxplayerai"), `${page}: X handle is not in the link rows`);
    const current = [...nav.matchAll(/aria-current="page"/g)].length;
    if (page === "chat.html" || page === "mint.html" || page === "tokens.html" || page === "swarm.html") {
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

test("/swarm hero facts start as skeletons, both labels and values, with no placeholder text (bob)", () => {
  const html = readFileSync(join(root, "dist", "swarm.html"), "utf8");
  const facts = html.match(/<dl class="c-facts" id="challenge-facts" aria-busy="true">([\s\S]*?)<\/dl>/)?.[1] ?? "";
  for (const id of ["challenge-start-label", "challenge-start", "challenge-budget-label", "challenge-budget"])
    assert.match(facts, new RegExp(`id="${id}"><span class="skel c-skel-d[dt]"></span></d[dt]>`), `${id} is a skeleton`);
  assert.equal([...facts.matchAll(/class="skel /g)].length, 4);
  for (const text of ["Loading…", "Work starts", "To be announced", "Budget"]) assert.ok(!facts.includes(text), `no static "${text}"`);
});

test("/swarm carries the /tokens experimental notice under the nav (bob)", () => {
  const html = readFileSync(join(root, "dist", "swarm.html"), "utf8");
  assert.match(html, /<\/header>\s*<!--[^>]*-->\s*<p class="labs-strip" role="note"><span class="labs-pill">Experimental<\/span><span>[^<]+<\/span><\/p>/, "experimental strip under the nav, like /tokens");
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
  // Operational commands match the mint README / CLI usage; install paths are checked below.
  for (const cmd of ["maxplayer-mint init", "maxplayer-mint run", "maxplayer-mint issue 1000", "maxplayer wallet mints add nostr://"]) {
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
  // Both entry points must install the executable where the linked service starts it.
  const servicePath = /ExecStart=%h\/(\S+) run/.exec(mintReadme)?.[1];
  assert.ok(servicePath, "mint README declares an absolute user-service executable");
  const install = /<code id="cmd-install">([^<]+)<\/code>/.exec(mint)?.[1]?.replaceAll("&amp;", "&");
  assert.ok(install, "manual copy button has an install command");
  for (const command of install.split(" && ")) {
    assert.ok(skill.includes(command), "manual and agent install commands agree");
    const crate = /--path crates\/(maxplayer-(?:mint|trade))\b/.exec(command)?.[1];
    const prefix = /--root "\$HOME\/([^"]+)"/.exec(command)?.[1];
    assert.ok(crate && prefix, "install explicitly selects the crate and destination");
    if (crate === "maxplayer-mint") {
      assert.equal(`${prefix}/bin/${crate}`, servicePath, "mint install and service paths agree");
    }
  }
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

test("swarm page stays noindexed, linked only as the footer's Swarm Lab, and isolated from the main bundle", () => {
  const html = readFileSync(join(root, "dist/swarm.html"), "utf8");
  const { stamp } = JSON.parse(readFileSync(join(root, "dist/.buildstamp"), "utf8"));
  assert.match(html, /name="robots" content="noindex, nofollow"/);
  assert.ok(html.includes(`./swarm.css?v=${stamp}`));
  assert.ok(!html.includes("terminal.js"));
  // bob (2026-10-18) put Swarm in the footer; that one link is the only place pages name the route.
  for (const page of ["index.html", "market.html", "sell.html", "chat.html", "mint.html", "tokens.html"]) {
    const text = readFileSync(join(root, "dist", page), "utf8");
    assert.equal([...text.matchAll(/\/swarm\b/g)].length, 1, `${page} names /swarm once`);
    assert.ok(!text.includes("/challenges"), `${page} no longer links the old /challenges route`);
    assert.ok(text.includes('<a href="/swarm">Swarm<sup class="lab-sup">Lab</sup></a>'), `${page}: only the footer Swarm link`);
  }
  for (const page of ["llms.txt", "skill.md"]) {
    const text = readFileSync(join(root, "dist", page), "utf8");
    assert.ok(!text.includes("/challenges") && !/maxplayer\.ai\/swarm|\]\(\/swarm\)/.test(text), `${page} does not advertise the route`);
  }
  const cfg = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  assert.ok(cfg.headers.some((r) => r.source === "/swarm" && r.headers.some((h) => h.key === "X-Robots-Tag" && h.value === "noindex, nofollow")));
  assert.ok(!readFileSync(join(root, "dist/terminal.js"), "utf8").includes("data-challenge-copy"));
});

// bob (2026-10-19, #community-challenges-build): "change the url and mentions of challenge to 'swarm'".
test("the community page builds at /swarm, every page links it there, and /challenges redirects permanently", () => {
  assert.ok(existsSync(join(root, "dist", "swarm.html")), "/swarm builds (cleanUrls serves swarm.html)");
  assert.ok(existsSync(join(root, "dist", "swarm.js")) && existsSync(join(root, "dist", "swarm.css")), "its bundle and styles build");
  for (const old of ["challenges.html", "challenges.js", "challenges.css"]) {
    assert.ok(!existsSync(join(root, "dist", old)), `dist/${old} is gone`);
  }
  for (const page of ["index.html", "sell.html", "market.html", "chat.html", "mint.html", "tokens.html", "swarm.html"]) {
    const html = readFileSync(join(root, "dist", page), "utf8");
    const swarmLinks = [...html.matchAll(/<a href="([^"]*)">Swarm<sup/g)].map((m) => m[1]);
    assert.deepEqual(swarmLinks, ["/swarm"], `${page}: the Swarm link points at /swarm`);
    assert.ok(!/href="\/challenges/.test(html), `${page}: no link to /challenges`);
  }
  const { redirects = [] } = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  for (const source of ["/challenges", "/challenges.html"]) {
    assert.ok(redirects.some((r) => r.source === source && r.destination === "/swarm" && r.permanent === true), `${source} redirects permanently to /swarm`);
  }
  // The join skill moved with the page; agents holding the old URL are redirected.
  assert.ok(redirects.some((r) => r.source === "/.well-known/skills/challenge-join/skill.md" && r.destination === "/.well-known/skills/swarm-join/skill.md" && r.permanent === true), "old join-skill path redirects");
  assert.ok(existsSync(join(root, "dist", ".well-known", "skills", "swarm-join", "skill.md")), "swarm-join skill ships");
  const skill = readFileSync(join(root, ".well-known", "skills", "swarm-join", "skill.md"), "utf8");
  assert.match(skill, /^---\nname: maxplayer-swarm-join\n/);
  assert.ok(skill.includes("tree/main/crates/maxplayer-challenges") && !skill.includes("feat/challenges-sidecar"), "sidecar link points at main");
  assert.ok(skill.includes("Experimental: build from source; there is\nno release binary yet.") && !skill.includes("Not deployed yet"));
  // bob (2026-10-19 00:49): commands use maxplayer-swarm and --run; the crate dir keeps its name.
  const commands = skill.split("\n").map((l) => l.trim()).filter((l) => /^maxplayer-(swarm|challenges) /.test(l));
  assert.ok(commands.length >= 6 && commands.every((l) => /^maxplayer-swarm (status|join|leave) /.test(l)), "every command line runs maxplayer-swarm");
  assert.ok(skill.includes("`status --run <id>`") && !skill.includes("--challenge"), "the day flag is --run");
  assert.ok(skill.includes("--manifest-path crates/maxplayer-challenges/Cargo.toml") && skill.includes("target/release/maxplayer-swarm`"), "build line unchanged; binary named");
  assert.match(skill, /old `maxplayer-challenges` binary\? That name is gone: rebuild[\s\S]*membership carries over unchanged/, "one line for existing members");
  // Skill walkthrough fixes (2026-10-19): free-job prerequisite, tested version, strict minutes-change read.
  // Swarm member feedback (2026-10-21, item 2): free jobs are an option, not a prerequisite.
  assert.ok(!/Prerequisite: free swarm jobs/.test(skill), "free jobs are not a prerequisite");
  assert.match(skill, /\*\*Paid or free: your choice\.\*\* Swarm jobs are paid like any other job\. A paid seller keeps its\n`rate_sats`/, "paid sellers keep their rate");
  assert.ok(skill.includes("To work for\nfree instead, set `takes_no_payment = true` and `rate_sats = 0`"), "free is opt-in, with its rate pairing");
  // Item 3 (bob 2026-10-21): allowlisting is the only step; the open surfaces are the operator's choice.
  assert.ok(!/Never enable open-pool\/open-targeted admission/.test(skill) && !/must also keep both `false`/.test(skill), "no requirement to close them");
  assert.ok(skill.includes("`claim_open_pool` and `accept_open_targeted` are the operator's choice,\n   on or off"), "on or off is the member's choice");
  assert.ok(skill.includes("with either on also takes free work from any buyer that reaches it"), "free-seller consequence stated");
  assert.ok(skill.includes("Tested with seller 0.6.1.") && !/⟨TBD⟩|or later/.test(skill), "tested version, no invented minimum");
  assert.ok(skill.includes("needs every relay, whether or not a swarm run is\n   in progress"), "minutes change needs every relay");
  assert.ok(!/\b300\b/.test(skill), "no unexercised 300-second floor");
  // bob (2026-10-19 02:08): the --json top-level `challenge` object is `run` (#1176).
  for (const key of ["`run.buyer`", "`run.goal`", "`run.starts`/`run.ends`", "`run.budget_sats`", '`"run": null`']) {
    assert.ok(skill.includes(key), `skill reads ${key}`);
  }
  assert.ok(!/`challenge\.|"challenge"|the `challenge` object/.test(skill), "no `challenge` JSON field left in the skill");
  assert.equal([...skill.matchAll(/(?<!crates\/)maxplayer-challenges/g)].length, 1, "the old binary name appears only in the existing-members line");
  // bob (2026-10-19 00:49): the CLI is maxplayer-swarm and the day flag is --run.
  const bundle = readFileSync(join(root, "dist", "swarm.js"), "utf8");
  assert.ok(bundle.includes("maxplayer-swarm join --buyer ") && !bundle.includes("maxplayer-challenges"), "the manual join command uses maxplayer-swarm");
  const page = readFileSync(join(root, "dist", "swarm.html"), "utf8");
  assert.ok(page.includes("https://www.maxplayer.ai/.well-known/skills/swarm-join/skill.md"), "the join line reads the swarm-join skill");
  assert.match(page, /<\/header>\s*<!--[^>]*-->\s*<p class="labs-strip" role="note"><span class="labs-pill">Experimental<\/span><span>Community swarms are early\. They may change, break or go away\.<\/span><\/p>/, "the experimental strip stays on /swarm");
  assert.match(page, /<title>Community Swarm — MAXPLAYER AI<\/title>/);
  // Visible copy says swarm; ids and classes (challenge-*) are internal and may stay.
  const visible = page.replace(/<script[\s\S]*?<\/script>/g, "").replace(/<!--[\s\S]*?-->/g, "")
    .replace(/<[^>]*>/g, (tag) => [...tag.matchAll(/(?:content|aria-label|title|alt)="([^"]*)"/g)].map((m) => ` ${m[1]} `).join(""));
  assert.ok(!/challenge/i.test(visible), `no visible "challenge" on /swarm: ${visible.match(/.{0,40}challenge.{0,40}/i)?.[0]}`);
});

// Swarm member feedback (2026-10-21, item 1): an agent following the one-line join must find the
// buyer key without running the page script. The build writes CHALLENGE_BUYER_PUBKEY into the
// static page and the join skill, and an override reaches both.
const SWARM_BUYER = "94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5";
const SWARM_NPUB = "npub1jjvf7aj6d8ju8ykkqv2qa4eu0lnpuyelhfm8ee2f2k85alzw2l2sf7haqz";
const swarmKeySlots = (html) => [...html.matchAll(/<code id="(buyer-hex|buyer-npub|apply-command)">([^<]*)<\/code>/g)].map((m) => [m[1], m[2]]);
test("the built /swarm page and join skill carry the swarm buyer key without JavaScript", () => {
  const page = readFileSync(join(root, "dist", "swarm.html"), "utf8");
  assert.deepEqual(swarmKeySlots(page), [
    ["buyer-hex", SWARM_BUYER],
    ["apply-command", `maxplayer-swarm join --buyer ${SWARM_NPUB} --minutes 60`],
    ["buyer-npub", SWARM_NPUB],
  ]);
  assert.ok(!/Not announced yet|announced with the first swarm|Available when the swarm is announced/.test(page), "no placeholder left");
  const skill = readFileSync(join(root, "dist", ".well-known", "skills", "swarm-join", "skill.md"), "utf8");
  assert.ok(skill.includes(`The swarm buyer's public key is \`${SWARM_BUYER}\` (npub \`${SWARM_NPUB}\`).`), "the shipped skill names the key");
  assert.ok(!/from the operator, not a member's note/.test(skill), "no 'ask the operator' step");
  // The page script keeps filling the same elements from the relay.
  const src = readFileSync(join(root, "src", "swarm.ts"), "utf8");
  for (const id of ["buyer-hex", "buyer-npub", "apply-command"]) assert.ok(src.includes(`text("${id}",`), `swarm.ts still fills #${id}`);
  // Item 3 (bob 2026-10-21): allowlisting the buyer is the only step; the open surfaces are the member's choice.
  assert.ok(!/Disable <code>open-pool<\/code>/.test(page) && !/With <code>open-pool<\/code> and <code>open-targeted<\/code> disabled/.test(page));
  assert.ok(!/keep <code>claim_open_pool<\/code> and <code>accept_open_targeted<\/code> off/.test(page), "no requirement to close them");
  assert.equal([...page.matchAll(/<code>accept_open_targeted<\/code> on or off; they only affect market work from other buyers/g)].length, 2, "manual steps and FAQ both say on or off");
  assert.ok(!/never enable `claim_open_pool`/.test(skill) && skill.includes("are the operator's choice"), "skill makes them optional");
});
test("CHALLENGE_BUYER_PUBKEY overrides the key in the shipped page, skill and discovery digest", async () => {
  const { mkdtempSync, cpSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const { schnorr } = await import("@noble/curves/secp256k1");
  const { npubEncode } = await import("../src/challenge/shared/events.mjs");
  const other = Buffer.from(schnorr.getPublicKey(schnorr.utils.randomPrivateKey())).toString("hex");
  // Built in a copy so the shared dist/ the other tests read keeps the default key.
  const copy = mkdtempSync(join(tmpdir(), "swarm-key-"));
  try {
    for (const d of ["public", "src", "scripts", ".well-known"]) cpSync(join(root, d), join(copy, d), { recursive: true });
    cpSync(join(root, "node_modules"), join(copy, "node_modules"), { recursive: true, dereference: false, verbatimSymlinks: true });
    execFileSync(process.execPath, ["scripts/build.mjs"], { cwd: copy, env: { ...process.env, CHALLENGE_BUYER_PUBKEY: other }, stdio: "pipe" });
    const page = readFileSync(join(copy, "dist", "swarm.html"), "utf8");
    assert.deepEqual(swarmKeySlots(page).map(([, v]) => v), [other, `maxplayer-swarm join --buyer ${npubEncode(other)} --minutes 60`, npubEncode(other)]);
    const skillBytes = readFileSync(join(copy, "dist", ".well-known", "skills", "swarm-join", "skill.md"));
    assert.ok(skillBytes.toString().includes(`\`${other}\` (npub \`${npubEncode(other)}\`)`));
    for (const f of ["dist/swarm.html", "dist/.well-known/skills/swarm-join/skill.md"]) assert.ok(!readFileSync(join(copy, f), "utf8").includes(SWARM_BUYER), `${f}: no default key left`);
    const index = JSON.parse(readFileSync(join(copy, "dist", ".well-known", "agent-skills", "index.json"), "utf8"));
    const row = index.skills.find((r) => r.url.endsWith("/swarm-join/skill.md"));
    assert.equal(row.digest, `sha256:${createHash("sha256").update(skillBytes).digest("hex")}`, "digest is of the shipped skill");
  } finally { rmSync(copy, { recursive: true, force: true }); }
});
test("the build refuses a page or skill whose buyer-key slot is missing", async () => {
  const { withSwarmBuyer } = await import("../scripts/swarm-buyer.mjs");
  const page = readFileSync(join(root, "public", "swarm.html"), "utf8");
  const skill = readFileSync(join(root, ".well-known", "skills", "swarm-join", "skill.md"), "utf8");
  // The sources carry the default key, so they read correctly unbuilt and build to themselves.
  assert.equal(withSwarmBuyer(page, "html", SWARM_BUYER), page);
  assert.equal(withSwarmBuyer(skill, "skill", SWARM_BUYER), skill);
  assert.throws(() => withSwarmBuyer(page.replace(`id="buyer-npub">${SWARM_NPUB}`, 'id="buyer-npub">TBD'), "html", SWARM_BUYER), /matched 0 times/);
  assert.throws(() => withSwarmBuyer(skill.replace("The swarm buyer's public key is", "Ask for the key"), "skill", SWARM_BUYER), /matched 0 times/);
  assert.throws(() => withSwarmBuyer(page, "html", "ABC"), /lowercase hex/);
});

test('challenge relay connection is allowed on the unlisted route', () => {
  const {headers}=JSON.parse(readFileSync(join(root,'vercel.json'),'utf8'));
  const csp=path=>headers.filter(h=>new RegExp(`^${h.source}$`).test(path)).flatMap(h=>h.headers.filter(x=>x.key==='Content-Security-Policy').map(x=>x.value));
  assert.equal(csp('/swarm').length,1);
  assert.match(csp('/swarm')[0],/connect-src 'self' wss:\/\/relay\.maxplayer\.ai wss:\/\/nos\.lol wss:\/\/relay\.primal\.net wss:\/\/offchain\.pub https:\/\/api\.coinbase\.com;/);
  for(const path of ['/','/market','/swarm.js'])assert.ok(!csp(path)[0].includes('community.maxplayer.ai'));
});

 test("build refuses the production challenge relay hostname", () => {
  assert.throws(() => execFileSync(process.execPath, ["scripts/build.mjs"], {cwd:root,env:{...process.env,CHALLENGE_RELAYS:"wss://RELAY.MAXPLAYER.AI./"},stdio:"pipe"}), /production relay forbidden/);
});
