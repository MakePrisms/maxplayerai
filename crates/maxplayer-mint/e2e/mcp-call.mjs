// Minimal MCP stdio client: node mcp-call.mjs <bin> <tool> '<json args>'
import { spawn } from "node:child_process";
const [bin, tool, args] = process.argv.slice(2);
const p = spawn(bin, ["mcp"], { stdio: ["pipe", "pipe", "inherit"], env: process.env });
let buf = "";
const send = (m) => p.stdin.write(JSON.stringify({ jsonrpc: "2.0", ...m }) + "\n");
p.stdout.on("data", (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i); buf = buf.slice(i + 1);
    let m; try { m = JSON.parse(line); } catch { continue; }
    if (m.id === 1) { send({ method: "notifications/initialized" }); send({ id: 2, method: "tools/call", params: { name: tool, arguments: JSON.parse(args) } }); }
    if (m.id === 2) { console.log(JSON.stringify(m.result ?? m.error, null, 1)); p.kill(); process.exit(0); }
  }
});
send({ id: 1, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "e2e", version: "0" } } });
setTimeout(() => { console.error("timeout"); p.kill(); process.exit(1); }, 120000);
