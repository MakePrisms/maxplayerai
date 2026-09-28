import { writeFileSync, mkdirSync } from "node:fs";
import { build } from "esbuild";
// Local visual fixtures only. This module is never a production entry point.
await build({
  stdin: {
    contents:
      'export {render} from \"./src/try/ui.ts\"; export {createRecord} from \"./src/try/store.ts\";',
    resolveDir: process.cwd(),
  },
  bundle: true,
  format: "esm",
  outfile: "dist/_preview.js",
  define: { TRY_IT_MARKET_LINK_ENABLED: "true" },
});
const tabs = await (await fetch("http://127.0.0.1:9433/json")).json();
const ws = new WebSocket(tabs[0].webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let n = 0;
const pending = new Map();
ws.onmessage = ({ data }) => {
  const m = JSON.parse(data);
  if (m.id) {
    const p = pending.get(m.id);
    pending.delete(m.id);
    m.error ? p.reject(Error(JSON.stringify(m.error))) : p.resolve(m.result);
  }
};
const call = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++n;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });
const evaluate = async (expression) => {
  const r = await call("Runtime.evaluate", {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (r.exceptionDetails) throw Error(JSON.stringify(r.exceptionDetails));
  return r.result.value;
};
await call("Page.enable");
await call("Network.enable");
await call("Network.setBlockedURLs", {
  urls: ["*relay.maxplayer.ai*", "*api.coinbase.com*"],
});
await call("Emulation.setEmulatedMedia", {
  features: [{ name: "prefers-reduced-motion", value: "reduce" }],
});
await call("Page.navigate", { url: "http://127.0.0.1:4907/" });
await new Promise((r) => setTimeout(r, 1500));
await evaluate(
  `(async()=>{globalThis.preview=await import('/_preview.js');globalThis.saved=preview.createRecord('Explain why race cars use slick tyres.',1800000000);document.querySelector('#try-prompt').value='Explain why race cars use slick tyres.';document.querySelector('#try-prompt').dispatchEvent(new Event('input'));})()`,
);
const out = process.env.TRY_IT_SCREENSHOTS ?? "/tmp/maxplayer-try-screenshots";
mkdirSync(out, { recursive: true });
const reports = [];
for (const width of [320, 390, 1440]) {
  await call("Emulation.setDeviceMetricsOverride", {
    width,
    height: width < 500 ? 844 : 1000,
    deviceScaleFactor: 1,
    mobile: false,
  });
  for (const phase of [
    "ready",
    "offline",
    "storage-unavailable",
    "publishing",
    "waiting",
    "delayed",
    "starting",
    "working",
    "accept-pending",
    "done",
    "timeout",
    "refused",
    "invalid",
    "conflict",
    "rate-limited",
    "uncertain",
  ]) {
    await evaluate(
      `(()=>{const phase=${JSON.stringify(phase)},s={...saved,phase,offerAck:!['publishing','ready','offline','storage-unavailable'].includes(phase)};if(['accept-pending','done'].includes(phase))s.binding={answer:'Slick tyres put more rubber in contact with dry tarmac. Without tread grooves, the tyre can spread load across a larger contact patch and generate more grip.\\n\\nThey only work well in dry conditions. On a wet track, treaded tyres move water out of the way and reduce aquaplaning.',resultId:'fixture',integrityHash:'fixture'};preview.render(s);if(['ready','offline','storage-unavailable'].includes(phase)){document.querySelector('#try-prompt').disabled=false;document.querySelector('#try-submit').disabled=phase!=='ready';document.querySelector('#try-check').hidden=true;document.querySelector('#try-status').textContent=phase==='ready'?'Ready for your question.':phase==='offline'?'worker-nemo may be offline.':'Enable browser storage to try it';}if(phase==='rate-limited')document.querySelector('#try-status').textContent='Temporarily rate limited. Check status in 60 seconds.';if(phase==='uncertain')document.querySelector('#try-status').textContent='Checking whether your question was sent…';document.querySelector('#try').scrollIntoView();})()`,
    );
    const metrics = await evaluate(
      `(()=>{const s=document.querySelector('#try'),r=s.getBoundingClientRect();return {width:innerWidth,scrollWidth:document.documentElement.scrollWidth,x:0,y:r.top+scrollY,height:Math.ceil(r.height),shortTaps:[...s.querySelectorAll('button,a,summary')].filter(e=>e.getClientRects().length&&e.getBoundingClientRect().height<44).map(e=>e.textContent)}})()`,
    );
    reports.push({ phase, ...metrics });
    if (width !== 320) {
      const { data } = await call("Page.captureScreenshot", {
        format: "png",
        captureBeyondViewport: true,
        clip: { x: 0, y: metrics.y, width, height: metrics.height, scale: 1 },
      });
      writeFileSync(
        `${out}/${width}-${phase}.png`,
        Buffer.from(data, "base64"),
      );
    }
  }
}
const keyboard = await evaluate(
  `(()=>{document.querySelector('#hero-cta').click();return {focused:document.activeElement.id,reducedMotion:matchMedia('(prefers-reduced-motion: reduce)').matches,scrollBehavior:getComputedStyle(document.documentElement).scrollBehavior,nav:document.querySelector('.nav-cta').getAttribute('href')}})()`,
);
writeFileSync(
  `${out}/layout.json`,
  JSON.stringify({ reports, keyboard }, null, 2),
);
console.log(
  JSON.stringify({
    screenshots: 32,
    layoutFailures: reports.filter(
      (r) => r.scrollWidth > r.width || r.shortTaps.length,
    ),
    keyboard,
  }),
);
ws.close();
if (
  reports.some((r) => r.scrollWidth > r.width || r.shortTaps.length) ||
  keyboard.focused !== "try-h" ||
  keyboard.scrollBehavior !== "auto"
)
  process.exitCode = 1;
