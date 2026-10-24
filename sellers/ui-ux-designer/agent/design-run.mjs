#!/usr/bin/env node
/**
 * design-run.mjs — hand a natural-language brief to a REAL model harness through the
 * ACTUAL maxplayer local driver, and prove the model did the design work.
 *
 * This is the correction to the e72e9bd verdict. Previously `agent/designer-agent.mjs`
 * only accepted a supplied candidate and rewrote it deterministically — it could not
 * create a design. Here:
 *
 *   - the brief is natural language, and NO candidate implementation is supplied;
 *   - the agent authors `status.html` itself;
 *   - the agent runs the browser/accessibility tools ITSELF and reads their output,
 *     iterating until the accessibility gate is clean at BOTH viewports;
 *   - the review pipeline is TOOLING the agent uses, never the designer.
 *
 * Nothing here writes a single line of the deliverable. If the model does not produce
 * it, this exits nonzero. Success is never reported without evidence:
 *
 *   1. the driver's event log must end `turn_ended: completed`;
 *   2. `status.html` must exist, be non-trivial, and NOT be a copy of any file we shipped;
 *   3. the event log must show the agent INVOKING shoot.mjs and a11y.mjs — screenshots
 *      that appear without a recorded tool call are not accepted as the agent's work;
 *   4. the accessibility JSON the agent produced must show a real page load and zero
 *      violations at desktop AND mobile.
 *
 * Usage:
 *   node agent/design-run.mjs --brief briefs/nova-status-brief.md \
 *     [--out runs/generative-<stamp>] [--agent-command <path>] [--idle-timeout 900]
 *
 * No relay, no wallet, no sats, no deployment: `maxplayer run` is the local driver only.
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "..");

const arg = (k, d) => {
  const i = process.argv.indexOf(k);
  return i > -1 ? process.argv[i + 1] : d;
};

const sha256 = (buf) => createHash("sha256").update(buf).digest("hex");

/** The maxplayer binary must be one WE built with the acp feature. */
export function resolveDriver() {
  const explicit = arg("--maxplayer");
  const candidates = explicit
    ? [explicit]
    : [
        path.resolve(ROOT, "../../target/release/maxplayer"),
        path.resolve(ROOT, "../../target/debug/maxplayer"),
      ];
  for (const c of candidates) {
    if (!fs.existsSync(c)) continue;
    const probe = spawnSync(c, ["run"], { encoding: "utf8" });
    const text = `${probe.stdout || ""}${probe.stderr || ""}`;
    if (/requires rebuilding with the acp feature/.test(text)) continue; // built without acp
    return c;
  }
  throw new Error(
    "no maxplayer binary with the `acp` feature found — build it with:\n" +
      "  cargo build -p maxplayer --features acp --release",
  );
}

/**
 * Probe an ACP adapter for a model that actually answers. Presence on PATH is not
 * availability: claude-agent-acp handshakes fine here but is refused at prompt time by a
 * standing org spend limit, so it must not be chosen.
 */
export function pickHarness(explicit) {
  const candidates = explicit ? [explicit] : ["/opt/homebrew/bin/codex-acp"];
  for (const c of candidates) if (fs.existsSync(c)) return c;
  throw new Error(
    `no authorized ACP harness available (looked for: ${candidates.join(", ")}).\n` +
      "This is a BLOCKED condition, not something to substitute deterministic execution for.",
  );
}

/** Copy the tools + tokens the agent needs, and link the pinned node_modules. */
function stageWorkspace(ws) {
  fs.mkdirSync(ws, { recursive: true });
  fs.mkdirSync(path.join(ws, "tools"), { recursive: true });
  for (const f of ["shoot.mjs", "a11y.mjs", "vdiff.mjs", "preview.mjs"])
    fs.copyFileSync(path.join(ROOT, "tools", f), path.join(ws, "tools", f));

  fs.mkdirSync(path.join(ws, "design-identity"), { recursive: true });
  for (const f of ["tokens.css", "tokens.json", "contrast.md"]) {
    const src = path.join(ROOT, "design-identity", f);
    if (fs.existsSync(src)) fs.copyFileSync(src, path.join(ws, "design-identity", f));
  }

  // Pinned dependencies, single copy — reuse, never re-resolve.
  const link = path.join(ws, "node_modules");
  if (!fs.existsSync(link)) fs.symlinkSync(path.join(ROOT, "node_modules"), link, "dir");
  return ws;
}

function buildTask(briefText) {
  return `You are a UI/UX designer. Design and BUILD the page described in the brief below.

There is no existing implementation and no candidate to edit. You are authoring this from
nothing. Write the source yourself.

=== BRIEF (verbatim from the buyer) ===
${briefText}
=== END BRIEF ===

Your working directory already contains everything you need:

  design-identity/tokens.css    the design tokens — link or inline these; do not invent
                                a second colour system. Read them before you design.
  design-identity/contrast.md   measured contrast ratios for the token palette
  tools/shoot.mjs               screenshots, desktop AND mobile in one run
  tools/a11y.mjs                axe-core accessibility audit
  node_modules/                 pinned Chromium/Playwright/axe — already installed

Do this, in order:

1. Read design-identity/tokens.css so you know the real token names and values.
2. Write status.html in the working directory: ONE self-contained file, styles inline,
   no framework, no build step, no external network requests.
3. Screenshot it and LOOK at what you produced:
     node tools/shoot.mjs --url file://$PWD/status.html --name status --out shots --full
   The tool prints the pixel dimensions, distinct colour count and ink ratio of each
   capture, and it fails if the image is blank. Read that output.
4. Audit accessibility at BOTH viewports, and read every violation:
     node tools/a11y.mjs --url file://$PWD/status.html --name status --viewport desktop --out a11y
     node tools/a11y.mjs --url file://$PWD/status.html --name status --viewport mobile  --out a11y
5. FIX what the tools report and repeat steps 3-4 until BOTH audits report
   violations=0. Do not stop while a violation remains. Do not silence a violation by
   deleting the content it was about.
6. Finish by writing DESIGN-NOTES.md in the working directory: what you designed and why,
   which token you used for what, how you met the colour-blindness and screen-reader
   constraints in the brief, and the final violation counts and screenshot dimensions you
   observed from the tool output.

Constraints that matter as much as the visual result: the status must be readable without
relying on colour alone, the page must make sense to a screen reader in source order, and
it must hold up at 390px wide. Contrast must meet WCAG AA.

Run the tools yourself and act on what they say. Do not claim a result you have not seen
in tool output.`;
}

/**
 * Parse what the agent actually did. The driver writes TWO streams and they carry
 * different things — an earlier version of this gate read only the first and wrongly
 * accused a passing run of never rendering anything:
 *
 *   events.jsonl        the durable job log. Envelope is {v,seq,ts,payload:{type,data}} —
 *                       the type is NESTED under `payload`. Carries driver.ready,
 *                       job.execution_changed and agent.message ONLY. Completion here is
 *                       job.execution_changed with status "completed"; there is no
 *                       `turn_ended` record in this file.
 *   driver-stdout.jsonl the forwarded ACP session/update notifications. THIS is where the
 *                       agent's tool calls and the `turn_ended` stop reason appear.
 *
 * Absence of a tool call in events.jsonl means nothing. Only stdout can answer it.
 */
function readEvents(logPath, stdoutPath) {
  const out = { turnEnded: null, jobStatus: null, toolText: [], lines: 0, stdoutLines: 0 };

  if (fs.existsSync(logPath))
    for (const line of fs.readFileSync(logPath, "utf8").split("\n")) {
      const t = line.trim();
      if (!t) continue;
      out.lines += 1;
      let ev;
      try {
        ev = JSON.parse(t);
      } catch {
        continue;
      }
      const p = ev.payload ?? ev;
      if (p.type === "job.execution_changed" && p.data?.status) out.jobStatus = p.data.status;
    }

  if (stdoutPath && fs.existsSync(stdoutPath))
    for (const line of fs.readFileSync(stdoutPath, "utf8").split("\n")) {
      const t = line.trim();
      if (!t) continue;
      out.stdoutLines += 1;
      let ev;
      try {
        ev = JSON.parse(t);
      } catch {
        continue;
      }
      if (ev.type === "turn_ended") out.turnEnded = ev.data;
      const s = JSON.stringify(ev);
      if (/turn_ended/.test(s) && out.turnEnded === null) {
        const m = s.match(/"turn_ended"\s*,\s*"data"\s*:\s*"([a-z_]+)"/);
        if (m) out.turnEnded = m[1];
      }
      if (/tool_call|shoot\.mjs|a11y\.mjs|exec_command|shell/.test(s)) out.toolText.push(s);
    }

  return out;
}

export async function main() {
  const briefRel = arg("--brief", "briefs/nova-status-brief.md");
  const briefPath = path.resolve(ROOT, briefRel);
  if (!fs.existsSync(briefPath)) throw new Error(`brief not found: ${briefPath}`);
  const briefText = fs.readFileSync(briefPath, "utf8");
  const briefDigest = sha256(fs.readFileSync(briefPath));

  // --verify-only re-reads a finished run's evidence without spending another model turn.
  // The checks below are the SAME ones the live gate applies, so this cannot be used to
  // wave a run through: it re-reads artifacts from disk rather than trusting a summary.
  const verifyOnly = process.argv.includes("--verify-only");

  const stamp = new Date().toISOString().replace(/[-:]/g, "").replace(/\..+/, "Z");
  const outDir = path.resolve(ROOT, arg("--out", `runs/generative-${stamp}`));
  if (verifyOnly && !fs.existsSync(outDir))
    throw new Error(`--verify-only needs an existing run directory: ${outDir}`);
  const ws = verifyOnly ? path.join(outDir, "workspace") : stageWorkspace(path.join(outDir, "workspace"));

  const driver = verifyOnly ? "(verify-only)" : resolveDriver();
  const harness = verifyOnly ? "(verify-only)" : pickHarness(arg("--agent-command"));
  const logPath = path.join(outDir, "events.jsonl");
  const task = buildTask(briefText);
  if (!verifyOnly) fs.writeFileSync(path.join(outDir, "task.txt"), task);

  // Guard: the deliverable must not exist before the model runs.
  const deliverable = path.join(ws, "status.html");
  if (!verifyOnly && fs.existsSync(deliverable))
    throw new Error("status.html already exists in a fresh workspace — refusing to claim authorship");

  const argv = [
    "run",
    "--agent-command",
    harness,
    "--task",
    task,
    "--cwd",
    ws,
    "--log",
    logPath,
    "--job-id",
    `design-${stamp}`,
    "--permission-policy",
    "allow",
    "--idle-timeout",
    arg("--idle-timeout", "900"),
  ];

  console.log(`driver   : ${driver}`);
  console.log(`harness  : ${harness}`);
  console.log(`workspace: ${ws}`);
  console.log(`brief    : ${briefRel} (sha256 ${briefDigest.slice(0, 16)}…)`);
  console.log("running the model — this is a real agent turn, it takes minutes…");

  let res;
  let elapsed;
  if (verifyOnly) {
    res = { status: 0, stdout: "", stderr: "" };
    elapsed = "0.0";
    console.log(`re-verifying existing run evidence in ${outDir}`);
  } else {
    const started = Date.now();
    res = spawnSync(driver, argv, {
      encoding: "utf8",
      maxBuffer: 256 * 1024 * 1024,
      stdio: ["ignore", "pipe", "pipe"],
    });
    elapsed = ((Date.now() - started) / 1000).toFixed(1);
    fs.writeFileSync(path.join(outDir, "driver-stdout.jsonl"), res.stdout || "");
    fs.writeFileSync(path.join(outDir, "driver-stderr.log"), res.stderr || "");
    console.log(`driver exit=${res.status} in ${elapsed}s`);
  }

  const failures = [];
  const ev = readEvents(logPath, path.join(outDir, "driver-stdout.jsonl"));

  if (res.status !== 0)
    failures.push(`driver exited ${res.status}: ${(res.stderr || "").trim().split("\n").pop()}`);
  // The job must have finished, by either stream's account.
  const finished = ev.jobStatus === "completed" || ev.turnEnded === "end_turn" || ev.turnEnded === "completed";
  if (!finished)
    failures.push(
      `the run did not finish (job status ${JSON.stringify(ev.jobStatus)}, ` +
        `stop reason ${JSON.stringify(ev.turnEnded)})`,
    );

  // 2. The deliverable must exist, be real, and be the model's own work.
  let html = null;
  if (!fs.existsSync(deliverable)) {
    failures.push("the agent did not create status.html — the model produced no deliverable");
  } else {
    html = fs.readFileSync(deliverable, "utf8");
    if (html.length < 800) failures.push(`status.html is only ${html.length} chars — not a real page`);
    const shippedDigests = new Set();
    const sampleDir = path.join(ROOT, "samples", "redesign");
    if (fs.existsSync(sampleDir))
      for (const d of ["before", "after"]) {
        const f = path.join(sampleDir, d, "index.html");
        if (fs.existsSync(f)) shippedDigests.add(sha256(fs.readFileSync(f)));
      }
    if (shippedDigests.has(sha256(Buffer.from(html))))
      failures.push("status.html is byte-identical to a file this repository shipped — not authored");
  }

  // 3. The AGENT must have invoked the tools itself.
  const ranShoot = ev.toolText.some((t) => t.includes("shoot.mjs"));
  const ranA11y = ev.toolText.some((t) => t.includes("a11y.mjs"));
  if (!ranShoot) failures.push("no tool call invoking shoot.mjs — the agent never rendered its design");
  if (!ranA11y) failures.push("no tool call invoking a11y.mjs — the agent never checked accessibility");

  // 4. The agent's own accessibility results, re-read from disk, both viewports.
  const a11yDir = path.join(ws, "a11y");
  const a11y = {};
  if (fs.existsSync(a11yDir))
    for (const f of fs.readdirSync(a11yDir).filter((f) => f.endsWith(".json"))) {
      let r;
      try {
        r = JSON.parse(fs.readFileSync(path.join(a11yDir, f), "utf8"));
      } catch {
        failures.push(`unparseable a11y report ${f}`);
        continue;
      }
      const vp = r.viewport || f;
      a11y[vp] = r;
      const proof = r.loadProof || {};
      if (!proof.title || (proof.elements ?? 0) < 20)
        failures.push(`${vp}: audit has no load proof — a zero-violation run on a blank page is void`);
      const v = r.counts?.violations;
      if (v !== 0) failures.push(`${vp}: ${v} accessibility violations remain`);
    }
  for (const vp of ["desktop", "mobile"])
    if (!a11y[vp]) failures.push(`no accessibility report for ${vp} viewport`);

  // Screenshots the agent produced.
  const shotsDir = path.join(ws, "shots");
  const shots = fs.existsSync(shotsDir)
    ? fs.readdirSync(shotsDir).filter((f) => f.endsWith(".png"))
    : [];
  if (shots.length < 2) failures.push(`expected desktop and mobile screenshots, found ${shots.length}`);

  const notes = path.join(ws, "DESIGN-NOTES.md");
  if (!fs.existsSync(notes)) failures.push("the agent did not write DESIGN-NOTES.md");

  const summary = {
    generatedAt: new Date().toISOString(),
    driver: { path: driver, exit: res.status, elapsedSeconds: Number(elapsed) },
    harness,
    brief: { path: briefRel, sha256: briefDigest, candidateSupplied: false },
    events: {
      lines: ev.lines,
      stdoutLines: ev.stdoutLines,
      jobStatus: ev.jobStatus,
      turnEnded: ev.turnEnded,
      toolCallsObserved: ev.toolText.length,
    },
    agentInvokedTools: { shoot: ranShoot, a11y: ranA11y },
    deliverable: html
      ? { file: "workspace/status.html", bytes: Buffer.byteLength(html), sha256: sha256(Buffer.from(html)) }
      : null,
    screenshots: shots.map((f) => {
      const p = path.join(shotsDir, f);
      return { file: `workspace/shots/${f}`, bytes: fs.statSync(p).size, sha256: sha256(fs.readFileSync(p)) };
    }),
    accessibility: Object.fromEntries(
      Object.entries(a11y).map(([vp, r]) => [
        vp,
        {
          engine: r.engine,
          violations: r.counts?.violations,
          passes: r.counts?.passes,
          rulesConsidered: r.counts?.rulesConsidered,
          loadProof: r.loadProof,
        },
      ]),
    ),
    failures,
    verdict: failures.length === 0 ? "PASS" : "FAIL",
  };
  fs.writeFileSync(path.join(outDir, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`);

  console.log(`\n--- generative design run: ${summary.verdict} ---`);
  console.log(
    `events=${ev.lines} job=${ev.jobStatus} stop=${ev.turnEnded} ` +
      `toolcalls=${ev.toolText.length} shoot=${ranShoot} a11y=${ranA11y}`,
  );
  for (const [vp, r] of Object.entries(summary.accessibility))
    console.log(`  ${vp}: ${r.violations} violations, ${r.passes} passes (${r.engine})`);
  for (const s of summary.screenshots) console.log(`  shot ${s.file} ${s.bytes}B`);
  if (failures.length) {
    for (const f of failures) console.error(`  FAIL ${f}`);
    console.error(`\nGENERATIVE RUN FAILED — ${failures.length} problem(s)`);
    process.exit(1);
  }
  console.log(`\nOK — the model authored, rendered and audited its own design. ${outDir}`);
}

if (process.argv[1] && process.argv[1].endsWith("design-run.mjs")) {
  main().catch((e) => {
    console.error(`GENERATIVE RUN FAILED — ${e.message}`);
    process.exit(1);
  });
}
