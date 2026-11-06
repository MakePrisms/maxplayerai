#!/usr/bin/env node
/**
 * a11y.mjs — axe-core accessibility run that cannot report a false clean bill of health.
 *
 * The failure this file exists to prevent: axe returns `violations: []` for a page that never
 * loaded. A 404 body, a blank document, a navigation that timed out before paint — all produce
 * zero violations, and a report that says "0 issues" is indistinguishable from a genuinely
 * accessible page unless you check that axe had a page to look at.
 *
 * So before any result is believed, the page must clear LOAD PROOF:
 *   - HTTP response was ok()
 *   - document.title is non-empty
 *   - > MIN_ELEMENTS elements in the DOM
 *   - > MIN_TEXT characters of rendered body text
 *   - axe's own rule accounting (passes + violations + incomplete + inapplicable) > MIN_RULES,
 *     which proves the engine actually executed its ruleset rather than bailing early
 * A zero-violation result that fails load proof is reported as FAILED, never as clean.
 *
 * Usage:
 *   node tools/a11y.mjs --url <url> --name <slug> [--max N] [--expect-min N] [--out DIR]
 *     --max N         exit nonzero if violations > N (default 0)
 *     --expect-min N  exit nonzero if violations < N — for asserting a known-bad page really is bad
 */
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { chromium } from "playwright";
import AxeBuilder from "@axe-core/playwright";
import { PKG, TOKENS } from "./tokens.mjs";

export const TAGS = ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"];
export const MIN_ELEMENTS = 20;
export const MIN_TEXT = 200;
export const MIN_RULES = 50;

/**
 * Run axe against one URL at one viewport. Returns the full axe result plus the load proof.
 * Throws if load proof fails — the caller never gets the chance to read a hollow zero.
 */
export async function audit({ url, name, viewport = "desktop", outDir, browser: given } = {}) {
  const vp = TOKENS.viewport[viewport];
  if (!vp) throw new Error(`unknown viewport ${viewport}`);
  const out = outDir || join(PKG, "evidence", "a11y");
  await mkdir(out, { recursive: true });
  const browser = given || (await chromium.launch());
  try {
    const context = await browser.newContext({
      viewport: { width: vp.width, height: vp.height },
      deviceScaleFactor: vp.deviceScaleFactor,
      isMobile: viewport === "mobile",
      reducedMotion: "reduce",
    });
    const page = await context.newPage();
    const resp = await page.goto(url, { waitUntil: "load", timeout: 30_000 });
    if (!resp) throw new Error(`${name}: no response for ${url}`);
    if (!resp.ok()) throw new Error(`${name}: HTTP ${resp.status()} for ${url}`);
    await page.waitForLoadState("networkidle", { timeout: 30_000 });

    const proof = await page.evaluate(() => ({
      title: document.title,
      elements: document.querySelectorAll("*").length,
      textLength: (document.body?.innerText || "").trim().length,
      stylesheets: document.styleSheets.length + document.querySelectorAll("style").length,
      url: location.href,
    }));

    const results = await new AxeBuilder({ page }).withTags(TAGS).analyze();

    const rulesConsidered =
      results.passes.length +
      results.violations.length +
      results.incomplete.length +
      results.inapplicable.length;

    const loadProof = { ...proof, rulesConsidered, httpStatus: resp.status() };
    const problems = [];
    if (!proof.title || !proof.title.trim()) problems.push("document.title is empty");
    if (proof.elements <= MIN_ELEMENTS)
      problems.push(`only ${proof.elements} DOM elements (need > ${MIN_ELEMENTS})`);
    if (proof.textLength <= MIN_TEXT)
      problems.push(`only ${proof.textLength} chars of rendered text (need > ${MIN_TEXT})`);
    if (proof.stylesheets === 0) problems.push("zero stylesheets");
    if (rulesConsidered <= MIN_RULES)
      problems.push(`axe considered only ${rulesConsidered} rules (need > ${MIN_RULES})`);
    if (problems.length)
      throw new Error(
        `${name}: LOAD PROOF FAILED — ${problems.join("; ")}. ` +
          `Any accessibility verdict from this run is void, including "${results.violations.length} violations".`,
      );

    const summary = {
      name,
      url,
      viewport,
      engine: `axe-core ${results.testEngine.version}`,
      tags: TAGS,
      generatedAt: new Date().toISOString(),
      loadProof,
      counts: {
        violations: results.violations.length,
        passes: results.passes.length,
        incomplete: results.incomplete.length,
        inapplicable: results.inapplicable.length,
        rulesConsidered,
        nodesFlagged: results.violations.reduce((n, v) => n + v.nodes.length, 0),
      },
      violations: results.violations.map((v) => ({
        id: v.id,
        impact: v.impact,
        help: v.help,
        helpUrl: v.helpUrl,
        tags: v.tags.filter((t) => t.startsWith("wcag")),
        nodeCount: v.nodes.length,
        sampleTargets: v.nodes.slice(0, 4).map((n) => n.target.join(" ")),
      })),
      incompleteIds: results.incomplete.map((i) => i.id),
    };

    const file = join(out, `${name}-${viewport}.json`);
    await writeFile(file, JSON.stringify(summary, null, 2) + "\n");
    await context.close();
    return { ...summary, file };
  } finally {
    if (!given) await browser.close();
  }
}

if (process.argv[1] && process.argv[1].endsWith("a11y.mjs")) {
  const arg = (k, d) => {
    const i = process.argv.indexOf(k);
    return i > -1 ? process.argv[i + 1] : d;
  };
  const url = arg("--url");
  const name = arg("--name", "page");
  const max = Number(arg("--max", "0"));
  const expectMin = Number(arg("--expect-min", "0"));
  if (!url) {
    console.error(
      "usage: node tools/a11y.mjs --url <url> --name <slug> [--max N] [--expect-min N] [--out DIR]",
    );
    process.exit(2);
  }
  try {
    const r = await audit({ url, name, viewport: arg("--viewport", "desktop"), outDir: arg("--out") });
    console.log(
      `${r.name}/${r.viewport} — ${r.engine}, load proof OK ` +
        `(title="${r.loadProof.title}", ${r.loadProof.elements} elements, ` +
        `${r.loadProof.textLength} chars text, ${r.counts.rulesConsidered} rules considered)`,
    );
    console.log(
      `  violations=${r.counts.violations} (${r.counts.nodesFlagged} nodes) ` +
        `passes=${r.counts.passes} incomplete=${r.counts.incomplete}`,
    );
    for (const v of r.violations)
      console.log(`  - [${v.impact}] ${v.id}: ${v.help} (${v.nodeCount} nodes)`);
    console.log(`  -> ${r.file}`);
    if (r.counts.violations > max) {
      console.error(`A11Y GATE FAILED — ${r.counts.violations} violations exceeds --max ${max}`);
      process.exit(1);
    }
    if (r.counts.violations < expectMin) {
      console.error(
        `A11Y GATE FAILED — expected at least ${expectMin} violations on this page but found ` +
          `${r.counts.violations}. Either the page changed or the check is not really running.`,
      );
      process.exit(1);
    }
  } catch (e) {
    console.error(`A11Y RUN FAILED — ${e.message}`);
    process.exit(1);
  }
}
