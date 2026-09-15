#!/usr/bin/env node
/**
 * svg.mjs — SVG and raster image handling via sharp (libvips).
 *
 * A designer seller has to be able to produce and check image assets, not just HTML: rasterise an
 * authored SVG at several densities, read back real dimensions, and confirm the output is not an
 * empty canvas. sharp's install script is blocked by npm's allow-scripts policy on this host, so
 * the FIRST thing this tool does is prove libvips actually loaded — an import that throws is a
 * blocker to report, not something to paper over.
 *
 * Usage:
 *   node tools/svg.mjs --check                        report sharp/libvips + format support
 *   node tools/svg.mjs --in a.svg --out a.png [--width N] [--scale 2]
 */
import { readFile, writeFile, mkdir, stat } from "node:fs/promises";
import { dirname, join } from "node:path";
import sharp from "sharp";
import { PKG } from "./tokens.mjs";
import { analyzePng, MIN_COLORS, MAX_DOMINANCE } from "./shoot.mjs";

/** Prove the native image pipeline is usable, with versions read from the loaded library. */
export function checkSharp() {
  const versions = sharp.versions;
  if (!versions || !versions.vips) throw new Error("sharp loaded but reports no libvips version");
  const fmt = sharp.format;
  for (const need of ["svg", "png", "jpeg", "webp"]) {
    if (!fmt[need]) throw new Error(`libvips has no ${need} support compiled in`);
  }
  return {
    sharp: versions.sharp,
    vips: versions.vips,
    svgInput: !!fmt.svg.input,
    pngOutput: !!fmt.png.output,
    webpOutput: !!fmt.webp.output,
    concurrency: sharp.concurrency(),
  };
}

/**
 * Rasterise an SVG (or any sharp-readable image) to PNG at a target width and density.
 * Asserts the output has the expected width, nonzero bytes, and real content.
 */
export async function rasterise({ input, output, width, scale = 2 } = {}) {
  const src = await readFile(input);
  const pipeline = sharp(src, { density: 72 * scale });
  const meta = await pipeline.metadata();
  if (!meta.format) throw new Error(`${input}: sharp could not identify the format`);

  let img = sharp(src, { density: 72 * scale });
  if (width) img = img.resize({ width, withoutEnlargement: false, fit: "contain" });
  const buf = await img.png({ compressionLevel: 9 }).toBuffer();

  const analysis = analyzePng(buf);
  if (buf.length === 0) throw new Error(`${output}: sharp produced zero bytes`);
  if (width && Math.abs(analysis.width - width) > 1)
    throw new Error(`${output}: raster width ${analysis.width} != requested ${width}`);
  if (analysis.distinctColors < MIN_COLORS || analysis.dominance > MAX_DOMINANCE)
    throw new Error(
      `${output}: raster is effectively blank (colors=${analysis.distinctColors}, ` +
        `dominance=${analysis.dominance.toFixed(4)})`,
    );

  const dest = output || join(PKG, "evidence", "images", "raster.png");
  await mkdir(dirname(dest), { recursive: true });
  await writeFile(dest, buf);
  const onDisk = await stat(dest);
  if (onDisk.size !== buf.length)
    throw new Error(`${dest}: wrote ${buf.length} bytes but disk has ${onDisk.size}`);

  return {
    input,
    output: dest,
    sourceFormat: meta.format,
    sourceSize: meta.width && meta.height ? `${meta.width}x${meta.height}` : "vector",
    bytes: buf.length,
    analysis,
  };
}

if (process.argv[1] && process.argv[1].endsWith("svg.mjs")) {
  const arg = (k, d) => {
    const i = process.argv.indexOf(k);
    return i > -1 ? process.argv[i + 1] : d;
  };
  try {
    if (process.argv.includes("--check") || process.argv.length === 2) {
      const c = checkSharp();
      console.log(
        `sharp ${c.sharp} / libvips ${c.vips} — svg input ${c.svgInput}, png output ${c.pngOutput}, ` +
          `webp output ${c.webpOutput}, concurrency ${c.concurrency}`,
      );
      if (process.argv.length === 2) process.exit(0);
    }
    const input = arg("--in");
    if (input) {
      const r = await rasterise({
        input,
        output: arg("--out"),
        width: arg("--width") ? Number(arg("--width")) : undefined,
        scale: Number(arg("--scale", "2")),
      });
      console.log(
        `OK ${r.input} (${r.sourceFormat} ${r.sourceSize}) -> ${r.output} ` +
          `${r.analysis.width}x${r.analysis.height} ${r.bytes}B colors=${r.analysis.distinctColors}`,
      );
    }
  } catch (e) {
    console.error(`IMAGE PIPELINE FAILED — ${e.message}`);
    process.exit(1);
  }
}
