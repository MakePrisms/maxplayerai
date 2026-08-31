/**
 * What `prefers-reduced-motion: reduce` is allowed to take away.
 *
 * The preference suppresses MOVEMENT, not MEANING. The runner lamps animate
 * `background` and nothing else — no transform, no offset, no scale — so
 * suppressing them removed the only cue that separates a working runner from
 * an idle one, which is information loss wearing an accessibility costume.
 *
 * Nothing else in `npm test` can see a media query: there is no browser and no
 * CSS engine here, and the lamp tests in market.test.ts assert model state
 * (which lamp is on), never the stylesheet. So this file parses the shipped
 * CSS as text. It is a stylesheet-shape gate, not a rendering test — the
 * rendering claim needs a browser, and `scripts/motion-check.mjs` makes it.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const cssText = readFileSync(join(root, "public", "styles.css"), "utf8");
const indicatorsText = readFileSync(join(root, "src", "ui", "indicators.ts"), "utf8");

/* --- a very small CSS reader -------------------------------------------
   Hand-rolled because this package ships zero runtime dependencies. It only
   has to read flat rules inside a media block, which is all styles.css has. */

const stripComments = (s) => s.replace(/\/\*[\s\S]*?\*\//g, "");

/** Brace-match every `@media` whose prelude mentions the preference. */
function reducedMotionBlocks(source) {
  const blocks = [];
  const re = /@media([^{]*)\{/g;
  let m;
  while ((m = re.exec(source))) {
    if (!/prefers-reduced-motion/.test(m[1])) continue;
    let depth = 1;
    let i = re.lastIndex;
    for (; i < source.length && depth > 0; i++) {
      if (source[i] === "{") depth++;
      else if (source[i] === "}") depth--;
    }
    assert.equal(depth, 0, "unbalanced braces in a prefers-reduced-motion block");
    blocks.push({ prelude: m[1].trim(), body: source.slice(re.lastIndex, i - 1) });
  }
  return blocks;
}

/** Flat `selector { prop: value; ... }` rules, one entry per selector. */
function rules(body) {
  const out = [];
  for (const [, selectorList, declText] of body.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const decls = declText
      .split(";")
      .map((d) => d.trim())
      .filter(Boolean)
      .map((d) => {
        const at = d.indexOf(":");
        return {
          prop: d.slice(0, at).trim().toLowerCase(),
          value: d.slice(at + 1).replace(/!important/i, "").trim().toLowerCase(),
        };
      });
    for (const selector of selectorList.split(",").map((s) => s.trim()).filter(Boolean)) {
      out.push({ selector, decls });
    }
  }
  return out;
}

const css = stripComments(cssText);
const blocks = reducedMotionBlocks(css);
const reducedRules = blocks.flatMap((b) => rules(b.body));

/** Selectors the preference switches an animation OFF for. */
const animationKilled = reducedRules
  .filter(({ decls }) =>
    decls.some(
      ({ prop, value }) =>
        (prop === "animation" || prop === "animation-name") && value === "none",
    ),
  )
  .map(({ selector }) => selector);

test("the reduced-motion parser actually read the blocks", () => {
  // The control for every negative below. A parser that silently returns
  // nothing makes "no rule suppresses the lamp" pass on an empty set, which
  // is the one way this file could certify a stylesheet it never read.
  assert.ok(blocks.length >= 1, "found no prefers-reduced-motion block at all");
  assert.ok(reducedRules.length >= 2, `parsed only ${reducedRules.length} rules`);
  assert.ok(
    reducedRules.some(
      ({ selector, decls }) =>
        selector === "html" && decls.some(({ prop }) => prop === "scroll-behavior"),
    ),
    "did not find the known `html { scroll-behavior }` rule — the reader is broken",
  );
  assert.ok(animationKilled.length >= 1, "found no animation suppression to reason about");
});

test("reduced motion does NOT suppress the runner lamps", () => {
  // lamp-a/b/c animate `background` only, at a 2.2s cycle (~0.45Hz) on three
  // hairlines 4.5px and narrower. That is not vestibular motion and it is far
  // under WCAG 2.3.1's 3Hz flash threshold, so the preference must leave it
  // alone. Under suppression a reader could not tell a sweeping lamp from a
  // static lit nose.
  const offenders = animationKilled.filter((selector) => /\.dot\b/.test(selector));
  assert.deepEqual(
    offenders,
    [],
    `reduced motion suppresses the lamp animation via: ${offenders.join(" | ")}`,
  );
});

test("reduced motion still suppresses .sl", () => {
  // Narrowing the lamp out of the shared suppression must not carry .sl with
  // it. .sl is the speed streak, and its companion rule below paints a static
  // colour precisely because its animation is gone.
  assert.ok(
    animationKilled.includes(".sl"),
    `.sl lost its animation suppression; killed selectors are: ${animationKilled.join(" | ")}`,
  );
});

test("reduced motion sets no dead lamp colour", () => {
  // A running animation beats a normal declaration for the property it
  // animates, so any `background` the preference sets on a working lamp is
  // overridden the instant the sweep runs. Such a rule reads as the live
  // decision on lamp colour while deciding nothing.
  const dead = reducedRules.filter(
    ({ selector, decls }) =>
      /\.dot\.working\b/.test(selector) && decls.some(({ prop }) => prop === "background"),
  );
  assert.deepEqual(
    dead.map(({ selector }) => selector),
    [],
    "a reduced-motion rule sets `background` on a working lamp the animation overrides",
  );
});

test("the lamp sweep is still configured at all", () => {
  // The positive half of the two negatives above: they would also pass on a
  // stylesheet that had deleted the animation outright.
  const working = rules(css).find(({ selector }) => selector === ".dot.working i");
  assert.ok(working, "lost the `.dot.working i` rule");
  assert.ok(
    working.decls.some(({ prop }) => prop === "animation-duration"),
    "`.dot.working i` no longer sets an animation-duration",
  );
  for (const name of ["lamp-a", "lamp-b", "lamp-c"]) {
    assert.match(css, new RegExp(`@keyframes\\s+${name}\\b`), `lost @keyframes ${name}`);
    assert.match(css, new RegExp(`animation-name:\\s*${name}\\b`), `nothing uses ${name}`);
  }
});

test("styles.css and indicators.ts agree on the lamp cycle", () => {
  // indicators.ts writes --race-light-delay as a phase offset into that same
  // cycle. If the two drift, every lamp on the page anchors to the wrong
  // phase, and no other test in this package compares them.
  const working = rules(css).find(({ selector }) => selector === ".dot.working i");
  assert.ok(working, "lost the `.dot.working i` rule");
  const declared = working.decls.find(({ prop }) => prop === "animation-duration");
  assert.ok(declared, "`.dot.working i` no longer sets an animation-duration");
  const cssSeconds = Number(declared.value.replace(/s$/, ""));
  assert.ok(Number.isFinite(cssSeconds), `unreadable duration: ${declared.value}`);

  const tsMatch = indicatorsText.match(/RACE_LIGHT_CYCLE_SECONDS\s*=\s*([\d.]+)/);
  assert.ok(tsMatch, "RACE_LIGHT_CYCLE_SECONDS is gone from indicators.ts");
  assert.equal(
    Number(tsMatch[1]),
    cssSeconds,
    `indicators.ts says ${tsMatch[1]}s, styles.css says ${cssSeconds}s`,
  );
});
