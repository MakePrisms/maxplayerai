/**
 * Renders the link-preview card and the PNG icons from the HTML templates in
 * og/ with headless Chrome, so the images are reproducible from source:
 *
 *   og/card.html → public/og/maxplayer-card.png   (1200×630, og:image / twitter:image)
 *   og/card.html?card=skill → public/og/maxplayer-skill-card.png (the /skill.md preview)
 *   og/icon.html → public/favicon-32.png, public/favicon-192.png (transparent)
 *                  public/apple-touch-icon.png     (180×180, solid background)
 *
 * Run `npm run og` after editing a template and commit the PNGs. The build
 * does not run this (Vercel has no Chrome); test/og.test.mjs checks the
 * committed files instead. Chrome path: $CHROME, else google-chrome/chromium.
 *
 * Templates are served over a throwaway localhost server rather than file://
 * because Chrome refuses @font-face loads across file:// origins.
 */
import { execFile, execFileSync } from "node:child_process";
import { promisify } from "node:util";
import { createServer } from "node:http";
import { mkdirSync, readFileSync } from "node:fs";
import { dirname, extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const TYPES = { ".html": "text/html", ".css": "text/css", ".woff2": "font/woff2" };

const server = createServer((req, res) => {
  const path = normalize(decodeURIComponent(new URL(req.url, "http://x").pathname)).replace(/^\/+/, "");
  try {
    const body = readFileSync(join(root, path));
    res.writeHead(200, { "content-type": TYPES[extname(path)] ?? "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
const base = `http://127.0.0.1:${server.address().port}`;

const chrome = process.env.CHROME ?? ["google-chrome", "chromium", "chromium-browser"].find((bin) => {
  try { execFileSync("which", [bin], { stdio: "ignore" }); return true; } catch { return false; }
});
if (!chrome) throw Error("No Chrome found: set CHROME=/path/to/chrome");

// async on purpose: a sync exec blocks the event loop, and with it the
// server Chrome is fetching the template from (deadlock).
async function shot(url, out, w, h, transparent) {
  mkdirSync(dirname(join(root, out)), { recursive: true });
  await promisify(execFile)(chrome, [
    "--headless=new", "--no-proxy-server", "--hide-scrollbars", "--disable-gpu",
    "--force-device-scale-factor=1", `--window-size=${w},${h}`,
    "--virtual-time-budget=3000", "--run-all-compositor-stages-before-draw",
    ...(transparent ? ["--default-background-color=00000000"] : []),
    `--screenshot=${join(root, out)}`, `${base}/${url}`,
  ], { timeout: 60_000 });
  console.log(`${out} ${w}×${h}`);
}

try {
  await shot("og/card.html", "public/og/maxplayer-card.png", 1200, 630, false);
  await shot("og/card.html?card=skill", "public/og/maxplayer-skill-card.png", 1200, 630, false);
  await shot("og/icon.html?size=32", "public/favicon-32.png", 32, 32, true);
  await shot("og/icon.html?size=192", "public/favicon-192.png", 192, 192, true);
  await shot("og/icon.html?size=180&bg=1", "public/apple-touch-icon.png", 180, 180, false);
} finally {
  server.close();
}
