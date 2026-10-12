#!/usr/bin/env node
/**
 * drive-agent.mjs — a minimal ACP client that drives agent/designer-agent.mjs over stdio the same
 * way the seller's Rust driver does: spawn the argv, then `initialize`, `session/new`,
 * `session/prompt`, numbering request ids from 1.
 *
 * This exists so the smoke gate exercises the REAL agent path — process spawn, protocol handshake,
 * prompt turn — instead of importing the tools directly and calling that an agent. A gate that
 * never spawns the agent proves the tools work and proves nothing about the seller.
 *
 * Exits nonzero if the handshake fails, the agent errors, or the turn ends for any reason other
 * than `end_turn`.
 *
 * Usage: node tools/drive-agent.mjs --brief <file.json> [--cwd DIR] [--timeout MS]
 */
import { spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import { createInterface } from "node:readline";
import { join, resolve } from "node:path";
import { PKG } from "./tokens.mjs";

const AGENT = join(PKG, "agent", "designer-agent.mjs");

export async function drive({ brief, cwd = PKG, timeoutMs = 300_000, argv } = {}) {
  const command = argv || [process.execPath, AGENT];
  const child = spawn(command[0], command.slice(1), {
    cwd,
    stdio: ["pipe", "pipe", "pipe"],
    // Least privilege: the agent inherits no ambient secrets it does not need. PATH and HOME only,
    // plus the Playwright browser cache location so it can find the pinned Chromium.
    env: {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      ...(process.env.PLAYWRIGHT_BROWSERS_PATH
        ? { PLAYWRIGHT_BROWSERS_PATH: process.env.PLAYWRIGHT_BROWSERS_PATH }
        : {}),
    },
  });

  const pending = new Map();
  let nextId = 1;
  const stderrLines = [];

  createInterface({ input: child.stderr }).on("line", (l) => {
    stderrLines.push(l);
    process.stderr.write(`    | ${l}\n`);
  });

  createInterface({ input: child.stdout, crlfDelay: Infinity }).on("line", (line) => {
    const raw = line.trim();
    if (!raw) return;
    let msg;
    try {
      msg = JSON.parse(raw);
    } catch {
      return; // a well-behaved agent never writes non-JSON to stdout; ignore rather than crash
    }
    if (msg.id !== undefined && pending.has(msg.id)) {
      const { resolve: res, reject: rej } = pending.get(msg.id);
      pending.delete(msg.id);
      if (msg.error) rej(new Error(`agent error on id ${msg.id}: ${msg.error.message}`));
      else res(msg.result);
    }
  });

  const exited = new Promise((_, rej) => {
    child.on("exit", (code, signal) => {
      if (pending.size)
        rej(new Error(`agent exited early (code=${code} signal=${signal}) with requests in flight`));
    });
    child.on("error", rej);
  });

  const request = (method, params) => {
    const id = nextId++;
    const p = new Promise((res, rej) => {
      pending.set(id, { resolve: res, reject: rej });
      setTimeout(() => {
        if (pending.has(id)) {
          pending.delete(id);
          rej(new Error(`timeout waiting for ${method} (id ${id}) after ${timeoutMs}ms`));
        }
      }, timeoutMs).unref?.();
    });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    return Promise.race([p, exited]);
  };

  try {
    const init = await request("initialize", { protocolVersion: 2, clientCapabilities: {} });
    const protocolVersion = init?.protocolVersion ?? init?.protocol_version;
    if (typeof protocolVersion !== "number")
      throw new Error(`initialize returned no protocol version: ${JSON.stringify(init)}`);

    const session = await request("session/new", { cwd, mcpServers: [] });
    if (!session?.sessionId) throw new Error(`session/new returned no sessionId`);

    const turn = await request("session/prompt", {
      sessionId: session.sessionId,
      prompt: [{ type: "text", text: JSON.stringify(brief) }],
    });
    if (turn?.stopReason !== "end_turn")
      throw new Error(`prompt turn ended with stopReason=${turn?.stopReason}`);

    return { protocolVersion, sessionId: session.sessionId, stopReason: turn.stopReason, agentInfo: init.agentInfo };
  } finally {
    child.stdin.end();
    child.kill();
  }
}

if (process.argv[1] && process.argv[1].endsWith("drive-agent.mjs")) {
  const arg = (k, d) => {
    const i = process.argv.indexOf(k);
    return i > -1 ? process.argv[i + 1] : d;
  };
  const briefPath = arg("--brief");
  if (!briefPath) {
    console.error("usage: node tools/drive-agent.mjs --brief <file.json> [--cwd DIR] [--timeout MS]");
    process.exit(2);
  }
  try {
    const brief = JSON.parse(await readFile(resolve(briefPath), "utf8"));
    const r = await drive({
      brief,
      cwd: arg("--cwd", PKG),
      timeoutMs: Number(arg("--timeout", "300000")),
    });
    console.log(
      `AGENT PATH OK — ${r.agentInfo?.name || "agent"} protocol ${r.protocolVersion}, ` +
        `session ${r.sessionId}, stopReason ${r.stopReason}`,
    );
  } catch (e) {
    console.error(`AGENT RUN FAILED — ${e.message}`);
    process.exit(1);
  }
}
