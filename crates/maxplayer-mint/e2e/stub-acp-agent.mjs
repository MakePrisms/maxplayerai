#!/usr/bin/env node
// Stub ACP agent for the seller-credits e2e (stage 3). Speaks just enough line-delimited
// JSON-RPC for the seller node: initialize, session/new, session/prompt.
// - Harness probe prompt ("Create a file named `probe.txt` ... exactly this line:\n\n<sentinel>"):
//   writes <cwd>/probe.txt with the sentinel.
// - Any other prompt: answers inline ("MAXPLAYER-ANSWER-V1\n<answer>") and writes no files, so
//   the seller delivers inline (no git).
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";

const sessions = new Map();
const send = (msg) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", ...msg }) + "\n");

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let req;
  try { req = JSON.parse(line); } catch { return; }
  if (req.id === undefined || req.id === null) return; // notifications / responses
  const params = req.params || {};
  switch (req.method) {
    case "initialize":
      send({ id: req.id, result: { protocolVersion: 2, agentCapabilities: {}, agentInfo: { name: "stub-credits-agent", version: "0.1.0" } } });
      break;
    case "session/new": {
      const id = `stub-${sessions.size + 1}`;
      sessions.set(id, params.cwd || process.cwd());
      send({ id: req.id, result: { sessionId: id } });
      break;
    }
    case "session/prompt": {
      const cwd = sessions.get(params.sessionId) || process.cwd();
      const text = (params.prompt || []).map((b) => b.text || "").join("\n");
      const probe = text.match(/probe\.txt[\s\S]*?exactly this line:\s*\n\s*\n([^\n]+)\n/);
      if (probe) {
        fs.writeFileSync(path.join(cwd, "probe.txt"), probe[1].trim() + "\n");
      } else {
        send({
          method: "session/update",
          params: {
            sessionId: params.sessionId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: "MAXPLAYER-ANSWER-V1\nPaid in seller A's credits over nostr://. 42.\n" },
            },
          },
        });
      }
      send({ id: req.id, result: { stopReason: "end_turn" } });
      break;
    }
    default:
      send({ id: req.id, error: { code: -32601, message: `unsupported: ${req.method}` } });
  }
});
