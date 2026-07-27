/**
 * Build — copy the app into a flat dist/ of static files.
 *
 * There is no bundler and no transform: the browser loads the same ES modules
 * that the tests import, so what ships is what was tested.
 */
import { cpSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");

rmSync(dist, { recursive: true, force: true });
mkdirSync(dist, { recursive: true });

for (const name of ["index.html", "styles.css", "config.js"]) {
  cpSync(join(root, name), join(dist, name));
}
cpSync(join(root, "js"), join(dist, "js"), { recursive: true });

writeFileSync(join(dist, ".buildstamp"), JSON.stringify({ flat: true }, null, 2) + "\n");

console.log(`built flat dist/ → ${dist}`);
