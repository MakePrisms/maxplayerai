/**
 * Build — copy the app into a flat dist/ of static files.
 *
 * There is no bundler and no transform: the browser loads the same ES modules
 * that the tests import, so what ships is what was tested.
 */
import { createHash } from "node:crypto";
import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");
const SITE_ORIGIN = "https://www.maxplayer.ai";

rmSync(dist, { recursive: true, force: true });
mkdirSync(dist, { recursive: true });

/**
 * Cache-bust every asset URL with a content stamp.
 *
 * The host sends NO Cache-Control on these files, so browsers cache them on
 * their own heuristic with nothing to tell them the file changed. Measured live:
 * a reviewer saw fresh index.html and a stale js/app.js — the HTML reloaded and
 * the module did not, so three shipped edits looked missing. Every visitor who
 * had loaded the page before was in the same position.
 *
 * Stamping the URLs makes each deploy a new URL, so a stale copy is
 * unreachable regardless of what any server or browser decides to cache. The
 * files themselves are untouched, so the browser still runs exactly the modules
 * the tests import — the query string is the only difference.
 *
 * Relative imports INSIDE js/ are stamped too. Versioning only the entry point
 * would leave app.js fresh while it pulled six cached modules.
 */
const jsNames = readdirSync(join(root, "js")).filter((f) => f.endsWith(".js"));
const stampSources = ["index.html", "styles.css", "config.js", ...jsNames.map((f) => join("js", f))];
const hash = createHash("sha256");
for (const rel of stampSources) hash.update(readFileSync(join(root, rel)));
const STAMP = hash.digest("hex").slice(0, 12);

for (const name of ["llms.txt"]) {
  cpSync(join(root, name), join(dist, name));
}
cpSync(join(root, "config.js"), join(dist, "config.js"));
cpSync(join(root, "styles.css"), join(dist, "styles.css"));

mkdirSync(join(dist, "js"), { recursive: true });
for (const f of jsNames) {
  const src = readFileSync(join(root, "js", f), "utf8")
    // `from "./x.js"` and `from "../config.js"` — stamp the specifier, not the file.
    .replace(/from "(\.\.?\/[^"?]+\.js)"/g, `from "$1?v=${STAMP}"`);
  writeFileSync(join(dist, "js", f), src);
}

writeFileSync(
  join(dist, "index.html"),
  readFileSync(join(root, "index.html"), "utf8")
    .replace('href="./styles.css"', `href="./styles.css?v=${STAMP}"`)
    .replace('src="./js/app.js"', `src="./js/app.js?v=${STAMP}"`),
);
cpSync(join(root, ".well-known"), join(dist, ".well-known"), { recursive: true });

// The legacy index remains the inventory for the skill URLs we already publish.
// Discovery v0.2.0 adds integrity metadata, so derive every digest from the same
// raw artifact bytes copied above instead of storing a second value that can drift.
const legacySkillIndex = JSON.parse(
  readFileSync(join(root, ".well-known", "skills", "index.json"), "utf8"),
);
const discoveryIndex = {
  $schema: "https://schemas.agentskills.io/discovery/0.2.0/schema.json",
  skills: legacySkillIndex.skills.map(({ name, description, path }) => {
    const artifact = readFileSync(join(root, path.slice(1)));
    return {
      name,
      type: "skill-md",
      description,
      url: new URL(path, SITE_ORIGIN).href,
      digest: `sha256:${createHash("sha256").update(artifact).digest("hex")}`,
    };
  }),
};
const discoveryDir = join(dist, ".well-known", "agent-skills");
mkdirSync(discoveryDir, { recursive: true });
writeFileSync(join(discoveryDir, "index.json"), JSON.stringify(discoveryIndex, null, 2) + "\n");

// /skill.md is an ALIAS, derived from the canonical file rather than kept as a
// second copy. Two files holding the same text drift, and the drift is silent —
// agent tooling probes both paths and would get different instructions.
const CANONICAL_SKILL = join(root, ".well-known", "skills", "default", "skill.md");
cpSync(CANONICAL_SKILL, join(dist, "skill.md"));

writeFileSync(join(dist, ".buildstamp"), JSON.stringify({ flat: true }, null, 2) + "\n");

console.log(`built flat dist/ → ${dist}`);
