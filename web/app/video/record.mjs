// Frame-step recorder: node record.mjs <page[?query]> <W> <H> <outDir> [fps=30] [dur=38.5] [port=9333] [times=comma list → stills only]
// Serves web/app on 127.0.0.1:port+1000, drives a headless Chromium over CDP, calls render(t) per frame, saves a PNG.
// <page> is a path under web/app (e.g. "video/first-job.html?rec=1&v=1") or a full http URL.
// Chromium: $CHROME_BIN, else the newest Playwright-cached chromium headless shell.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import http from "node:http";
import { fileURLToPath } from "node:url";

const [, , page, W, H, out, fps = "30", dur = "38.5", port = "9333", times = ""] = process.argv;
const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const MIME = { ".html": "text/html", ".svg": "image/svg+xml", ".woff2": "font/woff2", ".js": "text/javascript", ".css": "text/css" };
let url = page, server;
if (!/^https?:/.test(page)) {
  server = http.createServer((q, r) => {
    const f = path.join(root, decodeURIComponent(new URL(q.url, "http://x").pathname));
    if (!f.startsWith(root) || !fs.existsSync(f) || fs.statSync(f).isDirectory()) { r.writeHead(404); return r.end(); }
    r.writeHead(200, { "content-type": MIME[path.extname(f)] || "application/octet-stream" }); fs.createReadStream(f).pipe(r);
  });
  await new Promise((ok) => server.listen(+port + 1000, "127.0.0.1", ok));
  url = `http://127.0.0.1:${+port + 1000}/${page}`;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const find = (d) => { for (const e of fs.readdirSync(d, { withFileTypes: true })) { const p = path.join(d, e.name); if (e.isDirectory()) { const r = find(p); if (r) return r; } else if (/^(chrome-headless-shell|headless_shell)$/.test(e.name)) return p; } };
const cached = () => {
  const cache = [path.join(os.homedir(), "Library/Caches/ms-playwright"), path.join(os.homedir(), ".cache/ms-playwright")].find((d) => fs.existsSync(d));
  const shell = cache && fs.readdirSync(cache).filter((d) => d.startsWith("chromium_headless_shell")).sort().pop();
  return shell && find(path.join(cache, shell));
};
const bin = process.env.CHROME_BIN || cached();
if (!bin) { console.error("no Chromium: set CHROME_BIN or run `npx playwright install chromium-headless-shell`"); process.exit(1); }
fs.mkdirSync(out, { recursive: true });
const chrome = spawn(bin, ["--headless", "--disable-gpu", "--hide-scrollbars", "--force-color-profile=srgb", `--remote-debugging-port=${port}`, `--window-size=${W},${H}`, `--user-data-dir=${os.tmpdir()}/rec-${port}`, "about:blank"], { stdio: "ignore" });
try {
  for (let i = 0; i < 60; i++) { try { await fetch(`http://127.0.0.1:${port}/json/version`); break; } catch { await sleep(200); } }
  const tgt = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: "PUT" })).json();
  const ws = new WebSocket(tgt.webSocketDebuggerUrl); await new Promise((r) => (ws.onopen = r));
  let id = 0; const pend = new Map();
  ws.onmessage = (m) => { const d = JSON.parse(m.data); if (d.id && pend.has(d.id)) { pend.get(d.id)(d); pend.delete(d.id); } };
  const send = (method, params = {}) => new Promise((r) => { const i = ++id; pend.set(i, r); ws.send(JSON.stringify({ id: i, method, params })); });
  await send("Emulation.setDeviceMetricsOverride", { width: +W, height: +H, deviceScaleFactor: 1, mobile: false });
  await send("Page.navigate", { url });
  let up = false;
  for (let i = 0; i < 60 && !up; i++) { const r = await send("Runtime.evaluate", { expression: "document.fonts.ready.then(()=>typeof ready!=='undefined'&&ready)", awaitPromise: true, returnByValue: true }); up = r.result?.result?.value === true; if (!up) await sleep(200); }
  if (!up) throw new Error("scene did not load: " + url);
  await sleep(400);
  const list = times ? times.split(",").map(Number) : Array.from({ length: Math.round(+fps * +dur) }, (_, f) => f / +fps);
  let n = 0;
  for (const t of list) {
    const ev = await send("Runtime.evaluate", { expression: `render(${t.toFixed(4)})` });
    if (ev.result?.exceptionDetails) throw new Error(JSON.stringify(ev.result.exceptionDetails).slice(0, 600));
    const r = await send("Page.captureScreenshot", { format: "png" });
    const name = times ? `t${t.toFixed(2)}.png` : `f${String(n).padStart(4, "0")}.png`;
    fs.writeFileSync(path.join(out, name), Buffer.from(r.result.data, "base64")); n++;
  }
  console.log("frames", n);
} finally { chrome.kill(); server?.close(); }
process.exit(0);
