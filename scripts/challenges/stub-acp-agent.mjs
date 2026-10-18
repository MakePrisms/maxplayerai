#!/usr/bin/env node
// Stub ACP agent for the free community-challenges e2e. Speaks just enough line-delimited
// JSON-RPC for the seller node: initialize, session/new, session/prompt.
// - Harness probe prompt ("Create a file named `probe.txt` ... exactly this line:\n\n<sentinel>"):
//   writes <cwd>/probe.txt with the sentinel.
// - Any other prompt: answers inline ("MAXPLAYER-ANSWER-V1\n<answer>") and writes no files, so
//   the seller delivers inline (no git). While the E2E_STUB_FILE_MODE file exists it instead
//   writes REPORT.txt + data/notes.txt and gives no inline answer (git delivery).
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";

const sessions = new Map();
const send = (msg) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", ...msg }) + "\n");

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  let req;
  try { req = JSON.parse(line); } catch { return; }
  if (req.id === undefined || req.id === null) return; // notifications / responses
  const params = req.params || {};
  switch (req.method) {
    case "initialize":
      send({ id: req.id, result: { protocolVersion: 2, agentCapabilities: {}, agentInfo: { name: "stub-challenge-agent", version: "0.1.0" } } });
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
        const release = process.env.E2E_STUB_RELEASE;
        const until = Date.now() + 90000;
        while (release && !fs.existsSync(release)) {
          if (Date.now() >= until) throw Error('Fixture release gate timed out');
          await new Promise(resolve => setTimeout(resolve, 100));
        }
        // Ensure a positive metered duration even on a fast machine.
        const delayFile = process.env.E2E_STUB_DELAY_FILE;
        const delay = delayFile && fs.existsSync(delayFile) ? Number(fs.readFileSync(delayFile, 'utf8')) : 2000;
        if (!Number.isFinite(delay) || delay < 0 || delay > 60000) throw Error('Invalid fixture delay');
        await new Promise(resolve => setTimeout(resolve, delay));
        // File mode (live-run regression): like real harnesses, leave files and NO inline answer,
        // so the seller delivers through git and the buyer materializes no answer.txt.
        const fileMode = process.env.E2E_STUB_FILE_MODE;
        if (fileMode && fs.existsSync(fileMode)) {
          fs.writeFileSync(path.join(cwd, "REPORT.txt"), "Challenge fixture result: 42.\n");
          fs.mkdirSync(path.join(cwd, "data"), { recursive: true });
          fs.writeFileSync(path.join(cwd, "data", "notes.txt"), "worked notes\n");
          send({ method: "session/update", params: { sessionId: params.sessionId,
            update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "I wrote REPORT.txt.\n" } } } });
          send({ id: req.id, result: { stopReason: "end_turn" } });
          break;
        }
        send({
          method: "session/update",
          params: {
            sessionId: params.sessionId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: "MAXPLAYER-ANSWER-V1\nChallenge fixture result: 42.\n" },
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
