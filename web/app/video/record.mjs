import { spawn } from "node:child_process"; import fs from "node:fs";
const [,, url, W, H, out, fps = "30", dur = "30", port = "9333"] = process.argv;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
fs.mkdirSync(out, { recursive: true });
const chrome = spawn("google-chrome", ["--headless=new","--no-sandbox","--disable-gpu","--hide-scrollbars",`--remote-debugging-port=${port}`,`--window-size=${W},${H}`,`--user-data-dir=/tmp/rec-${port}`,"about:blank"], { stdio: "ignore" });
for (let i = 0; i < 50; i++) { try { await fetch(`http://127.0.0.1:${port}/json/version`); break; } catch { await sleep(200); } }
const tgt = await (await fetch(`http://127.0.0.1:${port}/json/new?${url}`, { method: "PUT" })).json();
const ws = new WebSocket(tgt.webSocketDebuggerUrl); await new Promise((r) => (ws.onopen = r));
let id = 0; const pend = new Map();
ws.onmessage = (m) => { const d = JSON.parse(m.data); if (d.id && pend.has(d.id)) { pend.get(d.id)(d); pend.delete(d.id); } };
const send = (method, params = {}) => new Promise((r) => { const i = ++id; pend.set(i, r); ws.send(JSON.stringify({ id: i, method, params })); });
await send("Emulation.setDeviceMetricsOverride", { width: +W, height: +H, deviceScaleFactor: 1, mobile: false });
for (let i = 0; i < 50; i++) { const r = await send("Runtime.evaluate", { expression: "document.fonts.ready.then(()=>typeof render)", awaitPromise: true, returnByValue: true }); if (r.result?.result?.value === "function") break; await sleep(200); }
await sleep(300);
const N = Math.round(+fps * +dur);
for (let f = 0; f < N; f++) {
  await send("Runtime.evaluate", { expression: `render(${(f / +fps).toFixed(4)})` });
  const r = await send("Page.captureScreenshot", { format: "jpeg", quality: 92 });
  fs.writeFileSync(`${out}/f${String(f).padStart(4, "0")}.jpg`, Buffer.from(r.result.data, "base64"));
}
chrome.kill(); console.log("frames", N); process.exit(0);
