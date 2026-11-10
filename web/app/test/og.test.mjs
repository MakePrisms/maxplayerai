/**
 * Link previews (bob, 2026-11-10): every shipped page carries canonical,
 * Open Graph and Twitter card tags with its own title and description and an
 * absolute https://www.maxplayer.ai URL, and the preview image + PNG icons
 * ship at the right sizes. Unfurlers (Discord, Slack, X, iMessage,
 * facebookexternalhit) read the static HTML only. Reads public/, which
 * scripts/build.mjs copies to dist/ unchanged apart from ?v= asset stamps:
 * build.test.mjs rebuilds dist/ at import, and node --test runs files in
 * parallel, so reading dist/ here would race it. build.test.mjs checks the
 * card and icons land in dist/.
 */
import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "public");

const ORIGIN = "https://www.maxplayer.ai";
// Route each page is served at (cleanUrls; legal pages via vercel.json rewrites).
const ROUTES = {
  "index.html": "/",
  "sell.html": "/sell",
  "market.html": "/market",
  "chat.html": "/chat",
  "mint.html": "/mint",
  "tokens.html": "/tokens",
  "swarm.html": "/swarm",
  "terms-20261029.html": "/terms/20261029",
  "privacy-20261029.html": "/privacy/20261029",
  "fees-20261029.html": "/fees/20261029",
  // The link-preview page bots get for /skill.md (vercel.json); not linked, noindex.
  "skill-preview.html": "/skill-preview",
};

const decode = (s) => s.replace(/&quot;/g, '"').replace(/&#x27;|&#39;/g, "'").replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&amp;/g, "&");
const meta = (html, attr, key) => {
  const all = [...html.matchAll(new RegExp(`<meta\\s+${attr}="${key.replace(/[.:]/g, "\\$&")}"\\s+content="([^"]*)"\\s*/?>`, "g"))];
  return all.map((m) => decode(m[1]));
};
const one = (html, attr, key, page) => {
  const v = meta(html, attr, key);
  assert.equal(v.length, 1, `${page}: exactly one ${key}`);
  return v[0];
};
const pngSize = (file) => {
  const b = readFileSync(file);
  assert.equal(b.subarray(0, 8).toString("hex"), "89504e470d0a1a0a", `${file} is a PNG`);
  return [b.readUInt32BE(16), b.readUInt32BE(20)];
};
const local = (url) => {
  assert.ok(url.startsWith(`${ORIGIN}/`), `${url} is absolute on ${ORIGIN}`);
  return join(dist, new URL(url).pathname.slice(1));
};

test("every shipped page is covered by the link-preview route map", () => {
  const shipped = readdirSync(dist).filter((f) => f.endsWith(".html")).sort();
  assert.deepEqual(shipped, Object.keys(ROUTES).sort(), "a new page needs a route here and its own preview tags");
});

test("every page has canonical, og:* and twitter:* tags with its own title and description", () => {
  const vercel = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  const redirected = new Set(vercel.redirects.map((r) => r.source));
  const titles = new Set();
  for (const [page, route] of Object.entries(ROUTES)) {
    const html = readFileSync(join(dist, page), "utf8");
    const head = html.slice(0, html.indexOf("</head>"));
    const url = ORIGIN + route;
    const title = decode(head.match(/<title>([^<]+)<\/title>/)?.[1] ?? "");
    const description = one(head, "name", "description", page);
    assert.ok(title && description, `${page}: <title> and description`);
    titles.add(title);

    const canon = [...head.matchAll(/<link rel="canonical" href="([^"]+)"/g)].map((m) => m[1]);
    assert.deepEqual(canon, [url], `${page}: canonical is ${url}`);
    assert.ok(!redirected.has(route), `${page}: canonical ${route} is not a redirect source`);

    assert.equal(one(head, "property", "og:url", page), url, `${page}: og:url`);
    assert.equal(one(head, "property", "og:type", page), "website");
    assert.equal(one(head, "property", "og:site_name", page), "Maxplayer");
    assert.equal(one(head, "property", "og:title", page), title, `${page}: og:title matches <title>`);
    assert.equal(one(head, "property", "og:description", page), description, `${page}: og:description matches description`);
    assert.equal(one(head, "name", "twitter:card", page), "summary_large_image");
    assert.equal(one(head, "name", "twitter:site", page), "@maxplayerai");
    assert.equal(one(head, "name", "twitter:title", page), title, `${page}: twitter:title`);
    assert.equal(one(head, "name", "twitter:description", page), description, `${page}: twitter:description`);

    const image = one(head, "property", "og:image", page);
    assert.equal(one(head, "name", "twitter:image", page), image, `${page}: twitter:image matches og:image`);
    assert.ok(existsSync(local(image)), `${page}: ${image} exists in public/`);
    const [w, h] = pngSize(local(image));
    assert.deepEqual([w, h], [1200, 630], `${page}: ${image} is 1200×630`);
    assert.equal(one(head, "property", "og:image:width", page), String(w), `${page}: og:image:width is the real width`);
    assert.equal(one(head, "property", "og:image:height", page), String(h), `${page}: og:image:height is the real height`);
    assert.equal(one(head, "property", "og:image:type", page), "image/png");
    assert.ok(one(head, "property", "og:image:alt", page).length > 10, `${page}: og:image:alt`);
    assert.ok(one(head, "name", "twitter:image:alt", page).length > 10, `${page}: twitter:image:alt`);

    // PNG icons, not the old inline SVG data: URI (unfurlers and iOS ignore it).
    assert.ok(!/rel="icon"[^>]*href="data:/.test(head), `${page}: no data: URI favicon`);
    for (const [rel, file, size] of [["icon", "/favicon-32.png", 32], ["icon", "/favicon-192.png", 192], ["apple-touch-icon", "/apple-touch-icon.png", 180]]) {
      assert.ok(head.includes(`<link rel="${rel}" href="${file}"`), `${page}: ${rel} ${file}`);
      assert.deepEqual(pngSize(join(dist, file.slice(1))), [size, size], `${file} is ${size}×${size}`);
    }
  }
  assert.equal(titles.size, Object.keys(ROUTES).length, "page titles are page-specific");
});

test("the preview image is 1200×630, small enough for every unfurler, and rendered from a template", () => {
  const card = join(dist, "og", "maxplayer-card.png");
  assert.deepEqual(pngSize(card), [1200, 630]);
  // X allows 5 MB, but WhatsApp drops previews over ~300 KB.
  assert.ok(statSync(card).size < 300 * 1024, `card is ${statSync(card).size} bytes`);
  const template = readFileSync(join(root, "og", "card.html"), "utf8");
  assert.match(template, /width: 1200px; height: 630px/, "the template renders at 1200×630");
  const render = readFileSync(join(root, "scripts", "render-og.mjs"), "utf8");
  assert.match(render, /"og\/card\.html", "public\/og\/maxplayer-card\.png", 1200, 630/, "npm run og writes the shipped card");
});

// bob (2026-11-10): https://www.maxplayer.ai/skill.md must unfurl too. It is markdown, which no
// unfurler reads, so vercel.json sends link-preview bots (only them) to /skill-preview.
const BOTS = [
  "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
  "Twitterbot/1.0",
  "facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)",
  "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)",
  "LinkedInBot/1.0 (compatible; Mozilla/5.0; Apache-HttpClient +http://www.linkedin.com)",
  "WhatsApp/2.23.20.0",
  "TelegramBot (like TwitterBot)",
];
// Everything an agent or a person fetches the skill with must still get the markdown.
const READERS = [
  "curl/8.5.0",
  "python-requests/2.32.3",
  "node",
  "Claude-User (claude-code/2.0; +https://www.anthropic.com/)",
  "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; ChatGPT-User/1.0; +https://openai.com/bot)",
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
  "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
  "",
];

test("/skill.md sends only link-preview bots to its preview page; agents and browsers get the markdown", () => {
  const { redirects } = JSON.parse(readFileSync(join(root, "vercel.json"), "utf8"));
  const rules = redirects.filter((r) => r.source === "/skill.md");
  assert.equal(rules.length, 1, "one /skill.md rule");
  const [rule] = rules;
  assert.equal(rule.destination, "/skill-preview");
  assert.equal(rule.permanent, false, "temporary: /skill.md itself stays the canonical markdown URL");
  assert.equal(rule.has?.length, 1);
  const [cond] = rule.has;
  assert.equal(cond.type, "header");
  assert.equal(cond.key.toLowerCase(), "user-agent");
  const re = new RegExp(cond.value.re);
  for (const ua of BOTS) assert.ok(re.test(ua), `bot gets the preview: ${ua}`);
  for (const ua of READERS) assert.ok(!re.test(ua), `reader keeps the markdown: ${ua || "(no UA)"}`);
  // No other rule may catch /skill.md for everyone.
  for (const r of [...redirects, ...(JSON.parse(readFileSync(join(root, "vercel.json"), "utf8")).rewrites ?? [])]) {
    if (r === rule) continue;
    assert.notEqual(r.source, "/skill.md", `only the bot rule touches /skill.md (${r.destination})`);
  }

  const html = readFileSync(join(dist, "skill-preview.html"), "utf8");
  assert.match(html, /<meta name="robots" content="noindex, nofollow" \/>/, "the preview page is noindex");
  assert.ok(html.includes('<a href="/skill.md">'), "a person who lands there can reach the skill");
  assert.equal(one(html, "property", "og:image", "skill-preview.html"), `${ORIGIN}/og/maxplayer-card.png`, "the same card as every page (bob)");
  // og:url must not be /skill.md: crawlers that re-fetch og:url would hit the redirect again.
  assert.equal(one(html, "property", "og:url", "skill-preview.html"), `${ORIGIN}/skill-preview`);
  for (const page of readdirSync(dist).filter((f) => f.endsWith(".html") && f !== "skill-preview.html")) {
    assert.ok(!readFileSync(join(dist, page), "utf8").includes("/skill-preview"), `${page} does not link /skill-preview`);
  }
});
