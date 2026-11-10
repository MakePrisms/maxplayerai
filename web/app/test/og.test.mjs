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
