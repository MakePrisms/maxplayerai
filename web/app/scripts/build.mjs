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

const relay = process.env.RELAY_URL ?? "wss://relay.maxplayer.ai";
if (!["ws:", "wss:"].includes(new URL(relay).protocol)) throw Error("Invalid RELAY_URL");
const watch = process.argv.includes("--watch");
// Legal pages are served one level deep, so they load assets from "/" rather than "./".
// Legal pages, one file per effective date: terms-20261029.html is served at /terms/20261029 (vercel.json rewrites); /terms, /privacy and /fees redirect to the current date; older dates stay live at their URLs.
// Stylesheets of the pages in the new look (/, /market and the legal pages). The other pages use styles.css.
const SITE_CSS = ["site.css", "market.css", "nojs.css", "legal.css", "join.css"];
const PAGES = ["index.html", "market.html", "chat.html", "mint.html", "tokens.html", "swarm.html", "terms-20261029.html", "privacy-20261029.html", "fees-20261029.html", "terms-20261123.html", "privacy-20261123.html", "skill-preview.html", "join-preview.html", "join-invite.html"];

const options = {
  // teams.js serves / and market.js serves /market (the two pages in the new look).
  // join.js serves the team invite page a browser gets for /join/<token> (its copy line);
  // legal.js serves the legal pages (their menu); terminal.js serves the pages that keep
  // the earlier look (chat, mint); tokens.js serves /tokens only, so the token-trade reader never ships
  // in (or changes) another bundle.
  entryPoints: { terminal: join(root, "src/main.ts"), tokens: join(root, "src/tokens.ts"), swarm: join(root, "src/swarm.ts"), teams: join(root, "src/teams.ts"), market: join(root, "src/market-page.ts"), legal: join(root, "src/legal.ts"), join: join(root, "src/join.ts") },
  bundle: true,
  define: {
    CONFIGURED_RELAY_URL: JSON.stringify(relay),
    WEB_ANALYTICS_ENABLED: String(process.env.VERCEL_ENV === "production"),
  },
  minify: true,
  format: "esm",
  target: "es2022",
  sourcemap: true,
  outdir: dist,
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
  const fontNames = readdirSync(join(root, "public/fonts")).sort();
  // The pictures of the new pages (agent marks, the sheet of soft marks), referenced from site.css and preloaded
  // by index.html.
  const assetNames = readdirSync(join(root, "public/assets"), { recursive: true })
    .filter((f) => statSync(join(root, "public/assets", f)).isFile()).sort();
  const hash = createHash("sha256");
  for (const rel of [...PAGES, "styles.css", "fonts.css", "swarm.css", ...SITE_CSS, ...fontNames.map((f) => join("fonts", f)), ...assetNames.map((f) => join("assets", f))]) {
    hash.update(readFileSync(join(root, "public", rel)));
  }
  for (const bundle of ["terminal.js", "tokens.js", "swarm.js", "teams.js", "market.js", "legal.js", "join.js"]) hash.update(readFileSync(join(dist, bundle)));
  const STAMP = hash.digest("hex").slice(0, 12);

  // Stamp the specifiers, not the files: fonts.css's url()s and every asset
  // URL in index.html, including the font preloads (a preload URL must match
  // the CSS URL byte-for-byte or the browser fetches the font twice).
  writeFileSync(
    join(dist, "fonts.css"),
    readFileSync(join(root, "public/fonts.css"), "utf8")
      .replace(/url\((['"])(\.\/fonts\/[^'"?]+)\1\)/g, `url($1$2?v=${STAMP}$1)`),
  );
  // The new pages' stylesheets point at pictures in assets/: stamp those url()s the same way, so an index.html
  // preload of a picture matches the stylesheet's URL byte-for-byte.
  for (const css of SITE_CSS) {
    writeFileSync(
      join(dist, css),
      readFileSync(join(root, "public", css), "utf8")
        .replace(/url\((['"])(\.\/assets\/[^'"?]+)\1\)/g, `url($1$2?v=${STAMP}$1)`),
    );
  }
  // Every page: the Teams homepage (/), the
  // market (/market), the chat page (/chat), the mint page (/mint), the token-trade page (/tokens) and the swarms (/swarm and /swarm/<naddr>) — cleanUrls serves them.
  for (const page of PAGES) {
    writeFileSync(
      join(dist, page),
      readFileSync(join(root, "public", page), "utf8")
        .replace(/(href|src)="(\.?\/)(styles\.css|fonts\.css|site\.css|market\.css|nojs\.css|legal\.css|legal\.js|join\.css|join\.js|terminal\.js|tokens\.js|swarm\.js|teams\.js|market\.js|swarm\.css|fonts\/[^"?]+|assets\/[^"?]+)"/g, `$1="$2$3?v=${STAMP}"`),
    );
  }

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
      // The SHIPPED bytes, as served.
      const artifact = readFileSync(join(dist, path.slice(1)));
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

  const kb = (f) => (statSync(join(dist, f)).size / 1024).toFixed(1);
  console.log(`dist/teams.js ${kb("teams.js")} KB · dist/market.js ${kb("market.js")} KB · dist/terminal.js ${kb("terminal.js")} KB · dist/tokens.js ${kb("tokens.js")} KB · stamp ${STAMP}`);
}
