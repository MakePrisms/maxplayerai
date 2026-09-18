#!/usr/bin/env node
/**
 * designer-agent.mjs — the UI/UX designer seller's agent, spoken to over ACP stdio.
 *
 * This is the process a maxplayer seller spawns via `[agents.ui-ux-designer] argv = [...]`. It
 * speaks line-delimited JSON-RPC 2.0 on stdin/stdout and answers exactly the three requests the
 * repo's ACP driver sends for one engine run (`crates/maxplayer-core/src/driver/acp_driver.rs`):
 *
 *   initialize      -> { protocolVersion: 2 }
 *   session/new     -> { sessionId }            (params carry the job cwd)
 *   session/prompt  -> { stopReason: "end_turn" } after the work is done
 *
 * SCOPE, stated plainly: this agent does design review and token-driven redesign verification. It
 * is NOT a general coding harness and it calls no language model. The prompt turn carries a JSON
 * job brief; anything else is answered with a refusal recorded in the report, not a guess.
 *
 * Everything it claims, it proves — it reuses the same tools the CLI does, each of which refuses to
 * report success without evidence (see tools/shoot.mjs, tools/a11y.mjs, tools/vdiff.mjs).
 *
 * Logs go to STDERR only. Anything on stdout that is not a JSON-RPC frame corrupts the protocol.
 */
import { createInterface } from "node:readline";
import { mkdir, writeFile, readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { createHash } from "node:crypto";
import { chromium } from "playwright";
import { startPreview } from "../tools/preview.mjs";
import { shoot } from "../tools/shoot.mjs";
import { audit } from "../tools/a11y.mjs";
import { vdiff } from "../tools/vdiff.mjs";
import { rasterise, checkSharp } from "../tools/svg.mjs";
import { PKG, TOKENS } from "../tools/tokens.mjs";

const PROTOCOL_VERSION = 2;
const log = (...a) => process.stderr.write(`[designer-agent] ${a.join(" ")}\n`);

async function sha256(file) {
  return createHash("sha256").update(await readFile(file)).digest("hex");
}

/**
 * The actual design work for one job.
 *
 * brief: {
 *   job: "review-redesign",
 *   baseline: "/samples/redesign/before/",   path served by the preview server
 *   candidate: "/samples/redesign/after/",
 *   name: "meridian-invoices",
 *   expectBaselineViolations: 1,             baseline must be at least this bad, or the check is not running
 *   maxCandidateViolations: 0,
 *   minChangeRatio: 0.02,
 *   outDir: "<abs>"                          defaults to <session cwd>/evidence
 * }
 */
export async function runJob(brief, cwd) {
  if (brief.job !== "review-redesign")
    throw new Error(
      `unsupported job "${brief.job}". This agent implements "review-redesign" only; it does not ` +
        `improvise other work.`,
    );
  const outDir = brief.outDir ? resolve(brief.outDir) : join(cwd || PKG, "evidence");
  const name = brief.name || "sample";
  await mkdir(outDir, { recursive: true });

  const started = new Date();
  const steps = [];
  const record = (step, detail) => {
    steps.push({ step, ...detail });
    log(`${step}: ${JSON.stringify(detail)}`);
  };

  // The image pipeline is checked before anything depends on it, so a missing native library is a
  // named blocker instead of a confusing failure three steps later.
  const sharpInfo = checkSharp();
  record("image-pipeline", sharpInfo);

  const preview = await startPreview({ port: 0 });
  const browser = await chromium.launch();
  try {
    record("preview", { origin: preview.origin, root: PKG });

    const shots = {};
    for (const [role, path] of [
      ["baseline", brief.baseline],
      ["candidate", brief.candidate],
    ]) {
      shots[role] = await shoot({
        url: preview.url(path),
        name: `${name}-${role}`,
        outDir: join(outDir, "screenshots"),
        browser,
      });
      record(`screenshot:${role}`, {
        captures: shots[role].map((s) => ({
          viewport: s.label,
          size: `${s.analysis.width}x${s.analysis.height}`,
          bytes: s.bytes,
          distinctColors: s.analysis.distinctColors,
          inkRatio: Number(s.analysis.inkRatio.toFixed(4)),
          file: s.file,
        })),
      });
    }

    const audits = {};
    for (const [role, path] of [
      ["baseline", brief.baseline],
      ["candidate", brief.candidate],
    ]) {
      audits[role] = {};
      for (const viewport of Object.keys(TOKENS.viewport)) {
        const r = await audit({
          url: preview.url(path),
          name: `${name}-${role}`,
          viewport,
          outDir: join(outDir, "a11y"),
          browser,
        });
        audits[role][viewport] = r;
        record(`axe:${role}:${viewport}`, {
          engine: r.engine,
          violations: r.counts.violations,
          nodesFlagged: r.counts.nodesFlagged,
          passes: r.counts.passes,
          rulesConsidered: r.counts.rulesConsidered,
          loadProof: `${r.loadProof.elements} elements / ${r.loadProof.textLength} chars`,
          file: r.file,
        });
      }
    }

    // Gate the audit numbers against the brief's expectations. A baseline that is suddenly clean
    // means the check stopped working far more often than it means the page got better.
    const baseWorst = Math.max(
      ...Object.values(audits.baseline).map((r) => r.counts.violations),
    );
    const candWorst = Math.max(
      ...Object.values(audits.candidate).map((r) => r.counts.violations),
    );
    const expectBase = brief.expectBaselineViolations ?? 1;
    const maxCand = brief.maxCandidateViolations ?? 0;
    if (baseWorst < expectBase)
      throw new Error(
        `baseline reported only ${baseWorst} violations, expected at least ${expectBase} — ` +
          `the accessibility check is probably not exercising the page`,
      );
    if (candWorst > maxCand)
      throw new Error(`candidate reported ${candWorst} violations, over the limit of ${maxCand}`);

    const diffs = {};
    for (const viewport of Object.keys(TOKENS.viewport)) {
      const a = shots.baseline.find((s) => s.label === viewport).file;
      const b = shots.candidate.find((s) => s.label === viewport).file;
      const d = await vdiff({
        a,
        b,
        out: join(outDir, "diff", `${name}-${viewport}.png`),
      });
      const minRatio = brief.minChangeRatio ?? 0.02;
      if (d.ratio < minRatio)
        throw new Error(
          `${viewport}: only ${(d.ratio * 100).toFixed(2)}% of pixels changed (need ` +
            `${(minRatio * 100).toFixed(2)}%) — the redesign did not take effect`,
        );
      diffs[viewport] = d;
      record(`vdiff:${viewport}`, {
        changed: d.changed,
        total: d.total,
        ratio: Number(d.ratio.toFixed(4)),
        file: d.diffFile,
      });
    }

    const mark = await rasterise({
      input: join(PKG, "design-identity", "mark.svg"),
      output: join(outDir, "images", `${name}-mark.png`),
      width: 480,
    });
    record("raster", { output: mark.output, bytes: mark.bytes, size: `${mark.analysis.width}x${mark.analysis.height}` });

    // Evidence manifest: every artifact, its size and its digest, so a reviewer can check that the
    // files described are the files present.
    const artifacts = [
      ...Object.values(shots).flat().map((s) => s.file),
      ...Object.values(audits).flatMap((byVp) => Object.values(byVp).map((r) => r.file)),
      ...Object.values(diffs).map((d) => d.diffFile),
      mark.output,
    ];
    const manifest = [];
    for (const file of artifacts) {
      const buf = await readFile(file);
      manifest.push({ file, bytes: buf.length, sha256: createHash("sha256").update(buf).digest("hex") });
    }

    const summary = {
      agent: "ui-ux-designer",
      identity: `${TOKENS.$identity} (${TOKENS.$status})`,
      job: brief.job,
      name,
      startedAt: started.toISOString(),
      finishedAt: new Date().toISOString(),
      environment: {
        node: process.version,
        chromium: browser.version(),
        axe: audits.candidate.desktop.engine,
        sharp: `${sharpInfo.sharp} / libvips ${sharpInfo.vips}`,
      },
      accessibility: {
        baselineViolations: Object.fromEntries(
          Object.entries(audits.baseline).map(([v, r]) => [v, r.counts.violations]),
        ),
        candidateViolations: Object.fromEntries(
          Object.entries(audits.candidate).map(([v, r]) => [v, r.counts.violations]),
        ),
        baselineFindings: audits.baseline.desktop.violations.map((v) => `${v.id} (${v.nodeCount})`),
      },
      visualChange: Object.fromEntries(
        Object.entries(diffs).map(([v, d]) => [v, `${(d.ratio * 100).toFixed(2)}%`]),
      ),
      steps,
      manifest,
    };

    const reportFile = join(outDir, `${name}-agent-result.json`);
    await writeFile(reportFile, JSON.stringify(summary, null, 2) + "\n");
    summary.reportFile = reportFile;
    summary.reportSha256 = await sha256(reportFile);
    log(`done — report ${reportFile}`);
    return summary;
  } finally {
    await browser.close();
    await preview.close();
  }
}

/** Extract the brief from an ACP prompt turn: the first text block that parses as JSON. */
export function briefFromPrompt(params) {
  const blocks = params?.prompt || params?.input || [];
  const texts = (Array.isArray(blocks) ? blocks : [blocks])
    .map((b) => (typeof b === "string" ? b : b?.text))
    .filter(Boolean);
  for (const t of texts) {
    const trimmed = t.trim();
    if (trimmed.startsWith("{")) {
      try {
        return JSON.parse(trimmed);
      } catch {
        /* fall through to the error below */
      }
    }
  }
  throw new Error(
    "prompt carried no JSON job brief. This agent takes a JSON brief " +
      '({"job":"review-redesign","baseline":"...","candidate":"..."}); it does not interpret free text.',
  );
}

function send(msg) {
  process.stdout.write(JSON.stringify(msg) + "\n");
}

async function main() {
  const state = { cwd: process.cwd(), lastResult: null };
  const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
  log(`ready — node ${process.version}, package ${PKG}`);
  for await (const line of rl) {
    const raw = line.trim();
    if (!raw) continue;
    let msg;
    try {
      msg = JSON.parse(raw);
    } catch {
      log(`ignoring non-JSON line (${raw.length} bytes)`);
      continue;
    }
    if (msg.id === undefined) continue; // notification: nothing to answer
    try {
      switch (msg.method) {
        case "initialize":
          send({
            jsonrpc: "2.0",
            id: msg.id,
            result: {
              protocolVersion: PROTOCOL_VERSION,
              agentInfo: { name: "maxplayer-ui-ux-designer", version: "0.1.0" },
            },
          });
          break;
        case "session/new": {
          const cwd = msg.params?.cwd || msg.params?.session_config?.cwd;
          if (cwd) state.cwd = cwd;
          send({ jsonrpc: "2.0", id: msg.id, result: { sessionId: `designer-${process.pid}` } });
          break;
        }
        case "session/prompt": {
          const brief = briefFromPrompt(msg.params);
          state.lastResult = await runJob(brief, state.cwd);
          send({ jsonrpc: "2.0", id: msg.id, result: { stopReason: "end_turn" } });
          break;
        }
        case "session/cancel":
          send({ jsonrpc: "2.0", id: msg.id, result: {} });
          break;
        default:
          send({
            jsonrpc: "2.0",
            id: msg.id,
            error: { code: -32601, message: `method not implemented: ${msg.method}` },
          });
      }
    } catch (e) {
      log(`ERROR ${e.message}`);
      send({ jsonrpc: "2.0", id: msg.id, error: { code: -32000, message: String(e.message || e) } });
    }
  }
}

if (process.argv[1] && process.argv[1].endsWith("designer-agent.mjs")) {
  await main();
}
