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
import { parseChallengeRelays, parseChallengeQuorum } from "../src/challenge/shared/relay.mjs";
import { withSwarmBuyer } from "./swarm-buyer.mjs";
import { build, context } from "esbuild";
import { createHash } from "node:crypto";
import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");
const SITE_ORIGIN = "https://www.maxplayer.ai";

const buyer = process.env.CHALLENGE_BUYER_PUBKEY ?? "94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5";
if (!/^[0-9a-f]{64}$/.test(buyer)) throw Error("CHALLENGE_BUYER_PUBKEY must be lowercase hex");
const relay = process.env.RELAY_URL ?? "wss://relay.maxplayer.ai";
if (!["ws:", "wss:"].includes(new URL(relay).protocol)) throw Error("Invalid RELAY_URL");
const challengeRelays = parseChallengeRelays(process.env.CHALLENGE_RELAYS);
// Challenge relay quorum (run 2 #13): default 2; must be a majority of CHALLENGE_RELAYS.
const challengeQuorum = parseChallengeQuorum(process.env.CHALLENGE_QUORUM, challengeRelays.length);
const watch = process.argv.includes("--watch");
// terms-v1/privacy-v1/fees-v1 are served at /terms/v1, /privacy/v1 and /fees/v1 (vercel.json rewrites); /terms, /privacy and /fees redirect to the current version.
const PAGES = ["index.html", "market.html", "sell.html", "chat.html", "mint.html", "tokens.html", "swarm.html", "terms-v1.html", "privacy-v1.html", "fees-v1.html"];

const options = {
  // terminal.js serves /, /sell and /market; tokens.js serves /tokens only,
  // so the token-trade reader never ships in (or changes) the jobs bundle.
  entryPoints: { terminal: join(root, "src/main.ts"), tokens: join(root, "src/tokens.ts"), swarm: join(root, "src/swarm.ts") },
  bundle: true,
  define: {
    CHALLENGE_BUYER_PUBKEY: JSON.stringify(buyer),
    CHALLENGE_RELAYS: JSON.stringify(challengeRelays),
    CHALLENGE_QUORUM: JSON.stringify(challengeQuorum),
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
  writeFileSync(join(dist, "swarm.html"), withSwarmBuyer(readFileSync(join(root, "public/swarm.html"), "utf8"), "html", buyer));
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
  for (const rel of [...PAGES, "styles.css", "fonts.css", "swarm.css", ...fontNames.map((f) => join("fonts", f))]) {
    hash.update(readFileSync(join(root, "public", rel)));
  }
  hash.update(readFileSync(join(dist, "terminal.js")));
  hash.update(readFileSync(join(dist, "tokens.js")));
  hash.update(readFileSync(join(dist, "swarm.js")));
  const STAMP = hash.digest("hex").slice(0, 12);

  // Stamp the specifiers, not the files: fonts.css's url()s and every asset
  // URL in index.html, including the font preloads (a preload URL must match
  // the CSS URL byte-for-byte or the browser fetches the font twice).
  writeFileSync(
    join(dist, "fonts.css"),
    readFileSync(join(root, "public/fonts.css"), "utf8")
      .replace(/url\((['"])(\.\/fonts\/[^'"?]+)\1\)/g, `url($1$2?v=${STAMP}$1)`),
  );
  // Every page: the buyer homepage (/), the seller page (/sell), the live
  // market (/market), the chat page (/chat), the mint page (/mint), the token-trade page (/tokens) and the community swarm (/swarm) — cleanUrls serves them.
  for (const page of PAGES) {
    writeFileSync(
      join(dist, page),
      withSwarmBuyer(readFileSync(join(root, "public", page), "utf8"), page === "swarm.html" ? "html" : null, buyer)
        .replace(/(href|src)="\.\/(styles\.css|fonts\.css|terminal\.js|tokens\.js|swarm\.js|swarm\.css|fonts\/[^"?]+)"/g, `$1="./$2?v=${STAMP}"`),
    );
  }

  // The agent-facing surface: skills, discovery index, /skill.md alias.
  cpSync(join(root, ".well-known"), join(dist, ".well-known"), { recursive: true });
  // The join skill names the swarm buyer's key in its text, written at build time like the page's,
  // so an agent following the one-line join finds it without running the page's script.
  const joinSkill = join(".well-known", "skills", "swarm-join", "skill.md");
  writeFileSync(join(dist, joinSkill), withSwarmBuyer(readFileSync(join(root, joinSkill), "utf8"), "skill", buyer));

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
      // The SHIPPED bytes: the join skill is rewritten above, so its source file is not what is served.
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
  console.log(`dist/terminal.js ${kb("terminal.js")} KB · dist/tokens.js ${kb("tokens.js")} KB · stamp ${STAMP}`);
}
