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

// The two pages in the new look (/ and /market) and what each loads; every other page keeps styles.css and its bundle.
const NEW_PAGES = { "index.html": ["fonts.css", "site.css", "nojs.css", "teams.js"], "market.html": ["fonts.css", "site.css", "market.css", "nojs.css", "market.js"] };
// Bob (2026-11-21): the legal pages are in the new look too, with a stylesheet and a small script of their own.
const LEGAL = /^(terms|privacy|fees)-\d{8}\.html$/;
const LEGAL_ASSETS = ["fonts.css", "site.css", "legal.css", "legal.js"];
// Bob (2026-11-21): /swarm too, on /market's stylesheet plus its own.
const SWARM_ASSETS = ["fonts.css", "site.css", "market.css", "swarm.css", "swarm.js"];

test("every asset URL in shipped HTML and CSS carries the deploy stamp", () => {
  const { stamp } = JSON.parse(readFileSync(join(root, "dist", ".buildstamp"), "utf8"));
  assert.match(stamp, /^[0-9a-f]{12}$/);

  const html = readFileSync(join(root, "dist", "index.html"), "utf8");
  for (const page of ["index.html", "market.html", "mint.html", "tokens.html", "swarm.html", "terms-20261029.html", "privacy-20261029.html", "fees-20261029.html", "terms-20261123.html", "privacy-20261123.html"]) {
    const pageHtml = readFileSync(join(root, "dist", page), "utf8");
    const script = page === "tokens.html" ? "tokens.js" : page === "swarm.html" ? "swarm.js" : "terminal.js";
    for (const asset of NEW_PAGES[page] ?? (LEGAL.test(page) ? LEGAL_ASSETS : page === "swarm.html" ? SWARM_ASSETS : ["styles.css", "fonts.css", script])) {
      // Legal pages live at /terms/<date> etc. and /swarm also at /swarm/<naddr>, one level deep:
      // "./styles.css" would miss, so they use "/".
      const prefix = /^(terms|privacy|fees)-/.test(page) || page === "swarm.html" ? "/" : "./";
      assert.ok(pageHtml.includes(`"${prefix}${asset}?v=${stamp}"`), `${asset} is stamped in ${page}`);
      assert.ok(!pageHtml.includes(`"./${asset}"`) && !pageHtml.includes(`"/${asset}"`), `no unstamped ${asset} reference remains in ${page}`);
    }
    if (NEW_PAGES[page] || LEGAL.test(page) || page === "swarm.html") {
      assert.ok(!pageHtml.includes("styles.css") && !pageHtml.includes("terminal.js"), `${page} loads none of the earlier look`);
    }
  }
  // The /skill.md bot preview page loads no script, but its stylesheets ship stamped too (advisor r1, PR #1294).
  const preview = readFileSync(join(root, "dist", "skill-preview.html"), "utf8");
  for (const asset of ["styles.css", "fonts.css"]) {
    assert.ok(preview.includes(`"/${asset}?v=${stamp}"`), `${asset} is stamped in skill-preview.html`);
    assert.ok(!preview.includes(`"/${asset}"`), `no unstamped ${asset} reference remains in skill-preview.html`);
  }
  // The /join/<token> bot preview page: new-look stylesheets, no script.
  const joinPreview = readFileSync(join(root, "dist", "join-preview.html"), "utf8");
  for (const asset of ["fonts.css", "site.css"]) {
    assert.ok(joinPreview.includes(`"/${asset}?v=${stamp}"`), `${asset} is stamped in join-preview.html`);
    assert.ok(!joinPreview.includes(`"/${asset}"`), `no unstamped ${asset} reference remains in join-preview.html`);
  }
  // The /join/<token> page for browsers: the new look, its own stylesheet and script, served one level deep.
  const joinInvite = readFileSync(join(root, "dist", "join-invite.html"), "utf8");
  for (const asset of ["fonts.css", "site.css", "join.css", "join.js", "fonts/instrument-sans-normal-latin.woff2"]) {
    assert.ok(joinInvite.includes(`"/${asset}?v=${stamp}"`), `${asset} is stamped in join-invite.html`);
    assert.ok(!joinInvite.includes(`"/${asset}"`), `no unstamped ${asset} reference remains in join-invite.html`);
  }
  assert.ok(!/(href|src)="\.\//.test(joinInvite), "join-invite.html uses absolute paths (it is served at /join/<token>)");
  assert.ok(readFileSync(join(root, "dist", "join.css"), "utf8").includes(`/assets/`) === false, "join.css loads no pictures of its own");
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
  // The same for the pictures the homepage preloads: site.css must ask for the very URL that was preloaded.
  const siteCss = readFileSync(join(root, "dist", "site.css"), "utf8");
  const pictures = [...html.matchAll(/<link rel="preload" as="image" href="\.\/(assets\/[^"?]+)\?v=([0-9a-f]{12})"/g)];
  assert.ok(pictures.length >= 8, "the agent marks are preloaded and stamped");
  for (const m of pictures) {
    assert.equal(m[2], stamp, `${m[1]} preload carries the stamp`);
    assert.ok(siteCss.includes(`url("./${m[1]}?v=${stamp}")`), `${m[1]} stamped identically in site.css`);
    assert.ok(existsSync(join(root, "dist", m[1])), `${m[1]} ships`);
  }
  for (const css of ["site.css", "market.css"]) {
    assert.ok(!/url\((['"])\.\/assets\/[^'"?]+\1\)/.test(readFileSync(join(root, "dist", css), "utf8")), `no unstamped picture URL remains in ${css}`);
  }
});

test("the link-preview cards and PNG icons ship in dist/ (contents: og.test.mjs)", () => {
  for (const file of ["og/maxplayer-card.png", "og/teams-card.png", "og/invite-card-2.png", "favicon-32.png", "favicon-192.png", "apple-touch-icon.png"]) {
    assert.deepEqual(readFileSync(join(root, "dist", file)), readFileSync(join(root, "public", file)), `${file} ships unchanged`);
  }
  const html = readFileSync(join(root, "dist", "index.html"), "utf8");
  assert.ok(html.includes('<meta property="og:image" content="https://www.maxplayer.ai/og/teams-card.png">'), "built homepage keeps og:image");
  assert.ok(existsSync(join(root, "dist", "skill-preview.html")), "the /skill.md bot preview page builds");
  assert.ok(readFileSync(join(root, "dist", "skill.md"), "utf8").startsWith("---\nname: maxplayer-marketplace"), "/skill.md itself stays the markdown");
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

// bob (2026-11-20): Buy, Sell and Market are one page, /market; the homepage is Teams.
test("the live market ships at /market with the buyer's and the seller's line, and /sell leads there", () => {
  // cleanUrls serves dist/market.html at /market. The board's static chrome (the three lists market.js draws into)
  // must live there, and the homepage must carry no board — it boots no relay.
  const market = readFileSync(join(root, "dist", "market.html"), "utf8");
  for (const id of ["market", "board", "x-rows", "a-rows", "b-rows", "windows", "st-online", "st-delivered", "st-posted", "st-buyers", "how", "hire", "sell", "faq"]) {
    assert.ok(market.includes(`id="${id}"`), `market.html carries #${id}`);
  }
  assert.ok(market.includes("follow the buyer instructions"), "the market page hands out the buyer line");
  assert.ok(market.includes("follow the seller instructions"), "and the seller line");
  const home = readFileSync(join(root, "dist", "index.html"), "utf8");
  assert.ok(!home.includes('id="board"') && !home.includes('id="x-rows"'), "the homepage carries no board");
  assert.ok(home.includes('href="/market"'), "the homepage links the live market");
  assert.ok(!readFileSync(join(root, "dist", "teams.js"), "utf8").includes("relay.maxplayer.ai"), "and its bundle never opens the relay");
  // The seller page is gone; its address leads to the selling section. Temporary, so the address can be reused.
  assert.ok(!existsSync(join(root, "dist", "sell.html")), "no separate seller page");
  const { redirects } = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  for (const source of ["/sell", "/sell.html"]) {
    assert.ok(redirects.some((r) => r.source === source && r.destination === "/market#sell" && r.permanent === false), `${source} leads to the selling section`);
  }
  // Old #market deep links (skill.md, llms.txt, shared URLs) still land on the board.
  // (Before the page's script is even loaded: that script rewrites the hash as soon as it starts, and a check
  // placed after it never saw #market.)
  assert.match(readFileSync(join(root, "src/teams.ts"), "utf8"), /if \(location\.hash === "#market"\) location\.replace\("\/market"\);\nelse void stylesLoaded\(\)\.then\(\(\) => import\("\.\/site\/teams\.js"\)\);/);
  // Old /market?seller=<hex> profile links still open (the page's own are ?expert=<npub>).
  assert.match(readFileSync(join(root, "src/site/market.js"), "utf8"), /query\.get\('expert'\) \|\| query\.get\('seller'\)/);
  assert.ok(!home.includes("<script>"), "no inline scripts in the browser-key origin");
});

// The site's CSP is style-src 'self' and script-src 'self': an inline style or script is dropped silently, and the
// page looks fine everywhere except where the header is sent.
test("the new pages and their bundles carry no inline style or script", () => {
  for (const page of Object.keys(NEW_PAGES)) {
    const html = readFileSync(join(root, "dist", page), "utf8");
    assert.ok(!/\sstyle="/.test(html), `${page}: no style attribute`);
    assert.ok(!/<style\b/.test(html), `${page}: no <style> element`);
    assert.match(html, /<noscript><link rel="stylesheet" href="\.\/nojs\.css\?v=[0-9a-f]{12}"><\/noscript>/, `${page}: its no-script rules are a stylesheet, not inline`);
  }
  for (const bundle of ["teams.js", "market.js", "swarm.js", "legal.js", "tokens.js", "join.js"]) {
    const js = readFileSync(join(root, "dist", bundle), "utf8");
    assert.ok(!/style=\\?["']/.test(js), `${bundle} writes no style attribute into markup`);
    assert.ok(!js.includes("cssText") && !/setAttribute\(["']style/.test(js), `${bundle} sets no style text`);
  }
});

// Safari runs a module script before the page's stylesheets have loaded; these two scripts measure the layout as
// soon as they start (src/site/styled.ts has the measurement). Neither may be imported statically by its entry.
test("the page scripts of / and /market start only once the stylesheets are in", () => {
  for (const [entry, script] of [["src/teams.ts", "teams.js"], ["src/market-page.ts", "market.js"]]) {
    const source = readFileSync(join(root, entry), "utf8");
    assert.match(source, new RegExp(`stylesLoaded\\(\\)\\.then\\(\\(\\) => import\\("\\./site/${script.replace(".", "\\.")}"\\)\\)`), `${entry} waits for the stylesheets`);
    assert.ok(!new RegExp(`^import [^;]*"\\./site/${script.replace(".", "\\.")}";`, "m").test(source), `${entry} does not import ${script} statically`);
  }
  // One file per page still: the dynamic import is bundled in, not split out into a second request.
  for (const bundle of ["teams.js", "market.js"]) {
    assert.ok(!/import\(["'`]\.\//.test(readFileSync(join(root, "dist", bundle), "utf8")), `${bundle} loads no second file`);
  }
});

// bob (2026-11-20): placeholders until the market is whole, and nothing moves when it arrives. What takes room must
// be in the HTML before any script runs.
test("/market starts as placeholders: the period buttons and the four number bars are in the HTML", () => {
  const market = readFileSync(join(root, "dist", "market.html"), "utf8");
  assert.deepEqual([...market.matchAll(/<div class="seg win" id="(windows2?)"[^>]*>((?:<button[^>]*>[^<]*<\/button>)+)<\/div>/g)].map((m) => [m[1], [...m[2].matchAll(/data-win="([^"]+)" aria-pressed="([^"]+)"/g)].map((b) => b.slice(1, 3).join(":")).join(" ")]),
    [["windows", "24h:false week:true all:false"], ["windows2", "24h:false week:true all:false"]], "the period buttons are static, with Week pressed");
  assert.equal([...market.matchAll(/<dd id="st-[a-z]+" class="sk"><\/dd>/g)].length, 4, "four empty number bars");
  assert.ok(!/id="(x|a|b)-rows">\s*<li class="row(?! sk)/.test(market), "no hand-written rows");
});

test("all site pages share navigation and footer links, with a current page and no inline scripts", () => {
  const links = (markup) =>
    [...markup.matchAll(/<a href="([^"]+)"[^>]*>([\s\S]*?)<\/a>/g)].map((m) => [m[1], m[2].replace(/<[^>]+>/g, "")]);
  const noInlineScript = (html, page) => {
    for (const script of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g)) {
      assert.match(script[1], /\bsrc="/);
      assert.equal(script[2].trim(), "", `${page} has no inline script`);
    }
  };
  // bob (2026-11-20): the top nav is Teams and Market, on every page. Buy and Sell are sections of Market.
  // The pages that keep the earlier look.
  const pages = ["mint.html", "tokens.html"];
  let expectedNav;
  let expectedFooter;
  for (const page of pages) {
    const html = readFileSync(join(root, "dist", page), "utf8");
    const nav = html.match(/<nav class="nav-links"[^>]*>([\s\S]*?)<\/nav>/)?.[1];
    const footer = html.match(/<nav class="foot-nav"[^>]*>([\s\S]*?)<\/nav>/)?.[1];
    assert.ok(nav && footer, `${page} has shared chrome`);
    expectedNav ??= links(nav);
    expectedFooter ??= links(footer);
    assert.deepEqual(links(nav), expectedNav, `${page} nav matches`);
    assert.deepEqual(links(footer), expectedFooter, `${page} footer matches`);
    // bob (2026-11-21): Chat, Mint and Tokens are linked from no footer or nav for now; Swarm is a product page now,
    // in the top nav and the footer, no longer marked Lab. The pages themselves stay up at their addresses.
    for (const lab of ["/chat", "/mint", "/tokens"]) {
      assert.ok(!nav.includes(`href="${lab}"`) && !footer.includes(`href="${lab}"`), `${page}: ${lab} is not linked`);
    }
    assert.match(footer, /<a href="\/market">Market<\/a>\s*<a href="\/swarm">Swarm<\/a>/, `${page}: Swarm after Market, not marked Lab`);
    assert.ok(!html.includes(">Lab<"), `${page}: nothing is marked Lab`);
    // bob: Skill and GitHub leave the top bar; since 2026-10-18 they are the footer's
    // third, dimmer row, under the Lab links.
    assert.deepEqual(links(nav), [["/", "Team"], ["/market", "Market"], ["/swarm", "Swarm"]], `${page}: top nav is Team, Market, Swarm`);
    assert.ok(!html.includes('href="/sell"'), `${page}: nothing links the old seller page`);
    // Two rows on the right: the product pages, then Skill · GitHub · Terms · Privacy (the Lab row is gone, 2026-11-21).
    const rows = [...footer.matchAll(/<div class="foot-row[^"]*">([\s\S]*?)<\/div>/g)].map((m) => links(m[1]).map(([, text]) => text));
    assert.deepEqual(rows, [["Team", "Market", "Swarm"], ["Skill", "GitHub", "Terms", "Privacy"]], `${page}: footer rows`);
    // ...and the @maxplayerai handle moves to the left, under "The Multiplayer Marketplace".
    const brand = html.match(/<div class="foot-brand">([\s\S]*?)<\/div>/)?.[1] ?? "";
    assert.match(brand, /<p>The Multiplayer Marketplace<\/p>\s*<a class="foot-x" href="https:\/\/x\.com\/maxplayerai"[^>]*>@maxplayerai<\/a>/, `${page}: X handle under the tagline`);
    assert.ok(!footer.includes("x.com/maxplayerai"), `${page}: X handle is not in the link rows`);
    assert.equal([...nav.matchAll(/aria-current="page"/g)].length, 0, `${page} is not in the top nav, so nothing there is current`);
    noInlineScript(html, page);
  }
  // The two pages in the new look: the same two tabs, and a footer in four columns that leads to the same places.
  const absolute = (href, self) => (href.startsWith("#") ? self : href);
  for (const [page, route] of [["index.html", "/"], ["market.html", "/market"], ["terms-20261029.html", "/terms/20261029"], ["privacy-20261029.html", "/privacy/20261029"], ["fees-20261029.html", "/fees/20261029"], ["terms-20261123.html", "/terms/20261123"], ["privacy-20261123.html", "/privacy/20261123"], ["swarm.html", "/swarm"]]) {
    const html = readFileSync(join(root, "dist", page), "utf8");
    const nav = html.match(/<nav class="tabs"[^>]*>([\s\S]*?)<\/nav>/)?.[1];
    const footer = html.match(/<nav class="ft-cols"[^>]*>([\s\S]*?)<\/nav>/)?.[1];
    assert.ok(nav && footer, `${page} has the new chrome`);
    assert.deepEqual(links(nav).map(([href, text]) => [absolute(href, route), text]), [["/", "Team"], ["/market", "Market"], ["/swarm", "Swarm"]], `${page}: top nav is Team, Market, Swarm`);
    assert.ok(!html.includes(">Lab<"), `${page}: nothing is marked Lab`);
    if (LEGAL.test(page)) {
      assert.equal([...nav.matchAll(/aria-current="page"/g)].length, 0, `${page} is not in the top nav, so nothing there is current`);
    } else {
      assert.equal([...nav.matchAll(/aria-current="page"/g)].length, 1, `${page} marks one tab current`);
      assert.match(nav, new RegExp(`<a href="#[a-z]+"[^>]*aria-current="page">${route === "/" ? "Team" : route === "/swarm" ? "Swarm" : "Market"}</a>`), `${page} marks itself current`);
    }
    const columns = [...footer.matchAll(/<div><h4>([^<]+)<\/h4>([\s\S]*?)<\/div>/g)].map((m) => [m[1], links(m[2]).map(([href, text]) => [absolute(href, route), text])]);
    assert.deepEqual(columns, [
      ["Product", [["/", "Team"], ["/market", "Market"], ["/swarm", "Swarm"]]],
      ["Developers", [["/skill.md", "Skill"], ["https://github.com/MakePrisms/maxplayerai", "GitHub"]]],
      ["Company", [["/terms", "Terms"], ["/privacy", "Privacy"], ["https://x.com/maxplayerai", "@maxplayerai"]]],
    ], `${page}: footer columns`);
    // Every place the earlier look's footer leads to, the new one leads to as well.
    const there = new Set(columns.flatMap(([, list]) => list.map(([href]) => href)));
    for (const [href] of expectedFooter) assert.ok(there.has(href), `${page}: the footer also links ${href}`);
    assert.ok(!html.includes('href="/sell"') && !html.includes("maxplayer.ai/sell"), `${page}: nothing links the old seller page`);
    noInlineScript(html, page);
  }
});

test("/swarm hero facts start as skeletons, both labels and values, with no placeholder text (bob)", () => {
  const html = readFileSync(join(root, "dist", "swarm.html"), "utf8");
  const facts = html.match(/<dl class="c-facts" id="swarm-facts" aria-busy="true" data-view="swarm">([\s\S]*?)<\/dl>/)?.[1] ?? "";
  for (const id of ["swarm-start-label", "swarm-start", "swarm-budget-label", "swarm-budget"])
    assert.match(facts, new RegExp(`id="${id}"><span class="skel c-skel-d[dt]"></span></d[dt]>`), `${id} is a skeleton`);
  assert.equal([...facts.matchAll(/class="skel /g)].length, 4);
  for (const text of ["Loading…", "Work starts", "To be announced", "Budget"]) assert.ok(!facts.includes(text), `no static "${text}"`);
});

test("/swarm is no longer a Lab page: no Lab mark or experimental note over the hero (bob, 2026-11-21)", () => {
  const html = readFileSync(join(root, "dist", "swarm.html"), "utf8");
  assert.ok(!html.includes(">Lab<") && !html.includes("Swarms are early"));
  assert.match(html, /<h1 id="top-h">Level up with swarms<\/h1>\s*<p class="lede">Agents working together to solve the hardest problems\.<\/p>/);
});

// bob (2026-11-24): Teams replaced peer chat. /chat is gone and its old address lands on the Teams homepage.
test("/chat is gone and its address redirects to the Teams homepage", () => {
  assert.ok(!existsSync(join(root, "public", "chat.html")) && !existsSync(join(root, "dist", "chat.html")), "no chat page ships");
  const { redirects = [] } = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  for (const source of ["/chat", "/chat.html"]) {
    assert.ok(redirects.some((r) => r.source === source && r.destination === "/"), `${source} redirects to /`);
  }
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
  assert.match(tokens, /<\/header>\s*<!--[^>]*-->\s*<p class="labs-strip" role="note"><span class="labs-pill">Early preview<\/span>/);
  // Agent-facing docs stay about the jobs market.
  for (const page of ["llms.txt", "skill.md"]) {
    const text = readFileSync(join(root, "dist", page), "utf8");
    assert.ok(!/\]\(\/(tokens|trades)\)|maxplayer\.ai\/tokens/.test(text), `${page} does not link the token page`);
  }
});

test("the /tokens CSP opens our relay and the public trade relays, only on /tokens, and is noindex", () => {
  const { headers } = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  const matching = (path) => headers.filter((h) => new RegExp(`^${h.source.replace(/^\//, "\\/")}$`).test(path));
  const csp = (path) => matching(path).flatMap((h) => h.headers.filter((x) => x.key === "Content-Security-Policy").map((x) => x.value));
  for (const path of ["/", "/market", "/sell", "/terminal.js", "/tokens.js", "/tokensx", "/swarm", "/swarm.js", "/swarm/naddr1x"]) {
    const [v, ...rest] = csp(path);
    assert.equal(rest.length, 0, `${path} gets exactly one CSP`);
    assert.match(v, /connect-src 'self' wss:\/\/relay\.maxplayer\.ai https:\/\/api\.coinbase\.com;/, path);
    assert.ok(!v.includes("nos.lol"), `${path} does not open the trade relays`);
  }
  const [t, ...more] = csp("/tokens");
  assert.equal(more.length, 0, "/tokens gets exactly one CSP");
  assert.match(t, /connect-src 'self' wss:\/\/relay\.maxplayer\.ai wss:\/\/nos\.lol wss:\/\/relay\.primal\.net wss:\/\/offchain\.pub;/);
  // Every relay the page reads is allowed, and no other.
  const tokensJs = readFileSync(join(root, "dist", "tokens.js"), "utf8");
  const connect = t.match(/connect-src ([^;]+);/)[1].split(" ").filter((x) => x.startsWith("wss://"));
  for (const url of connect) assert.ok(tokensJs.includes(`"${url}"`), `tokens.js reads ${url}`);
  const label = readFileSync(join(root, "dist", "tokens.html"), "utf8").match(/<span class="col-meta">Read-only · ([^<]+)<\/span>/)[1];
  assert.deepEqual(label.split(" · "), connect.map((u) => u.replace("wss://", "")), "the board label names the relays it reads");
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
  for (const cmd of ["maxplayer-mint init", "maxplayer-mint serve", "maxplayer-mint issue 1000", "maxplayer wallet mints add nostr://"]) {
    assert.ok(mint.includes(cmd), `shows ${cmd}`);
  }
  assert.match(mint, /<\/header>\s*<!--[^>]*-->\s*<p class="labs-strip" role="note"><span class="labs-pill">Early preview<\/span>/, "early-preview strip under the nav, like /tokens");
  assert.match(mint, /Are my tokens bitcoin\?<\/summary><p>No\./, "seller tokens are distinguished from bitcoin");
  assert.doesNotMatch(mint, /stopped backups|separate trading home/i, "backups are automatic; trading uses the same home");
  const skill = readFileSync(join(root, "dist", ".well-known", "skills", "mint-and-trade", "skill.md"), "utf8");
  assert.match(skill, /maxplayer-mint\s+restore/, "the skill restores with the binary, newest backup only");
  assert.match(skill, /usage has no `restore`/, "the skill upgrades a mint binary that predates automatic backups");
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
  const servicePath = /ExecStart=%h\/(\S+) serve$/m.exec(mintReadme)?.[1];
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
  // The pages in the earlier look that have an FAQ. (/ and /market have their own, in a grid that cannot move the heading.)
  for (const page of ["mint.html"]) {
    const html = readFileSync(join(root, "dist", page), "utf8");
    assert.match(html, /class="h-section h-faq[" ]/, `${page} has an FAQ`);
    assert.match(html, /src="\.\/terminal\.js/, `${page} loads the bundle that pins it`);
  }
});

test("swarm page stays noindexed, linked from the nav and footer, and isolated from the earlier bundle", () => {
  const html = readFileSync(join(root, "dist/swarm.html"), "utf8");
  const { stamp } = JSON.parse(readFileSync(join(root, "dist/.buildstamp"), "utf8"));
  assert.match(html, /name="robots" content="noindex, nofollow"/);
  assert.ok(html.includes(`"/swarm.css?v=${stamp}"`));
  assert.ok(!html.includes("terminal.js"));
  // bob (2026-11-21): Swarm is in every top nav and footer; those two links are the only places the other pages name
  // the route.
  for (const page of ["index.html", "market.html", "mint.html", "tokens.html"]) {
    const text = readFileSync(join(root, "dist", page), "utf8");
    assert.equal([...text.matchAll(/\/swarm\b/g)].length, 2, `${page} names /swarm twice: nav and footer`);
    assert.ok(!text.includes("/challenges"), `${page} no longer links the old /challenges route`);
  }
  for (const page of ["llms.txt", "skill.md"]) {
    const text = readFileSync(join(root, "dist", page), "utf8");
    assert.ok(!text.includes("/challenges") && !/maxplayer\.ai\/swarm|\]\(\/swarm\)/.test(text), `${page} does not advertise the route`);
  }
  const cfg = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  assert.ok(!readFileSync(join(root, "dist/terminal.js"), "utf8").includes("data-challenge-copy"));
});

// bob (2026-10-19, #community-challenges-build): "change the url and mentions of challenge to 'swarm'".
test("the community page builds at /swarm, every page links it there, and /challenges redirects permanently", () => {
  assert.ok(existsSync(join(root, "dist", "swarm.html")), "/swarm builds (cleanUrls serves swarm.html)");
  assert.ok(existsSync(join(root, "dist", "swarm.js")) && existsSync(join(root, "dist", "swarm.css")), "its bundle and styles build");
  for (const old of ["challenges.html", "challenges.js", "challenges.css"]) {
    assert.ok(!existsSync(join(root, "dist", old)), `dist/${old} is gone`);
  }
  for (const page of ["index.html", "market.html", "mint.html", "tokens.html", "swarm.html"]) {
    const html = readFileSync(join(root, "dist", page), "utf8");
    // bob (2026-11-21): Swarm is in the top nav and the footer (on /swarm itself the nav tab is the current page).
    const swarmLinks = [...html.matchAll(/<a href="([^"]*)"[^>]*>Swarm<\/a>/g)].map((m) => m[1]);
    assert.deepEqual(swarmLinks, page === "swarm.html" ? ["#swarms", "/swarm"] : ["/swarm", "/swarm"], `${page}: the Swarm links point at /swarm`);
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
  // docs/specs/swarm.md §6: the sidecar is gone; every command is `maxplayer swarm …` on the home's relay.
  const commands = skill.split("\n").map((l) => l.trim()).filter((l) => /^maxplayer[ -](swarm|challenges)\b/.test(l));
  assert.ok(commands.length >= 6 && commands.every((l) => /^maxplayer swarm (list|show|join|leave|job status) /.test(l)), `every command line runs maxplayer swarm: ${commands}`);
  assert.ok(!/maxplayer-swarm (join|status|leave)|--buyer|quorum|--run\b/.test(skill), "no sidecar command, buyer flag, quorum or --run left");
  assert.ok(!skill.includes("crates/maxplayer-challenges") && !/target\/release\/maxplayer-swarm/.test(skill), "no sidecar build line");
  assert.ok(skill.includes("those memberships\ndo not carry over"), "old members are told to join again");
  // Sessions are on by default; the skill says so and names the way to turn them off.
  assert.ok(skill.includes("**Session jobs.** On by default") && skill.includes("`[sessions] enabled = false`"), "the skill says session jobs are on by default");
  // Swarm member feedback (2026-10-21, item 2): free jobs are an option, not a prerequisite.
  assert.ok(!/Prerequisite: free swarm jobs/.test(skill), "free jobs are not a prerequisite");
  assert.match(skill, /\*\*Paid or free: your choice\.\*\* Swarm tasks are paid like any other job\. A paid seller keeps its\n`rate_sats`/, "paid sellers keep their rate");
  assert.ok(skill.includes("To work for\nfree instead, set `takes_no_payment = true` and `rate_sats = 0`"), "free is opt-in, with its rate pairing");
  // Item 3 (bob 2026-10-21): allowlisting is the only step; the open surfaces are the operator's choice.
  assert.ok(!/Never enable open-pool\/open-targeted admission/.test(skill) && !/must also keep both `false`/.test(skill), "no requirement to close them");
  assert.ok(skill.includes("`claim_open_pool` and `accept_open_targeted` are the operator's choice,\n   on or off"), "on or off is the member's choice");
  assert.ok(skill.includes("with either on also takes free work from any buyer that reaches it"), "free-seller consequence stated");
  assert.ok(!/\b300\b/.test(skill), "no unexercised 300-second floor");
  assert.ok(!/`challenge\.|"challenge"|the `challenge` object/.test(skill), "no `challenge` JSON field left in the skill");
  assert.ok(!skill.includes("maxplayer-challenges"), "the old binary name is gone");
  // Bob (2026-11-21): one join line, for an agent, naming the swarm; the separate CLI line is gone.
  const bundle = readFileSync(join(root, "dist", "swarm.js"), "utf8");
  assert.ok(bundle.includes("join the Maxplayer swarm ") && !bundle.includes("maxplayer-challenges"), "the join line names the swarm");
  const page = readFileSync(join(root, "dist", "swarm.html"), "utf8");
  assert.ok(page.includes("https://www.maxplayer.ai/.well-known/skills/swarm-join/skill.md"), "the join line reads the swarm-join skill");
  assert.match(page, /<title>Maxplayer - Swarm<\/title>/);
  // Visible copy says swarm; ids and classes (challenge-*) are internal and may stay.
  const visible = page.replace(/<script[\s\S]*?<\/script>/g, "").replace(/<!--[\s\S]*?-->/g, "")
    .replace(/<[^>]*>/g, (tag) => [...tag.matchAll(/(?:content|aria-label|title|alt)="([^"]*)"/g)].map((m) => ` ${m[1]} `).join(""));
  assert.ok(!/challenge/i.test(visible), `no visible "challenge" on /swarm: ${visible.match(/.{0,40}challenge.{0,40}/i)?.[0]}`);
});

// docs/specs/swarm.md §12 PR 3: the board lists every buyer's swarms from our relay. No buyer key
// and no public relay is built into the page or its bundle; a swarm's page fills the join line,
// the join command and the buyer key from the swarm it shows.
const OLD_BUYER = "94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5";
test("the /swarm page and bundle name no buyer and no public relay", () => {
  const page = readFileSync(join(root, "dist", "swarm.html"), "utf8");
  const js = readFileSync(join(root, "dist", "swarm.js"), "utf8");
  for (const [name, text] of [["swarm.html", page], ["swarm.js", js]]) {
    assert.ok(!text.includes(OLD_BUYER) && !text.includes("npub1jjvf7aj6"), `${name}: no built-in buyer`);
    assert.ok(!/nos\.lol|relay\.primal\.net|offchain\.pub/.test(text), `${name}: no public relay`);
    assert.ok(!/maxplayer-swarm join|--buyer/.test(text), `${name}: no sidecar command`);
  }
  assert.ok(js.includes("wss://relay.maxplayer.ai"), "the bundle reads our relay");
  const src = readFileSync(join(root, "src", "swarm.ts"), "utf8");
  // Bob (2026-11-21): one join line, not two (the separate CLI command is gone).
  assert.ok(src.includes('text("cmd-join",'), "swarm.ts fills #cmd-join");
  assert.ok(!page.includes('id="apply-command"'), "no second join line");
  // Bob (2026-11-21): a member's or the buyer's profile opens in the swarm's own panel, with Back to the swarm; no
  // Live tag over the members and no minutes footnote.
  assert.ok(page.includes('id="sheet-back"') && page.includes('id="pf"') && page.includes('id="pf-npub"'), "the panel has a profile view and Back");
  assert.ok(!page.includes('id="agents-eyebrow"') && !page.includes("Minutes are today"), "no Live tag, no minutes note");
  // Item 3 (bob 2026-10-21): the open surfaces stay the member's choice.
  assert.ok(!/keep <code>claim_open_pool<\/code> and <code>accept_open_targeted<\/code> off/.test(page), "no requirement to close them");
  assert.equal([...page.matchAll(/<code>accept_open_targeted<\/code> on or off; they only affect market work from other buyers/g)].length, 1, "the FAQ says on or off");
  const skill = readFileSync(join(root, "dist", ".well-known", "skills", "swarm-join", "skill.md"), "utf8");
  assert.ok(!skill.includes(OLD_BUYER), "the shipped skill names no buyer");
});

test("/swarm/<naddr> serves the same page, which loads its assets from the site root", () => {
  const cfg = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  assert.ok(cfg.rewrites.some((r) => r.source === "/swarm/:address" && r.destination === "/swarm"), "/swarm/<naddr> rewrites to the page");
  for (const source of ["/swarm", "/swarm/:address"]) {
    assert.ok(cfg.headers.some((r) => r.source === source && r.headers.some((h) => h.key === "X-Robots-Tag" && h.value === "noindex, nofollow")), `${source} is noindex`);
  }
  const page = readFileSync(join(root, "dist", "swarm.html"), "utf8");
  assert.ok(!/(?:href|src)="\.\//.test(page), "no relative asset paths (the page is served one level deep)");
  assert.match(page, /<script type="module" src="\/swarm\.js\?v=[0-9a-f]{12}"><\/script>/, "the bundle is stamped at its root path");
});

// Email signup spec v2.10 §7.1 (Bob, Petar, Josip, 2026-10-27): the terms and privacy policy are
// static pages on this site, one fixed URL per version; /terms and /privacy point at the current one.
test("terms, privacy and fees are dated static pages, linked from every footer", () => {
  // Bob (20261029): versions are effective dates, not v1/v2; each date keeps its own URL.
  // 20261123: new Terms and Privacy Policy; the Fee Schedule is unchanged and stays 20261029.
  const cfg = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  const CURRENT = { terms: "20261123", privacy: "20261123", fees: "20261029" };
  const VERSIONS = { terms: ["20261029", "20261123"], privacy: ["20261029", "20261123"], fees: ["20261029"] };
  const label = (date) => new Date(`${date.slice(0, 4)}-${date.slice(4, 6)}-${date.slice(6)}T00:00:00Z`)
    .toLocaleDateString("en-US", { month: "long", day: "numeric", year: "numeric", timeZone: "UTC" });
  for (const doc of ["terms", "privacy", "fees"]) {
    const current = cfg.redirects.find((r) => r.source === `/${doc}`);
    assert.equal(current?.destination, `/${doc}/${CURRENT[doc]}`, `/${doc} points at the current version`);
    assert.equal(current?.permanent, false, `/${doc} is a temporary redirect, so a new version can replace it`);
    for (const date of VERSIONS[doc]) {
      // Every published version stays live at its own URL after a newer one replaces it.
      assert.ok(existsSync(join(root, "dist", `${doc}-${date}.html`)), `${doc} ${date} builds`);
      assert.ok(cfg.rewrites.some((r) => r.source === `/${doc}/${date}` && r.destination === `/${doc}-${date}`), `/${doc}/${date} serves it`);
      const html = readFileSync(join(root, "dist", `${doc}-${date}.html`), "utf8");
      assert.match(html, /<html lang="en" class="lg">/, `${doc} ${date}: in the look of / and /market`);
      assert.match(html, /<body class="legal-page">/);
      assert.match(html, /<h1 id="legal-h">/);
      assert.ok(html.includes(`Effective ${label(date)}`), `${doc} ${date} shows its effective date`);
      assert.ok(html.includes(`<link rel="canonical" href="https://www.maxplayer.ai/${doc}/${date}" />`), `${doc} ${date} canonicalises to its own URL`);
      assert.ok(!/[Nn]ot part of|Clean draft|DRAFT|\[(?:OPEN|FACT|CHECKED|DATE|NOTE|LAWYER|Source|Bob|BOB|Confirmed|SWEEP|FULL PASS)/.test(html), `${doc} ${date}: no internal notes`);
      assert.ok(!/(?:href|src)="\.\//.test(html), `${doc} ${date}: no relative asset paths (the page is served one level deep)`);
    }
  }
  // The new Terms and Privacy Policy link the current Fee Schedule and each other through the undated redirects.
  const terms = readFileSync(join(root, "dist", "terms-20261123.html"), "utf8");
  assert.ok(terms.includes('<a href="/fees">Fee Schedule</a>') && terms.includes('<a href="/privacy">Privacy Policy</a>'), "terms link /fees and /privacy");
  assert.ok(readFileSync(join(root, "dist", "privacy-20261123.html"), "utf8").includes('<a href="/terms">Terms of Service</a>'), "privacy links /terms");
  for (const page of ["index.html", "mint.html", "market.html", "tokens.html", "swarm.html"]) {
    const html = readFileSync(join(root, "dist", page), "utf8");
    assert.ok(html.includes('<a href="/terms">Terms</a>') && html.includes('<a href="/privacy">Privacy</a>'), `${page} links both`);
  }
});

// Bob (2026-10-22, #email-signup): the Try it section is removed, with its /api/try function.
test("the Try it flow and its Vercel function are gone", () => {
  assert.ok(!existsSync(join(root, "api")), "no Vercel functions remain");
  assert.ok(!existsSync(join(root, "src", "try")), "no buyer UI source remains");
  const home = readFileSync(join(root, "dist", "index.html"), "utf8");
  assert.ok(!home.includes("try-section"), "no Try it buyer flow on the homepage");
  // bob (2026-11-19): the Teams homepage has a section called "Try it" (#try). It is a scripted demonstration in a
  // phone mock-up: it posts no job and calls no function, which is what this test is about.
  for (const bundle of ["terminal.js", "teams.js", "market.js"]) {
    const js = readFileSync(join(root, "dist", bundle), "utf8");
    assert.ok(!js.includes("/api/try") && !js.includes("bootTry") && !js.includes("/api/"), `${bundle} never loads the buyer UI or calls a function`);
  }
  assert.ok(!readFileSync(join(root, "dist", "teams.js"), "utf8").includes("fetch("), "the homepage bundle makes no request of its own");
  assert.ok(!readFileSync(join(root, "dist", "styles.css"), "utf8").includes(".try-section"), "its styles are gone");
});

// The invite page's script holds a token in its address: it sends nothing anywhere (no analytics, no request that
// could use the invite); it only fills the line for the agent and copies it.
test("join.js only fills in the invite line and copies it", () => {
  const js = readFileSync(join(root, "dist", "join.js"), "utf8");
  for (const call of ["fetch(", "sendBeacon", "XMLHttpRequest", "WebSocket", "_vercel/insights", "localStorage", "indexedDB"]) {
    assert.ok(!js.includes(call), `join.js has no ${call}`);
  }
  assert.ok(js.includes("join this team for me"), "it builds the line for the agent");
});
