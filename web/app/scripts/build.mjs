/**
 * Build: one minified ES module + the static shell, flat in dist/.
 *
 * The shell (index.html, styles.css, fonts, snapshot.json) is static on
 * purpose — the entire chrome renders before a byte of JS runs, which is the
 * "structure loads instantly, nothing moves" contract.
 *
 * Everything the browser fetches is cache-stamped. The host sends NO
 * Cache-Control on these files, so browsers cache them on their own heuristic
 * with nothing to tell them a file changed. Measured live on the previous
 * build: a reviewer saw fresh index.html and a stale app module — shipped
 * edits looked missing. Stamping the URLs makes each deploy a new URL, so a
 * stale copy is unreachable regardless of what any server or browser decides
 * to cache. The files themselves keep their names; the query string is the
 * only difference.
 */
import { build, context } from "esbuild";
import { createHash } from "node:crypto";
import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");
const SITE_ORIGIN = "https://www.maxplayer.ai";

const watch = process.argv.includes("--watch");

const options = {
  entryPoints: [join(root, "src/main.ts")],
  bundle: true,
  minify: true,
  format: "esm",
  target: "es2022",
  sourcemap: true,
  outfile: join(dist, "terminal.js"),
  logLevel: "info",
};

if (watch) {
  // Dev loop: unstamped copies, rebuild on save. Stamping is a deploy concern.
  mkdirSync(dist, { recursive: true });
  cpSync(join(root, "public"), dist, { recursive: true });
  const ctx = await context(options);
  await ctx.watch();
  console.log("watching src/ …");
} else {
  rmSync(dist, { recursive: true, force: true });
  mkdirSync(dist, { recursive: true });
  cpSync(join(root, "public"), dist, { recursive: true });
  await build(options);

  // One stamp derived from every shipped byte the HTML/CSS reference.
  const fontNames = readdirSync(join(root, "public/fonts"));
  const hash = createHash("sha256");
  for (const rel of ["index.html", "styles.css", "fonts.css", ...fontNames.map((f) => join("fonts", f))]) {
    hash.update(readFileSync(join(root, "public", rel)));
  }
  hash.update(readFileSync(join(dist, "terminal.js")));
  const STAMP = hash.digest("hex").slice(0, 12);

  // Stamp the specifiers, not the files: fonts.css's url()s and every asset
  // URL in index.html, including the font preloads (a preload URL must match
  // the CSS URL byte-for-byte or the browser fetches the font twice).
  writeFileSync(
    join(dist, "fonts.css"),
    readFileSync(join(root, "public/fonts.css"), "utf8")
      .replace(/url\((['"])(\.\/fonts\/[^'"?]+)\1\)/g, `url($1$2?v=${STAMP}$1)`),
  );
  writeFileSync(
    join(dist, "index.html"),
    readFileSync(join(root, "public/index.html"), "utf8")
      .replace(/(href|src)="\.\/(styles\.css|fonts\.css|terminal\.js|fonts\/[^"?]+)"/g, `$1="./$2?v=${STAMP}"`),
  );

  // The agent-facing surface: skills, discovery index, /skill.md alias.
  cpSync(join(root, ".well-known"), join(dist, ".well-known"), { recursive: true });

  // The legacy index remains the inventory for the skill URLs we already
  // publish. Discovery v0.2.0 adds integrity metadata, so derive every digest
  // from the same raw artifact bytes copied above instead of storing a second
  // value that can drift.
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

  // /skill.md is an ALIAS, derived from the canonical file rather than kept as
  // a second copy. Two files holding the same text drift, and the drift is
  // silent — agent tooling probes both paths and would get different
  // instructions.
  cpSync(join(root, ".well-known", "skills", "default", "skill.md"), join(dist, "skill.md"));

  writeFileSync(join(dist, ".buildstamp"), JSON.stringify({ flat: true, stamp: STAMP }, null, 2) + "\n");

  const kb = (statSync(join(dist, "terminal.js")).size / 1024).toFixed(1);
  console.log(`dist/terminal.js ${kb} KB · stamp ${STAMP}`);
}
