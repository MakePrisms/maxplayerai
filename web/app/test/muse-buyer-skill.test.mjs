/**
 * The maxplayer-muse-buyer skill's shipped invariants: that the bundle installs from
 * its own manifest into an empty home with every link resolving, that it leaks no
 * operator identity, and that every tool-call example it publishes validates against
 * the MCP schema in THIS tree's source.
 *
 *   node --test web/app/test/muse-buyer-skill.test.mjs
 *
 * Node builtins only. No network, no relay, no mint, no daemon, no sats, and no Muse
 * account: this is a bundle-and-schema gate, never an acceptance run. The clean-account
 * Muse acceptance gate is separate and, at the time of writing, unpassed — see the
 * skill's references/verification.md.
 */
import assert from "node:assert/strict";
import {
  copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import test, { after } from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");   // web/app
const REPO = resolve(root, "..", "..");                             // repository root
const SKILLS_DIR = join(root, ".well-known", "skills");
const BUYER_DIR = join(SKILLS_DIR, "muse-buyer");
const SKILL_FILE = join(BUYER_DIR, "skill.md");
const MCP_SOURCE = join(REPO, "crates", "maxplayer", "src", "mcp.rs");
const BUYER_SOURCE = join(REPO, "crates", "maxplayer-core", "src", "buyer", "mod.rs");

const temps = [];
function tempRoot(label) {
  const dir = mkdtempSync(join(tmpdir(), `muse-buyer-${label}-`));
  temps.push(dir);
  return dir;
}
after(() => {
  for (const dir of temps) rmSync(dir, { recursive: true, force: true });
});

const skillText = () => readFileSync(SKILL_FILE, "utf8");

function walk(dir) {
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walk(full));
    else out.push(full);
  }
  return out;
}

// --- the shipped bundle ------------------------------------------------------

/**
 * The install manifest, parsed as the source → installed-path mapping it publishes.
 * Every row is `<repo-relative source>  ->  <path the reader saves it to>`, so the test
 * installs exactly the layout the reader is told to create — including its capitalisation.
 */
function installManifest() {
  const text = skillText();
  const marker = text.indexOf("<!-- install-manifest -->");
  assert.notEqual(marker, -1, "the skill must publish an install manifest");
  const fence = /```[a-z]*\n([\s\S]*?)```/.exec(text.slice(marker));
  assert.ok(fence, "the install manifest must be a fenced block");
  const rows = fence[1].split("\n").map((line) => line.trim())
    .filter((line) => line && !line.startsWith("#"));
  return rows.map((row) => {
    const parts = row.split("->").map((part) => part.trim());
    assert.equal(parts.length, 2,
      `manifest row "${row}" must map a shipped file to the path the reader saves it to`);
    return { source: parts[0], target: parts[1] };
  });
}

/**
 * Every markdown link in the shipped bundle, with the file it appears in.
 * A link to another published skill would make the bundle depend on a page this
 * install does not carry, so the closure test needs them all, not just relative ones.
 */
function bundleLinks() {
  const out = [];
  for (const file of walk(BUYER_DIR)) {
    if (!file.endsWith(".md")) continue;
    const text = readFileSync(file, "utf8");
    for (const match of text.matchAll(/\]\(([^)\s]+)\)/g)) {
      out.push({ file: relative(BUYER_DIR, file), href: match[1] });
    }
  }
  return out;
}

test("the install manifest names every file that is shipped, and ships every file it names", () => {
  const sources = new Set(installManifest().map((row) => row.source));
  for (const entry of sources) {
    assert.equal(existsSync(join(BUYER_DIR, entry)), true,
      `the manifest names ${entry}, which is not shipped`);
  }
  const shipped = walk(BUYER_DIR).map((file) => relative(BUYER_DIR, file));
  for (const file of shipped) {
    assert.ok(sources.has(file),
      `${file} is shipped but missing from the install manifest, so a reader's copy would lack it`);
  }
});

/** Install-layout problems in a set of manifest rows. */
function manifestProblems(rows) {
  const problems = [];
  const core = rows.find((row) => row.source === "skill.md");
  if (!core) problems.push("the manifest never says where the core itself is saved");
  else if (!/\/SKILL\.md$/.test(core.target)) {
    problems.push(`the core installs to ${core.target}, not to an uppercase SKILL.md entry point`);
  }
  for (const row of rows) {
    if (row.source === "skill.md") continue;
    if (!row.target.endsWith(row.source)) {
      problems.push(`${row.source} loses its relative layout when installed as ${row.target}`);
    }
  }
  return problems;
}

test("the install-layout check rejects a manifest that would not load in a Muse workspace", () => {
  const good = [
    { source: "skill.md", target: "~/workspace/skills/muse-buyer/SKILL.md" },
    { source: "references/settlement.md", target: "~/workspace/skills/muse-buyer/references/settlement.md" },
  ];
  assert.deepEqual(manifestProblems(good), [], "a correct manifest must pass");
  assert.equal(manifestProblems([good[1]]).length, 1,
    "a manifest with no entry point row must be caught");
  assert.equal(manifestProblems([
    { source: "skill.md", target: "~/workspace/skills/muse-buyer/skill.md" }, good[1],
  ]).length, 1, "a lowercase entry point must be caught: a Muse workspace reads SKILL.md");
  assert.equal(manifestProblems([
    good[0], { source: "references/settlement.md", target: "~/workspace/skills/settlement.md" },
  ]).length, 1, "a flattened reference path breaks the relative links and must be caught");
});

test("the manifest creates the uppercase entry point a Muse workspace reads", () => {
  const problems = manifestProblems(installManifest());
  assert.deepEqual(problems, [], `shipped install manifest: ${problems.join("; ")}`);
});

test("the bundle is self-contained: it requires no other skill and defers to none", () => {
  // The install is exactly this bundle. A row pointing outside it would mean the reader
  // must fetch and trust a page whose instructions this gate never checks.
  for (const row of installManifest()) {
    assert.equal(row.source.startsWith("/"), false,
      `manifest row ${row.source} installs a file from outside this bundle`);
    assert.equal(existsSync(join(BUYER_DIR, row.source)), true,
      `manifest row ${row.source} is not part of this bundle`);
  }
  // No shipped file may link to, or tell the reader to install, another skill.
  for (const { file, href } of bundleLinks()) {
    assert.equal(/\.well-known\/skills\//.test(href), false,
      `${file} links to another published skill (${href}), which this install does not carry`);
  }
  const bundle = walk(BUYER_DIR).filter((file) => file.endsWith(".md"))
    .map((file) => readFileSync(file, "utf8")).join("\n");
  for (const name of ["buyer-operate", "seller-operate", "debug-buying"]) {
    const mentions = bundle.split("\n").filter((line) => line.includes(name)
      && !line.startsWith("description:"));
    assert.deepEqual(mentions, [],
      `${name} is named as a dependency or handoff, but nothing installs it: ${mentions[0]}`);
  }
  // And the core must say so, so a reader does not go looking for a missing page.
  assert.match(skillText().replace(/\s+/g, " "), /self-contained/i,
    "the core must state that the install needs no other skill");
  assert.match(skillText().replace(/\s+/g, " "), /this bundle governs|takes precedence/i,
    "where other buyer material disagrees, the core must say which instruction wins");
});

test("a fresh-home install carries every link the skill follows, with nothing left behind", () => {
  const home = tempRoot("freshhome");
  // Install into the exact paths the manifest publishes, rooted at a temp home.
  const place = (targetPath, sourceFile) => {
    const target = join(home, targetPath.replace(/^~\//, ""));
    mkdirSync(dirname(target), { recursive: true });
    copyFileSync(sourceFile, target);
    return target;
  };
  let installedCore;
  for (const row of installManifest()) {
    const written = place(row.target, join(BUYER_DIR, row.source));
    if (row.source === "skill.md") installedCore = written;
  }
  assert.ok(installedCore && existsSync(installedCore),
    "the install must produce the entry point the manifest promises");
  const installed = dirname(installedCore);
  // Every relative markdown link in every installed file resolves inside the install.
  for (const file of walk(installed)) {
    if (!file.endsWith(".md")) continue;
    const text = readFileSync(file, "utf8");
    for (const match of text.matchAll(/\]\(([^)\s#][^)\s]*)\)/g)) {
      const href = match[1];
      if (/^(https?:|mailto:)/.test(href)) continue;
      assert.equal(href.startsWith("/"), false,
        `${relative(installed, file)} links to the website path ${href}, which does not resolve from an installed copy`);
      assert.equal(existsSync(resolve(dirname(file), href)), true,
        `${relative(installed, file)} links to ${href}, which the install does not contain`);
    }
  }
});

test("the installed skill states its prerequisites and assumes no working directory", () => {
  const text = skillText();
  assert.match(text, /## Prerequisites/, "a reader must be told what they need before step 1");
  assert.match(text, /maxplayer --version/, "the version check is the first prerequisite");
  // No command may depend on the reader standing in a particular directory.
  for (const match of text.matchAll(/^\s*(?:cd|\.\/)\s.*$/gm)) {
    assert.fail(`the skill assumes a working directory: ${match[0].trim()}`);
  }
});

test("nothing shipped carries an operator identity, home path, key, invoice or balance", () => {
  const forbidden = [
    [/\/Users\/[a-z]+\//i, "an absolute home path"],
    [/\/home\/[a-z]+\//i, "an absolute home path"],
    [/\b(?:npub|nsec)1[02-9ac-hj-np-z]{20,}/i, "a nostr key"],
    [/\b[0-9a-f]{64}\b/i, "a 64-hex key or event id"],
    [/\blnbc[0-9a-z]{20,}/i, "a lightning invoice"],
    [/\bbalance[^.\n]{0,20}\b\d{3,}\s*sats?\b/i, "a wallet balance"],
  ];
  for (const file of walk(BUYER_DIR)) {
    const text = readFileSync(file, "utf8");
    for (const [pattern, what] of forbidden) {
      assert.equal(pattern.test(text), false, `${relative(root, file)} leaks ${what}`);
    }
  }
});

test("the operational core stays inside the 10000-character reusable-skill budget", () => {
  const size = skillText().length;
  assert.ok(size < 10000,
    `skill.md is ${size} characters; branch detail belongs in references/`);
});

test("every version the bundle states equals this tree's version, with no stale literal", () => {
  const cargo = readFileSync(join(REPO, "Cargo.toml"), "utf8");
  const version = /^version\s*=\s*"([0-9.]+)"/m.exec(cargo)[1];
  assert.match(skillText(), new RegExp(`\\b${version.replace(/\./g, "\\.")}\\b`),
    `the core must pin this tree's version (${version})`);
  // EVERY version stated anywhere in the bundle must be this tree's, unless that same
  // statement labels itself a historical field report. The exemption is bound to the
  // matched line, so one labelled Tier-2 line cannot license a stale claim elsewhere.
  let checked = 0;
  for (const file of walk(BUYER_DIR)) {
    const lines = readFileSync(file, "utf8").split("\n");
    lines.forEach((line, index) => {
      for (const match of line.matchAll(/\b(\d+\.\d+\.\d+)\b/g)) {
        const stated = match[1];
        checked += 1;
        if (stated === version) continue;
        assert.match(line, /field[- ]report|Tier 2/i,
          `${relative(root, file)}:${index + 1} states version ${stated}, not this tree's `
          + `${version}, and does not label itself a field report: ${line.trim()}`);
      }
    });
  }
  assert.ok(checked > 0, "no version statement was found to check; the scan is broken");
});

/** Funding-instruction problems in a piece of prose, per wallet_cli.rs's real CLI. */
function fundingProblems(text) {
  const problems = [];
  for (const match of text.matchAll(/maxplayer wallet mint-complete([^\n]*)/g)) {
    const rest = match[1];
    // The first token after the subcommand must be the positional quote id, not a flag.
    const first = rest.trim().split(/\s+/)[0] || "";
    if (!first || first.startsWith("--")) {
      problems.push(`mint-complete shown without its positional quote id: ${match[0].trim()}`);
    }
  }
  if (!/quote_id/.test(text)) problems.push("the printed quote_id is never named");
  return problems;
}

test("the funding check rejects a bare mint-complete and accepts the shipped one", () => {
  assert.equal(fundingProblems(
    'maxplayer wallet mint-complete --home "$MAXPLAYER_HOME"\nquote_id\n').length, 1,
    "a mint-complete with only flags exits USAGE_ERROR and must be caught");
  assert.equal(fundingProblems("maxplayer wallet mint-complete\nquote_id\n").length, 1,
    "a mint-complete with no argument at all must be caught");
  assert.equal(fundingProblems("maxplayer wallet setup 500\n").length, 1,
    "prose that never names the printed quote_id must be caught");
  assert.deepEqual(fundingProblems(
    'maxplayer wallet mint-complete <quote_id> --home "$H"\n'), [],
    "the correct positional form must pass");
});

test("the funding completion command carries the quote id the setup output prints", () => {
  const text = skillText();
  const problems = fundingProblems(text);
  assert.deepEqual(problems, [], `shipped funding instructions: ${problems.join("; ")}`);
  const wallet = readFileSync(join(REPO, "crates", "maxplayer", "src", "wallet_cli.rs"), "utf8");
  // The file carries a feature-gated stub of the same name; the real implementation is
  // the one compiled with the wallet feature, so take the last definition.
  const at = wallet.lastIndexOf("fn cmd_mint_complete");
  assert.notEqual(at, -1, "mint-complete is gone from this tree; the skill is stale");
  const command = [wallet.slice(at, wallet.indexOf("\n}", at) + 2)];
  assert.match(command[0], /\[quote_id\]\s*=>/,
    "this tree no longer requires exactly one positional quote id; revisit the instruction");
  assert.match(command[0], /USAGE_ERROR/,
    "this tree no longer refuses mint-complete without its quote id; revisit the instruction");
  assert.match(wallet, /quote_id=\{\}/,
    "setup no longer prints the quote id the skill tells the reader to keep");
});

test("the skill gives a launchable MCP registration recipe, home included", () => {
  const text = skillText();
  assert.match(text, /maxplayer\s+mcp|"mcp"/,
    "setting MAXPLAYER_HOME without the server command leaves the reader unable to start");
  const configs = [...text.matchAll(/```json\n([\s\S]*?)```/g)]
    .map((match) => { try { return JSON.parse(match[1]); } catch { return null; } })
    .filter((parsed) => parsed && parsed.command);
  assert.equal(configs.length >= 1, true, "the MCP server registration must be shown, not described");
  for (const config of configs) {
    assert.ok(Array.isArray(config.args) && config.args.includes("mcp"),
      "the registration must launch the mcp subcommand");
    assert.ok(config.env && typeof config.env.MAXPLAYER_HOME === "string",
      "MAXPLAYER_HOME must be set on the server process, so it belongs in the registration env");
    assert.match(config.env.MAXPLAYER_HOME, /^\//,
      "the server does not inherit a shell cwd; the home must be absolute");
  }
  assert.match(text.replace(/\s+/g, " "), /\*\*Not verified:?\*\*|Not verified/i,
    "Muse-side MCP registration was never tested; that must be labelled, not implied");
});

// --- money discipline the skill must keep --------------------------------------

test("the skill gets a human amount before the single funding invoice, and asks once", () => {
  const text = skillText();
  const setups = [...text.matchAll(/maxplayer wallet setup/g)];
  assert.equal(setups.length, 1,
    "wallet setup appears more than once: a reader could print a second invoice");
  assert.match(text, /21 sats/,
    "omitting the amount silently requests 21 sats; the skill must say so");
  assert.match(text, /mint-complete/, "the second funding step must be shown");
  const source = readFileSync(join(REPO, "crates", "maxplayer", "src", "wallet_cli.rs"), "utf8");
  assert.match(source, /SETUP_FUND_SATS: u64 = 21/,
    "this tree's default funding amount is no longer 21 sats; the skill is stale");
});

test("the skill demands a fresh human approval per paid post and offers no standing budget", () => {
  const text = skillText();
  assert.match(text, /fresh spend/i, "a re-post must be named as a fresh spend");
  assert.match(text, /fresh yes|fresh approval/i, "a re-post must need a fresh approval");
  assert.match(text, /never a standing budget|not a standing budget/i,
    "approval must be scoped to one post");
});

test("the skill never promises that a failed command cost nothing", () => {
  const text = `${skillText()}\n${readFileSync(join(BUYER_DIR, "references", "settlement.md"), "utf8")}`;
  // Written without newline sensitivity: a wrapped sentence is the same promise.
  const flat = text.replace(/\s+/g, " ");
  for (const forbidden of [
    /a refusal costs nothing/i,
    /never charges twice/i,
    /no money (?:has )?moved/i,
    /guaranteed not to (?:charge|spend)/i,
  ]) {
    assert.equal(forbidden.test(flat), false,
      `the buyer pages must not carry a blanket no-charge assurance (${forbidden})`);
  }
  assert.match(flat, /paid/i);
  assert.match(flat, /idempotent|reconcil/i,
    "the recovery from a failed collect must be stated");
});

test("background settlement is documented, since money can move with no collect call", () => {
  const settlement = readFileSync(join(BUYER_DIR, "references", "settlement.md"), "utf8")
    .replace(/\s+/g, " ");
  assert.match(settlement, /background/i);
  assert.match(settlement, /without you (?:ever )?calling `?collect|without you/i);
  assert.match(readFileSync(BUYER_SOURCE, "utf8"), /watcher|settle/i,
    "this tree's buyer no longer shows a background settlement path; the reference is stale");
});

test("an unsupported network route is named as a blocker and no bypass is published", () => {
  const text = skillText().replace(/\s+/g, " ");
  assert.match(text, /relay is not reachable|cannot reach the relay/i,
    "the unreachable-relay case must be named");
  for (const bypass of [/\/etc\/hosts/i, /tunnel/i, /mount namespace/i, /proxy it does not support/i]) {
    if (!bypass.test(text)) continue;
    // Mentioning it is allowed only as a prohibition, never as a recipe.
    const line = text.split("\n").find((candidate) => bypass.test(candidate));
    assert.match(line, /not|never|do not|refuse|blocker/i,
      `${line.trim()} reads as a workaround rather than a prohibition`);
  }
});

// --- published examples against this tree's schema ----------------------------

/** The text of the balanced `{ … }` block that starts at `open` in `text`. */
function braceBlock(text, open) {
  let depth = 0;
  for (let index = open; index < text.length; index += 1) {
    if (text[index] === "{") depth += 1;
    else if (text[index] === "}") {
      depth -= 1;
      if (depth === 0) return text.slice(open, index + 1);
    }
  }
  assert.fail("unbalanced schema block in mcp.rs");
}

/** The crate sources a named schema bound may be defined in. */
const CONSTANT_SOURCES = [
  join(REPO, "crates", "maxplayer-core", "src", "long_poll.rs"),
  join(REPO, "crates", "maxplayer-core", "src", "buyer", "mod.rs"),
  join(REPO, "crates", "maxplayer", "src", "mcp.rs"),
];

/**
 * A numeric schema bound, whether the source writes it as a literal or as a named
 * constant such as `long_poll::WAIT_FOR_CAP_SECS`. A named bound is looked up in the
 * crate that defines it; an unresolvable name FAILS the gate instead of silently
 * becoming null, because a dropped bound is how an example above the real cap goes green.
 */
function resolveBound(tool, property, kind, token) {
  if (/^[0-9]+$/.test(token)) return Number(token);
  const constant = token.split("::").pop();
  for (const file of CONSTANT_SOURCES) {
    if (!existsSync(file)) continue;
    const found = new RegExp(`const\\s+${constant}\\s*:\\s*[a-z0-9]+\\s*=\\s*([0-9_]+)`)
      .exec(readFileSync(file, "utf8"));
    if (found) return Number(found[1].replace(/_/g, ""));
  }
  assert.fail(`${tool}.${property} declares ${kind} = ${token}, which this gate cannot `
    + "resolve to a number; resolve it or narrow the claim rather than ignoring the bound");
}

/**
 * One MCP tool's DECLARED schema, read out of the Rust source with the constraints
 * intact — not just the property names. A checker that only knows the names would pass
 * a string where an integer is declared, or a negative amount, which is exactly the
 * kind of example that looks valid and is refused in the field.
 */
function toolSchema(name) {
  const source = readFileSync(MCP_SOURCE, "utf8");
  const start = source.indexOf(`"name": "${name}"`);
  assert.notEqual(start, -1, `${name} is not declared in mcp.rs`);
  const schemaAt = source.indexOf('"inputSchema"', start);
  assert.notEqual(schemaAt, -1, `${name} declares no inputSchema`);
  const schema = braceBlock(source, source.indexOf("{", schemaAt));
  const required = /"required":\s*\[([^\]]*)\]/.exec(schema);
  const propertiesAt = schema.indexOf('"properties"');
  assert.notEqual(propertiesAt, -1, `${name} declares no properties`);
  const propertiesBlock = braceBlock(schema, schema.indexOf("{", propertiesAt));
  const properties = new Map();
  // Walk the top level of the properties object, key by key.
  const keyPattern = /"([a-z_]+)":\s*\{/g;
  let match;
  while ((match = keyPattern.exec(propertiesBlock)) !== null) {
    const body = braceBlock(propertiesBlock, propertiesBlock.indexOf("{", match.index + match[0].length - 1));
    // Only top-level keys: skip anything nested inside a body already consumed.
    if (keyPattern.lastIndex < match.index + body.length) {
      keyPattern.lastIndex = match.index + match[0].length - 1 + body.length;
    }
    const type = /"type":\s*"([a-z]+)"/.exec(body);
    const enumeration = /"enum":\s*\[([^\]]*)\]/.exec(body);
    const minimum = /"minimum":\s*([A-Za-z0-9_:]+)/.exec(body);
    const maximum = /"maximum":\s*([A-Za-z0-9_:]+)/.exec(body);
    const itemsAt = body.indexOf('"items"');
    const itemsType = itemsAt === -1
      ? null
      : /"type":\s*"([a-z]+)"/.exec(braceBlock(body, body.indexOf("{", itemsAt)));
    // A bound written as a Rust constant is still a real bound. Resolve it from the
    // source that defines it; if it cannot be resolved, fail closed rather than drop it.
    properties.set(match[1], {
      type: type ? type[1] : null,
      values: enumeration
        ? enumeration[1].split(",").map((part) => part.trim().replace(/"/g, "")).filter(Boolean)
        : null,
      minimum: minimum ? resolveBound(name, match[1], "minimum", minimum[1]) : null,
      maximum: maximum ? resolveBound(name, match[1], "maximum", maximum[1]) : null,
      items: itemsType ? itemsType[1] : (itemsAt === -1 ? null : "unresolved"),
    });
  }
  return {
    properties,
    required: required
      ? required[1].split(",").map((part) => part.trim().replace(/"/g, "")).filter(Boolean)
      : [],
  };
}

/** Check one argument object against a declared schema; returns a list of problems. */
function schemaProblems(tool, args) {
  const schema = toolSchema(tool);
  const problems = [];
  for (const key of schema.required) {
    if (!(key in args)) problems.push(`omits required argument ${key}`);
  }
  for (const [key, value] of Object.entries(args)) {
    const declared = schema.properties.get(key);
    if (!declared) {
      problems.push(`passes ${key}, which the schema does not declare`);
      continue;
    }
    const actual = Array.isArray(value) ? "array" : typeof value;
    if (declared.type === "integer") {
      if (!Number.isInteger(value)) problems.push(`${key} must be an integer, got ${actual}`);
    } else if (declared.type === "string" && actual !== "string") {
      problems.push(`${key} must be a string, got ${actual}`);
    } else if (declared.type === "boolean" && actual !== "boolean") {
      problems.push(`${key} must be a boolean, got ${actual}`);
    } else if (declared.type === "array" && actual !== "array") {
      problems.push(`${key} must be an array, got ${actual}`);
    } else if (declared.type === "array" && Array.isArray(value)) {
      assert.notEqual(declared.items, "unresolved",
        `${key} declares an items constraint this gate cannot read; resolve it or narrow the claim`);
      for (const element of value) {
        const elementType = Array.isArray(element) ? "array" : typeof element;
        if (declared.items === "string" && elementType !== "string") {
          problems.push(`${key} must contain strings, got ${elementType}`);
        } else if (declared.items === "integer" && !Number.isInteger(element)) {
          problems.push(`${key} must contain integers, got ${elementType}`);
        }
      }
    }
    if (declared.minimum !== null && typeof value === "number" && value < declared.minimum) {
      problems.push(`${key} is below the declared minimum ${declared.minimum}`);
    }
    if (declared.maximum !== null && typeof value === "number" && value > declared.maximum) {
      problems.push(`${key} is above the declared maximum ${declared.maximum}`);
    }
    if (declared.values && !declared.values.includes(value)) {
      problems.push(`${key}=${JSON.stringify(value)} is outside the declared enum ${declared.values.join("|")}`);
    }
  }
  return problems;
}

/** Every {"tool","arguments"} example the skill publishes. */
function publishedExamples() {
  const examples = [];
  for (const match of skillText().matchAll(/```json\n([\s\S]*?)```/g)) {
    let parsed;
    try {
      parsed = JSON.parse(match[1]);
    } catch {
      assert.fail(`the skill publishes a json block that does not parse: ${match[1].slice(0, 80)}`);
    }
    if (parsed && typeof parsed === "object" && parsed.tool) examples.push(parsed);
  }
  return examples;
}

test("every published example satisfies the declared types, bounds and enums, not just the names", () => {
  const examples = publishedExamples();
  assert.ok(examples.length >= 4, "the skill should show its calls, not describe them");
  const shown = new Set(examples.map((example) => example.tool));
  for (const tool of ["post_job", "get_job", "collect", "award_claim"]) {
    assert.ok(shown.has(tool), `${tool} is part of the buyer surface and must be shown`);
  }
  for (const example of examples) {
    const problems = schemaProblems(example.tool, example.arguments || {});
    assert.deepEqual(problems, [],
      `${example.tool} example: ${problems.join("; ")}`);
  }
});

test("the constraint checker itself rejects the shapes it is meant to catch", () => {
  // A checker that cannot fail is not evidence. These are the exact mistakes a
  // plausible-looking example makes, and each must be caught.
  const base = { task: "t", output: "text/plain", amount_sats: 100, untargeted: true };
  const cases = [
    ["post_job", { ...base, amount_sats: "100" }, /integer/],
    ["post_job", { ...base, payment: "free" }, /enum/],
    ["post_job", { ...base, untargeted: "yes" }, /boolean/],
    ["post_job", { ...base, nonsense_field: 1 }, /does not declare/],
    ["post_job", { output: "text/plain", amount_sats: 1, untargeted: true }, /required argument task/],
    // Bounds, both ends. amount_sats declares minimum 0; get_job's timeout_secs
    // declares minimum 1 and a maximum written as a named constant.
    ["post_job", { ...base, amount_sats: -1 }, /below the declared minimum 0/],
    ["get_job", { job_id: "abc", timeout_secs: 0 }, /below the declared minimum 1/],
    ["get_job", { job_id: "abc", timeout_secs: 11 }, /above the declared maximum 10/],
    ["get_job", { job_id: "abc", wait_for: "delivery" }, /enum/],
  ];
  for (const [tool, args, expected] of cases) {
    const problems = schemaProblems(tool, args).join("; ");
    assert.match(problems, expected,
      `the checker passed ${tool} ${JSON.stringify(args)}, which the declared schema forbids`);
  }
  assert.deepEqual(schemaProblems("post_job", base), [],
    "the checker must still accept a valid call");
  assert.deepEqual(schemaProblems("get_job", { job_id: "abc", timeout_secs: 10 }), [],
    "the value exactly at the resolved cap is legal and must not be flagged");
});

test("the long-poll cap is resolved from the constant the schema names, not dropped", () => {
  // mcp.rs writes get_job.timeout_secs.maximum as long_poll::WAIT_FOR_CAP_SECS. A gate
  // that keeps only digit literals would treat that as unbounded, and an example above
  // the real cap would pass while the server refuses it.
  const cap = toolSchema("get_job").properties.get("timeout_secs").maximum;
  const longPoll = readFileSync(
    join(REPO, "crates", "maxplayer-core", "src", "long_poll.rs"), "utf8");
  const declared = /const\s+WAIT_FOR_CAP_SECS\s*:\s*[a-z0-9]+\s*=\s*([0-9_]+)/.exec(longPoll);
  assert.ok(declared, "WAIT_FOR_CAP_SECS is gone from this tree; the resolver is stale");
  assert.equal(cap, Number(declared[1].replace(/_/g, "")),
    "the gate's cap must equal the constant this tree defines");
  assert.equal(typeof cap, "number", "a named bound must resolve to a number, never to null");
  // An unresolvable name must fail closed rather than pass.
  assert.throws(() => resolveBound("get_job", "timeout_secs", "maximum", "nope::NO_SUCH_CONST"),
    /cannot resolve/, "an unresolvable bound must fail the gate, not be ignored");
});

/** Target-mode problems with one post_job argument set, per job_lifecycle.rs's rule. */
function targetProblems(args) {
  const targeted = typeof args.seller_pubkey === "string" && args.seller_pubkey.length > 0;
  const open = args.untargeted === true;
  if (!targeted && !open) return ["neither seller_pubkey nor untargeted=true: refused"];
  if (targeted && open) return ["both target modes at once: refused"];
  return [];
}

test("the target-mode check rejects a post with no target, and one with both", () => {
  const base = { task: "t", output: "text/plain", amount_sats: 1 };
  assert.deepEqual(targetProblems({ ...base }).length, 1,
    "a post with no target mode must be caught; the schema's three required fields accept it");
  assert.deepEqual(targetProblems({ ...base, seller_pubkey: "ab", untargeted: true }).length, 1,
    "a post setting both target modes must be caught");
  assert.deepEqual(targetProblems({ ...base, seller_pubkey: "ab" }), [],
    "a targeted post is legal");
  assert.deepEqual(targetProblems({ ...base, untargeted: true }), [],
    "an open offer is legal");
  // A blank pubkey is not a target either.
  assert.equal(targetProblems({ ...base, seller_pubkey: "" }).length, 1,
    "a blank seller_pubkey is not a target mode");
});

test("every post_job example picks a target mode, which the schema alone cannot enforce", () => {
  const posts = publishedExamples().filter((one) => one.tool === "post_job");
  assert.ok(posts.length >= 2, "both the paid and the free route must be shown");
  for (const post of posts) {
    const problems = targetProblems(post.arguments || {});
    assert.deepEqual(problems, [], `published post_job example: ${problems.join("; ")}`);
  }
  // The rule is enforced in the source, not in the MCP schema: prove it is still there.
  const lifecycle = readFileSync(
    join(REPO, "crates", "maxplayer-core", "src", "job_lifecycle.rs"), "utf8");
  assert.match(lifecycle, /post_job requires seller_pubkey \(targeted default\) or untargeted=true/,
    "this tree no longer refuses an untargeted-less post; the skill's rule is stale");
  assert.match(lifecycle, /untargeted=true cannot also set seller_pubkey/,
    "this tree no longer refuses both target modes at once; the skill's rule is stale");
  // And the reader must be told, not left to infer it from an example.
  assert.match(skillText().replace(/\s+/g, " "),
    /must choose a target mode|requires seller_pubkey|untargeted/i,
    "the target rule belongs in the prose, since the three required fields are not enough");
});

test("the approval checklist covers the target choice and public publication of a free task", () => {
  const text = skillText().replace(/\s+/g, " ");
  assert.match(text, /who may take it|named seller|open offer/i,
    "the human chooses the target mode; it must be in the approval checklist");
  assert.match(text, /publish[^.]{0,60}publicly|public offer/i,
    "a free job still publishes the task text publicly, which needs its own yes");
  assert.match(text, /not new authority|is not new authority|a max_sats the schema would accept is not new authority/i,
    "a schema-permitted higher cap must not read as fresh human authority");
});

test("every post_job example carries output, since a post without it is refused", () => {
  for (const example of publishedExamples().filter((one) => one.tool === "post_job")) {
    assert.ok(typeof example.arguments.output === "string" && example.arguments.output.length > 0,
      "post_job requires output; an example without it teaches a refused call");
  }
});

test("the free-job example obeys the enforcement this tree actually performs", () => {
  const free = publishedExamples().filter(
    (one) => one.tool === "post_job" && (one.arguments || {}).payment === "none");
  assert.ok(free.length >= 1, "the free route must be shown as the schema declares it");
  for (const example of free) {
    assert.equal(example.arguments.amount_sats, 0,
      "payment=none with a non-zero amount is refused by the buyer path");
  }
  const source = readFileSync(BUYER_SOURCE, "utf8");
  assert.match(source, /payment=none requires amount_sats = 0/,
    "this tree no longer enforces the free-job rule the skill teaches");
  assert.match(source, /post_job payment must be/,
    "this tree no longer validates the payment vocabulary the skill teaches");
});

test("the skill separates the declared schema from what the server actually enforces", () => {
  const text = `${skillText()}\n${readFileSync(join(BUYER_DIR, "references", "verification.md"), "utf8")}`;
  assert.match(text, /additionalProperties/,
    "the declared strictness must be named so it is not mistaken for enforcement");
  assert.match(text, /deny_unknown_fields/,
    "the absence of runtime unknown-field rejection must be stated");
  assert.equal(/deny_unknown_fields/.test(readFileSync(BUYER_SOURCE, "utf8")), false,
    "this tree now denies unknown fields; the skill's caveat is stale and must be revisited");
});

test("the manual award example carries both required arguments and explains its ceiling", () => {
  const awards = publishedExamples().filter((one) => one.tool === "award_claim");
  assert.ok(awards.length >= 1, "manual award is the fine-grained spend and must be shown");
  for (const award of awards) {
    for (const key of ["job_id", "claim_id"]) {
      assert.ok(key in (award.arguments || {}), `award_claim example omits ${key}`);
    }
  }
  assert.match(skillText(), /write-once/i, "the write-once award must be stated");
});

test("the skill records that MAXPLAYER_HOME is refused as a flag, matching this tree's CLI", () => {
  const text = skillText();
  assert.match(text, /MAXPLAYER_HOME/);
  assert.match(text, /refus/i, "the skill must say --home is refused, not silently ignored");
  const cli = readFileSync(join(REPO, "crates", "maxplayer", "src", "cli.rs"), "utf8");
  assert.match(cli, /buyer_serve_with_home_flag_refuses_instead_of_silently_ignoring_it/,
    "this tree no longer refuses --home; the skill is stale");
});

// --- discovery ----------------------------------------------------------------

const index = JSON.parse(readFileSync(join(SKILLS_DIR, "index.json"), "utf8"));

test("the buyer skill is published in the discovery index and every prior entry survives", () => {
  const names = index.skills.map((entry) => entry.name);
  assert.ok(names.includes("maxplayer-muse-buyer"), "the buyer skill must be indexed");
  for (const name of ["maxplayer-marketplace", "maxplayer-buyer-operate",
    "maxplayer-seller-operate", "maxplayer-multi-turn-buying", "maxplayer-debug-buying",
    "maxplayer-debug-selling", "maxplayer-grok-bot-operate"]) {
    assert.ok(names.includes(name), `${name} must survive in the index`);
  }
  assert.equal(new Set(names).size, names.length, "no duplicate index entries");
  assert.equal(names.includes("maxplayer-muse-seller"), false,
    "the seller skill is out of this branch's scope and must not be indexed here");
});

test("every indexed skill resolves to a file whose frontmatter agrees with the index", () => {
  for (const entry of index.skills) {
    const file = join(root, entry.path.replace(/^\//, ""));
    assert.equal(existsSync(file), true, `${entry.name} points at a missing file`);
    const match = /^---\n([\s\S]*?)\n---/.exec(readFileSync(file, "utf8"));
    assert.ok(match, `${entry.name} has no YAML frontmatter for a client to install by`);
    const fields = {};
    for (const line of match[1].split("\n")) {
      const field = /^([a-z_]+):\s*(.*)$/.exec(line);
      if (field) fields[field[1]] = field[2].replace(/^["']|["']$/g, "");
    }
    assert.equal(fields.name, entry.name, `${entry.name} frontmatter disagrees with the index`);
    assert.ok(fields.description && fields.description.length > 20,
      `${entry.name} needs a description a client can match on`);
  }
});

test("the free collect response is documented, and its missing field is not read as zero spend", () => {
  const settlement = readFileSync(join(BUYER_DIR, "references", "settlement.md"), "utf8")
    .replace(/\s+/g, " ");
  for (const field of ["attempt_id", "amount_sats", "spent_total_sats"]) {
    assert.ok(settlement.includes(field),
      `a free collect's response shape must name ${field}`);
  }
  assert.match(settlement, /not[^.]{0,80}(lifetime|spent nothing)/i,
    "the absent spent_total_sats must be explained as the free shape, not a zero lifetime spend");
  assert.match(settlement, /same[^.]{0,60}(integrity|verification|checks)/i,
    "a free job runs the same integrity checks; that must be stated, not implied");
  // The response shape is source truth: fail loudly if this tree changes it.
  const buyer = readFileSync(BUYER_SOURCE, "utf8");
  assert.match(buyer, /PaymentMode::None\.as_wire\(\)/,
    "this tree no longer reports a free payment state; the reference is stale");
  assert.match(buyer, /"attempt_id": Value::Null/,
    "this tree no longer nulls attempt_id on a free collect; the reference is stale");
});

test("pay-and-attempt-id language is scoped to paid jobs", () => {
  const settlement = readFileSync(join(BUYER_DIR, "references", "settlement.md"), "utf8");
  const heading = settlement.split("\n").find((line) => /^##\s.*collect/i.test(line));
  assert.ok(heading && /paid/i.test(heading),
    `the money-call section must say it is about paid jobs, got ${heading}`);
  assert.match(settlement.replace(/\s+/g, " "), /free jobs? collect differently|payment: "none"/i,
    "the free route must be distinguished from the paid one");
});

test("a retry is described as preserving the award, not as guaranteed convergence", () => {
  const text = `${skillText()}\n${readFileSync(join(BUYER_DIR, "references", "settlement.md"), "utf8")}`
    .replace(/\s+/g, " ");
  assert.equal(/and it converges|is how you converge|always converges/i.test(text), false,
    "an expired attempt is probed without transmission and can stay unresolved: do not promise convergence");
  assert.match(text, /prob(e|ed|es)/i, "the probe-only retry behaviour must be stated");
  assert.match(text, /unresolved|remain blocked|can come back refused|stay unresolved/i,
    "the reader must be told a retry can end unresolved");
  assert.match(text, /same\W{0,4}job/i,
    "recovery must stay on the same job rather than reposting");
  assert.match(text, /fresh (human )?yes|fresh approval/i,
    "a replacement post is a second spend and needs fresh approval");
});

test("the skill is honest that no clean-account Muse acceptance has been run", () => {
  const verification = readFileSync(join(BUYER_DIR, "references", "verification.md"), "utf8");
  assert.match(verification, /unpassed|not verified|no clean-account/i,
    "the open acceptance gate must be stated, not implied");
  assert.match(skillText(), /UNPROVEN|unpassed|verification\.md/,
    "the core must point at the verification tiers");
});
