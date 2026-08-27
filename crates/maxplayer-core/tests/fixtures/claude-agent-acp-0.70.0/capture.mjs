// Prove whether the concrete model is reachable: session/new with
// _meta.claudeCode.emitRawSDKMessages, then one tiny prompt turn. Capture every
// _claude/sdkMessage and report any `.model` field seen.
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";

const BIN = process.argv[2];
const OUT = process.argv[3];
const CWD = process.cwd();

const child = spawn(BIN, [], { stdio: ["pipe", "pipe", "pipe"], cwd: CWD, env: process.env });
let buf = "";
const notes = [];
const pending = new Map();
let nextId = 1;

child.stdout.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i); buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    let msg; try { msg = JSON.parse(line); } catch { continue; }
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      const r = pending.get(msg.id); if (r) { pending.delete(msg.id); r(msg); }
    } else if (msg.method) {
      notes.push(msg);
      if (msg.id !== undefined) child.stdin.write(JSON.stringify({ jsonrpc:"2.0", id: msg.id, result: { outcome: { outcome: "cancelled" } } }) + "\n");
    }
  }
});
let stderr = ""; child.stderr.on("data", (d) => { stderr += d.toString(); });

function req(method, params) {
  const id = nextId++;
  child.stdin.write(JSON.stringify({ jsonrpc:"2.0", id, method, params }) + "\n");
  return new Promise((res, rej) => {
    pending.set(id, res);
    setTimeout(() => { if (pending.delete(id)) rej(new Error(`timeout ${method}`)); }, 180000);
  });
}

const out = {};
try {
  out.initialize = (await req("initialize", { protocolVersion: 2,
    clientCapabilities: { fs: { readTextFile: true, writeTextFile: true }, terminal: false } })).result;
  const sn = await req("session/new", { cwd: CWD, mcpServers: [],
    _meta: { claudeCode: { emitRawSDKMessages: [{ type: "system", subtype: "init" }] } } });
  out.sessionNew = sn.result;
  const sid = sn.result.sessionId;
  out.prompt = await req("session/prompt", { sessionId: sid,
    prompt: [{ type: "text", text: "Reply with exactly: ok" }] });
} catch (e) { out.error = String(e); }

// every distinct model string seen on any raw SDK message
const models = new Set();
const kinds = {};
for (const n of notes) {
  kinds[n.method] = (kinds[n.method] || 0) + 1;
  if (n.method !== "_claude/sdkMessage") continue;
  const m = n.params?.message;
  if (!m) continue;
  const seen = [m.model, m.message?.model, m.event?.message?.model];
  for (const s of seen) if (typeof s === "string") models.add(`${m.type}${m.subtype?"/"+m.subtype:""} -> ${s}`);
  if (m.modelUsage) for (const k of Object.keys(m.modelUsage)) models.add(`${m.type} modelUsage key -> ${k}`);
}
out.bin = BIN;
out.cwd = CWD;
out.notificationKinds = kinds;
out.modelStringsSeen = [...models];
out.stderr = stderr.slice(0, 4000);
out.rawSdkNotifications = notes.filter(n => n.method === "_claude/sdkMessage");
writeFileSync(OUT, JSON.stringify(out, null, 2));
console.log("kinds:", JSON.stringify(kinds));
console.log("models:", JSON.stringify([...models], null, 1));
console.log("err:", out.error ?? "none");
child.kill(); process.exit(0);
