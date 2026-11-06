#!/usr/bin/env node
/**
 * shoot.mjs — desktop + mobile screenshots that PROVE they contain a rendered page.
 *
 * The failure this file exists to prevent: a screenshot step that "succeeds" while producing a
 * blank white PNG, because the URL 404'd, the CSS never loaded, or the page was captured before
 * paint. A nonzero byte count does not rule any of that out — an all-white 1280×900 PNG is a
 * perfectly valid, perfectly worthless ~6KB file.
 *
 * So every capture is decoded with pngjs and must clear ALL of:
 *   - dimensions match the requested viewport (× deviceScaleFactor)
 *   - byte length >= MIN_BYTES
 *   - >= MIN_COLORS distinct RGB colors
 *   - the most common color covers <= MAX_DOMINANCE of pixels (catches near-blank)
 *   - >= MIN_INK_RATIO of pixels are meaningfully darker than the page background (catches
 *     "styled background painted, no text rendered")
 * Any miss throws, and the CLI exits nonzero.
 *
 * Usage: node tools/shoot.mjs --url <url> --name <slug> [--out DIR] [--full]
 */
import { mkdir, writeFile, stat } from "node:fs/promises";
import { join } from "node:path";
import { chromium } from "playwright";
import { PNG } from "pngjs";
import { PKG, TOKENS } from "./tokens.mjs";

export const MIN_BYTES = 3000;
export const MIN_COLORS = 12;
export const MAX_DOMINANCE = 0.985;
export const MIN_INK_RATIO = 0.002;

/**
 * Decode a PNG buffer and describe what is actually in it.
 * Pure measurement — no verdict, so the thresholds live in one visible place below.
 */
export function analyzePng(buffer) {
  const png = PNG.sync.read(buffer);
  const counts = new Map();
  const total = png.width * png.height;
  let dark = 0;
  for (let i = 0; i < png.data.length; i += 4) {
    const r = png.data[i];
    const g = png.data[i + 1];
    const b = png.data[i + 2];
    const key = (r << 16) | (g << 8) | b;
    counts.set(key, (counts.get(key) || 0) + 1);
    // Rec.601 luma; "ink" = anything clearly darker than a light surface.
    if (0.299 * r + 0.587 * g + 0.114 * b < 170) dark += 1;
  }
  let top = 0;
  for (const n of counts.values()) if (n > top) top = n;
  return {
    width: png.width,
    height: png.height,
    pixels: total,
    distinctColors: counts.size,
    dominance: top / total,
    inkRatio: dark / total,
  };
}

/** Throw unless the image demonstrably contains a rendered page. */
export function assertRendered(buffer, label, expect) {
  const a = analyzePng(buffer);
  const fail = (why) =>
    new Error(
      `${label}: ${why} (bytes=${buffer.length} ${a.width}x${a.height} colors=${a.distinctColors} ` +
        `dominance=${a.dominance.toFixed(4)} ink=${a.inkRatio.toFixed(4)})`,
    );
  if (buffer.length < MIN_BYTES) throw fail(`screenshot is only ${buffer.length} bytes`);
  if (expect) {
    // ±2px: Chromium rounds emulated dimensions by a pixel at deviceScaleFactor 2 (observed on the
    // `before` sample, which has no viewport meta). The tolerance still catches a wrong viewport,
    // which is what this check is for; it is not a pixel-accuracy assertion.
    if (Math.abs(a.width - expect.width) > 2)
      throw fail(`width ${a.width} != expected ${expect.width}`);
    if (!expect.full && Math.abs(a.height - expect.height) > 2)
      throw fail(`height ${a.height} != expected ${expect.height}`);
  }
  if (a.distinctColors < MIN_COLORS) throw fail(`only ${a.distinctColors} distinct colors — blank?`);
  if (a.dominance > MAX_DOMINANCE)
    throw fail(`${(a.dominance * 100).toFixed(2)}% of pixels are one color — effectively blank`);
  if (a.inkRatio < MIN_INK_RATIO)
    throw fail(`only ${(a.inkRatio * 100).toFixed(3)}% dark pixels — nothing legible rendered`);
  return a;
}

/**
 * Capture one URL at both viewports. Returns [{ label, file, bytes, analysis }].
 * `page.goto` is checked for HTTP status too: a 404 body renders fine and screenshots fine.
 */
export async function shoot({ url, name, outDir, full = false, browser: given } = {}) {
  const out = outDir || join(PKG, "evidence", "screenshots");
  await mkdir(out, { recursive: true });
  const browser = given || (await chromium.launch());
  const results = [];
  try {
    for (const [label, vp] of Object.entries(TOKENS.viewport)) {
      const context = await browser.newContext({
        viewport: { width: vp.width, height: vp.height },
        deviceScaleFactor: vp.deviceScaleFactor,
        isMobile: label === "mobile",
        hasTouch: label === "mobile",
        reducedMotion: "reduce",
      });
      const page = await context.newPage();
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e)));
      const resp = await page.goto(url, { waitUntil: "load", timeout: 30_000 });
      if (!resp) throw new Error(`${name}/${label}: no response object for ${url}`);
      if (!resp.ok()) throw new Error(`${name}/${label}: HTTP ${resp.status()} for ${url}`);
      await page.waitForLoadState("networkidle", { timeout: 30_000 });
      // A stylesheet that 404s leaves the DOM intact and the design gone. Catch it here.
      const sheets = await page.evaluate(
        () => document.styleSheets.length + document.querySelectorAll("style").length,
      );
      if (sheets === 0) throw new Error(`${name}/${label}: page loaded with zero stylesheets`);
      const buffer = await page.screenshot({ fullPage: full });
      const analysis = assertRendered(buffer, `${name}/${label}`, {
        width: vp.width * vp.deviceScaleFactor,
        height: vp.height * vp.deviceScaleFactor,
        full,
      });
      const file = join(out, `${name}-${label}.png`);
      await writeFile(file, buffer);
      const onDisk = await stat(file);
      if (onDisk.size !== buffer.length)
        throw new Error(`${name}/${label}: wrote ${buffer.length} bytes, disk has ${onDisk.size}`);
      if (errors.length) console.warn(`  ! ${name}/${label} page errors: ${errors.join(" | ")}`);
      results.push({ label, file, bytes: buffer.length, analysis, viewport: vp });
      await context.close();
    }
  } finally {
    if (!given) await browser.close();
  }
  return results;
}

if (process.argv[1] && process.argv[1].endsWith("shoot.mjs")) {
  const arg = (k, d) => {
    const i = process.argv.indexOf(k);
    return i > -1 ? process.argv[i + 1] : d;
  };
  const url = arg("--url");
  const name = arg("--name", "page");
  if (!url) {
    console.error("usage: node tools/shoot.mjs --url <url> --name <slug> [--out DIR] [--full]");
    process.exit(2);
  }
  try {
    const res = await shoot({
      url,
      name,
      outDir: arg("--out"),
      full: process.argv.includes("--full"),
    });
    for (const r of res)
      console.log(
        `OK ${name}/${r.label} ${r.analysis.width}x${r.analysis.height} ${r.bytes}B ` +
          `colors=${r.analysis.distinctColors} ink=${(r.analysis.inkRatio * 100).toFixed(2)}% -> ${r.file}`,
      );
  } catch (e) {
    console.error(`SCREENSHOT FAILED — ${e.message}`);
    process.exit(1);
  }
}
