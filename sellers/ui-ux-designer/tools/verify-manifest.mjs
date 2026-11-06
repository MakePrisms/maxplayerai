#!/usr/bin/env node
/**
 * verify-manifest.mjs — re-check every artifact the agent claimed to produce, then write
 * evidence/MANIFEST.md.
 *
 * The agent computes digests for the files it just wrote, which proves nothing on its own: a
 * manifest self-certified in the same breath as the work is a receipt, not evidence. This runs
 * afterwards, from the outside, and re-reads each file from disk — so a missing, truncated or
 * swapped artifact fails the gate.
 *
 * Usage: node tools/verify-manifest.mjs --result <agent-result.json> [--out evidence/MANIFEST.md]
 */
import { readFile, writeFile, stat } from "node:fs/promises";
import { createHash } from "node:crypto";
import { join, relative } from "node:path";
import { PKG } from "./tokens.mjs";

export async function verify(resultFile) {
  const result = JSON.parse(await readFile(resultFile, "utf8"));
  if (!Array.isArray(result.manifest) || result.manifest.length === 0)
    throw new Error(`${resultFile}: no manifest entries — the agent produced no evidence`);

  const rows = [];
  const problems = [];
  for (const entry of result.manifest) {
    const row = { ...entry, ok: false, note: "" };
    try {
      const info = await stat(entry.file);
      if (!info.isFile()) throw new Error("not a regular file");
      if (info.size === 0) throw new Error("zero bytes on disk");
      if (info.size !== entry.bytes)
        throw new Error(`size ${info.size} != manifest ${entry.bytes}`);
      const actual = createHash("sha256").update(await readFile(entry.file)).digest("hex");
      if (actual !== entry.sha256)
        throw new Error(`sha256 ${actual.slice(0, 12)}… != manifest ${entry.sha256.slice(0, 12)}…`);
      row.ok = true;
    } catch (e) {
      row.note = e.message;
      problems.push(`${entry.file}: ${e.message}`);
    }
    rows.push(row);
  }
  return { result, rows, problems };
}

export function renderManifest({ result, rows }) {
  const lines = [
    "# Evidence manifest — GENERATED",
    "",
    "Written by `tools/verify-manifest.mjs`, which re-reads every file from disk after the agent",
    "run finished. Sizes and digests below were recomputed here, not copied from the agent's own",
    "report.",
    "",
    `- agent: \`${result.agent}\` · identity ${result.identity}`,
    `- job: \`${result.job}\` (${result.name})`,
    `- run: ${result.startedAt} → ${result.finishedAt}`,
    `- node ${result.environment.node} · Chromium ${result.environment.chromium} · ` +
      `${result.environment.axe} · sharp ${result.environment.sharp}`,
    "",
    "## Results",
    "",
    `- baseline violations: ${JSON.stringify(result.accessibility.baselineViolations)}`,
    `- candidate violations: ${JSON.stringify(result.accessibility.candidateViolations)}`,
    `- baseline findings: ${result.accessibility.baselineFindings.join(", ")}`,
    `- visual change: ${JSON.stringify(result.visualChange)}`,
    "",
    "## Artifacts",
    "",
    "```",
    "status  bytes      sha256                                                            file",
  ];
  for (const r of rows) {
    lines.push(
      [
        (r.ok ? "OK    " : "FAIL  ").padEnd(6),
        String(r.bytes).padStart(9),
        r.sha256,
        relative(PKG, r.file) + (r.note ? `   <-- ${r.note}` : ""),
      ].join("  "),
    );
  }
  lines.push("```", "");
  const bad = rows.filter((r) => !r.ok).length;
  lines.push(
    bad === 0
      ? `All ${rows.length} artifacts verified on disk.`
      : `**${bad} of ${rows.length} artifacts FAILED verification.**`,
  );
  lines.push("");
  return lines.join("\n");
}

if (process.argv[1] && process.argv[1].endsWith("verify-manifest.mjs")) {
  const arg = (k, d) => {
    const i = process.argv.indexOf(k);
    return i > -1 ? process.argv[i + 1] : d;
  };
  const resultFile = arg("--result");
  if (!resultFile) {
    console.error("usage: node tools/verify-manifest.mjs --result <agent-result.json> [--out FILE]");
    process.exit(2);
  }
  try {
    const v = await verify(resultFile);
    const out = arg("--out", join(PKG, "evidence", "MANIFEST.md"));
    await writeFile(out, renderManifest(v));
    console.log(`verified ${v.rows.length} artifacts, ${v.problems.length} problems -> ${out}`);
    for (const p of v.problems) console.error(`  FAIL ${p}`);
    process.exit(v.problems.length === 0 ? 0 : 1);
  } catch (e) {
    console.error(`MANIFEST VERIFICATION FAILED — ${e.message}`);
    process.exit(1);
  }
}
