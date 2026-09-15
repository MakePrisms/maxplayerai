#!/usr/bin/env node
/**
 * vdiff.mjs — pixel comparison between two PNGs, used two ways:
 *   1. before-vs-after: prove the redesign actually changed the rendering (--min-ratio).
 *   2. regression: prove a rerender did NOT change (--max-ratio), for catching drift.
 *
 * The failure this file exists to prevent: a "visual comparison" step that reports 0.00% changed
 * and is read as "matches", when in fact both inputs were the same blank image, or one was never
 * regenerated. So a zero ratio is a FAILURE by default in before/after mode, and both inputs are
 * checked for actual content before they are compared at all.
 *
 * Images of differing size are compared over their overlapping region, with the size mismatch
 * itself reported — never silently resized, which would invent pixels.
 *
 * Usage: node tools/vdiff.mjs --a <png> --b <png> --out <png> [--min-ratio 0.02] [--max-ratio 1]
 */
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { dirname, join } from "node:path";
import { PNG } from "pngjs";
import pixelmatch from "pixelmatch";
import { PKG } from "./tokens.mjs";
import { analyzePng, MIN_COLORS, MAX_DOMINANCE } from "./shoot.mjs";

/** Compare two PNG files. Returns { width, height, changed, total, ratio, sizeMismatch, diffFile }. */
export async function vdiff({ a, b, out, threshold = 0.1 } = {}) {
  const [bufA, bufB] = await Promise.all([readFile(a), readFile(b)]);

  // Both inputs must contain something. Comparing two blanks is not a comparison.
  for (const [label, buf] of [
    ["a", bufA],
    ["b", bufB],
  ]) {
    const an = analyzePng(buf);
    if (an.distinctColors < MIN_COLORS || an.dominance > MAX_DOMINANCE)
      throw new Error(
        `input ${label} looks blank (colors=${an.distinctColors}, dominance=${an.dominance.toFixed(4)}) — ` +
          `refusing to compare blank images`,
      );
  }

  const pngA = PNG.sync.read(bufA);
  const pngB = PNG.sync.read(bufB);
  const width = Math.min(pngA.width, pngB.width);
  const height = Math.min(pngA.height, pngB.height);
  const sizeMismatch =
    pngA.width !== pngB.width || pngA.height !== pngB.height
      ? { a: `${pngA.width}x${pngA.height}`, b: `${pngB.width}x${pngB.height}` }
      : null;

  // Crop both to the overlap rather than scaling: scaling would fabricate pixel values.
  const crop = (png) => {
    const c = new PNG({ width, height });
    for (let y = 0; y < height; y++) {
      const from = (png.width * y) << 2;
      png.data.copy(c.data, (width * y) << 2, from, from + (width << 2));
    }
    return c;
  };
  const cA = crop(pngA);
  const cB = crop(pngB);
  const diff = new PNG({ width, height });
  const changed = pixelmatch(cA.data, cB.data, diff.data, width, height, {
    threshold,
    includeAA: false,
    alpha: 0.25,
    diffColor: [211, 37, 30],
  });

  const diffFile = out || join(PKG, "evidence", "diff", "diff.png");
  await mkdir(dirname(diffFile), { recursive: true });
  await writeFile(diffFile, PNG.sync.write(diff));

  const total = width * height;
  return { width, height, changed, total, ratio: changed / total, sizeMismatch, diffFile };
}

if (process.argv[1] && process.argv[1].endsWith("vdiff.mjs")) {
  const arg = (k, d) => {
    const i = process.argv.indexOf(k);
    return i > -1 ? process.argv[i + 1] : d;
  };
  const a = arg("--a");
  const b = arg("--b");
  if (!a || !b) {
    console.error(
      "usage: node tools/vdiff.mjs --a <png> --b <png> [--out <png>] [--min-ratio R] [--max-ratio R]",
    );
    process.exit(2);
  }
  const minRatio = Number(arg("--min-ratio", "0"));
  const maxRatio = Number(arg("--max-ratio", "1"));
  try {
    const r = await vdiff({ a, b, out: arg("--out"), threshold: Number(arg("--threshold", "0.1")) });
    if (r.sizeMismatch)
      console.log(
        `  note: sizes differ (a=${r.sizeMismatch.a} b=${r.sizeMismatch.b}); compared the ` +
          `${r.width}x${r.height} overlap`,
      );
    console.log(
      `changed ${r.changed} of ${r.total} px = ${(r.ratio * 100).toFixed(2)}% -> ${r.diffFile}`,
    );
    if (r.ratio < minRatio) {
      console.error(
        `VISUAL DIFF FAILED — ${(r.ratio * 100).toFixed(2)}% changed is below --min-ratio ` +
          `${(minRatio * 100).toFixed(2)}%. Nothing meaningfully changed, so the redesign did not apply.`,
      );
      process.exit(1);
    }
    if (r.ratio > maxRatio) {
      console.error(
        `VISUAL DIFF FAILED — ${(r.ratio * 100).toFixed(2)}% changed exceeds --max-ratio ` +
          `${(maxRatio * 100).toFixed(2)}%`,
      );
      process.exit(1);
    }
  } catch (e) {
    console.error(`VISUAL DIFF FAILED — ${e.message}`);
    process.exit(1);
  }
}
